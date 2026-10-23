// ============================================================
// 后端 API 封装：客户端本地 API（无认证） + 服务端管理 API（Bearer）
// ============================================================

// ---------- 通用类型 ----------

/** GET /api/local/mode */
export interface ModeInfo {
  mode: string
  server_url: string
  registered: boolean
  client_id: string
  has_token: boolean
}

/** GET /api/local/info */
export interface HostInfo {
  hostname: string
  ip: string
  os: string
  user: string
}

/** POST /api/local/server/register */
export interface RegisterResult {
  client_id: string
  status: 'pending' | 'approved' | 'rejected'
  token?: string
}

/** GET /api/local/status */
export interface LocalStatus {
  registered: boolean
  status?: string
  server_url: string
  hostname: string
}

/** 注册/凭证记录（registrations 与 credentials 接口结构相同） */
export interface Registration {
  id: string | number
  fingerprint: string
  hostname: string
  ip: string
  os: string
  user: string
  status: 'pending' | 'approved' | 'rejected' | 'revoked'
  has_token: boolean
  whitelist: string[]
  created_at: string
  approved_at?: string | null
  last_seen?: string | null
}

/** GET /api/admin/audit */
export interface AuditEntry {
  ts: string
  event: string
  actor: string
  detail?: string
}

export interface ExecutionEntry {
  ts: string
  hostname: string
  program: string
  args: string[]
  exit_code: number | null
  duration_ms: number
  timeout: boolean
  truncated: boolean
}

export interface AuditData {
  audit: AuditEntry[]
  executions: ExecutionEntry[]
}

export interface LoginResult {
  session_token: string
  expires_at: string
}

/** 携带状态码的错误，供页面区分 401 / 429 等 */
export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

async function errMsg(res: Response): Promise<string> {
  try {
    const data = await res.json()
    return (data && (data.error || data.message)) || res.statusText
  } catch {
    return res.statusText
  }
}

// ---------- 客户端本地 API（HTTP，无认证，经 /api/local 代理） ----------

async function localRequest<T>(method: string, url: string, body?: unknown): Promise<T> {
  const res = await fetch(url, {
    method,
    headers: body !== undefined ? { 'Content-Type': 'application/json' } : undefined,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  })
  if (!res.ok) throw new ApiError(res.status, await errMsg(res))
  return (await res.json()) as T
}

export const localApi = {
  /** 当前工作模式与注册信息 */
  getMode: () => localRequest<ModeInfo>('GET', '/api/local/mode'),

  /** 本机信息自动采集 */
  getInfo: () => localRequest<HostInfo>('GET', '/api/local/info'),

  /** 向服务器申请注册 */
  register: (serverUrl: string) =>
    localRequest<RegisterResult>('POST', '/api/local/server/register', { server_url: serverUrl }),

  /** 注册状态（后端持续轮询远端） */
  getStatus: () => localRequest<LocalStatus>('GET', '/api/local/status'),

  /** 别名列表 */
  getAliases: () =>
    localRequest<Record<string, { template: string }>>('GET', '/api/local/aliases'),

  /** 创建/更新别名（template 支持 {args} 占位） */
  putAlias: (name: string, template: string) =>
    localRequest<{ ok: boolean }>('PUT', '/api/local/aliases', { name, template }),

  /** 删除别名 */
  deleteAlias: (name: string) =>
    localRequest<{ ok: boolean }>('DELETE', `/api/local/aliases/${encodeURIComponent(name)}`),

  /**
   * 执行命令（SSE 流式响应）。
   * 返回原始 Response，由 shared/sse.ts 的 consumeSse 解析。
   * signal 用于在面板关闭/页面离开时中断流。
   */
  async exec(alias: string, args: string[], signal?: AbortSignal): Promise<Response> {
    const res = await fetch('/api/local/exec', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ alias, args }),
      signal,
    })
    if (!res.ok) throw new ApiError(res.status, await errMsg(res))
    return res
  },

  /** 为别名生成脚本文件 */
  script: (alias: string) =>
    localRequest<{ path: string; bin_dir: string }>('POST', '/api/local/script', { alias }),
}

// ---------- 管理端 API（HTTPS，Bearer 认证，经 /api/admin 代理） ----------

export const ADMIN_TOKEN_KEY = 'rc_admin_token'

export function getAdminToken(): string | null {
  return localStorage.getItem(ADMIN_TOKEN_KEY)
}

export function setAdminToken(token: string): void {
  localStorage.setItem(ADMIN_TOKEN_KEY, token)
}

export function clearAdminToken(): void {
  localStorage.removeItem(ADMIN_TOKEN_KEY)
}

/** 401 时清除凭证并跳回登录页 */
function handleUnauthorized(): never {
  clearAdminToken()
  window.location.assign('/admin/login')
  throw new ApiError(401, '登录已过期，请重新登录')
}

async function adminRequest<T>(method: string, url: string, body?: unknown): Promise<T> {
  const token = getAdminToken()
  const headers: Record<string, string> = {}
  if (body !== undefined) headers['Content-Type'] = 'application/json'
  if (token) headers['Authorization'] = `Bearer ${token}`

  const res = await fetch(url, {
    method,
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
  })
  if (res.status === 401) handleUnauthorized()
  if (!res.ok) throw new ApiError(res.status, await errMsg(res))
  return (await res.json()) as T
}

export const adminApi = {
  /** 密码登录（此接口本身无需 token，失败时抛 ApiError(401/429)） */
  async login(password: string): Promise<LoginResult> {
    const res = await fetch('/api/admin/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password }),
    })
    if (!res.ok) throw new ApiError(res.status, await errMsg(res))
    return (await res.json()) as LoginResult
  },

  /** 注册申请列表 */
  registrations: () => adminRequest<Registration[]>('GET', '/api/admin/registrations'),

  /** 批准申请，返回签发的凭证 */
  approve: (id: string | number) =>
    adminRequest<{ client_id: string; token: string }>(
      'POST',
      `/api/admin/registrations/${id}/approve`,
    ),

  /** 拒绝申请 */
  reject: (id: string | number) =>
    adminRequest<{ ok: boolean }>('POST', `/api/admin/registrations/${id}/reject`),

  /** 删除申请记录 */
  deleteRegistration: (id: string | number) =>
    adminRequest<{ ok: boolean }>('DELETE', `/api/admin/registrations/${id}`),

  /** 已批准/已吊销凭证列表 */
  credentials: () => adminRequest<Registration[]>('GET', '/api/admin/credentials'),

  /** 更新白名单（允许执行的程序名列表） */
  putWhitelist: (id: string | number, programs: string[]) =>
    adminRequest<{ ok: boolean; whitelist: string[] }>(
      'PUT',
      `/api/admin/credentials/${id}/whitelist`,
      { programs },
    ),

  /** 吊销凭证 */
  revoke: (id: string | number) =>
    adminRequest<{ ok: boolean }>('DELETE', `/api/admin/credentials/${id}`),

  /** 审计事件与执行历史 */
  audit: (limit = 200) => adminRequest<AuditData>('GET', `/api/admin/audit?limit=${limit}`),
}
