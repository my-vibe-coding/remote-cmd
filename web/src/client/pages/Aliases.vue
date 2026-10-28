<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { ApiError, localApi, type AliasDefInfo, type ServerRegInfo } from '../../shared/api'
import { confirm } from '../../shared/utils'

interface AliasItem {
  name: string
  template: string
  server: string
}

const aliases = ref<AliasItem[]>([])
// 已注册服务器（用于绑定选择）
const servers = ref<ServerRegInfo[]>([])
// 表单
const name = ref('')
const template = ref('')
const server = ref('') // '' = 默认服务器
const editing = ref<string | null>(null) // 正在编辑的原名称
const formError = ref('')
const saving = ref(false)
/** 模板是否被手动编辑过：新建模式下未编辑时模板跟随别名名自动生成，减少输入量 */
const templateTouched = ref(false)
const nameInput = ref<HTMLInputElement | null>(null)

/** 别名输入：仅新建模式且模板未手动编辑时，自动生成 <别名> {args} 形态 */
function onNameInput() {
  if (templateTouched.value || editing.value) return
  template.value = name.value ? `${name.value} {args}` : ''
}

// 后端约束：名称仅允许字母数字下划线连字符
const NAME_RE = /^[A-Za-z0-9_-]+$/

const usableServers = computed(() => servers.value)

/** 绑定服务器的显示名 */
function serverLabel(url: string): string {
  const s = servers.value.find((x) => x.server_url === url)
  if (!s) return url || '默认'
  return `${s.hostname || s.server_url}`
}

async function load() {
  try {
    const data = await localApi.getAliases()
    aliases.value = Object.entries(data).map(([n, v]: [string, AliasDefInfo]) => ({
      name: n,
      template: v.template,
      server: v.server || '',
    }))
    // 编辑中的别名若已被外部删除，退出编辑态
    if (editing.value && !aliases.value.some((a) => a.name === editing.value)) resetForm()
  } catch (e) {
    formError.value = e instanceof ApiError ? `加载别名失败：${e.message}` : '加载别名失败：网络错误'
  }
}

async function loadServers() {
  try {
    servers.value = (await localApi.getServers()).servers
  } catch {
    /* 服务器列表加载失败不阻塞别名管理 */
  }
}

function resetForm() {
  name.value = ''
  template.value = ''
  server.value = ''
  editing.value = null
  templateTouched.value = false
  formError.value = ''
}

function edit(item: AliasItem) {
  editing.value = item.name
  name.value = item.name
  // 编辑模式不自动改写模板（包括不自动追加 {args}），原样保留
  template.value = item.template
  server.value = item.server
  templateTouched.value = true
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
    await localApi.putAlias(n, t, server.value || null)
    // 修改了名称时，旧别名视为不再需要（PUT 按名称创建/更新）
    if (editing.value && editing.value !== n) await localApi.deleteAlias(editing.value).catch(() => {})
    resetForm()
    await load()
  } catch (e) {
    formError.value = e instanceof ApiError ? `保存失败 (HTTP ${e.status})：${e.message}` : '保存失败：网络错误'
  } finally {
    saving.value = false
    // 保存后聚焦别名框，便于连续录入
    nameInput.value?.focus()
  }
}

async function remove(item: AliasItem) {
  const { confirmed, value } = await confirm({
    title: '删除确认',
    content: `确定删除别名「${item.name}」吗？`,
    options: [{ label: '同时删除对应脚本', value: 'script', checked: true }],
    multiple: true, // 单选项需要可取消勾选，用 checkbox 而非 radio
    danger: true,
  })
  if (!confirmed) return
  if (value?.includes('script')) {
    // 脚本可能不存在，删除失败不阻塞别名删除
    await localApi.deleteScript(item.name).catch(() => {})
  }
  try {
    await localApi.deleteAlias(item.name)
    await load()
  } catch (e) {
    formError.value = e instanceof ApiError ? `删除失败：${e.message}` : '删除失败：网络错误'
  }
}

onMounted(() => {
  void load()
  void loadServers()
})
</script>

<template>
  <div class="page">
    <h1 class="page-title">命令别名</h1>

    <section class="card">
      <h2 class="card-title">已有别名（{{ aliases.length }}）</h2>
      <table v-if="aliases.length" class="table">
        <thead>
          <tr>
            <th style="width: 160px">名称</th>
            <th>模板</th>
            <th style="width: 180px">执行服务器</th>
            <th style="width: 140px">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="a in aliases" :key="a.name">
            <td><span class="mono alias-name">{{ a.name }}</span></td>
            <td><code class="template-code">{{ a.template }}</code></td>
            <td>
              <span v-if="a.server" class="mono">{{ serverLabel(a.server) }}</span>
              <span v-else class="dim">默认</span>
            </td>
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
        模板中的 <code>{args}</code> 占位符会在执行时替换为传入的参数（多参数以空格拼接）。
        例如模板 <code>git add {args}</code> 配合参数 <code>-A</code> 执行 <code>git add -A</code>。
        可为每个别名选择执行的服务器；选择「默认」时使用第一个已注册的服务器。
      </p>
      <div class="form-row">
        <input
          ref="nameInput"
          v-model="name"
          class="input"
          style="width: 180px"
          type="text"
          placeholder="别名，如 deploy"
          @input="onNameInput"
          @keyup.enter="save"
        />
        <input
          v-model="template"
          class="input grow"
          type="text"
          placeholder="命令模板，如 git add {args}"
          @input="templateTouched = true"
          @keyup.enter="save"
        />
        <select v-model="server" class="select" style="width: 220px" title="执行服务器">
          <option value="">默认服务器（自动选择）</option>
          <option v-for="s in usableServers" :key="s.server_url" :value="s.server_url">
            {{ s.hostname || s.server_url }}{{ s.registered ? '' : '（待审批）' }}
          </option>
        </select>
        <button class="btn btn-primary" :disabled="saving" @click="save">
          {{ saving ? '保存中…' : editing ? '保存修改' : '创建' }}
        </button>
        <button v-if="editing" class="btn" @click="resetForm">取消</button>
      </div>
      <p v-if="usableServers.length === 0" class="dim tip">
        尚未注册服务器，请先到「注册状态」页完成注册，否则别名只能使用默认服务器。
      </p>
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

.tip {
  margin: 10px 0 0;
  line-height: 1.6;
}

.error-text {
  color: var(--danger);
  margin: 12px 0 0;
}
</style>
