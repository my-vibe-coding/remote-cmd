# remote-cmd 实施计划：远程执行 stdin 输入（WebSocket 全双工）

## Context

当前执行链路为服务端到客户端的单向 SSE 输出流：

- 服务端 `POST /api/exec`（[routes/exec.rs](../../src/server/routes/exec.rs)）：凭证认证 → 并发限制 → 白名单校验 → SSE 流式执行
- 客户端 `run_cmd::run`（[run_cmd.rs](../../src/client/run_cmd.rs)）：模板渲染 → SSE 消费 → 退出码透传
- Web「执行命令」页 → 本地客户端 `/api/local/exec`（SSE 代理）→ 服务端

目标：**执行链路整体迁移到 WebSocket 全双工**，并借此新增 stdin 输入能力（管道转发 + TTY 行模式）。

已确认决策：
- 传输选型 **WebSocket 全双工**，替换现有 SSE 执行端点（服务端不再保留 SSE 执行路径）
- Web「执行命令」页面**不做 stdin 交互**（无输入 UI/演示），但其执行流量同样经由 WS：浏览器 ↔ 本地客户端保持现有 HTTP+SSE 代理形态不变，本地客户端 ↔ 服务端段改为 WS（对浏览器完全透明）

## 核心设计：WS 连接即会话

不引入会话注册表（无 uuid、无 HashMap、无清理任务）——**一条 WebSocket 连接就是一个执行会话**，连接关闭即会话结束。

### 协议（`GET /api/exec/ws`，upgrade 前走现有 `X-Client-Token` 认证提取器）

客户端 → 服务端：

| 帧 | 内容 | 说明 |
|---|---|---|
| Text | `{"type":"init","program":"git","args":["add","-A"],"timeout_secs":60}` | 首帧，等价于旧 POST body |
| Text | 服务端回复 `{"type":"started"}`；白名单/并发不通过则 `{"type":"rejected","reason":"..."}` 后关闭 | |
| Binary | 原始字节 | stdin 数据块 |
| Text | `{"type":"stdin_close"}` | EOF，关闭子进程 stdin |

服务端 → 客户端：

| 帧 | 内容 | 说明 |
|---|---|---|
| Binary | `[0x01, bytes]` / `[0x02, bytes]` | stdout / stderr 输出块（1 字节 tag 前缀，免 base64 开销） |
| Text | `{"type":"exit","exit_code":n,"timeout":b,"duration_ms":n,"truncated":b}` | 终态帧，随后服务端关闭连接 |

### 生命周期与策略

- **并发限制**：全局/每凭证信号量在连接期间持有（含 init 校验）
- **断连策略**：客户端 WS 断开 → **终止子进程**（SSH 语义；超时兜底仍在）
- **限额**：单 stdin 帧 ≤ 64KB，单会话累计 stdin ≤ 10MB
- **审计**：沿用现有 `exec.start / exec.finish`，stdin 只记累计字节数（不记内容）

## 实施步骤

### 1. 依赖（Cargo.toml）

- axum 增加 `ws` feature（服务端 WS，无新依赖）
- 新增 `tokio-tungstenite`（rustls 系列 feature；自签证书用 danger verifier 放行，与现 reqwest `danger_accept_invalid_certs` 策略一致）

### 2. runner.rs：stdin 能力（单一代码路径）

- `spawn_exec` 改造为 `spawn_exec_stdin(req) -> (Receiver<ExecEvent>, mpsc::Sender<StdinMsg>)`，`StdinMsg = Data(Vec<u8>) | Eof`
- 子进程 stdin 为 piped；独立 writer 任务消费 mpsc → `child_stdin.write_all`，`Eof` 时 `shutdown()` 产生 EOF（mpsc 背压天然防膨胀）
- 无 stdin 需求的调用方（如 WS 桥接路径）拿到 sender 后直接 drop 即可，行为等同 stdin 关闭

### 3. 服务端：SSE 端点替换为 WS（routes/ws.rs 新增，exec.rs 删除）

