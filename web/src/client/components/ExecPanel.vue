<script setup lang="ts">
/**
 * 执行输出面板：命令执行的统一输出展示组件（可复用）。
 *
 * 两种渲染模式（互斥，由 mode 决定，生命周期内不变）：
 * - lines    ：行缓冲只读视图 —— 输出按 \n 分行、单独 \r 按终端覆盖语义回写
 *              （进度条场景），stdout 白 / stderr 红 / 系统提示黄，自动滚动到底部。
 * - terminal ：交互终端视图 —— xterm.js 渲染 PTY 原始字节（完整 ANSI/全屏程序支持）。
 *
 * 解耦边界：组件只负责「展示 + 行缓冲/终端实例状态」，不感知任何连接细节；
 * SSE/WebSocket 会话由父级管理，通过 defineExpose 方法喂数据，
 * 通过事件（term-input / term-resize / ready）上报终端交互。
 */
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import { ToolPanel } from '@develon/webui-panel'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { deep as xtermTheme } from 'xterm-theme'
import '@xterm/xterm/css/xterm.css'
import { formatDuration } from '../../shared/utils'

// ---------- 类型 ----------
type LineKind = 'stdout' | 'stderr' | 'sys'

interface OutLine {
  kind: LineKind
  text: string
}

/** 终态载荷（结构与 /api/local/exec 的 exit 事件一致，便于父级直接透传） */
interface PanelExit {
  exit_code: number | null
  timeout: boolean
  duration_ms: number
  truncated: boolean
}

/** 终端列行数（ready / term-resize 事件载荷） */
interface TermSize {
  cols: number
  rows: number
}

const props = defineProps<{
  /** 面板标题（标题栏文本） */
  title: string
  /** 渲染模式：lines=行缓冲只读视图；terminal=xterm 交互终端 */
  mode: 'lines' | 'terminal'
  /** lines 模式的首行系统提示（如 ▶ 命令回显）；terminal 模式忽略 */
  initialSysLine?: string
}>()

const emit = defineEmits<{
  /** 标题栏关闭按钮点击 */
  close: []
  /** terminal 模式：终端就绪（已按容器尺寸自适应），父级据此建立连接 */
  ready: [size: TermSize]
  /** terminal 模式：用户键入（xterm onData 原文），父级转发到远端 stdin */
  'term-input': [data: string]
  /** terminal 模式：容器尺寸变化导致行列数变化，父级转发 resize 到远端 PTY */
  'term-resize': [size: TermSize]
}>()

// ---------- 会话终态 ----------
interface ExitState {
  code: number | null
  timeout: boolean
  truncated: boolean
  durationMs: number | null
}

const finished = ref(false)
const exit = ref<ExitState | null>(null)

/**
 * 标记会话终态（幂等：exit 事件与异常断开可能双触发，后者不再覆盖前者）。
 * payload 为 null 表示异常结束（无退出码）；lines 模式补一条 ■ 系统行（与原行为一致）。
 */
function finish(payload: PanelExit | null) {
  if (finished.value) return
  finished.value = true
  exit.value = payload
    ? {
        code: payload.exit_code,
        timeout: payload.timeout,
        truncated: payload.truncated,
        durationMs: payload.duration_ms,
      }
    : { code: null, timeout: false, truncated: false, durationMs: null }
  if (props.mode === 'lines') {
    lines.value.push({
      kind: 'sys',
      text: `■ 进程退出，耗时 ${formatDuration(exit.value.durationMs)}${exit.value.timeout ? '（超时终止）' : ''}`,
    })
    scrollToBottom()
  }
}

// ---------- lines 模式：输出行缓冲 ----------
// 输出是管道任意分片（不保证按行分割），需按 \n 缓冲成完整行；
// \r 按终端覆盖语义处理（进度条回写）：\r 后的内容从行首覆盖；
// stdout/stderr 各自独立缓冲。
interface StreamBufState {
  acc: string // 当前未落行的新写入内容（\r 后的光标区）
  crBase: string | null // 最近一次 \r 时的行内容基准；null = 无待处理 \r
  openIdx: number | null // 正在构建的行在 lines 中的下标
}

const lines = ref<OutLine[]>([])
const streamBufs = new Map<string, StreamBufState>()

if (props.initialSysLine && props.mode === 'lines') {
  lines.value.push({ kind: 'sys', text: props.initialSysLine })
}

/** 终端覆盖语义：over 从行首覆盖 base，超出部分保留 base 尾部 */
function overlay(base: string, over: string): string {
  return over.length >= base.length ? over : over + base.slice(over.length)
}

function displayOf(st: StreamBufState): string {
  return st.crBase != null ? overlay(st.crBase, st.acc) : st.acc
}

function bufStateFor(kind: 'stdout' | 'stderr'): StreamBufState {
  let st = streamBufs.get(kind)
  if (!st) {
    st = { acc: '', crBase: null, openIdx: null }
    streamBufs.set(kind, st)
  }
  return st
}

const bodyEl = ref<HTMLElement | null>(null)

