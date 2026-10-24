<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { adminApi, ApiError, type Registration } from '../../shared/api'
import StatusBadge from '../../shared/StatusBadge.vue'
import { formatTs } from '../../shared/utils'

const rows = ref<Registration[]>([])
const loading = ref(false)
const error = ref('')
const busyId = ref<string | null>(null)

// 白名单草稿（tag 编辑中的值），键为 String(id)
const drafts = reactive<Record<string, string[]>>({})
// 每行白名单的新 tag 输入框内容
const tagInput = reactive<Record<string, string>>({})

const keyOf = (row: Registration) => String(row.id)

/** 程序名白名单：允许字母数字下划线连字符与点（如 app.exe、node） */
const PROGRAM_RE = /^[A-Za-z0-9_.-]+$/

async function load() {
  loading.value = true
  error.value = ''
  try {
    rows.value = await adminApi.credentials()
    for (const row of rows.value) {
      const k = keyOf(row)
      // 仅在无未保存草稿时同步，避免刷新覆盖编辑中内容
      if (!(k in drafts)) drafts[k] = [...(row.whitelist ?? [])]
    }
  } catch (e) {
    error.value = e instanceof ApiError ? `加载失败：${e.message}` : '加载失败：网络错误'
  } finally {
    loading.value = false
  }
}

function addTag(row: Registration) {
  const k = keyOf(row)
  const v = (tagInput[k] ?? '').trim()
  if (!v) return
  if (!PROGRAM_RE.test(v)) {
    error.value = `程序名「${v}」不合法，仅允许字母、数字、下划线、连字符与点`
    return
  }
  error.value = ''
  if (!drafts[k].includes(v)) drafts[k].push(v)
  tagInput[k] = ''
}

function removeTag(row: Registration, tag: string) {
  const k = keyOf(row)
  drafts[k] = drafts[k].filter((t) => t !== tag)
}

/** 草稿与已保存白名单是否有差异 */
function isDirty(row: Registration): boolean {
  const k = keyOf(row)
  const draft = drafts[k] ?? []
  const saved = row.whitelist ?? []
  return draft.length !== saved.length || draft.some((t, i) => t !== saved[i])
}

async function saveWhitelist(row: Registration) {
  const k = keyOf(row)
  busyId.value = k
  error.value = ''
  try {
    const r = await adminApi.putWhitelist(row.id, drafts[k] ?? [])
    row.whitelist = r.whitelist
    drafts[k] = [...r.whitelist]
  } catch (e) {
    error.value = e instanceof ApiError ? `保存失败：${e.message}` : '保存失败：网络错误'
  } finally {
    busyId.value = null
  }
}

async function revoke(row: Registration) {
  if (
    !window.confirm(
      `确认吊销「${row.hostname}（${row.ip}）」的凭证？吊销后该客户端将无法继续执行任何命令。`,
    )
  )
    return
  busyId.value = keyOf(row)
  error.value = ''
  try {
    await adminApi.revoke(row.id)
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? `吊销失败：${e.message}` : '吊销失败：网络错误'
  } finally {
    busyId.value = null
  }
}

/** 恢复已吊销的凭证：重新签发 token（等同重新批准） */
async function restore(row: Registration) {
  if (!window.confirm(`确认恢复「${row.hostname}（${row.ip}）」的凭证？将重新签发 token。`)) return
  busyId.value = keyOf(row)
  error.value = ''
  try {
    await adminApi.approve(row.id)
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? `恢复失败：${e.message}` : '恢复失败：网络错误'
  } finally {
    busyId.value = null
  }
}

function resetDraft(row: Registration) {
  drafts[keyOf(row)] = [...(row.whitelist ?? [])]
}

