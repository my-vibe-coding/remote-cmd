<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { adminApi, ApiError, type Registration } from '../../shared/api'
import StatusBadge from '../../shared/StatusBadge.vue'
import { formatTs, confirm } from '../../shared/utils'

const rows = ref<Registration[]>([])
const loading = ref(false)
const error = ref('')
const busyId = ref<string | null>(null) // 正在操作的行

// 批准成功后弹窗展示签发的凭证
const approveResult = ref<{ hostname: string; client_id: string; token: string } | null>(null)

async function load() {
  loading.value = true
  error.value = ''
  try {
    rows.value = await adminApi.registrations()
  } catch (e) {
    error.value = e instanceof ApiError ? `加载失败：${e.message}` : '加载失败：网络错误'
  } finally {
    loading.value = false
  }
}

async function approve(row: Registration) {
  busyId.value = String(row.id)
  error.value = ''
  try {
    const r = await adminApi.approve(row.id)
    approveResult.value = { hostname: row.hostname, client_id: r.client_id, token: r.token }
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? `批准失败：${e.message}` : '批准失败：网络错误'
  } finally {
    busyId.value = null
  }
}

async function reject(row: Registration) {
  const { confirmed } = await confirm({
    title: '拒绝申请',
    content: `确定拒绝「${row.hostname}（${row.ip}）」的注册申请吗？`,
    danger: true,
  })
  if (!confirmed) return
  busyId.value = String(row.id)
  error.value = ''
  try {
    await adminApi.reject(row.id)
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? `拒绝失败：${e.message}` : '拒绝失败：网络错误'
  } finally {
    busyId.value = null
  }
}

async function remove(row: Registration) {
  const { confirmed } = await confirm({
    title: '删除确认',
    content: `确定删除「${row.hostname}」的申请记录吗？删除后客户端可重新申请。`,
    danger: true,
  })
  if (!confirmed) return
  busyId.value = String(row.id)
  error.value = ''
  try {
    await adminApi.deleteRegistration(row.id)
    await load()
  } catch (e) {
    error.value = e instanceof ApiError ? `删除失败：${e.message}` : '删除失败：网络错误'
  } finally {
    busyId.value = null
  }
}

async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    /* 剪贴板不可用时忽略 */
  }
}

onMounted(load)
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <button class="btn btn-sm" :disabled="loading" @click="load">{{ loading ? '刷新中…' : '刷新' }}</button>
      <span class="dim">共 {{ rows.length }} 条申请</span>
    </div>

    <p v-if="error" class="error-text">{{ error }}</p>

    <section class="card table-card">
      <table v-if="rows.length" class="table">
        <thead>
          <tr>
            <th>主机名</th>
            <th>IP</th>
            <th>操作系统</th>
            <th>用户</th>
            <th>指纹</th>
            <th>状态</th>
            <th>申请时间</th>
            <th style="width: 200px">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="row in rows" :key="String(row.id)">
            <td class="strong">{{ row.hostname }}</td>
            <td class="mono">{{ row.ip }}</td>
            <td>{{ row.os }}</td>
            <td>{{ row.user }}</td>
            <td class="mono dim" :title="row.fingerprint">{{ row.fingerprint.slice(0, 8) }}</td>
            <td><StatusBadge :status="row.status" /></td>
            <td class="dim">{{ formatTs(row.created_at) }}</td>
            <td>
              <template v-if="row.status === 'pending'">
                <button class="btn btn-sm btn-success" :disabled="busyId === String(row.id)" @click="approve(row)">
                  批准
                </button>
                <button class="btn btn-sm btn-danger gap" :disabled="busyId === String(row.id)" @click="reject(row)">
                  拒绝
                </button>
              </template>
              <button class="btn btn-sm btn-danger gap" :disabled="busyId === String(row.id)" @click="remove(row)">
                删除
              </button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-tip">{{ loading ? '加载中…' : '暂无注册申请' }}</p>
    </section>

    <!-- 批准成功：展示签发的凭证 -->
    <div v-if="approveResult" class="modal-mask" @click.self="approveResult = null">
      <div class="card modal">
        <h2 class="card-title">已批准「{{ approveResult.hostname }}」</h2>
        <p class="dim modal-tip">以下凭证已同时下发给客户端，请妥善保管，仅此一次完整展示：</p>
        <div class="kv">
          <span class="dim">client_id</span>
          <span class="mono">{{ approveResult.client_id }}</span>
          <button class="btn btn-sm" @click="copy(approveResult.client_id)">复制</button>
        </div>
        <div class="kv">
          <span class="dim">token</span>
          <span class="mono token">{{ approveResult.token }}</span>
          <button class="btn btn-sm" @click="copy(approveResult.token)">复制</button>
        </div>
        <div class="modal-actions">
          <button class="btn btn-primary" @click="approveResult = null">我已保存，关闭</button>
        </div>
      </div>
    </div>
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

.table-card {
  padding: 6px 8px;
}

.strong {
  font-weight: 600;
}

.gap {
  margin-left: 8px;
}

.error-text {
  color: var(--danger);
  margin: 0;
}

// 凭证弹窗
.modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
}

.modal {
  width: 480px;
}

.modal-tip {
  margin: 0 0 12px;
  font-size: 13px;
}

.kv {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 8px;
  font-size: 13px;

  .token {
    flex: 1;
    overflow-wrap: anywhere;
    color: var(--warning);
  }
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  margin-top: 14px;
}
</style>
