<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { ToolPanelManager } from '@develon/webui-panel'
import ExecPanel from '../components/ExecPanel.vue'
import { ApiError, localApi, type AliasDefInfo, type ServerRegInfo } from '../../shared/api'
import { consumeSse, decodeBase64Utf8, type ExecOutputPayload } from '../../shared/sse'
import { nowShort } from '../../shared/utils'

// ---------- 会话数据模型 ----------
// 展示状态（输出行缓冲、终端实例、终态徽标）全部内聚在 ExecPanel 组件内，
// 页面只持有会话元信息与连接句柄（SSE AbortController / PTY WebSocket）。
interface RunSession {
  id: number
  title: string
  alias: string
  args: string[]
  /** 交互终端模式（PTY WS 桥）；否则为 SSE 只读流 */
  pty: boolean
  /** 首行系统提示（▶ 命令回显，仅只读模式显示） */
  sysLine: string
}

const aliases = ref<Record<string, AliasDefInfo>>({})
const servers = ref<ServerRegInfo[]>([])
const alias = ref('')
const argsText = ref('')
const formError = ref('')
/** 交互模式开关（PTY 终端）；关闭则走 SSE 只读流 */
const ptyMode = ref(true)
const runs = reactive<RunSession[]>([])
let nextId = 1

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

// ---------- 面板句柄与连接句柄 ----------
type PanelApi = InstanceType<typeof ExecPanel>

const panels = new Map<number, PanelApi>()
const controllers = new Map<number, AbortController>()
const ptySockets = new Map<number, WebSocket>()

/** v-for 动态 ref：面板卸载时自动清理 */
function registerPanel(id: number, el: unknown) {
  if (el) panels.set(id, el as PanelApi)
  else panels.delete(id)
}

const enc = new TextEncoder()

// ---------- 执行入口 ----------
async function run() {
  if (!alias.value) {
    formError.value = '请先选择要执行的命令别名'
    return
  }
  formError.value = ''
  const args = argsText.value.trim() ? argsText.value.trim().split(/\s+/) : []

  const serverLabel = boundServerLabel(alias.value)
  const item: RunSession = {
    id: nextId++,
    title: `${alias.value} · ${nowShort()}`,
    alias: alias.value,
    args,
    pty: ptyMode.value,
    sysLine: `▶ ${serverLabel ? `[${serverLabel}] ` : ''}${alias.value}${args.length ? ' ' + args.join(' ') : ''}`,
  }
  runs.push(item)
  // 参数保留不清空：便于连续执行同一命令（微调参数或重复执行）
  // PTY 模式的 WS 连接在面板 ready 事件后建立（需要终端自适应出的列行数）
  if (!item.pty) startSse(item)
}

// ---------- SSE 只读会话（管道模式，无 stdin 交互） ----------
async function startSse(item: RunSession) {
  const panel = () => panels.get(item.id)
  const controller = new AbortController()
  controllers.set(item.id, controller)

  try {
    const res = await localApi.exec(item.alias, item.args, controller.signal)
    await consumeSse(res, (ev) => {
      if (ev.event === 'output') {
        const payload = JSON.parse(ev.data) as ExecOutputPayload
        if (payload.type !== 'output') return
        if (payload.stream === '__truncated__') {
          panel()?.markTruncated()
        } else {
          panel()?.feed(payload.stream, decodeBase64Utf8(payload.data_b64))
        }
      } else if (ev.event === 'exit') {
        panel()?.finish(JSON.parse(ev.data))
      }
    })
    // 流提前结束但没收到 exit 事件
    panel()?.finish(null)
  } catch (e) {
    const msg = e instanceof ApiError ? `请求失败 (HTTP ${e.status})：${e.message}` : String(e)
    panel()?.feed('stderr', msg)
    panel()?.finish(null)
  } finally {
    controllers.delete(item.id)
  }
}

// ---------- PTY 交互会话（WS 桥，协议见 routes.rs exec_pty） ----------

