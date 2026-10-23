<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { ApiError, localApi } from '../../shared/api'

interface AliasItem {
  name: string
  template: string
}

const aliases = ref<AliasItem[]>([])
// 表单
const name = ref('')
const template = ref('')
const editing = ref<string | null>(null) // 正在编辑的原名称
const formError = ref('')
const saving = ref(false)

// 后端约束：名称仅允许字母数字下划线连字符
const NAME_RE = /^[A-Za-z0-9_-]+$/

async function load() {
  try {
    const data = await localApi.getAliases()
    aliases.value = Object.entries(data).map(([n, v]) => ({ name: n, template: v.template }))
    // 编辑中的别名若已被外部删除，退出编辑态
    if (editing.value && !aliases.value.some((a) => a.name === editing.value)) resetForm()
  } catch (e) {
    formError.value = e instanceof ApiError ? `加载别名失败：${e.message}` : '加载别名失败：网络错误'
  }
}

function resetForm() {
  name.value = ''
  template.value = ''
  editing.value = null
  formError.value = ''
}

function edit(item: AliasItem) {
  editing.value = item.name
  name.value = item.name
  template.value = item.template
  formError.value = ''
}

async function save() {
  const n = name.value.trim()
  const t = template.value.trim()
  if (!n) {
    formError.value = '请输入别名名称'
    return
  }
  if (!NAME_RE.test(n)) {
    formError.value = '名称仅允许字母、数字、下划线与连字符'
    return
  }
  if (!t) {
    formError.value = '请输入命令模板'
    return
  }
  saving.value = true
  formError.value = ''
  try {
    await localApi.putAlias(n, t)
    // 修改了名称时，旧别名视为不再需要（PUT 按名称创建/更新）
    if (editing.value && editing.value !== n) await localApi.deleteAlias(editing.value).catch(() => {})
    resetForm()
    await load()
  } catch (e) {
    formError.value = e instanceof ApiError ? `保存失败 (HTTP ${e.status})：${e.message}` : '保存失败：网络错误'
  } finally {
    saving.value = false
  }
}

async function remove(item: AliasItem) {
  if (!window.confirm(`确认删除别名「${item.name}」？`)) return
  try {
    await localApi.deleteAlias(item.name)
    await load()
  } catch (e) {
    formError.value = e instanceof ApiError ? `删除失败：${e.message}` : '删除失败：网络错误'
  }
}

onMounted(load)
</script>

<template>
  <div class="page">
    <h1 class="page-title">命令别名</h1>

    <section class="card">
      <h2 class="card-title">已有别名（{{ aliases.length }}）</h2>
      <table v-if="aliases.length" class="table">
        <thead>
          <tr>
            <th style="width: 180px">名称</th>
            <th>模板</th>
            <th style="width: 140px">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="a in aliases" :key="a.name">
            <td><span class="mono alias-name">{{ a.name }}</span></td>
            <td><code class="template-code">{{ a.template }}</code></td>
            <td>
              <button class="btn btn-sm" @click="edit(a)">编辑</button>
              <button class="btn btn-sm btn-danger" style="margin-left: 8px" @click="remove(a)">删除</button>
            </td>
          </tr>
        </tbody>
      </table>
      <p v-else class="empty-tip">还没有别名，先在下方创建一个</p>
    </section>

    <section class="card">
      <h2 class="card-title">{{ editing ? `编辑别名「${editing}」` : '新建别名' }}</h2>
      <p class="dim usage">
        模板中的 <code>{'{'}args{'}'}</code> 占位符会在执行时替换为传入的参数（多参数以空格拼接）。
        例如模板 <code>git add {'{'}args{'}'}</code> 配合参数 <code>-A</code> 执行 <code>git add -A</code>。
      </p>
      <div class="form-row">
        <input
          v-model="name"
          class="input"
          style="width: 200px"
          type="text"
          placeholder="别名，如 deploy"
          @keyup.enter="save"
        />
        <input
          v-model="template"
          class="input grow"
          type="text"
          placeholder="命令模板，如 git add {args}"
          @keyup.enter="save"
        />
        <button class="btn btn-primary" :disabled="saving" @click="save">
          {{ saving ? '保存中…' : editing ? '保存修改' : '创建' }}
        </button>
        <button v-if="editing" class="btn" @click="resetForm">取消</button>
      </div>
      <p v-if="formError" class="error-text">{{ formError }}</p>
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

.alias-name {
  color: var(--primary);
  font-weight: 600;
}

.template-code,
.usage code {
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 2px 6px;
  font-family: Consolas, 'JetBrains Mono', monospace;
  font-size: 13px;
}

.usage {
  margin: 0 0 14px;
  line-height: 1.8;
}

.form-row {
  display: flex;
  gap: 12px;
  align-items: center;
}

.grow {
  flex: 1;
}

.error-text {
  color: var(--danger);
  margin: 12px 0 0;
}
</style>
