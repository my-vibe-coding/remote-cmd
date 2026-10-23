<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import {
  ApiError,
  localApi,
  type HostInfo,
  type LocalStatus,
  type ModeInfo,
} from '../../shared/api'
import StatusBadge from '../../shared/StatusBadge.vue'

// ---------- 本机信息 ----------
const info = ref<HostInfo | null>(null)

// ---------- 注册状态 ----------
const status = ref<LocalStatus | null>(null)
const mode = ref<ModeInfo | null>(null)
const serverUrl = ref('')
const submitting = ref(false)
const errorMsg = ref('')
// 申请注册时若直接返回了 token，用于掩码展示
const regToken = ref('')

const approved = computed(
  () => status.value?.registered === true || status.value?.status === 'approved',
)
const pending = computed(() => !approved.value && status.value?.status === 'pending')
const rejected = computed(() => status.value?.status === 'rejected')

const shortClientId = computed(() => {
  const id = mode.value?.client_id ?? ''
  return id ? id.slice(0, 8) + (id.length > 8 ? '…' : '') : '-'
})

const tokenMask = computed(() => {
  const t = regToken.value
  if (t) return t.slice(0, 6) + '••••••' + t.slice(-4)
  return mode.value?.has_token ? '已保存在本机' : '-'
})

// ---------- 每 3s 轮询状态直到 approved / rejected ----------
async function pollStatus() {
  try {
    const s = await localApi.getStatus()
    status.value = s
    if (s.registered || s.status === 'approved' || s.status === 'rejected') {
      stopPolling()
      if (s.registered || s.status === 'approved') void localApi.getMode().then((m) => (mode.value = m))
    }
  } catch {
    // 网络抖动时忽略，等下一轮
  }
}

const { pause: stopPolling, resume: startPolling } = useIntervalFn(pollStatus, 3000, {
  immediate: false,
})

// ---------- 提交注册 ----------
async function submit() {
  const url = serverUrl.value.trim()
  if (!url) {
    errorMsg.value = '请输入服务器地址，例如 https://192.168.1.100:3233'
    return
  }
  submitting.value = true
  errorMsg.value = ''
  try {
    const r = await localApi.register(url)
    regToken.value = r.token ?? ''
    const s = await localApi.getStatus()
    status.value = s
    if (!(s.registered || s.status === 'approved')) startPolling()
  } catch (e) {
    errorMsg.value =
      e instanceof ApiError ? `注册失败 (HTTP ${e.status})：${e.message}` : '注册失败：网络错误'
  } finally {
    submitting.value = false
  }
}

onMounted(async () => {
  try {
    info.value = await localApi.getInfo()
  } catch {
    /* 本机信息获取失败不阻塞注册流程 */
  }
  try {
    mode.value = await localApi.getMode()
  } catch {
    /* 同上 */
  }
  try {
    status.value = await localApi.getStatus()
    if (!status.value.registered && status.value.status === 'pending') startPolling()
  } catch {
    errorMsg.value = '无法连接本地服务（/api/local），请确认客户端服务已启动'
  }
})
</script>

<template>
  <div class="page">
    <h1 class="page-title">注册状态</h1>

    <!-- 本机信息 -->
    <section class="card">
      <h2 class="card-title">本机信息</h2>
      <div v-if="info" class="info-grid">
        <div class="info-item"><span class="dim">主机名</span><span>{{ info.hostname }}</span></div>
        <div class="info-item"><span class="dim">IP</span><span class="mono">{{ info.ip }}</span></div>
        <div class="info-item"><span class="dim">操作系统</span><span>{{ info.os }}</span></div>
        <div class="info-item"><span class="dim">当前用户</span><span>{{ info.user }}</span></div>
      </div>
      <p v-else class="empty-tip">正在获取本机信息…</p>
    </section>

    <!-- 注册流程 -->
    <section class="card">
      <h2 class="card-title">服务器注册</h2>

      <!-- 已批准 -->
      <template v-if="approved">
        <div class="reg-row">
          <StatusBadge status="approved" />
          <span>已注册并获管理员批准</span>
        </div>
        <div class="info-grid">
          <div class="info-item"><span class="dim">服务器地址</span><span class="mono">{{ status?.server_url }}</span></div>
          <div class="info-item"><span class="dim">客户端 ID</span><span class="mono">{{ shortClientId }}</span></div>
          <div class="info-item"><span class="dim">凭证</span><span class="mono">{{ tokenMask }}</span></div>
        </div>
        <p class="dim tip">凭证由本机安全保管，无需手动配置。现在可以到「命令别名」配置命令，然后去「执行命令」试一试。</p>
      </template>

      <!-- 等待批准 -->
      <template v-else-if="pending">
        <div class="reg-row">
          <StatusBadge status="pending" />
          <span>等待管理员批准…</span>
        </div>
        <p class="dim tip">申请已提交到服务器，正在每 3 秒自动查询审批结果，批准后本页会自动更新。</p>
        <div class="spinner"></div>
      </template>

      <!-- 表单：未注册 / 被拒绝后重新申请 -->
      <template v-else>
        <div v-if="rejected" class="reg-row">
          <StatusBadge status="rejected" />
          <span>注册申请被拒绝，可修改地址后重新申请</span>
        </div>
        <div class="form-row">
          <input
            v-model="serverUrl"
            class="input grow"
            type="text"
            placeholder="服务器地址，如 https://192.168.1.100:3233"
            :disabled="submitting"
            @keyup.enter="submit"
          />
          <button class="btn btn-primary" :disabled="submitting" @click="submit">
            {{ submitting ? '提交中…' : '申请注册' }}
          </button>
        </div>
        <p v-if="errorMsg" class="error-text">{{ errorMsg }}</p>
      </template>
    </section>
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

.info-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: 12px 24px;
  margin-top: 8px;
}

.info-item {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 14px;
}

.reg-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 8px;
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
  line-height: 1.6;
}

.error-text {
  color: var(--danger);
  margin: 12px 0 0;
}

.spinner {
  width: 22px;
  height: 22px;
  margin-top: 14px;
  border: 3px solid var(--border);
  border-top-color: var(--primary);
  border-radius: 50%;
  animation: spin 0.9s linear infinite;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
</style>
