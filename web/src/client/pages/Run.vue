<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { ToolPanel, ToolPanelManager } from '@develon/webui-panel'
import { ApiError, localApi, type AliasDefInfo, type ServerRegInfo } from '../../shared/api'
import {
  consumeSse,
  decodeBase64Utf8,
  type ExecExitPayload,
  type ExecOutputPayload,
} from '../../shared/sse'
import { formatDuration, nowShort } from '../../shared/utils'

// ---------- 输出面板数据模型 ----------
type LineKind = 'stdout' | 'stderr' | 'sys'

interface OutLine {
  kind: LineKind
  text: string
}

interface RunItem {
  id: number
  title: string
  alias: string
  lines: OutLine[]
  running: boolean
  finished: boolean
  exitCode: number | null
  timeout: boolean
  truncated: boolean
  durationMs: number | null
}

const aliases = ref<Record<string, AliasDefInfo>>({})
const servers = ref<ServerRegInfo[]>([])
const alias = ref('')
const argsText = ref('')
const formError = ref('')
const runs = reactive<RunItem[]>([])
let nextId = 1

// ---------- 输出行缓冲 ----------
// SSE 的 Output 事件是管道任意分片（不保证按行分割），需按 \n 缓冲成完整行；
// \r 按终端覆盖语义处理（进度条回写）：\r 后的内容从行首覆盖，最终行 = overlay(基准, 新内容)；
// stdout/stderr 各自独立缓冲。
interface StreamBufState {
  acc: string // 当前未落行的新写入内容（\r 后的光标区）
  crBase: string | null // 最近一次 \r 时的行内容基准；null = 无待处理 \r
  openIdx: number | null // 正在构建的行在 lines 中的下标
}
const streamBufs = new Map<number, Map<string, StreamBufState>>()

/** 终端覆盖语义：over 从行首覆盖 base，超出部分保留 base 尾部 */
function overlay(base: string, over: string): string {
  return over.length >= base.length ? over : over + base.slice(over.length)
}

function displayOf(st: StreamBufState): string {
  return st.crBase != null ? overlay(st.crBase, st.acc) : st.acc
}

function bufStateFor(runId: number, stream: string): StreamBufState {
  let m = streamBufs.get(runId)
  if (!m) {
    m = new Map()
    streamBufs.set(runId, m)
  }
  let st = m.get(stream)
  if (!st) {
    st = { acc: '', crBase: null, openIdx: null }
    m.set(stream, st)
  }
  return st
}

function dropRunBuffers(runId: number) {
  streamBufs.delete(runId)
}

/** 将一块输出文本按行并入面板（跨块续行 + \r 覆盖语义） */
function feedOutput(item: RunItem, stream: 'stdout' | 'stderr', text: string) {
  const st = bufStateFor(item.id, stream)
  const flush = () => {
    const line = displayOf(st)
    if (st.openIdx != null && item.lines[st.openIdx]) item.lines[st.openIdx].text = line
    else item.lines.push({ kind: stream, text: line })
    st.acc = ''
    st.crBase = null
    st.openIdx = null
  }
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]
    if (ch === '\r') {
      if (text[i + 1] === '\n') {
        // CRLF：正常换行，内容保持不变
        i++
        flush()
      } else {
        // 单独 \r：光标回到行首，后续内容从行首覆盖
        st.crBase = st.crBase != null ? overlay(st.crBase, st.acc) : st.acc
        st.acc = ''
      }
    } else if (ch === '\n') {
      flush()
    } else {
      st.acc += ch
    }
  }
  // 行尾缓冲落到正在构建的行（无则新建；空内容不占行）
  const disp = displayOf(st)
  if (st.openIdx != null && item.lines[st.openIdx]) {
    item.lines[st.openIdx].text = disp
  } else if (disp !== '') {
    item.lines.push({ kind: stream, text: disp })
    st.openIdx = item.lines.length - 1
  }
}

const aliasNames = computed(() => Object.keys(aliases.value))

/** 别名绑定服务器的显示名（未绑定返回空串 = 默认服务器） */
function boundServerLabel(name: string): string {
  const url = aliases.value[name]?.server
  if (!url) return ''
  const s = servers.value.find((x) => x.server_url === url)
  return s?.hostname || url
}

