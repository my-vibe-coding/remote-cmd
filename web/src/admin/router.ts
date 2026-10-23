import { createRouter, createWebHistory } from 'vue-router'
import { getAdminToken } from '../shared/api'

// 管理端路由：base 为 /admin/（dev 下由 vite 插件将 /admin/* 回退到 admin.html）
export const router = createRouter({
  history: createWebHistory('/admin/'),
  routes: [
    {
      path: '/login',
      name: 'login',
      component: () => import('./pages/Login.vue'),
      meta: { public: true, title: '登录' },
    },
    {
      path: '/registrations',
      name: 'registrations',
      component: () => import('./pages/Registrations.vue'),
      meta: { title: '申请审批' },
    },
    {
      path: '/credentials',
      name: 'credentials',
      component: () => import('./pages/Credentials.vue'),
      meta: { title: '凭证管理' },
    },
    {
      path: '/audit',
      name: 'audit',
      component: () => import('./pages/Audit.vue'),
      meta: { title: '审计日志' },
    },
    { path: '/', redirect: '/registrations' },
    { path: '/:pathMatch(.*)*', redirect: '/registrations' },
  ],
})

// 路由守卫：无 session_token 时一律跳转登录页
router.beforeEach((to) => {
  if (!to.meta.public && !getAdminToken()) {
    return { path: '/login' }
  }
})

router.afterEach((to) => {
  document.title = `${to.meta.title ?? ''} · Remote CMD 管理端`
})
