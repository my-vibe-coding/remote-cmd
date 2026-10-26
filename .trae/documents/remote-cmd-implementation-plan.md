# remote-cmd 实施计划

## Context

remote-cmd 是远程命令执行工具：本地开发机（客户端）需要让异构服务器执行本地不可用的命令（如 yt-dlp）。同一个 Rust 二进制承担两种角色：
- **服务端模式**（`remote-cmd serve`）：HTTPS(3233) 管理后台，注册审批、凭证/白名单管理、命令代执行（SSE 流式返回）、审计。
- **客户端模式**（双击 / `remote-cmd run`）：本地 web 页（127.0.0.1:3234）申请注册、管理命令别名、执行命令；CLI `remote-cmd run <别名> [args]` 供 bat 脚本调用，流式输出、退出码透传。

已确认的设计决策：命令别名映射（命令名→服务器命令模板）、SSE 流式输出、JSON 文件持久化、凭证+命令白名单。

关键查证结论：`macro_log::read_dir!` 可用（macro-log-macros 0.3.0 `fs.rs`），展开为 `[(相对路径, include_bytes!); N]` 数组，目录不存在时返回空数组仍可编译；需配 `build.rs` 加 `cargo:rerun-if-changed=web/dist`。

## 1. 依赖

**后端**：tokio(full), axum 0.8（路径参数 `{id}` 语法）, axum-server(tls-rustls), rcgen, clap(derive), macro-log 0.3.0, serde/serde_json, sha2, argon2, rand, uuid(v4), chrono(serde), reqwest(json,stream,rustls-tls, danger_accept_invalid_certs), base64, tempfile（原子写 persist，Windows rename 不能覆盖）, open, dirs, futures, tokio-stream, thiserror/anyhow, tower-http(trace)

**前端**：vue3, vue-router, @vueuse/core, @develon/webui-panel, sass, vite, @vitejs/plugin-vue, typescript

## 2. 后端模块

```
src/
├── main.rs            # clap 分派：serve / run / gen-script / 默认=客户端UI
├── cli.rs             # Serve{port,password,data_dir}, Run{alias,args}, GenScript{alias}
├── embed.rs           # read_dir!("web/dist") → OnceLock<HashMap>，MIME 推断，/admin→admin.html，其余→index.html
├── common/
│   ├── fingerprint.rs # SHA256(MachineGuid|hostname|ip|os归一化)；Windows 用 reg query 读 MachineGuid
│   ├── crypto.rs      # 随机 token(32字节hex)、argon2、随机密码
│   └── json_file.rs   # load_json / save_json_atomic(tempfile persist) / append_jsonl
├── server/
│   ├── mod.rs         # TLS 加载/生成 → AppState → Router → axum-server :3233
│   ├── state.rs       # Store、admin 会话表、限流器、全局/每凭证 Semaphore(8/2)
│   ├── tls.rs         # rcgen 自签证书持久化 cert.pem/key.pem，重启复用
│   ├── store.rs       # clients.json / audit.jsonl / history.jsonl（RwLock）
│   ├── auth.rs        # AdminAuth(Bearer) / ClientToken(X-Client-Token) extractor
│   ├── rate_limit.rs  # register 10次/分/IP；login 5次/5分/IP
│   ├── runner.rs      # 命令执行核心（见 §5）
│   ├── audit.rs       # register.approve/exec.finish 等事件写入
│   └── routes/        # register.rs, exec.rs, admin.rs, frontend.rs
└── client/
    ├── mod.rs         # 本地 Axum(127.0.0.1:3234，占用则随机)→open 浏览器
    ├── routes.rs / config.rs / remote.rs / run_cmd.rs / script.rs
build.rs              # rerun-if-changed=web/dist
```

## 3. API 路由

**服务端（HTTPS 0.0.0.0:3233）**

