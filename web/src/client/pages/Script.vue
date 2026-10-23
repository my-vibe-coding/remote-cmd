<script setup lang="ts">
import { onMounted, reactive, ref } from 'vue'
import { ApiError, localApi } from '../../shared/api'

interface AliasItem {
  name: string
  template: string
}

interface GenResult {
  path: string
  bin_dir: string
}

const aliases = ref<AliasItem[]>([])
const results = reactive<Record<string, GenResult>>({})
const generating = ref<string | null>(null) // 正在生成的别名
const error = ref('')

async function load() {
  try {
    const data = await localApi.getAliases()
    aliases.value = Object.entries(data).map(([n, v]) => ({ name: n, template: v.template }))
  } catch (e) {
    error.value = e instanceof ApiError ? `加载别名失败：${e.message}` : '加载别名失败：网络错误'
  }
}

async function generate(name: string) {
  generating.value = name
  error.value = ''
  try {
    results[name] = await localApi.script(name)
  } catch (e) {
    error.value = e instanceof ApiError ? `生成失败 (HTTP ${e.status})：${e.message}` : '生成失败：网络错误'
  } finally {
    generating.value = null
  }
}

async function copy(text: string) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    /* 剪贴板不可用时忽略 */
  }
}

onMounted(load)
</script>

<template>
  <div class="page">
    <h1 class="page-title">脚本生成</h1>

    <p class="dim intro">
      为别名一键生成 <code>.bat</code> 脚本：生成后无需打开本页面，在任意终端输入别名即可执行命令。
    </p>

    <section v-for="a in aliases" :key="a.name" class="card">
      <div class="row">
        <div class="head">
          <span class="mono alias-name">{{ a.name }}</span>
          <code class="template-code">{{ a.template }}</code>
        </div>
        <button class="btn btn-primary btn-sm" :disabled="generating === a.name" @click="generate(a.name)">
          {{ generating === a.name ? '生成中…' : '生成 .bat 脚本' }}
        </button>
      </div>

      <template v-if="results[a.name]">
        <div class="result">
          <div class="kv">
            <span class="dim">脚本路径</span>
            <span class="mono path">{{ results[a.name].path }}</span>
            <button class="btn btn-sm" @click="copy(results[a.name].path)">复制路径</button>
          </div>
          <div class="kv">
            <span class="dim">所在目录（bin_dir）</span>
            <span class="mono path">{{ results[a.name].bin_dir }}</span>
            <button class="btn btn-sm" @click="copy(results[a.name].bin_dir)">复制目录</button>
          </div>
          <div class="guide">
            <strong>将 bin_dir 加入 PATH 的操作指引（Windows）：</strong>
            <ol>
              <li>按 <kbd>Win + R</kbd>，输入 <code>sysdm.cpl</code> 回车，打开「系统属性」。</li>
              <li>切换到「高级」选项卡 → 点击「环境变量」。</li>
              <li>在「用户变量」（仅当前用户可用）或「系统变量」（所有用户可用）中找到 <code>Path</code>，点击「编辑」。</li>
              <li>点击「新建」，粘贴上面的 bin_dir 目录，确定保存。</li>
              <li>重新打开终端，直接输入 <code>{{ a.name }}</code> 加参数即可执行。</li>
            </ol>
          </div>
        </div>
      </template>
    </section>

    <p v-if="aliases.length === 0" class="empty-tip">还没有可用别名，请先到「命令别名」创建</p>
    <p v-if="error" class="error-text">{{ error }}</p>
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

.intro {
  margin: 0;
}

.intro code {
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 1px 6px;
}

.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}

.head {
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

.alias-name {
  color: var(--primary);
  font-weight: 600;
  font-size: 15px;
}

.template-code {
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 2px 8px;
  font-family: Consolas, 'JetBrains Mono', monospace;
  font-size: 13px;
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.result {
  margin-top: 14px;
  padding-top: 14px;
  border-top: 1px dashed var(--border);
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.kv {
  display: flex;
  align-items: center;
  gap: 10px;
  font-size: 13px;
}

.path {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.guide {
  margin-top: 6px;
  font-size: 13px;
  line-height: 1.8;
  color: var(--text);

  ol {
    margin: 6px 0 0;
    padding-left: 22px;
  }

  kbd,
  code {
    background: var(--bg-input);
    border: 1px solid var(--border);
    border-radius: 4px;
    padding: 0 6px;
    font-family: Consolas, monospace;
  }
}

.error-text {
  color: var(--danger);
  margin: 0;
}
</style>