// 面板初始位置按序号错开，避免完全重叠（之后仍可拖拽）
function panelStyle(id: number) {
  const step = (id - 1) % 8
  return { left: `${160 + step * 32}px`, top: `${90 + step * 28}px` }
}

async function load() {
  try {
    aliases.value = await localApi.getAliases()
    if (!alias.value && aliasNames.value.length) alias.value = aliasNames.value[0]
  } catch {
    /* 别名加载失败不阻塞手动输入 */
  }
  try {
    servers.value = (await localApi.getServers()).servers
  } catch {
    /* 服务器列表加载失败不影响执行 */
  }
}

// ---------- 执行（POST SSE） ----------
const controllers = new Map<number, AbortController>()
const bodyEls = new Map<number, HTMLElement>()

function registerBody(id: number, el: unknown) {
  if (el instanceof HTMLElement) bodyEls.set(id, el)
  else bodyEls.delete(id)
}

function scrollToBottom(id: number) {
  void nextTick(() => {
    const el = bodyEls.get(id)
    if (el) el.scrollTop = el.scrollHeight
  })
}

async function run() {
  if (!alias.value) {
    formError.value = '请先选择要执行的命令别名'
    return
  }
  formError.value = ''
  const args = argsText.value.trim() ? argsText.value.trim().split(/\s+/) : []

  const serverLabel = boundServerLabel(alias.value)
  const item = reactive<RunItem>({
    id: nextId++,
    title: `${alias.value} · ${nowShort()}`,
    alias: alias.value,
    lines: [
      {
        kind: 'sys',
        text: `▶ ${serverLabel ? `[${serverLabel}] ` : ''}${alias.value}${args.length ? ' ' + args.join(' ') : ''}`,
      },
    ],
    running: true,
    finished: false,
    exitCode: null,
    timeout: false,
    truncated: false,
    durationMs: null,
  })
  runs.push(item)
  argsText.value = ''

  const controller = new AbortController()
  controllers.set(item.id, controller)

  try {
    const res = await localApi.exec(item.alias, args, controller.signal)
    await consumeSse(res, (ev) => {
      if (ev.event === 'output') {
        const payload = JSON.parse(ev.data) as ExecOutputPayload
        if (payload.type !== 'output') return
        if (payload.stream === '__truncated__') {
          item.truncated = true
          item.lines.push({ kind: 'sys', text: '⚠ 输出超过 1MB，已被截断' })
        } else {
          feedOutput(item, payload.stream, decodeBase64Utf8(payload.data_b64))
        }
      } else if (ev.event === 'exit') {
        const payload = JSON.parse(ev.data) as ExecExitPayload
        item.exitCode = payload.exit_code
        item.timeout = payload.timeout
        item.durationMs = payload.duration_ms
        item.truncated = item.truncated || payload.truncated
        item.lines.push({
          kind: 'sys',
          text: `■ 进程退出，耗时 ${formatDuration(payload.duration_ms)}${payload.timeout ? '（超时终止）' : ''}`,
        })
        item.running = false
        item.finished = true
      }
      scrollToBottom(item.id)
    })
    // 流提前结束但没收到 exit 事件
    if (item.running) {
      item.running = false
      item.finished = true
    }
    dropRunBuffers(item.id)
  } catch (e) {
    item.running = false
    item.finished = true
    dropRunBuffers(item.id)
    const msg = e instanceof ApiError ? `请求失败 (HTTP ${e.status})：${e.message}` : String(e)
    item.lines.push({ kind: 'stderr', text: msg })
    scrollToBottom(item.id)
  } finally {
    controllers.delete(item.id)
  }
}

function closeRun(id: number) {
  controllers.get(id)?.abort()
  controllers.delete(id)
  dropRunBuffers(id)
  const i = runs.findIndex((r) => r.id === id)
  if (i >= 0) runs.splice(i, 1)
}

onMounted(load)

onBeforeUnmount(() => {
  // 离开页面时终止所有仍在执行的流
  controllers.forEach((c) => c.abort())
  controllers.clear()
})
</script>

