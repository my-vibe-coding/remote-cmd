<script setup lang="ts">
import { computed } from 'vue'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import { clearAdminToken } from '../shared/api'

const route = useRoute()
const router = useRouter()

// 登录页使用独立布局（无侧栏）
const isLogin = computed(() => route.meta.public === true)
const pageTitle = computed(() => (route.meta.title as string | undefined) ?? '')

const navItems = [
  { to: '/registrations', label: '申请审批' },
  { to: '/credentials', label: '凭证管理' },
  { to: '/audit', label: '审计日志' },
]

function logout() {
  clearAdminToken()
  router.replace('/login')
}
</script>

<template>
  <RouterView v-if="isLogin" />

  <div v-else class="layout">
    <aside class="sidebar">
      <div class="logo">
        <span>Remote CMD</span>
        <span class="logo-sub">管理端</span>
      </div>
      <nav class="nav">
        <RouterLink v-for="item in navItems" :key="item.to" :to="item.to" class="nav-link">
          {{ item.label }}
        </RouterLink>
      </nav>
      <button class="btn logout-btn" @click="logout">退出登录</button>
    </aside>

    <div class="main">
      <header class="topbar">
        <h1 class="topbar-title">{{ pageTitle }}</h1>
        <span class="dim">Remote Command Center</span>
      </header>
      <main class="content">
        <RouterView />
      </main>
    </div>
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
    color: var(--primary);
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

.logout-btn {
  margin-top: 12px;
}

.main {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.topbar {
  display: flex;
  align-items: baseline;
  gap: 14px;
  padding: 14px 28px;
  border-bottom: 1px solid var(--border);
  background: #161920;

  .topbar-title {
    font-size: 17px;
    margin: 0;
  }
}

.content {
  flex: 1;
  overflow: auto;
  padding: 24px 28px;
}
</style>
