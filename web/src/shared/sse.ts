// ============================================================
// SSE 解析：支持 POST 请求返回的 text/event-stream
// ============================================================

/** 一个解析完成的 SSE 事件 */
export interface SseEvent {
  /** 事件名，如 output / exit；未指定时为 message */
  event: string
  /** data 内容（多行 data 以 \n 拼接） */
  data: string
}

/**
 * 逐块读取 Response 流，按 "event:"/"data:" 行 + 空行分隔解析为事件。
 * 适用于 POST /api/local/exec 这类无法使用 EventSource 的 SSE 接口。
 */
export async function consumeSse(
  response: Response,
  onEvent: (ev: SseEvent) => void,
): Promise<void> {
  if (!response.body) throw new Error('响应不支持流式读取')

  const reader = response.body.getReader()
  const decoder = new TextDecoder('utf-8')
  let buffer = ''

  const dispatch = (rawBlock: string) => {
    const ev = parseEventBlock(rawBlock)
    if (ev) onEvent(ev)
  }

  for (;;) {
    const { done, value } = await reader.read()
    if (done) break
    buffer += decoder.decode(value, { stream: true })

    // 以空行（\n\n）切分完整事件
    let sep: number
    while ((sep = buffer.indexOf('\n\n')) >= 0) {
      dispatch(buffer.slice(0, sep))
      buffer = buffer.slice(sep + 2)
    }
  }

  // 处理结尾未被空行终止的残留数据
  buffer += decoder.decode()
  if (buffer.trim().length > 0) dispatch(buffer)
}

/** 解析单个事件块，忽略注释行与空行 */
function parseEventBlock(block: string): SseEvent | null {
  let event = 'message'
  const dataLines: string[] = []

  for (const rawLine of block.split('\n')) {
    const line = rawLine.replace(/\r$/, '') // 兼容 CRLF
    if (line === '' || line.startsWith(':')) continue
    if (line.startsWith('event:')) {
      event = line.slice(6).trim()
    } else if (line.startsWith('data:')) {
      // 按规范去掉 data: 后的一个空格
      dataLines.push(line.slice(5).replace(/^ /, ''))
    }
    // 其余字段（id/retry 等）本前端不使用，忽略
  }

  if (dataLines.length === 0) return null
  return { event, data: dataLines.join('\n') }
}

// ---------- /api/local/exec 的载荷解码 ----------

/** 输出事件：stream 为 stdout / stderr / __truncated__ */
export interface ExecOutputPayload {
  type: 'output'
  stream: 'stdout' | 'stderr' | '__truncated__'
  data_b64: string
}

/** 退出事件 */
export interface ExecExitPayload {
  type: 'exit'
  exit_code: number | null
  timeout: boolean
  duration_ms: number
  truncated: boolean
}

/** base64 → UTF-8 字符串（保证二进制/中文等编码安全） */
export function decodeBase64Utf8(b64: string): string {
  const bytes = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0))
  return new TextDecoder('utf-8').decode(bytes)
}
