# remote-cmd

[English](README.en.md) | 中文

单二进制的远程命令执行工具：服务端提供 HTTPS 管理后台与命令代执行，客户端常驻本机、注册审批后即可远程执行白名单命令，实时回传输出。

## 功能特性

- **一体化单文件**：同一个二进制既是服务端也是客户端，前端页面编译期嵌入，无外部依赖
- **HTTPS 全程加密**：首次启动自动生成自签名证书（rustls），无需额外配置
- **注册审批制**：客户端需在管理后台审批通过后才能执行命令
- **命令白名单**：服务端只执行白名单内的程序，拒绝任意命令
- **实时输出**：基于 WebSocket 全双工的流式回传，输出实时展示、保留退出码，支持 stdin 交互与 PTY resize
- **PTY 交互会话**：支持交互式程序（bash/tmux/vim/REPL）远程操作，服务端自动处理终端查询/应答，杜绝应答乱码
- **别名与模板**：别名绑定命令模板（如 `git add {args}`），支持参数透传，可绑定到多个已注册服务器
- **可执行脚本 shim**：一键为别名生成可执行脚本到 bin 目录，任意终端直接调用；Ctrl+C 直接终止、无确认提示
- **审计日志**：所有执行记录可追溯
- **公网地址展示**：服务端启动时自动通过 ipv4.ip.sb 获取公网 IP，便于外部访问

## 快速开始

### 下载预编译版本

从 [GitHub Releases](https://github.com/my-vibe-coding/remote-cmd/releases) 下载对应平台的包：

- `remote-cmd-<版本>-x86_64-pc-windows-msvc.zip`（Windows）
- `remote-cmd-<版本>-x86_64-unknown-linux-musl.tar.gz`（静态链接，Alpine 等可直接运行）

### 启动服务端

```bash
remote-cmd serve              # 默认端口 3233，自动生成管理员密码并打印
remote-cmd serve --port 8443 --password mypass
```

启动后访问管理后台：`https://<主机>:3233/admin`

### 启动客户端

```bash
remote-cmd                    # 默认打开本地页面 http://127.0.0.1:3234
```

在页面中注册到服务端 → 管理后台审批通过 → 即可在「执行命令」页远程执行白名单命令。

## 使用别名

在客户端页面创建别名，例如 `gadd` → 模板 `git add {args}`：

```bash
# 生成可执行脚本（默认为所有别名）
remote-cmd gen-script

# 之后在任意终端直接调用（需将 bin 目录加入 PATH）
gadd -A
```

脚本位于 `%USERPROFILE%\.remote-cmd\bin`，是主程序的副本：以别名命名，启动时自动识别并以该别名执行远程命令。相比传统的 `.bat` 包装，Ctrl+C 直接终止且保留退出码。

也可以不生成脚本，直接调用：

```bash
remote-cmd run gadd -A
```

## CLI 参考

| 命令 | 说明 |
|---|---|
| `remote-cmd` | 以客户端模式运行，打开本地管理页面 |
| `remote-cmd serve` | 以服务端模式运行（`--port`、`--password`、`--data-dir`） |
| `remote-cmd run <别名> [参数...]` | 执行别名对应的远程命令 |
| `remote-cmd gen-script [别名]` | 生成别名脚本（省略则为所有别名） |

数据目录：

- 服务端：`%USERPROFILE%\.remote-cmd\server`（`--data-dir` 可覆盖）
- 客户端：`%USERPROFILE%\.remote-cmd\client`

## 从源码构建

```bash
# 前端（产物在编译期嵌入，必须先构建）
cd web
npm ci
npm run build

# Rust
cargo build --release
```

推送 `v*` 标签会触发 GitHub Actions，自动构建 Windows 与 Alpine (musl) 版本并上传至 Release。

## 安全模型

- 管理员密码以 Argon2 哈希存储，忘记密码可用 `--password` 重置
- 客户端注册需管理员审批，凭据绑定指纹
- 执行请求仅在白名单内的程序会被运行
- 全部通信走 HTTPS（自签名证书），执行记录写入审计日志

## 技术栈

- 后端：Rust · tokio · axum · rustls(ring) · WebSocket · portable-pty · Vue 3 + Vite（嵌入）
- Windows 原生能力：`SHOpenFolderAndSelectItems`（资源管理器定位）、ConPTY 伪终端、OEM 代码页解码（中文输出不乱码）

## License

MIT
