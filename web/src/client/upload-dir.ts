/**
 * 目录上传引擎：目录收集（webkitdirectory 选择器 / 拖放 entry API 递归）→
 * 并发队列（K=3 worker，每文件独立 relay WS）→ 客户端本地桥 → 远端 /api/upload/ws。
 *
 * 协议（与 server/routes/upload.rs 对应，帧经本地桥纯转发）：
 *   init {type:"upload", name, session, rel_path, resume, mkdir}
 *     → {"type":"ready"}   可发送数据
 *     → {"type":"exists"}  断点续传命中（批次重试跳过）
 *     → {"type":"mkdir_ok"} 空目录还原完成
 *   Binary 帧 ×N（64KB 分块，bufferedAmount 背压）
 *   {type:"upload_end"} → {"type":"uploaded", path} / {"type":"rejected", reason}
 *
 * 断点续传为批次级：服务端按会话维护已成功文件清单，重试批次 resume=true 命中即跳过；
 * 单文件中断整文件重传（服务端异常收尾自动清理残留）。
 */

export interface UploadTask {
  file: File
  /** 目录相对路径（含所选目录根名，POSIX '/' 分隔，如 "mydir/sub/a.txt"） */
  rel: string
}

export type FileStatus = 'pending' | 'uploading' | 'ok' | 'exists' | 'failed'

export interface FileState {
  rel: string
  size: number
  sent: number
  status: FileStatus
  error?: string
}

export interface BatchStats {
  total: number
  /** ok + exists */
  done: number
  failed: number
  bytesTotal: number
  bytesSent: number
  running: boolean
}

export interface DropCollect {
  tasks: UploadTask[]
  /** 空目录（rel 含根名，mkdir 还原用） */
  emptyDirs: string[]
  /** true = 部分 item 无法转 entry（Safari 等），已按可解析项收集，提示降级 */
  partial: boolean
}

// ---------- 能力检测 ----------

/** 目录选择器（webkitdirectory 为事实标准，Chrome/Edge/FF50+/Safari11.1+） */
export function supportsDirInput(): boolean {
  const i = document.createElement('input')
  return 'webkitdirectory' in i
}

/** 拖放目录 entry API */
export function supportsEntryApi(): boolean {
  return (
    typeof DataTransferItem !== 'undefined' &&
    'webkitGetAsEntry' in DataTransferItem.prototype
  )
}

// ---------- 收集 ----------

/** 目录选择器：File.webkitRelativePath 即相对路径（含根目录名） */
export function collectFromInput(files: FileList): UploadTask[] {
  return Array.from(files).map((f) => ({
    file: f,
    rel: (f as File & { webkitRelativePath?: string }).webkitRelativePath || f.name,
  }))
}

/** 拖放 entry 的最小形状（标准 FileSystemEntry 的 webkit 前缀实现） */
interface FsEntry {
  isFile: boolean
  isDirectory: boolean
  fullPath: string
  file(cb: (f: File) => void, err?: (e: unknown) => void): void
  createReader(): { readEntries(cb: (es: FsEntry[]) => void, err?: (e: unknown) => void): void }
}

const WALK_DEPTH_MAX = 32
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms))

/** readEntries 分页读取：每批最多 100 条，必须循环至空批（否则 Chrome 丢项） */
function readAllEntries(dir: FsEntry): Promise<FsEntry[]> {
  return new Promise((resolve, reject) => {
    const reader = dir.createReader()
    const all: FsEntry[] = []
    const step = () => {
      reader.readEntries((batch) => {
        if (batch.length === 0) return resolve(all)
        all.push(...batch)
        step()
      }, reject)
    }
    step()
  })
}

function entryFile(entry: FsEntry): Promise<File> {
  return new Promise((resolve, reject) => entry.file(resolve, reject))
}

/** 递归收集目录树（深度限 32 层防病态结构/循环链接）；完全空的目录记入 emptyDirs */
async function walkDir(
  dir: FsEntry,
  depth: number,
  tasks: UploadTask[],
  emptyDirs: string[],
): Promise<void> {
  if (depth > WALK_DEPTH_MAX) return
  const entries = await readAllEntries(dir)
  if (entries.length === 0) {
    emptyDirs.push(dir.fullPath.replace(/^\/+/, ''))
    return
  }
  for (const e of entries) {
    if (e.isFile) {
      try {
        tasks.push({ file: await entryFile(e), rel: e.fullPath.replace(/^\/+/, '') })
      } catch {
        /* 单文件读取失败（权限等）忽略 */
      }
    } else if (e.isDirectory) {
      await walkDir(e, depth + 1, tasks, emptyDirs)
    }
  }
}

