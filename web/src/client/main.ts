import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import { router } from './router'
import '../shared/styles.scss'
// webui-panel 的面板注册表依赖 pinia，必须先安装 pinia
import '@develon/webui-panel/dist/index.css'

createApp(App).use(createPinia()).use(router).mount('#app')
