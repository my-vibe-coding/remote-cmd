<script setup lang="ts">
/**
 * 命令式确认弹窗（内部组件）：由 shared/utils.ts 的 confirm() 动态挂载到 body，
 * 不要在模板中直接使用。挂载/卸载与 Promise 落定由 utils.ts 负责。
 *
 * 职责单一：只渲染遮罩 / 标题 / 内容 / 选项 / 按钮组，并把用户操作以 done 事件抛回；
 * 选项支持单选（radio，默认预选第一项）与多选（checkbox，默认全不选）。
 */
import { onBeforeUnmount, onMounted, ref } from 'vue'
import type { ConfirmOption, ConfirmResult } from './utils'

const props = withDefaults(
  defineProps<{
    /** 弹窗标题 */
    title?: string
    /** 正文说明（可空） */
    content?: string
    confirmText?: string
    cancelText?: string
    /** 附加选项；单选/多选由 multiple 决定 */
    options?: ConfirmOption[]
    /** 选项多选模式（默认单选） */
    multiple?: boolean
    /** 危险操作：确认按钮使用红色描边样式 */
    danger?: boolean
  }>(),
  {
    title: '确认',
    content: '',
    confirmText: '确定',
    cancelText: '取消',
    options: () => [],
    multiple: false,
    danger: false,
  },
)

const emit = defineEmits<{ done: [result: ConfirmResult] }>()

// 选中集合统一用数组承载；单选模式下选中项即整体替换。
// 预选规则：checked 标记优先（多选可多项，单选取第一个标记项）；
// 无任何标记时单选回退为预选第一项，多选则全不选。
const selected = ref<string[]>([])
if (props.options.length) {
  const pre = props.options.filter((o) => o.checked).map((o) => o.value)
  if (pre.length) selected.value = props.multiple ? pre : [pre[0]!]
  else if (!props.multiple) selected.value = [props.options[0]!.value]
}

/** 以确认/取消落定弹窗；取消时不携带 value（区别于"确认但未选任何项"） */
function settle(confirmed: boolean) {
  emit('done', confirmed ? { confirmed: true, value: [...selected.value] } : { confirmed: false })
}

/** 单选：点击即整体替换（radio 的 v-model 只能绑字符串，这里统一走数组） */
function pickSingle(value: string) {
  selected.value = [value]
}

// 键盘支持：Esc 取消、Enter 确认
function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') settle(false)
  else if (e.key === 'Enter') settle(true)
}
onMounted(() => document.addEventListener('keydown', onKeydown))
onBeforeUnmount(() => document.removeEventListener('keydown', onKeydown))
</script>

<template>
  <div class="confirm-mask" @click.self="settle(false)">
    <div class="confirm-box" role="dialog" aria-modal="true">
      <h3 class="confirm-title">{{ title }}</h3>
      <p v-if="content" class="confirm-content">{{ content }}</p>
      <div v-if="options.length" class="confirm-options">
        <label v-for="opt in options" :key="opt.value" class="confirm-option">
          <input v-if="multiple" v-model="selected" type="checkbox" :value="opt.value" />
          <input
            v-else
            type="radio"
            name="confirm-dialog-radio"
            :checked="selected.includes(opt.value)"
            @change="pickSingle(opt.value)"
          />
          <span>{{ opt.label }}</span>
        </label>
      </div>
      <div class="confirm-actions">
        <button class="btn" @click="settle(false)">{{ cancelText }}</button>
        <button class="btn" :class="danger ? 'btn-danger' : 'btn-primary'" @click="settle(true)">
          {{ confirmText }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped lang="scss">
.confirm-mask {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgba(0, 0, 0, 0.5);
  animation: confirm-fade 0.15s ease;
}

.confirm-box {
  width: 420px;
  max-width: calc(100vw - 48px);
  padding: 20px;
  background: var(--bg-panel);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.4);
  animation: confirm-pop 0.15s ease;
}

.confirm-title {
  margin: 0 0 8px;
  font-size: 16px;
  font-weight: 600;
  user-select: none;
}

.confirm-content {
  margin: 0;
  color: var(--text-dim);
  line-height: 1.7;
  white-space: pre-wrap;
}

.confirm-options {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-top: 14px;
}

.confirm-option {
  display: flex;
  align-items: center;
  gap: 8px;
  cursor: pointer;
  user-select: none;

  input {
    accent-color: var(--primary);
    margin: 0;
  }
}

.confirm-actions {
  display: flex;
  justify-content: flex-end;
  gap: 10px;
  margin-top: 20px;
}

@keyframes confirm-fade {
  from {
    opacity: 0;
  }
}

@keyframes confirm-pop {
  from {
    opacity: 0;
    transform: translateY(8px) scale(0.98);
  }
}
</style>