/** terminal 就绪：按自适应出的列行数建立 WS 连接 */
function openPtySession(item: RunSession, size: { cols: number; rows: number }) {
  const panel = panels.get(item.id)
  if (!panel) return

  const qs = new URLSearchParams()
  qs.set('alias', item.alias)
  for (const a of item.args) qs.append('args', a)
  qs.set('cols', String(size.cols))
  qs.set('rows', String(size.rows))
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  const ws = new WebSocket(`${proto}://${location.host}/api/local/exec-pty?${qs}`)
  ws.binaryType = 'arraybuffer'
  ptySockets.set(item.id, ws)

  ws.onmessage = (e) => {
    if (typeof e.data === 'string') {
      try {
        const v = JSON.parse(e.data)
        if (v.type === 'exit') panel.finish(v)
        else if (v.type === 'error') {
          panel.termWrite(`\r\n\x1b[31m${v.message ?? '连接失败'}\x1b[0m\r\n`)
          panel.finish(null)
        }
      } catch {
        /* 忽略非 JSON 文本帧 */
      }
    } else {
      panel.termWrite(new Uint8Array(e.data as ArrayBuffer))
    }
  }
  // finish 幂等：已收到 exit 则此处无操作；异常断开则标记终态
  ws.onclose = () => panel.finish(null)
  ws.onerror = () => panel.termWrite('\r\n\x1b[31m[连接中断]\x1b[0m\r\n')
}

/** 终端键入 → 远端 stdin */
function sendPtyInput(id: number, data: string) {
  const ws = ptySockets.get(id)
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(enc.encode(data))
}

/** 终端尺寸变化 → 远端 PTY resize */
function sendPtyResize(id: number, size: { cols: number; rows: number }) {
  const ws = ptySockets.get(id)
  if (ws && ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify({ type: 'resize', cols: size.cols, rows: size.rows }))
  }
}

// ---------- 会话关闭 ----------
function closeRun(id: number) {
  controllers.get(id)?.abort()
  controllers.delete(id)
  ptySockets.get(id)?.close()
  ptySockets.delete(id)
  const i = runs.findIndex((r) => r.id === id)
  if (i >= 0) runs.splice(i, 1) // 面板卸载时组件自清理终端实例
}

onMounted(load)

onBeforeUnmount(() => {
  // 离开页面时终止所有仍在执行的会话
  controllers.forEach((c) => c.abort())
  controllers.clear()
  ptySockets.forEach((ws) => ws.close())
  ptySockets.clear()
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
        <label class="pty-toggle dim">
          <input v-model="ptyMode" type="checkbox" /> 交互终端
        </label>
        <button class="btn btn-primary" :disabled="!alias" @click="run">执行</button>
      </div>
      <p class="dim tip">
        参数将按空格拆分后传给后端，替换模板中的 <code>{args}</code> 占位符。
        交互终端模式下可实时键入（vim/top 等全屏程序可用），关闭则走只读流式输出。
        每次执行会新开一个输出面板，可同时运行多个命令互不干扰；面板支持拖拽、缩放，最小化后可从底部任务栏恢复。
      </p>
      <p v-if="formError" class="error-text">{{ formError }}</p>
    </section>

    <p v-if="runs.length === 0" class="empty-tip">暂无执行任务，执行命令后此处将弹出输出面板</p>

    <!-- 输出面板：展示与终端状态内聚在 ExecPanel，页面只管连接与生命周期 -->
    <ExecPanel
      v-for="run in runs"
      :key="run.id"
      :ref="(el) => registerPanel(run.id, el)"
      :title="run.title"
      :mode="run.pty ? 'terminal' : 'lines'"
      :initial-sys-line="run.sysLine"
      class="exec-panel"
      :style="panelStyle(run.id)"
      @ready="(size) => openPtySession(run, size)"
      @term-input="(d) => sendPtyInput(run.id, d)"
      @term-resize="(size) => sendPtyResize(run.id, size)"
      @close="closeRun(run.id)"
    />

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

.pty-toggle {
  display: flex;
  align-items: center;
  gap: 6px;
  white-space: nowrap;
  cursor: pointer;
  user-select: none;
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

// 输出面板的初始尺寸与位置（class 经 fallthrough 落在组件根 ToolPanel 上）
:deep(.exec-panel) {
  width: 660px;
  height: 420px;
}
</style>
