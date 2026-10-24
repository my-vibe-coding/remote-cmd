<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { RouterLink, RouterView, useRoute } from 'vue-router'
import { useIntervalFn } from '@vueuse/core'
import { localApi, type ModeInfo } from '../shared/api'

// 侧栏顶部展示注册概要（失败时静默）
const route = useRoute()
const mode = ref<ModeInfo | null>(null)
const refreshMode = () =>
  localApi
    .getMode()
    .then((m) => (mode.value = m))
    .catch(() => {})
refreshMode()
// 仅在「注册状态」页轮询以同步注册结果；其它页面停止轮询，切换进来时刷新一次
const { pause, resume } = useIntervalFn(refreshMode, 3000, { immediate: false })
watch(
  () => route.name,
  (name) => {
    if (name === 'status') {
      resume()
    } else {
      pause()
      refreshMode()
    }
  },
  { immediate: true },
)

const navItems = [
  { to: '/', label: '注册状态' },
  { to: '/aliases', label: '命令别名' },
  { to: '/run', label: '执行命令' },
  { to: '/script', label: '脚本生成' },
]

const shortId = computed(() => {
  const id = mode.value?.client_id ?? ''
  return id ? id.slice(0, 8) + (id.length > 8 ? '…' : '') : '未注册'
})
</script>

<template>
  <div class="layout">
    <aside class="sidebar">
      <div class="logo">
        <span>Remote CMD</span>
        <span class="logo-sub">客户端</span>
      </div>
      <nav class="nav">
        <RouterLink v-for="item in navItems" :key="item.to" :to="item.to" class="nav-link">
          {{ item.label }}
        </RouterLink>
      </nav>
      <div class="sidebar-footer">
        <span class="dim">客户端 ID</span>
        <span class="mono">{{ shortId }}</span>
      </div>
    </aside>
    <main class="content">
      <RouterView />
    </main>
  </div>
</template>

<style scoped lang="scss">
.layout {
  display: flex;
  height: 100%;
}

.sidebar {
  width: 220px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  background: #161920;
  border-right: 1px solid var(--border);
  padding: 18px 12px;
}

.logo {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: 17px;
  font-weight: 700;
  padding: 4px 10px 18px;
  border-bottom: 1px solid var(--border);

  .logo-sub {
    font-size: 12px;
    font-weight: 400;
    color: var(--text-dim);
  }
}

.nav {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-top: 14px;
  flex: 1;
}

.nav-link {
  padding: 9px 12px;
  border-radius: var(--radius-sm);
  color: var(--text-dim);
  transition: background 0.15s, color 0.15s;

  &:hover {
    background: var(--bg-hover);
    color: var(--text);
  }

  &.router-link-active {
    background: rgba(39, 194, 220, 0.12);
    color: var(--primary);
    font-weight: 600;
  }
}

.sidebar-footer {
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 10px;
  font-size: 12px;
  border-top: 1px solid var(--border);
}

.content {
  flex: 1;
  overflow: auto;
  padding: 28px 32px;
  max-width: 1200px;
}
</style>
