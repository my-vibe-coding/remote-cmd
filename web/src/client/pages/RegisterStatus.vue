<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useIntervalFn } from '@vueuse/core'
import {
  ApiError,
  localApi,
  type HostInfo,
  type LocalStatus,
  type ServerRegInfo,
} from '../../shared/api'
import StatusBadge from '../../shared/StatusBadge.vue'

// ---------- 本机信息 ----------
const info = ref<HostInfo | null>()

// ---------- 注册状态（多服务器） ----------
const status = ref<LocalStatus | null>(null)
const serverUrl = ref('')
const submitting = ref(false)
const errorMsg = ref('')
const notice = ref('')
// 申请注册时若直接返回了 token，用于掩码展示
const regToken = ref('')
// 正在注销的服务器
const unregistering = ref('')

const servers = computed<ServerRegInfo[]>(() => status.value?.servers ?? [])

/** 展示用状态：deleted/unreachable 映射为可显示的徽章 */
function displayStatus(s: ServerRegInfo): string {
  if (s.status === 'deleted') return 'revoked'
  if (s.status === 'unreachable') return 'pending'
  return s.status || (s.registered ? 'approved' : 'pending')
}

/** 状态说明文字 */
function statusText(s: ServerRegInfo): string {
  switch (s.status) {
    case 'deleted':
      return '服务器已删除该注册记录'
    case 'unreachable':
      return '无法连接服务器'
    case 'revoked':
      return '凭证已被管理员吊销'
    case 'pending':
      return '等待管理员批准…'
    case 'rejected':
      return '注册申请被拒绝'
    default:
      return ''
  }
}

const shortId = (id: string) => (id ? id.slice(0, 8) + (id.length > 8 ? '…' : '') : '-')

// ---------- 每 3s 轮询：审批结果 / 吊销与删除感知 ----------
async function pollStatus() {
  try {
    status.value = await localApi.getStatus()
  } catch {
    // 网络抖动时忽略，等下一轮
  }
}

const { resume: startPolling } = useIntervalFn(pollStatus, 3000, {
  immediate: false,
})

// ---------- 提交注册（可添加多台服务器） ----------
async function submit() {
  const url = serverUrl.value.trim()
  if (!url) {
    errorMsg.value = '请输入服务器地址，例如 https://192.168.1.100:3233'
    return
  }
  submitting.value = true
  errorMsg.value = ''
  notice.value = ''
  try {
    const r = await localApi.register(url)
    regToken.value = r.token ?? ''
    serverUrl.value = ''
    await pollStatus()
  } catch (e) {
    errorMsg.value =
      e instanceof ApiError ? `注册失败 (HTTP ${e.status})：${e.message}` : '注册失败：网络错误'
  } finally {
    submitting.value = false
  }
}

// ---------- 重新注册（吊销/拒绝/删除后，凭保留的服务器数据重新申请） ----------
async function reRegister(s: ServerRegInfo) {
  unregistering.value = s.server_url
  errorMsg.value = ''
  notice.value = ''
  try {
    const r = await localApi.register(s.server_url)
    regToken.value = r.token ?? ''
    notice.value = `已向「${s.hostname || s.server_url}」重新提交注册申请`
    await pollStatus()
  } catch (e) {
    errorMsg.value =
      e instanceof ApiError ? `重新注册失败 (HTTP ${e.status})：${e.message}` : '重新注册失败：网络错误'
  } finally {
    unregistering.value = ''
  }
}

// ---------- 注销 ----------
async function unregister(s: ServerRegInfo) {
  const name = s.hostname || s.server_url
  if (!window.confirm(`确认注销在服务器「${name}」的注册？本机凭证将被删除。`)) return
  unregistering.value = s.server_url
  errorMsg.value = ''
  notice.value = ''
  try {
    await localApi.unregisterServer(s.server_url)
    notice.value = `已注销服务器「${name}」的注册`
    await pollStatus()
  } catch (e) {
    errorMsg.value =
      e instanceof ApiError ? `注销失败 (HTTP ${e.status})：${e.message}` : '注销失败：网络错误'
  } finally {
    unregistering.value = ''
  }
}

