import { createRouter, createWebHistory } from 'vue-router'

// 客户端路由：base 为 /
export const router = createRouter({
  history: createWebHistory('/'),
  routes: [
    {
      path: '/',
      name: 'status',
      component: () => import('./pages/RegisterStatus.vue'),
      meta: { title: '注册状态' },
    },
    {
      path: '/aliases',
      name: 'aliases',
      component: () => import('./pages/Aliases.vue'),
      meta: { title: '命令别名' },
    },
    {
      path: '/run',
      name: 'run',
      component: () => import('./pages/Run.vue'),
      meta: { title: '执行命令' },
    },
    {
      path: '/script',
      name: 'script',
      component: () => import('./pages/Script.vue'),
      meta: { title: '脚本生成' },
    },
  ],
})

router.afterEach((to) => {
  document.title = `${to.meta.title ?? ''} · Remote CMD`
})