<template>
  <div class="page">
    <h1 class="page-title">执行命令</h1>

    <section class="card">
      <h2 class="card-title">手动执行</h2>
      <div class="form-row">
        <select v-model="alias" class="select" style="width: 200px">
          <option v-for="n in aliasNames" :key="n" :value="n">{{ n }}</option>
        </select>
        <input
          v-model="argsText"
          class="input grow"
          type="text"
          placeholder="参数（以空格分隔，可留空）"
          @keyup.enter="run"
        />
        <button class="btn btn-primary" :disabled="!alias" @click="run">执行</button>
      </div>
      <p class="dim tip">
        参数将按空格拆分后传给后端，替换模板中的 <code>{args}</code> 占位符。
        每次执行会新开一个输出面板，可同时运行多个命令互不干扰；面板支持拖拽、缩放，最小化后可从底部任务栏恢复。
      </p>
      <p v-if="formError" class="error-text">{{ formError }}</p>
    </section>

    <p v-if="runs.length === 0" class="empty-tip">暂无执行任务，执行命令后此处将弹出输出面板</p>

    <!-- 输出面面板：每次执行一个新面板，stdout 白 / stderr 红 / 系统提示黄 -->
    <ToolPanel
      v-for="run in runs"
      :key="run.id"
      :title="run.title"
      :btns="['min', 'close']"
      :resizable="true"
      theme="dark"
      class="exec-panel"
      :style="panelStyle(run.id)"
      @close="closeRun(run.id)"
    >
      <div class="run-output" :ref="(el) => registerBody(run.id, el)">
        <pre v-for="(line, i) in run.lines" :key="i" class="line" :class="`line-${line.kind}`">{{ line.text }}</pre>
        <div v-if="run.finished" class="exit-row">
          <span class="exit-badge" :class="run.exitCode === 0 ? 'ok' : run.exitCode == null ? 'warn' : 'fail'">
            退出码: {{ run.exitCode ?? '未知' }}
          </span>
          <span v-if="run.timeout" class="exit-badge warn">已超时</span>
          <span v-if="run.truncated" class="exit-badge warn">输出已截断</span>
          <span class="dim">耗时 {{ formatDuration(run.durationMs) }}</span>
        </div>
        <div v-else class="dim running-tip">运行中…</div>
      </div>
    </ToolPanel>

    <!-- 底部任务栏：列出所有面板，支持最小化/还原与右键窗口管理 -->
    <ToolPanelManager v-if="runs.length > 0" theme="dark" mode="horizontal" />
  </div>
</template>

<style scoped lang="scss">
.page {
  display: flex;
  flex-direction: column;
  gap: 20px;
}

.page-title {
  font-size: 20px;
  margin: 0;
}

.form-row {
  display: flex;
  gap: 12px;
  align-items: center;
}

.grow {
  flex: 1;
}

.tip {
  margin: 10px 0 0;
  line-height: 1.8;
}

.tip code {
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 1px 6px;
}

.error-text {
  color: var(--danger);
  margin: 12px 0 0;
}

// 输出面板的初始尺寸（位置由 :style 错开）
:deep(.exec-panel) {
  width: 660px;
  height: 420px;
}

.run-output {
  height: 100%;
  overflow: auto;
  padding: 4px 2px;
}

.line {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-all;
  font-family: Consolas, 'JetBrains Mono', monospace;
  font-size: 13px;
  line-height: 1.5;

  &.line-stdout {
    color: #eaeef5; // stdout 白色
  }

  &.line-stderr {
    color: var(--stderr); // stderr 红色
  }

  &.line-sys {
    color: var(--warning); // 系统提示黄色
  }
}

.exit-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 10px;
  padding-top: 8px;
  border-top: 1px dashed var(--border);
  font-size: 13px;
}

.exit-badge {
  padding: 1px 10px;
  border-radius: 999px;
  border: 1px solid transparent;

  &.ok {
    color: var(--success);
    border-color: var(--success);
    background: rgba(52, 211, 153, 0.12);
  }

  &.fail {
    color: var(--danger);
    border-color: var(--danger);
    background: rgba(248, 113, 113, 0.12);
  }

  &.warn {
    color: var(--warning);
    border-color: var(--warning);
    background: rgba(251, 191, 36, 0.12);
  }
}

.running-tip {
  margin-top: 8px;
  font-size: 13px;
}
</style>