```
ClientAuth + WebSocketUpgrade
  → on_upgrade: 获取信号量 → 读 init 帧 → 白名单校验（不通过发 rejected 并关闭）
  → 审计 exec.start → spawn_exec_stdin
  → tokio::select! 双向泵：
      下行: exec Receiver 事件 → 帧转发（Output 按 stream 打 tag；Exit 发终态帧后 close）
      上行: WS Binary → stdin_tx；Text stdin_close → drop sender
      上行收到 Close/错误 → kill 子进程，退出 select
```

旧 `POST /api/exec`（SSE）及 [routes/exec.rs](../../src/server/routes/exec.rs) 整体删除，路由表同步更新。

### 4. 客户端 WS 连接（remote.rs）

- `wss://` URL 由 `server_url` 派生（https→wss）
- 复用 rustls + danger verifier（ring provider，项目已统一）
- 新增 `ws_exec()`：连接、认证头、init/started 握手
- 现有 `exec_sse / consume_sse / SseEvent` 随 SSE 端点一并移除，输出/退出语义改为统一的 WS 帧解析

### 5. 客户端 CLI 双泵（run_cmd.rs）

`run()` 重构为：resolve（不变）→ WS 连接 → init/started →

- **stdin 泵**（tokio::spawn）：
  - 本地 stdin 是管道（`echo hi | gadd`）→ `spawn_blocking` 读循环 → Binary 帧 → EOF 发 `stdin_close`
  - TTY 且 `-i` → 行模式：逐行读取、回车即发送（覆盖 y/n、REPL）；Ctrl+D/Ctrl+Z 为 EOF
  - TTY 且无 `-i` → 立即发 `stdin_close`（行为等同 stdin 关闭，不挂起）
- **输出泵**（主任务）：Binary 按 tag 分流 stdout/stderr；`exit` 帧解析退出码后 `std::process::exit(code)`

### 6. Web 代理桥接（client/routes.rs，形态不变、底层换 WS）

`/api/local/exec` 对浏览器仍是 HTTP POST → SSE 响应（Run.vue 零改动、无 stdin UI）：

```
浏览器 POST /api/local/exec
  → 本地客户端以 WS 连接服务端（init program/args，不启 stdin 泵，直接 stdin_close）
  → axum Sse<Stream> 由 WS 下行帧映射：output→output、exit→exit
  → 浏览器侧消费逻辑与迁移前完全一致
```

### 7. CLI（cli.rs）

- `Run` 子命令加 `#[arg(short = 'i', long)] interactive: bool`
- shim 副本路径（main.rs 拦截）暂不支持 `-i`（参数全量透传语义不冲突），后续可用环境变量补充

## 涉及文件

| 文件 | 改动 |
|---|---|
| Cargo.toml | axum `ws` feature；tokio-tungstenite |
| src/server/runner.rs | spawn_exec → spawn_exec_stdin（含 StdinMsg） |
| src/server/routes/ws.rs（新增） | WS 执行端点 |
| src/server/routes/exec.rs | 删除（SSE 执行端点） |
| src/server/routes.rs | 路由表更新 |
| src/client/remote.rs | exec_sse/consume_sse → ws_exec + WS 帧解析 |
| src/client/run_cmd.rs | 双泵重构 |
| src/client/routes.rs | /api/local/exec 内部改走 WS（对外 SSE 形态不变） |
| src/cli.rs | `-i` flag |

## 验证

1. `cargo test`：spawn_exec_stdin 单元测试（喂字节 → 子进程回显）
2. 手动 E2E：
   - 管道：`echo hello | remote-cmd run <cat别名>` 远端回显
   - TTY 行模式：`remote-cmd run -i <别名>`（远端 `python`/`node` REPL 逐行交互）
   - 断连：WS 中途断开 → 远端进程被终止；审计记录核对
   - Web 回归：「执行命令」页执行、流式输出、退出码展示与迁移前一致（无输入框）
   - shim 回归：`gadd -A` 不受影响