/** 拖放收集：目录 entry 递归展开；entry API 不可用的 item 降级计为 partial */
export async function collectFromDrop(dt: DataTransfer): Promise<DropCollect> {
  const tasks: UploadTask[] = []
  const emptyDirs: string[] = []
  let partial = false
  const entries: FsEntry[] = []
  for (const it of Array.from(dt.items || [])) {
    if (it.kind !== 'file') continue
    // webkitGetAsEntry 的标准类型缺 file/createReader 方法，按实际实现断言
    const entry = it.webkitGetAsEntry?.() as FsEntry | null | undefined
    if (entry) entries.push(entry)
    else partial = true
  }
  for (const e of entries) {
    if (e.isFile) {
      try {
        tasks.push({ file: await entryFile(e), rel: e.fullPath.replace(/^\/+/, '') })
      } catch {
        /* ignore */
      }
    } else if (e.isDirectory) {
      await walkDir(e, 0, tasks, emptyDirs)
    }
  }
  return { tasks, emptyDirs, partial }
}

// ---------- 单文件/单目录上传 ----------

const CHUNK = 64 * 1024
const BACKPRESSURE = 8 * 1024 * 1024
const OPEN_TIMEOUT = 15000
const RESULT_TIMEOUT = 30000

function relayUrl(alias: string, session: string): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${location.host}/api/local/upload?alias=${encodeURIComponent(alias)}&session=${encodeURIComponent(session)}`
}

function splitRel(rel: string): { dir: string; name: string } {
  const i = rel.lastIndexOf('/')
  return i < 0 ? { dir: '', name: rel } : { dir: rel.slice(0, i), name: rel.slice(i + 1) }
}

function openRelay(alias: string, session: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(relayUrl(alias, session))
    ws.binaryType = 'arraybuffer'
    const t = setTimeout(() => {
      ws.close()
      reject(new Error('relay 连接超时'))
    }, OPEN_TIMEOUT)
    ws.onopen = () => {
      clearTimeout(t)
      resolve(ws)
    }
    ws.onerror = () => {
      clearTimeout(t)
      reject(new Error('relay 连接失败'))
    }
  })
}

/** 等待一条文本应答（onmessage 替换为单发等待；onclose/超时 → 空串） */
function waitText(ws: WebSocket, timeout: number, arm: () => void): Promise<string> {
  return new Promise((resolve) => {
    let settled = false
    const done = (s: string) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      resolve(s)
    }
    const timer = setTimeout(() => done(''), timeout)
    ws.onmessage = (e) => {
      if (typeof e.data === 'string') done(e.data)
    }
    ws.onclose = () => done('')
    arm()
  })
}

type UploadTarget = UploadTask | { mkdir: string }
const isMkdirTask = (t: UploadTarget): t is { mkdir: string } => 'mkdir' in t

/**
 * 单文件/单目录上传：'ok' 成功、'exists' 续传跳过、'fail' 失败。
 * resume 用于批次重试（服务端清单命中跳过）；attempt>0 自动启用。
 */
async function uploadOne(
  alias: string,
  session: string,
  target: UploadTarget,
  resume: boolean,
  onSent: (sent: number) => void,
): Promise<'ok' | 'exists' | 'fail'> {
  let ws: WebSocket
  try {
    ws = await openRelay(alias, session)
  } catch {
    return 'fail'
  }
  try {
    const mkdir = isMkdirTask(target)
    const { dir, name } = mkdir ? { dir: target.mkdir, name: '' } : splitRel(target.rel)

    // init → ready / exists / mkdir_ok
    const first = await waitText(ws, OPEN_TIMEOUT, () => {
      ws.send(
        JSON.stringify({
          type: 'upload',
          name,
          session,
          rel_path: dir,
          resume,
          mkdir,
        }),
      )
    })
    if (!first) return 'fail'
    let v: { type?: string; reason?: string }
    try {
      v = JSON.parse(first)
    } catch {
      return 'fail'
    }
    if (v.type === 'exists') return 'exists'
    if (v.type === 'mkdir_ok') return 'ok'
    if (v.type !== 'ready') return 'fail'
    if (mkdir) {
      ws.close()
      return 'ok'
    }

    // 数据流：64KB 分块 + bufferedAmount 背压（发送侧水位即进度）
    const file = target.file
    let offset = 0
    while (offset < file.size) {
      if (ws.readyState !== WebSocket.OPEN) return 'fail'
      if (ws.bufferedAmount > BACKPRESSURE) {
        await sleep(50)
        continue
      }
      const end = Math.min(offset + CHUNK, file.size)
      ws.send(file.slice(offset, end))
      offset = end
      onSent(offset)
    }

    // upload_end → uploaded / rejected
    const result = await waitText(ws, RESULT_TIMEOUT, () => {
      ws.send(JSON.stringify({ type: 'upload_end' }))
    })
    if (result) {
      try {
        const rv = JSON.parse(result)
        if (rv.type === 'uploaded') return 'ok'
        if (rv.type === 'rejected') return 'fail'
      } catch {
        /* ignore */
      }
    }
    return 'fail'
  } finally {
    try {
      ws.close()
    } catch {
      /* ignore */
    }
  }
}

// ---------- 批次引擎 ----------

interface QueueItem {
  /** mkdir 伪任务为 -1（不进文件状态表） */
  idx: number
  target: UploadTarget
}

export class DirUploadEngine {
  private alias: string
  private session: string
  private tasks: UploadTask[]
  private queue: QueueItem[] = []
  private files: FileState[]
  private cancelled = false
  private running = false
  private throttle: number | null = null
  private resolveBatch: ((s: BatchStats) => void) | null = null
  private onUpdate: (stats: BatchStats, files: FileState[]) => void

  constructor(opts: {
    alias: string
    session: string
    tasks: UploadTask[]
    emptyDirs: string[]
    onUpdate: (stats: BatchStats, files: FileState[]) => void
  }) {
    this.alias = opts.alias
    this.session = opts.session
    this.tasks = opts.tasks
    this.onUpdate = opts.onUpdate
    this.files = opts.tasks.map((t) => ({ rel: t.rel, size: t.file.size, sent: 0, status: 'pending' }))
    // 空目录还原排最前，随后文件任务
    this.queue = [
      ...opts.emptyDirs.map((d) => ({ idx: -1, target: { mkdir: d } as UploadTarget })),
      ...opts.tasks.map((t, i) => ({ idx: i, target: t as UploadTarget })),
    ]
  }

  stats(): BatchStats {
    let done = 0
    let failed = 0
    let bytesTotal = 0
    let bytesSent = 0
    for (const f of this.files) {
      if (f.status === 'ok' || f.status === 'exists') done++
      if (f.status === 'failed') failed++
      bytesTotal += f.size
      bytesSent += f.sent
    }
    return {
      total: this.files.length,
      done,
      failed,
      bytesTotal,
      bytesSent,
      running: this.running,
    }
  }

  private notify() {
    if (this.throttle != null) return
    this.throttle = window.setTimeout(() => {
      this.throttle = null
      this.onUpdate(this.stats(), this.files.map((f) => ({ ...f })))
    }, 200)
  }

  private async worker() {
    while (!this.cancelled) {
      const item = this.queue.shift()
      if (!item) return
      if (item.idx >= 0) {
        const st = this.files[item.idx]
        if (st.status !== 'pending') continue // 已取消/已重排
        st.status = 'uploading'
        this.notify()
      }
      // 失败重试 3 次（500ms/1s/2s 退避），attempt>0 启用服务端清单续传
      let res: 'ok' | 'exists' | 'fail' = 'fail'
      for (let attempt = 0; attempt < 3 && !this.cancelled; attempt++) {
        if (attempt > 0) await sleep(500 * 2 ** (attempt - 1))
        const onSent =
          item.idx >= 0
            ? (sent: number) => {
                this.files[item.idx].sent = sent
                this.notify()
              }
            : () => {}
        res = await uploadOne(this.alias, this.session, item.target, attempt > 0, onSent)
        if (res !== 'fail') break
      }
      if (item.idx >= 0) {
        const st = this.files[item.idx]
        st.sent = res === 'ok' ? st.size : st.sent
        if (this.cancelled && res === 'fail') {
          st.status = 'pending' // 取消：未完成项回队列待"重传失败项"
          st.sent = 0
        } else {
          st.status = res === 'fail' ? 'failed' : res === 'exists' ? 'exists' : 'ok'
        }
      }
      this.notify()
    }
  }

  private async run(): Promise<void> {
    if (this.running) return
    this.running = true
    this.cancelled = false
    const workers = Array.from({ length: 3 }, () => this.worker())
    await Promise.all(workers)
    this.running = false
    this.resolveBatch?.(this.stats())
    this.resolveBatch = null
    this.notify()
  }

  /** 启动批次（空目录选择时队列为空，立即完成）；返回终态统计 */
  start(): Promise<BatchStats> {
    return new Promise((resolve) => {
      if (this.queue.length === 0) {
        const s = this.stats()
        this.onUpdate(s, [])
        resolve(s)
        return
      }
      this.resolveBatch = resolve
      void this.run()
    })
  }

  /** 取消：在途文件回置 pending，未派发任务保留在队列（可 retryFailed 继续） */
  cancel() {
    this.cancelled = true
    for (const f of this.files) {
      if (f.status === 'uploading') {
        f.status = 'pending'
        f.sent = 0
      }
    }
    this.notify()
  }

  /** 重传失败/被取消的文件（resume=true，服务端清单命中自动跳过） */
  retryFailed(): Promise<BatchStats> {
    if (this.running) return Promise.resolve(this.stats())
    this.queue = this.tasks
      .map((t, i) => ({ idx: i, target: t as UploadTarget }))
      .filter(({ idx }) => this.files[idx].status === 'failed' || this.files[idx].status === 'pending')
    for (const f of this.files) {
      if (f.status === 'failed') {
        f.status = 'pending'
        f.sent = 0
        f.error = undefined
      }
    }
    return this.start()
  }
}
