<script setup lang="ts">
/**
 * 目录上传进度卡片：批次状态（总体进度 / 在途文件 / 失败重传）展示。
 * 引擎逻辑内聚在 upload-dir.ts，本组件只做状态渲染与用户操作入口。
 */
import { computed, ref } from 'vue'
import {
  DirUploadEngine,
  collectFromDrop,
  collectFromInput,
  type BatchStats,
  type FileState,
  type UploadTask,
} from '../upload-dir'

const visible = ref(false)
const stats = ref<BatchStats>({ total: 0, done: 0, failed: 0, bytesTotal: 0, bytesSent: 0, running: false })
const files = ref<FileState[]>([])
const message = ref('')
/** 当前批次引擎（批次完成后保留，支持"重传失败项"） */
let engine: DirUploadEngine | null = null

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v.toFixed(1)} ${units[i]}`
}

const pct = computed(() =>
  stats.value.bytesTotal > 0 ? Math.round((stats.value.bytesSent / stats.value.bytesTotal) * 100) : 0,
)

function statusText(f: FileState): string {
  switch (f.status) {
    case 'uploading':
      return `${fmtBytes(f.sent)}/${fmtBytes(f.size)}`
    case 'ok':
      return '完成'
    case 'exists':
      return '已存在（跳过）'
    case 'pending':
      return '排队中'
    default:
      return '失败'
  }
}

async function run(alias: string, session: string, tasks: UploadTask[], emptyDirs: string[]) {
  visible.value = true
  message.value = ''
  const eng = new DirUploadEngine({
    alias,
    session,
    tasks,
    emptyDirs,
    onUpdate: (s, fs) => {
      stats.value = s
      // 在途视图只展示未完成的前 8 个（大目录下避免长列表渲染）
      files.value = fs
        .filter((f) => f.status === 'uploading' || f.status === 'pending' || f.status === 'failed')
        .slice(0, 8)
    },
  })
  engine = eng
  await eng.start()
}

/** 目录选择器入口 */
async function startFromInput(alias: string, session: string, list: FileList) {
  const tasks = collectFromInput(list)
  if (tasks.length === 0) {
    message.value = '所选目录为空，没有可上传的文件'
    visible.value = true
    window.setTimeout(() => {
      if (!stats.value.running) visible.value = false
    }, 4000)
    return
  }
  await run(alias, session, tasks, [])
}

/** 拖放入口 */
async function startFromDrop(alias: string, session: string, dt: DataTransfer) {
  const { tasks, emptyDirs, partial } = await collectFromDrop(dt)
  if (tasks.length === 0 && emptyDirs.length === 0) {
    message.value = '拖入内容中没有可上传的文件'
    visible.value = true
    window.setTimeout(() => {
      if (!stats.value.running) visible.value = false
    }, 4000)
    return
  }
  message.value = partial
    ? '部分拖入项无法识别为目录条目（浏览器限制），已按可识别文件上传；完整目录请使用"上传目录"按钮'
    : ''
  await run(alias, session, tasks, emptyDirs)
}

function retry() {
  void engine?.retryFailed()
}

function dismiss() {
  engine?.cancel()
  visible.value = false
}

defineExpose({ startFromInput, startFromDrop })
</script>

<template>
  <div v-if="visible" class="dir-card">
    <div class="du-head">
      <span class="du-title">目录上传</span>
      <button class="du-close" title="关闭并取消" @click="dismiss">×</button>
    </div>
    <p v-if="message" class="du-msg">{{ message }}</p>
    <div class="du-bar">
      <div class="du-bar-fill" :style="{ width: pct + '%' }"></div>
    </div>
    <div class="du-stats">
      {{ stats.done }}/{{ stats.total }} 文件 ·
      {{ fmtBytes(stats.bytesSent) }}/{{ fmtBytes(stats.bytesTotal) }}
      <span v-if="stats.failed > 0" class="du-fail">失败 {{ stats.failed }}</span>
      <span v-if="stats.running" class="dim">上传中…</span>
    </div>
    <ul v-if="files.length" class="du-files">
      <li v-for="f in files" :key="f.rel" class="du-file">
        <span class="du-rel">{{ f.rel }}</span>
        <span class="du-st" :class="f.status">{{ statusText(f) }}</span>
      </li>
    </ul>
    <div v-if="!stats.running && stats.failed > 0 && engine" class="du-actions">
      <button class="btn" @click="retry">重传失败项</button>
    </div>
  </div>
</template>

<style scoped lang="scss">
.dir-card {
  position: fixed;
  right: 16px;
  bottom: 66px;
  z-index: 1000;
  width: 420px;
  max-height: 320px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px 14px;
  background: var(--bg-panel, #1e2230);
  border: 1px solid var(--border, #3a4154);
  border-radius: 8px;
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
  font-size: 13px;
}

.du-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
}

.du-title {
  font-weight: 600;
}

.du-close {
  background: none;
  border: none;
  color: inherit;
  font-size: 16px;
  cursor: pointer;
  padding: 0 4px;
}

.du-msg {
  margin: 0;
  color: var(--warning, #fbbf24);
  word-break: break-all;
}

.du-bar {
  height: 6px;
  border-radius: 999px;
  background: var(--bg-input, #2a3040);
  overflow: hidden;
}

.du-bar-fill {
  height: 100%;
  background: var(--success, #34d399);
  transition: width 0.2s ease;
}

.du-stats {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}

.du-fail {
  color: var(--danger, #f87171);
}

.du-files {
  flex: 1;
  min-height: 0;
  overflow: auto;
  margin: 0;
  padding: 0;
  list-style: none;
}

.du-file {
  display: flex;
  justify-content: space-between;
  gap: 10px;
  padding: 2px 0;
}

.du-rel {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.du-st.uploading {
  color: var(--warning, #fbbf24);
}

.du-st.failed {
  color: var(--danger, #f87171);
}

.du-st.exists {
  color: var(--muted, #8a93a8);
}

.du-actions {
  display: flex;
  justify-content: flex-end;
}
</style>