onMounted(async () => {
  try {
    info.value = await localApi.getInfo()
  } catch {
    /* 本机信息获取失败不阻塞注册流程 */
    info.value = null
  }
  try {
    await pollStatus()
  } catch {
    errorMsg.value = '无法连接本地服务（/api/local），请确认客户端服务已启动'
  }
  startPolling()
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
      <p v-else-if="info === null" class="empty-tip">
        <div>获取客户端信息失败, 程序是否以服务模式运行? </div>
        <div>访问 <a href="/admin" style="color: #007bff;">服务端</a> 页面</div>
      </p>
      <p v-else-if="info === undefined" class="empty-tip">正在获取客户端信息…</p>
    </section>

    <!-- 已注册的服务器列表 -->
    <section class="card">
      <h2 class="card-title">已注册服务器（{{ servers.length }}）</h2>
      <p v-if="notice" class="notice-text">{{ notice }}</p>
      <template v-if="servers.length">
        <div v-for="s in servers" :key="s.server_url" class="server-row">
          <div class="server-head">
            <StatusBadge :status="displayStatus(s)" />
            <span class="mono server-url">{{ s.server_url }}</span>
            <button
              v-if="!s.registered && s.status !== 'pending' && s.status !== 'unreachable'"
              class="btn btn-sm"
              :disabled="unregistering === s.server_url"
              @click="reRegister(s)"
            >
              {{ unregistering === s.server_url ? '提交中…' : '重新注册' }}
            </button>
            <button
              class="btn btn-sm btn-danger"
              :disabled="unregistering === s.server_url"
              @click="unregister(s)"
            >
              {{ unregistering === s.server_url ? '注销中…' : '注销注册' }}
            </button>
          </div>
          <div class="server-detail">
            <span class="dim">客户端 ID</span><span class="mono">{{ shortId(s.client_id) }}</span>
            <span class="dim">状态</span>
            <span :class="{ 'warn-text': s.status === 'revoked' || s.status === 'deleted' || s.status === 'unreachable' }">
              {{ s.registered ? '已批准可用' : statusText(s) || '待审批' }}
            </span>
          </div>
        </div>
        <p class="dim tip">
          支持注册多台服务器；在「命令别名」中可为每个别名选择执行的服务器。
          服务器删除或吊销凭证后，本页会自动感知并更新状态，服务器数据仍会保留，可点击「重新注册」再次申请。
        </p>
      </template>
      <p v-else class="empty-tip">尚未注册任何服务器，在下方填写地址完成注册</p>
    </section>

    <!-- 注册新服务器 -->
    <section class="card">
      <h2 class="card-title">注册新服务器</h2>
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
      <p v-if="regToken" class="dim tip">
        注册已获批准，凭证已保存在本机（{{ regToken.slice(0, 6) }}••••••{{ regToken.slice(-4) }}）
      </p>
      <p v-if="errorMsg" class="error-text">{{ errorMsg }}</p>
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

.server-row {
  padding: 12px 0;
  border-bottom: 1px dashed var(--border);

  &:last-of-type {
    border-bottom: none;
  }
}

.server-head {
  display: flex;
  align-items: center;
  gap: 12px;
}

.server-url {
  flex: 1;
  font-size: 13px;
}

.server-detail {
  display: grid;
  grid-template-columns: 80px 1fr;
  gap: 4px 16px;
  margin-top: 8px;
  padding-left: 4px;
  font-size: 13px;
}

.warn-text {
  color: var(--warning);
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

.notice-text {
  color: var(--success);
  margin: 8px 0;
  font-size: 13px;
}

.error-text {
  color: var(--danger);
  margin: 12px 0 0;
}
</style>
