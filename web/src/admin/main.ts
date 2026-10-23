import { createApp } from 'vue'
import App from './App.vue'
import { router } from './router'
import '../shared/styles.scss'

createApp(App).use(router).mount('#app')
