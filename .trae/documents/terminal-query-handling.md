# 终端查询/应答（Terminal Query）问题：根因分析与架构决策

> 记录 2026-10 修复 PTY 会话中 vim/tmux 乱码问题的完整过程。这个问题跨越了客户端、服务端、ConPTY 与宿主终端四个层级，是本项目最隐蔽的一类 bug，特此沉淀为文档。

## 现象

PTY 交互会话（`remote-cmd run <pty别名>`，如 bash/tmux/vim）在部分终端中输出大量乱码：

- tmux 启动时显示：`[?61;4;6;7;14;21;22;23;24;28;32;42;52c]10;rgb:cccc/cccc/cccc\]11;rgb:0c0c/0c0c/0c0c\[8;35;145t[4;700;1450t`
- vim 底部显示：`：cccc/cccc/cccc\]11;rgb:0c0c/0c0c/0c0c\[?2026;2$y[?2048;0$y`

这些字符串本质上都是**终端查询的应答序列**（应答被当成了输入回显到屏幕上）。

## 背景：终端查询/应答机制

全屏 TUI 程序（vim/tmux）启动时会向终端发出查询，等待应答来感知终端能力：

| 查询 | 形态 | 应答 |
|---|---|---|
| CPR 光标位置 | `ESC[6n` | `ESC[<row>;<col>R` |
| DA1 主属性 | `ESC[c` | `ESC[?1;0c` |
| DA2 二级属性 | `ESC[>c` | `ESC[>0;10;1c` 等 |
| OSC 10/11 颜色 | `ESC]10;?BEL` | `ESC]10;rgb:ffff/ffff/ffff BEL` |
| DECRQM 模式 | `ESC[?2026$p` | `ESC[?2026;2$y` |
| XTWINOPS 窗口 | `ESC[14t` 等 | `ESC[4;700;1450t` |

正常链路中：程序 → PTY → 终端模拟器应答 → PTY → 程序读取。应答程序永远不该"看见"。

## 根因链（多因素叠加，逐个堵漏不可行）

1. **crossterm 抢答**：客户端开启 raw mode 后，crossterm 自身也可能响应部分查询；更重要的是本机 Windows Terminal 对所有查询形态都会应答。
2. **应答进入输入流**：WT 的应答写回 ConPTY 输入侧，被 crossterm 当作**按键**转发到服务端（客户端 `pty-in`）。
3. **ESC 被吞、裸键穿透**：crossterm 在解析按键时把应答序列的 ESC 前缀消耗掉，残余以**无 ESC 的裸键流**到达服务端（如 `[?61;4;...c]10;rgb:...`）——服务端按 ESC 锚定的过滤完全失效。
4. **批量共享终结符**：vim 会把多个 OSC 查询合并发送、共享一个终结符（`ESC]10;?ESC]11;?ESC\`），逐条匹配会漏。
5. **延迟兜底串台**：应答若延迟 300ms 发送，与后续输出/输入交错，会"回答给错误的查询"。
6. **查询形态不可穷举**：WT 对 CPR/DA1/DA2/OSC/DECRQM/XTWINOPS/kitty 等十余种形态均应答，且各程序组合不同，客户端侧逐类过滤永远追不完。

## 最终架构：源头丢弃 + 确定性兜底

原则：**在查询进入 PTY 之前从输出流中剥离它，并立即给出确定性的应答**。

### 服务端（`src/server/runner.rs`）——根治层

PTY 输出泵内置 `ScanState` 状态机（Normal/Esc/Csi/Osc/OscEsc）逐字节扫描：

- **查询序列判定后丢弃**：`is_query_csi`（CSI 查询形态 + DECRQM `$p` 规则）、`is_query_osc`（OSC `10;?`/`11;?` 颜色查询，含批量共享终结符）。丢弃的字节不会到达客户端终端，查询"从未发生"。
- **确定性兜底应答（立即发送，无延迟）**：`maybe_query_fallback` 对每类查询立即回以固定应答（CPR→`ESC[1;1R`、DA1→`ESC[?1;0c`、DA2→`ESC[>0;10;1c`、DECRQM→`$y`、XTWINOPS 忽略）；`maybe_osc_query_fallback` 回固定前景/背景色。程序拿到合理应答即可正常渲染，是否精确无关紧要。
- 非查询字节照常透传（按块 base64 转发）。

### 客户端（`src/client/run_cmd.rs`）——保险网

`ReplyFilter` 六态状态机过滤 stdin→服务端方向的应答残余：

- 正常 ESC 锚定的应答序列（`R`/`c`/`t`/`$y` 终结）直接吞掉；
- **Naked 态**处理 crossterm 吞 ESC 后的裸键残余（`[`+数字参数+终结字节 → 吞），`[c`/`[Z` 等真实按键放行；
- 即使服务端版本较旧未升级，客户端也能兜住大部分泄漏。

## 验证方法

- **python 探针**（服务器 `cpr_probe.py`）：raw 模式发出 CPR/DA1/DA2/OSC10/OSC11 五类查询，断言应答内容与到达时间（0.00s 即时）。
- **pty_probe**（`examples/pty_probe.rs`）：外层 ConPTY 模拟宿主终端全应答，任何 REPLY 触发即视为泄漏。
- **实测**：vim/tmux 渲染正常、无任何应答序列回显。

## 经验教训

1. **不要假设 ConPTY 行为与 ssh 等价**。ConPTY 在中间做了大量"翻译"，查询/应答会被截断、改写、以按键形式重新注入，字节级行为必须实测。
2. **应答泄漏要从源头治理，不要在下游逐类过滤**。查询形态不可穷举（终端厂商各自扩展），唯一可靠的位置是"查询序列经过的那条流"——即服务端输出泵。
3. **时序是隐形变量**。任何"延迟 XX ms 再应答"的策略都会与并发输出交错产生新 bug；兜底应答必须确定性且立即。
4. **日志要字节级、带时间戳、落在文件里**。`macro_log::d!` 走 stdout 会污染 PTY 终端本身（stdout 即终端流），客户端日志必须走独立的 `dlog!`（写 `%USERPROFILE%\.remote-cmd\client\debug.log`，含 file!/line!）。
5. **可控的复现环境是定位前提**：SSH 免交互（SSH_ASKPASS）+ 服务器端可随时改代码重编译 + 本地 `REMOTE_CMD_HOME` 隔离数据目录，才能高频迭代验证。
6. **联调用探针先行**：先把"什么算正确"用最小脚本（cpr_probe.py）定义出来，再改代码，避免靠肉眼扫终端输出判断。

## 相关代码

- 服务端输出泵与兜底：[runner.rs](../../src/server/runner.rs)（`ScanState`、`maybe_query_fallback`、`maybe_osc_query_fallback`）
- 客户端过滤器：[run_cmd.rs](../../src/client/run_cmd.rs)（`ReplyFilter`）
- 客户端文件日志：[dlog.rs](../../src/client/dlog.rs)
- 探针：[examples/pty_probe.rs](../../examples/pty_probe.rs)
