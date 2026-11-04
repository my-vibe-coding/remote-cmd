/**
 * 目录上传引擎：目录收集（webkitdirectory 选择器 / 拖放 entry API 递归）→
 * 根目录探测（probe：已存在时经 onConflict 询问覆盖合并 / 目录级重命名）→
 * 并发队列（K=3 worker，每文件独立 relay WS）→ 客户端本地桥 → 远端 /api/upload/ws。
 *
 * 协议（与 server/routes/upload.rs 对应，帧经本地桥纯转发）：
 *   init {type:"upload", name, session, rel_path, resume, mkdir, probe, auto_rename, overwrite}
 *     → {"type":"ready"}     可发送数据
 *     → {"type":"exists"}    断点续传命中（批次重试跳过）
 *     → {"type":"mkdir_ok", path, name}   目录创建完成（path = 服务端确认绝对路径）
 *     → {"type":"dir_exists", path}       目录已存在（probe 模式，未创建）
 *     → {"type":"rejected", reason}
 *   Binary 帧 ×N（64KB 分块，bufferedAmount 背压）
 *   {type:"upload_end"} → {"type":"uploaded", path} / {"type":"rejected", reason}
 *
 * 语义（与 CLI 目录上传对齐）：
 * - 目录上传 = 合并 + 同名文件覆盖（overwrite=true），冲突时经 onConflict 询问；
 * - 重命名 = 目录级新建 root_N（不是内部逐文件改名）；
 * - rel_path 只含目录部分（不含文件名），服务端落地 = cwd/rel_path/name；
 * - 断点续传为批次级（服务端按会话清单跳过已成功文件）；单文件中断整文件重传。
 */

export interface UploadTask {
  file: File
  /** 目录相对路径（含所选目录根名，POSIX '/' 分隔，如 "mydir/sub/a.txt"）；拖入的单文件无根目录概念 */
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
  /** 服务端确认的根目录绝对路径（probe/创建结果，完成后展示） */
  rootPaths: string[]
}

export interface DropCollect {
  tasks: UploadTask[]
  /** 空目录（rel 含根名，mkdir 还原用） */
  emptyDirs: string[]
  /** 来自目录条目的根目录名（用于 probe/冲突询问；拖入的单文件不产生根） */
  roots: string[]
  /** true = 部分 item 无法转 entry（Safari 等），已按可解析项收集，提示降级 */
  partial: boolean
}

export interface DirRootConflict {
  root: string
  /** 服务端已存在的目录绝对路径 */
  path: string
}

export type ConflictChoice = 'overwrite' | 'rename'

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
export function collectFromInput(files: FileList): DropCollect {
  const tasks: UploadTask[] = []
  const roots = new Set<string>()
  for (const f of Array.from(files)) {
    const rel = (f as File & { webkitRelativePath?: string }).webkitRelativePath || f.name
    tasks.push({ file: f, rel })
    const i = rel.indexOf('/')
    if (i > 0) roots.add(rel.slice(0, i))
  }
  return { tasks, emptyDirs: [], roots: [...roots], partial: false }
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

/** 拖放收集：目录 entry 递归展开并登记根目录；entry API 不可用的 item 降级计为 partial */
export async function collectFromDrop(dt: DataTransfer): Promise<DropCollect> {
  const tasks: UploadTask[] = []
  const emptyDirs: string[] = []
  const roots = new Set<string>()
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
        // 拖入的裸文件无根目录概念：不登记 roots、不参与冲突询问
        tasks.push({ file: await entryFile(e), rel: e.fullPath.replace(/^\/+/, '') })
      } catch {
        /* ignore */
      }
    } else if (e.isDirectory) {
      roots.add(e.fullPath.replace(/^\/+/, ''))
      await walkDir(e, 0, tasks, emptyDirs)
    }
  }
  return { tasks, emptyDirs, roots: [...roots], partial }
}

// ---------- 目录级协议操作 ----------

