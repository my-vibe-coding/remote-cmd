<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { adminApi, ApiError, type AuditData } from '../../shared/api'
import { formatDuration, formatTs } from '../../shared/utils'

const tab = ref<'audit' | 'executions'>('audit')
const data = ref<AuditData>({ audit: [], executions: [] })
const loading = ref(false)
const error = ref('')

// 最新在前：ts 兼容 epoch 数值与 RFC3339 字符串
function byTsDesc(a: string, b: string): number {
  const na = Number(a)
  const nb = Number(b)
  if (Number.isFinite(na) && Number.isFinite(nb) && a !== '' && b !== '') return nb - na
  return String(b).localeCompare(String(a))
}

const auditRows = computed(() =>
  [...data.value.audit].sort((x, y) => byTsDesc(String(x.ts), String(y.ts))),
)
const execRows = computed(() =>
  [...data.value.executions].sort((x, y) => byTsDesc(String(x.ts), String(y.ts))),
)

async function load() {
  loading.value = true
  error.value = ''
  try {
    data.value = await adminApi.audit(200)
  } catch (e) {
    error.value = e instanceof ApiError ? `加载失败：${e.message}` : '加载失败：网络错误'
  } finally {
    loading.value = false
  }
}

onMounted(load)
</script>

<template>
  <div class="page">
    <!-- tab 切换 -->
    <div class="tabs">
      <button class="tab" :class="{ active: tab === 'audit' }" @click="tab = 'audit'">
        审计事件（{{ auditRows.length }}）
      </button>
      <button class="tab" :class="{ active: tab === 'executions' }" @click="tab = 'executions'">
        执行历史（{{ execRows.length }}）
      </button>
      <button class="btn btn-sm refresh" :disabled="loading" @click="load">
        {{ loading ? '刷新中…' : '刷新' }}
      </button>
    </div>

    <p v-if="error" class="error-text">{{ error }}</p>

    <!-- 审计事件 -->
    <section v-show="tab === 'audit'" class="card table-card">
      <table v-if="auditRows.length" class="table">
        <thead>
          <tr>
            <th style="width: 180px">时间</th>
            <th style="width: 220px">事件</th>
            <th style="width: 160px">操作者</th>
            <th>详情</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(a, i) in auditRows" :key="i">
            <td class="dim mono">{{ formatTs(a.ts) }}</td>
            <td><span class="event-chip">{{ a.event }}</span></td>
            <td>{{ a.actor }}</td>
            <td class="dim detail">{{ a.detail ?? '-' }}</td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-tip">{{ loading ? '加载中…' : '暂无审计事件' }}</p>
    </section>

    <!-- 执行历史 -->
    <section v-show="tab === 'executions'" class="card table-card">
      <table v-if="execRows.length" class="table">
        <thead>
          <tr>
            <th style="width: 180px">时间</th>
            <th style="width: 140px">主机</th>
            <th style="width: 180px">程序</th>
            <th>参数</th>
            <th style="width: 90px">退出码</th>
            <th style="width: 100px">耗时</th>
            <th style="width: 130px">备注</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(e, i) in execRows" :key="i">
            <td class="dim mono">{{ formatTs(e.ts) }}</td>
            <td>{{ e.hostname }}</td>
            <td><span class="mono program">{{ e.program }}</span></td>
            <td class="mono args">{{ e.args.join(' ') || '-' }}</td>
            <td>
              <span v-if="e.exit_code === 0" class="exit-ok">0</span>
              <span v-else-if="e.exit_code == null" class="dim">-</span>
              <span v-else class="exit-fail">{{ e.exit_code }}</span>
            </td>
            <td class="dim">{{ formatDuration(e.duration_ms) }}</td>
            <td>
              <span v-if="e.timeout" class="mini-badge warn">超时</span>
              <span v-if="e.truncated" class="mini-badge warn">截断</span>
              <span v-if="!e.timeout && !e.truncated" class="dim">-</span>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-tip">{{ loading ? '加载中…' : '暂无执行记录' }}</p>
    </section>
  </div>
</template>

<style scoped lang="scss">
.page {
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.tabs {
  display: flex;
  align-items: center;
  gap: 8px;
}

.tab {
  background: transparent;
  border: 1px solid var(--border);
  color: var(--text-dim);
  border-radius: var(--radius-sm);
  padding: 8px 18px;
  cursor: pointer;
  font-size: 14px;

  &:hover {
    color: var(--text);
  }

  &.active {
    background: rgba(39, 194, 220, 0.12);
    border-color: var(--primary);
    color: var(--primary);
    font-weight: 600;
  }
}

.refresh {
  margin-left: auto;
}

.table-card {
  padding: 6px 8px;
}

.error-text {
  color: var(--danger);
  margin: 0;
}

.event-chip {
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 2px 8px;
  font-size: 12px;
  font-family: Consolas, monospace;
}

.detail {
  overflow-wrap: anywhere;
}

.program {
  color: var(--primary);
}

.args {
  color: var(--text-dim);
  overflow-wrap: anywhere;
}

.exit-ok {
  color: var(--success);
  font-weight: 600;
}

.exit-fail {
  color: var(--danger);
  font-weight: 600;
}

.mini-badge {
  display: inline-block;
  padding: 1px 8px;
  border-radius: 999px;
  font-size: 11px;
  margin-right: 4px;

  &.warn {
    color: var(--warning);
    border: 1px solid var(--warning);
    background: rgba(251, 191, 36, 0.1);
  }
}
</style>