| 方法 | 路径 | 认证 | 说明 |
|---|---|---|---|
| GET | /api/health | 无 | 健康检查 |
| POST | /api/register | 无(限流) | `{hostname,ip,os,machine_guid,user}` → `{status:pending\|approved\|rejected, token?}`，指纹幂等 |
| GET | /api/register/status/{request_id} | 无(限流) | 轮询状态 |
| POST | /api/exec | 凭证 | `{program,args,timeout_secs?}` → SSE 流 |
| POST | /api/admin/login | 无(限流) | `{password}` → `{session_token}`（Bearer，内存会话 12h） |
| GET | /api/admin/registrations | 管理员 | 申请列表 |
| POST | /api/admin/registrations/{id}/approve \| /reject | 管理员 | 审批 |
| DELETE | /api/admin/registrations/{id} | 管理员 | 删除 |
| GET | /api/admin/credentials | 管理员 | 凭证列表 |
| PUT | /api/admin/credentials/{id}/whitelist | 管理员 | `{programs:[...]}` |
| DELETE | /api/admin/credentials/{id} | 管理员 | 吊销 |
| GET | /api/admin/audit \| /executions | 管理员 | 审计/执行历史 |
| GET | /* | 无 | 内嵌前端 |

**客户端本地（HTTP 127.0.0.1:3234，仅回环）**：`/api/local/mode`、`/api/local/server/register`（代理转发）、`/api/local/status`、`/api/local/aliases[/{name}]` CRUD、`/api/local/exec`（SSE 转发）、`/api/local/script`（生成 bat）

## 4. 数据文件

**服务端** `%USERPROFILE%\.remote-cmd\server\`：
- `config.json`：`{admin_password_hash, port}`
- `cert.pem`/`key.pem`：自签证书
- `clients.json`：`[{id, fingerprint, hostname, ip, os, user, status, token?, whitelist, created_at, approved_at, last_seen}]`
- `audit.jsonl` / `history.jsonl`：追加写

**客户端** `%USERPROFILE%\.remote-cmd\client\`：
- `client.json`：`{server_url, machine_guid, hostname, request_id, token, registered}`
- `aliases.json`：`{别名: {template}}`，模板支持 `{args}` 占位（出现多次每处替换；未出现且 args 非空则追加末尾）
- `bin/<别名>.bat`：`@"<exe绝对路径>" run <别名> %*`

## 5. 执行引擎（runner.rs）要点

1. 校验链：凭证 approved → 信号量（全局 8 / 每凭证 2）→ 白名单匹配（程序名不区分大小写）→ 403
2. `tokio::process::Command` 不经 shell（防注入绕过白名单），双管道并发读，mpsc 合流 → SSE
3. SSE 事件：`output {stream: stdout|stderr, data_b64}`（base64 保证二进制/编码安全）、`exit {exit_code, timeout, duration_ms}`，KeepAlive 15s
4. 超时默认 60s 上限 600s，超时 kill；输出累计 >1MB 截断；客户端断连 kill 子进程
5. CLI 侧逐行解析 SSE，stdout/stderr 分流写入，`std::process::exit(exit_code)` 透传

## 6. 注册状态机

指纹 = SHA256(MachineGuid|hostname|ip|os)。同一指纹仅一条记录：
- 不存在 → pending 新建，等待管理员批准
- pending → 返回 pending（幂等）
- approved → 直接返回 token（领取凭证，防重复）
- rejected → 返回 rejected（删除后可重新申请）
- 吊销凭证 → token 失效 exec 401

白名单默认空 = 禁止一切执行，需管理员显式配置。

## 7. 前端

单一 Vite 工程、双入口（`index.html` 客户端 / `admin.html` 管理端），vue-router：

**客户端**：`/` 注册状态页（填服务器地址，本机信息自动采集，轮询审批结果）、`/aliases` 别名管理、`/run` 手动执行（@develon/webui-panel 多面板并发跑命令，输出按流着色）、`/script` 生成 bat + PATH 指引

**管理端**：`/admin/login`、`/admin/registrations` 审批、`/admin/credentials` 白名单编辑+吊销、`/admin/audit` 审计与历史（路由守卫）

**共享**：`api.ts`、`sse.ts`（fetch+ReadableStream 支持 POST 的 SSE 解析）、`OutputConsole.vue`、`WhitelistEditor.vue`、usePolling(VueUse useIntervalFn)

## 8. 构建/开发流程

- 前端 dev：`npm run dev`，vite 代理 `/api/*`→`https://127.0.0.1:3233`(secure:false)，`/api/local/*`→`http://127.0.0.1:3234`
- 产线：`npm run build`（多入口输出 web/dist）→ `cargo build`（read_dir! 嵌入 + build.rs rerun 触发重编）

## 9. 实施里程碑

- **M1 骨架**：依赖 + build.rs + cli.rs + common/，serve 可启动
- **M2 服务端核心**：TLS、store、auth、register 状态机、admin 审批/凭证 API
- **M3 执行引擎**：runner + exec SSE + audit + 限流
- **M4 客户端角色**：config/remote/routes/run_cmd/script
- **M5 客户端前端**：4 页面 + sse.ts + 嵌入验证
- **M6 管理端前端**：4 页面
- **M7 加固**：限流参数、截断、并发、审计覆盖

## 10. 端到端验证

1. `cargo run -- serve` → 打印随机管理员密码
2. `curl -k -X POST https://127.0.0.1:3233/api/register -d '{...}'` → pending
3. 管理端登录 → 批准 → 重复注册直接返回同一 token
4. 配置白名单 `["ping"]` → `curl -k -N -X POST .../api/exec -H "X-Client-Token: ..."` → 逐行 SSE 输出 + exit 事件；非白名单 → 403
5. 客户端页建别名 → 生成 bat → `remote-cmd run <别名>` 流式输出且 ERRORLEVEL 透传
6. 安全用例：错误密码 5 次 → 429；无 token → 401；吊销后 → 401；审计页可见全部事件

---

## 附：架构演进现状（本文档为初始 MVP 计划，以下为实施后的演进记录）

初始计划已全部实施并通过端到端验证。此后经历两轮大演进，**当前架构与上文若干描述已不同**，以本节为准：

### 演进 1：执行链路迁移 WebSocket 全双工（已实施，见 [stdin-implementation-plan.md](stdin-implementation-plan.md)）

- `POST /api/exec`（SSE）→ `GET /api/exec/ws`（WS）：init/started/rejected 握手，0x01/0x02 输出帧，Exit JSON 终态帧
- 新增 stdin 双泵（管道转发 / TTY 行模式 `-i`）、PTY resize、断连 kill
- 浏览器 ↔ 本地客户端仍为 HTTP+SSE 代理形态（Run.vue 零改动），本地客户端 ↔ 服务端走 WS

### 演进 2：PTY 交互会话与终端查询治理

- **PTY 交互会话**：`tty: true` 请求走 portable-pty + raw mode，支持交互式程序（bash/tmux/vim/REPL），Ctrl+] 脱离，客户端随 init 帧上报终端尺寸
- **终端查询/应答治理**（详见 [terminal-query-handling.md](terminal-query-handling.md)）：
  - 服务端输出泵 `ScanState` 从输出流**丢弃查询序列**并**立即确定性兜底应答**（根治层）
  - 客户端 `ReplyFilter` 过滤 stdin 方向的应答残余（保险网）

### 其他实施后新增/变更

- **exe shim 脚本**：别名脚本从 `.bat` 演进为主程序副本（`shim_alias` 拦截），启动时自动刷新过期副本（`refresh_scripts`）
- **多服务器注册**：客户端 `ServerReg` 支持注册多个服务端，别名可绑定指定服务器（默认服务器兜底）
- **客户端 Host 白名单中间件**：本地页面防 DNS rebinding
- **Windows OEM 转码**：管道模式输出按首块 UTF-8 校验判定编码，GBK 等legacy 代码页统一转 UTF-8（`OutputTranscoder`）
- **安全加固**：rustls 显式 ring provider、argon2 密码哈希、注册/登录限流、审计 jsonl、原子写 persist 重试兜底
- **客户端文件日志**：`dlog!` 宏（file!/line!/时间戳）写 `client/debug.log`——PTY 场景 stdout 即终端流，禁止向 stdout 打日志
