// ============================================================
// 通用小工具：时间/耗时格式化
// ============================================================

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
