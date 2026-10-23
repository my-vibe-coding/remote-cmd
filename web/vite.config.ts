import { defineConfig, Plugin } from 'vite'
import vue from '@vitejs/plugin-vue'
import { fileURLToPath, URL } from 'node:url'

/** 以当前配置文件为基准解析相对路径（ESM 下没有 __dirname） */
const r = (p: string) => fileURLToPath(new URL(p, import.meta.url))

/**
 * dev 下将 /admin 与 /admin/* 回退到根目录的 admin.html，
 * 使管理端 SPA 可以在 history 模式下直接刷新子路由。
 * 生产环境由后端负责同样的回退规则。
 */
function adminHtmlFallback(): Plugin {
  return {
    name: 'admin-html-fallback',
    apply: 'serve',
    configureServer(server) {
      server.middlewares.use((req, _res, next) => {
        const url = (req.url || '').split('?')[0]
        if (url === '/admin' || url.startsWith('/admin/')) {
          req.url = '/admin.html'
        }
        next()
      })
    },
  }
}

// 远端服务端（自签 HTTPS 证书，需要关闭证书校验）
const toRemoteServer = {
  target: 'https://127.0.0.1:3233',
  changeOrigin: true,
  secure: false,
}

export default defineConfig({
  plugins: [vue(), adminHtmlFallback()],
  build: {
    rollupOptions: {
      input: {
        main: r('index.html'), // 客户端入口
        admin: r('admin.html'), // 管理端入口
      },
    },
  },
  server: {
    proxy: {
      // 管理端相关 API → 远端服务端
      '/api/admin': toRemoteServer,
      '/api/register': toRemoteServer,
      '/api/exec': toRemoteServer,
      '/api/health': toRemoteServer,
      // 客户端本地 API → 本机服务
      '/api/local': {
        target: 'http://127.0.0.1:3234',
        changeOrigin: true,
      },
    },
  },
})