/** 视图滚动到底部（DOM 更新后执行） */
function scrollToBottom() {
  void nextTick(() => {
    const el = bodyEl.value
    if (el) el.scrollTop = el.scrollHeight
  })
}

/** 将一块输出文本按行并入视图（跨块续行 + \r 覆盖语义） */
function feed(kind: 'stdout' | 'stderr', text: string) {
  if (props.mode !== 'lines') return
  const st = bufStateFor(kind)
  const flush = () => {
    const line = displayOf(st)
    if (st.openIdx != null && lines.value[st.openIdx]) lines.value[st.openIdx].text = line
    else lines.value.push({ kind, text: line })
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
  if (st.openIdx != null && lines.value[st.openIdx]) {
    lines.value[st.openIdx].text = disp
  } else if (disp !== '') {
    lines.value.push({ kind, text: disp })
    st.openIdx = lines.value.length - 1
  }
  scrollToBottom()
}

/** 追加一条系统提示行 */
function sys(text: string) {
  if (props.mode !== 'lines') return
  lines.value.push({ kind: 'sys', text })
  scrollToBottom()
}

/** 标记输出截断（⚠ 系统行 + 退出徽标） */
function markTruncated() {
  exit.value = exit.value ?? { code: null, timeout: false, truncated: false, durationMs: null }
  exit.value.truncated = true
  sys('⚠ 输出超过 1MB，已被截断')
}

// ---------- terminal 模式：xterm 交互终端 ----------
const termHost = ref<HTMLElement | null>(null)
let term: Terminal | null = null
let fit: FitAddon | null = null
let ro: ResizeObserver | null = null

onMounted(() => {
  if (props.mode !== 'terminal' || !termHost.value) return
  term = new Terminal({
    fontFamily: "Consolas, 'JetBrains Mono', monospace",
    fontSize: 14,
    cursorBlink: true,
    theme: xtermTheme, // xterm-theme 的 Deep 主题（纯黑底高对比）
  })
  fit = new FitAddon()
  term.loadAddon(fit)
  term.open(termHost.value)
  if (termHost.value.clientWidth > 0) fit.fit()

  // 键入 / 尺寸变化上报父级（连接细节不在组件内）
  term.onData((d) => emit('term-input', d))
  term.onResize(({ cols, rows }) => emit('term-resize', { cols, rows }))

  // 面板尺寸变化 → 自适应列行数 → 触发 onResize → 父级通知远端 PTY
  ro = new ResizeObserver(() => {
    if (termHost.value && termHost.value.clientWidth > 0 && termHost.value.clientHeight > 0) {
      fit?.fit()
    }
  })
  ro.observe(termHost.value)

  emit('ready', { cols: term.cols, rows: term.rows })
})

onBeforeUnmount(() => {
  ro?.disconnect()
  term?.dispose()
  term = null
})

/** terminal 模式写入原始输出（二进制或含 ANSI 的字符串） */
function termWrite(data: Uint8Array | string) {
  term?.write(data)
}

// ---------- 对父级的命令式接口 ----------
defineExpose({
  /** 追加一块 stdout/stderr 文本（lines 模式） */
  feed,
  /** 追加系统提示行（lines 模式） */
  sys,
  /** 标记输出截断 */
  markTruncated,
  /** 标记会话终态（幂等） */
  finish,
  /** terminal 模式写入原始输出 */
  termWrite,
})
</script>

<template>
  <ToolPanel
    :title="title"
    :btns="['min', 'close']"
    :resizable="true"
    theme="dark"
    class="exec-panel"
    @close="emit('close')"
  >
    <!-- lines 模式：行缓冲视图 -->
    <div v-if="mode === 'lines'" ref="bodyEl" class="run-output">
      <pre
        v-for="(line, i) in lines"
        :key="i"
        class="line"
        :class="`line-${line.kind}`"
      >{{ line.text }}</pre>
      <div v-if="!finished" class="dim running-tip">运行中…</div>
    </div>
    <!-- terminal 模式：xterm 挂载点 -->
    <div v-else ref="termHost" class="run-term"></div>

    <!-- 终态徽标（两种模式共用） -->
    <div v-if="finished && exit" class="exit-row">
      <span class="exit-badge" :class="exit.code === 0 ? 'ok' : exit.code == null ? 'warn' : 'fail'">
        退出码: {{ exit.code ?? '未知' }}
      </span>
      <span v-if="exit.timeout" class="exit-badge warn">已超时</span>
      <span v-if="exit.truncated" class="exit-badge warn">输出已截断</span>
      <span class="dim">耗时 {{ formatDuration(exit.durationMs) }}</span>
    </div>
  </ToolPanel>
</template>

<style scoped lang="scss">
.exec-panel {
  width: 50vw;
  display: flex;
  flex-direction: column;
  padding: 4px 10px 8px;
  box-sizing: border-box;
}

.run-output {
  flex: 1;
  min-height: 0;
  overflow: auto;
  padding: 4px 2px;
}

.run-term {
  flex: 1;
  height: 100%;
  min-height: 0;

  :deep(.xterm) {
    height: 100%;
  }
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
