// ============================================================
// 通用小工具：时间/耗时格式化
// ============================================================
import { createApp } from 'vue'
import ConfirmDialog from './ConfirmDialog.vue'

/** 将后端时间戳格式化为本地可读时间（解析失败时原样返回） */
export function formatTs(ts: string | null | undefined): string {
  if (!ts) return '-'
  const n = Number(ts)
  const d = Number.isFinite(n) && ts !== '' ? new Date(n > 1e12 ? n : n * 1000) : new Date(ts)
  if (isNaN(d.getTime())) return String(ts)
  const pad = (v: number) => String(v).padStart(2, '0')
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  )
}

/** 耗时（毫秒）→ 可读文本 */
export function formatDuration(ms: number | null | undefined): string {
  if (ms == null) return '-'
  if (ms < 1000) return `${ms} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`
  const m = Math.floor(ms / 60_000)
  const s = Math.round((ms % 60_000) / 1000)
  return `${m}m${s}s`
}

/** 当前时间的短格式，用作执行面板标题 */
export function nowShort(): string {
  const d = new Date()
  const pad = (v: number) => String(v).padStart(2, '0')
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

// ============================================================
// 命令式确认弹窗
// ============================================================

/** confirm 弹窗的附加选项 */
export interface ConfirmOption {
  /** 选项显示文本 */
  label: string
  /** 选中后返回的值 */
  value: string
  /** 默认选中（多选可多项标记；单选取第一个标记项，无标记时回退为第一项） */
  checked?: boolean
}

export interface ConfirmParams {
  /** 弹窗标题 */
  title?: string
  /** 正文说明（可空） */
  content?: string
  confirmText?: string
  cancelText?: string
  /** 附加选项；单选/多选由 multiple 控制 */
  options?: ConfirmOption[]
  /** 选项多选模式（默认单选，单选默认预选第一项） */
  multiple?: boolean
  /** 危险操作：确认按钮使用红色描边样式 */
  danger?: boolean
}

export interface ConfirmResult {
  confirmed: boolean
  /** 确认且带 options 时的选中值集合；取消时缺省 */
  value?: string[]
}

/**
 * 命令式确认弹窗：动态挂载 ConfirmDialog 到 body，Promise 随用户操作落定后自动卸载。
 * 替代 window.confirm——文案可控、样式统一、支持附加选项。
 *
 * @example
 * const { confirmed } = await confirm({ title: '删除确认', content: '确定删除吗？', danger: true })
 * if (!confirmed) return
 *
 * @example
 * const { confirmed, value } = await confirm({
 *   title: '执行方式',
 *   content: '选择本次执行方式',
 *   options: [{ label: '仅本次', value: 'once' }],
 * })
 *
 * @example
 * const { confirmed, value } = await confirm({ title: '批量操作', multiple: true, options: [...] })
 */
export function confirm(params: ConfirmParams = {}): Promise<ConfirmResult> {
  return new Promise((resolve) => {
    const container = document.createElement('div')
    document.body.appendChild(container)
    const app = createApp(ConfirmDialog, {
      ...params,
      // 弹窗以 done 事件落定：resolve 后立即卸载并移除容器
      onDone(result: ConfirmResult) {
        resolve(result)
        app.unmount()
        container.remove()
      },
    })
    app.mount(container)
  })
}