onMounted(load)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <button class="btn btn-sm" :disabled="loading" @click="load">{{ loading ? '刷新中…' : '刷新' }}</button>
      <span class="dim">共 {{ rows.length }} 条凭证（仅展示已批准/已吊销）</span>
    </div>

    <p v-if="error" class="error-text">{{ error }}</p>

    <p v-if="rows.length === 0 && !loading" class="empty-tip">暂无凭证，请先在「申请审批」中批准客户端</p>

    <!-- 卡片式凭证列表：每张卡含 tag 式白名单编辑器 -->
    <section v-for="row in rows" :key="String(row.id)" class="card cred-card">
      <div class="cred-head">
        <div class="cred-meta">
          <span class="strong hostname">{{ row.hostname }}</span>
          <span class="mono dim">{{ row.ip }}</span>
          <span class="dim">用户 {{ row.user }}</span>
          <span class="dim">最近在线 {{ formatTs(row.last_seen) }}</span>
        </div>
        <div class="cred-actions">
          <StatusBadge :status="row.status" />
          <button class="btn btn-sm" :disabled="!isDirty(row) || busyId === keyOf(row)" @click="saveWhitelist(row)">
            保存白名单
          </button>
          <button
            v-if="row.status === 'approved'"
            class="btn btn-sm btn-danger"
            :disabled="busyId === keyOf(row)"
            @click="revoke(row)"
          >
            吊销
          </button>
          <button
            v-if="row.status === 'revoked'"
            class="btn btn-sm"
            :disabled="busyId === keyOf(row)"
            @click="restore(row)"
          >
            恢复
          </button>
        </div>
      </div>

      <!-- 白名单 tag 编辑器 -->
      <div class="whitelist">
        <span class="dim wl-label">程序白名单：</span>
        <div class="tags">
          <span v-for="tag in drafts[keyOf(row)] ?? []" :key="tag" class="tag">
            <span class="mono">{{ tag }}</span>
            <button class="tag-x" title="移除" @click="removeTag(row, tag)">×</button>
          </span>
          <input
            v-model="tagInput[keyOf(row)]"
            class="input tag-input"
            type="text"
            placeholder="输入程序名后回车添加，如 node"
            @keydown.enter.prevent="addTag(row)"
          />
          <button
            v-if="isDirty(row)"
            class="btn btn-sm revert"
            title="放弃未保存的修改"
            @click="resetDraft(row)"
          >
            还原
          </button>
        </div>
      </div>
      <p class="dim wl-hint">白名单为空表示该客户端不允许执行任何程序；命中白名单的程序才会被放行执行。</p>
    </section>
  </div>
</template>

<style scoped lang="scss">
.page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.toolbar {
  display: flex;
  align-items: center;
  gap: 12px;
}

.error-text {
  color: var(--danger);
  margin: 0;
}

.strong {
  font-weight: 600;
}

.cred-card {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.cred-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  flex-wrap: wrap;
}

.cred-meta {
  display: flex;
  align-items: center;
  gap: 14px;
  flex-wrap: wrap;

  .hostname {
    font-size: 15px;
  }
}

.cred-actions {
  display: flex;
  align-items: center;
  gap: 8px;
}

.whitelist {
  display: flex;
  align-items: flex-start;
  gap: 10px;
}

.wl-label {
  flex-shrink: 0;
  line-height: 30px;
}

.tags {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
}

.tag {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  background: rgba(39, 194, 220, 0.1);
  border: 1px solid rgba(39, 194, 220, 0.45);
  color: var(--primary);
  border-radius: 999px;
  padding: 3px 6px 3px 12px;
  font-size: 12px;
}

.tag-x {
  background: none;
  border: none;
  color: inherit;
  cursor: pointer;
  font-size: 14px;
  line-height: 1;
  padding: 2px 4px;

  &:hover {
    color: var(--danger);
  }
}

.tag-input {
  width: 220px;
  padding: 5px 10px;
  font-size: 12px;
}

.revert {
  margin-left: 4px;
}

.wl-hint {
  margin: 0;
  font-size: 12px;
}
</style>