function relayUrl(alias: string, session: string): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${location.host}/api/local/upload?alias=${encodeURIComponent(alias)}&session=${encodeURIComponent(session)}`
}

function openRelay(alias: string, session: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(relayUrl(alias, session))
    ws.binaryType = 'arraybuffer'
    const t = setTimeout(() => {
      ws.close()
      reject(new Error('relay 连接超时'))
    }, 15000)
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

/** 等待一条文本应答（arm 为发送动作；onclose/超时 → 空串） */
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

function parseReply(
  text: string,
): { type?: string; reason?: string; path?: string; name?: string; base?: string } | null {
  try {
    return JSON.parse(text)
  } catch {
    return null
  }
}

export interface DirSetupResult {
  /** 目录在操作前是否已存在 */
  existed: boolean
  /** 服务端确认的目录绝对路径 */
  path: string
  /** 实际相对路径（auto_rename 命中时为 root_N 形态） */
  actualRel: string
  /** 服务端解析的基准目录（cwd/uploads）：批次钉定用，后续请求回传 base_dir */
  base: string
}

/** 目录探测/创建（mkdir 帧协议，见服务端 upload.rs）： */
async function dirSetup(
  alias: string,
  session: string,
  relPath: string,
  baseDir: string,
  opts: { probe?: boolean; autoRename?: boolean },
): Promise<DirSetupResult> {
  const ws = await openRelay(alias, session)
  try {
    const first = await waitText(ws, 15000, () => {
      ws.send(
        JSON.stringify({
          type: 'upload',
          name: '',
          session,
          rel_path: relPath,
          mkdir: true,
          probe: opts.probe === true,
          auto_rename: opts.autoRename === true,
          base_dir: baseDir,
        }),
      )
    })
    const v = first ? parseReply(first) : null
    if (!v) throw new Error('目录操作无应答')
    if (v.type === 'mkdir_ok') {
      return { existed: false, path: v.path ?? '', actualRel: v.name ?? relPath, base: v.base ?? '' }
    }
    if (v.type === 'dir_exists') {
      return { existed: true, path: v.path ?? '', actualRel: relPath, base: v.base ?? '' }
    }
    if (v.type === 'rejected') throw new Error(v.reason ?? '目录操作被拒绝')
    throw new Error('目录操作应答异常')
  } finally {
    try {
      ws.close()
    } catch {
      /* ignore */
    }
  }
}

// ---------- 单文件上传 ----------

const CHUNK = 64 * 1024
const BACKPRESSURE = 8 * 1024 * 1024
const RESULT_TIMEOUT = 30000

/** 单文件上传：'ok' 成功、'exists' 续传跳过、'fail' 失败 */
async function uploadFile(
  alias: string,
  session: string,
  file: File,
  dir: string,
  name: string,
  overwrite: boolean,
  resume: boolean,
  baseDir: string,
  onSent: (sent: number) => void,
): Promise<'ok' | 'exists' | 'fail'> {
  let ws: WebSocket
  try {
    ws = await openRelay(alias, session)
  } catch {
    return 'fail'
  }
  try {
    const first = await waitText(ws, 15000, () => {
      ws.send(
        JSON.stringify({
          type: 'upload',
          name,
          session,
          rel_path: dir,
          resume,
          overwrite,
          base_dir: baseDir,
        }),
      )
    })
    const v = first ? parseReply(first) : null
    if (!v) return 'fail'
    if (v.type === 'exists') return 'exists'
    if (v.type !== 'ready') return 'fail'

    // 数据流：64KB 分块 + bufferedAmount 背压（发送侧水位即进度）
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

    const result = await waitText(ws, RESULT_TIMEOUT, () => {
      ws.send(JSON.stringify({ type: 'upload_end' }))
    })
    const rv = result ? parseReply(result) : null
    return rv?.type === 'uploaded' ? 'ok' : 'fail'
  } finally {
    try {
      ws.close()
    } catch {
      /* ignore */
    }
  }
}

// ---------- 批次引擎 ----------

type UploadTarget = UploadTask | { mkdir: string }
const isMkdirTask = (t: UploadTarget): t is { mkdir: string } => 'mkdir' in t

interface QueueItem {
  /** mkdir 伪任务为 -1（不进文件状态表） */
  idx: number
  target: UploadTarget
}

/** 根目录处理计划（probe + 冲突选择的结果） */
interface RootPlan {
  root: string
  actualRoot: string
  overwrite: boolean
  path: string
}

export class DirUploadEngine {
  private alias: string
  private session: string
  private tasks: UploadTask[]
  private roots: string[]
  private queue: QueueItem[] = []
  private files: FileState[]
  private cancelled = false
  private running = false
  private throttle: number | null = null
  private resolveBatch: ((s: BatchStats) => void) | null = null
  private onUpdate: (stats: BatchStats, files: FileState[]) => void
  /** 目录冲突询问回调（浏览器 UI 实现）；未提供时默认重命名 */
  private onConflict?: (c: DirRootConflict) => Promise<ConflictChoice>
  private rootPlans: RootPlan[] = []
  /** 根目录 rel → actualRoot 的映射（重命名后子路径前缀同步） */
  private remaps: Array<{ from: string; to: string }> = []
  /** 批次钉定的基准目录（首次目录操作应答捕获，整批固定防 shell cd 漂移） */
  private baseDir = ''
  rootPaths: string[] = []

  constructor(opts: {
    alias: string
    session: string
    tasks: UploadTask[]
    emptyDirs: string[]
    /** 来自目录条目的根目录名（probe/冲突询问用；拖入的裸文件不在其中） */
    roots: string[]
    onUpdate: (stats: BatchStats, files: FileState[]) => void
    onConflict?: (c: DirRootConflict) => Promise<ConflictChoice>
  }) {
    this.alias = opts.alias
    this.session = opts.session
    this.tasks = opts.tasks
    this.roots = opts.roots
    this.onConflict = opts.onConflict
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
      rootPaths: this.rootPaths,
    }
  }

  private notify() {
    if (this.throttle != null) return
    this.throttle = window.setTimeout(() => {
      this.throttle = null
      this.onUpdate(this.stats(), this.files.map((f) => ({ ...f })))
    }, 200)
  }

  /** 根目录 rel → actualRoot 的前缀映射（重命名后子路径同步） */
  private remap(rel: string): string {
    for (const m of this.remaps) {
      if (rel === m.from || rel.startsWith(m.from + '/')) {
        return m.to + rel.slice(m.from.length)
      }
    }
    return rel
  }

  /** 每轮批量执行前的准备：逐根目录 probe →（冲突时询问）→ 记录映射。
   *  首次目录操作捕获服务端解析的 base，整批钉定（防 shell cd 导致文件漂移） */
  private async prepare(): Promise<void> {
    for (const root of this.roots) {
      try {
        const probe = await dirSetup(this.alias, this.session, root, this.baseDir, { probe: true })
        if (this.baseDir === '' && probe.base) this.baseDir = probe.base
        if (!probe.existed) {
          this.rootPlans.push({ root, actualRoot: root, overwrite: false, path: probe.path })
          continue
        }
        let choice: ConflictChoice = 'rename'
        if (this.onConflict) choice = await this.onConflict({ root, path: probe.path })
        if (choice === 'overwrite') {
          this.rootPlans.push({ root, actualRoot: root, overwrite: true, path: probe.path })
        } else {
          const created = await dirSetup(this.alias, this.session, root, this.baseDir, { autoRename: true })
          if (this.baseDir === '' && created.base) this.baseDir = created.base
          this.rootPlans.push({ root, actualRoot: created.actualRel, overwrite: false, path: created.path })
          this.remaps.push({ from: root, to: created.actualRel })
        }
      } catch (e) {
        // 探测失败降级：按重命名语义直接上传（不阻塞批次）
        this.rootPlans.push({ root, actualRoot: root, overwrite: false, path: '' })
        console.warn(`[upload-dir] 根目录 ${root} 探测失败，按重命名语义继续`, e)
      }
    }
    this.rootPaths = this.rootPlans.map((p) => p.path)
  }

  /** 目标 rel（已 remap）的覆盖语义：命中根目录计划则跟随，裸文件默认重命名 */
  private overwriteFor(remappedRel: string): boolean {
    const plan = this.rootPlans.find((p) => remappedRel === p.actualRoot || remappedRel.startsWith(p.actualRoot + '/'))
    return plan?.overwrite ?? false
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
        const resume = attempt > 0
        const onSent =
          item.idx >= 0
            ? (sent: number) => {
                this.files[item.idx].sent = sent
                this.notify()
              }
            : () => {}
        if (isMkdirTask(item.target)) {
          const dir = this.remap(item.target.mkdir)
          res = await dirSetup(this.alias, this.session, dir, this.baseDir, {})
            .then(() => 'ok' as const)
            .catch(() => 'fail' as const)
        } else {
          const rel = this.remap(item.target.rel)
          const i = rel.lastIndexOf('/')
          const dir = i < 0 ? '' : rel.slice(0, i)
          const name = rel.slice(i + 1)
          res = await uploadFile(
            this.alias,
            this.session,
            item.target.file,
            dir,
            name,
            this.overwriteFor(rel),
            resume,
            this.baseDir,
            onSent,
          )
        }
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
    await this.prepare()
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
