# 调试：别隔着手机屏幕测

> 核心是个**只听 `127.0.0.1` 的 HTTP 服务**（决策 D3）。所以从电脑上驱动它只需要
> 一条 `adb forward` —— 不用在手机小键盘上打字，也不用来回重装 APK。

```sh
just forward          # 端口是启动时随机分的，脚本从 logcat 里捞
just phone "在沙箱里建个 fizzbuzz 跑一下"
just phone            # 交互模式，/help 看命令
```

本机的核心（`just dev`）直接连：

```sh
python3 scripts/agentctl.py "…"
python3 scripts/agentctl.py            # 交互模式
python3 scripts/agentctl.py --json "…" # 一整份机器可读转录，给脚本/我吃
```

---

## 1. 边界：什么不用重装，什么必须重装

| | 要重装 APK 吗 |
|---|---|
| 调**系统提示词** | ❌ 不用（`/api/debug/system-prompt`，见下） |
| 试沙箱、跑命令、看文件 | ❌ 不用 |
| 读界面 / 发设备指令 | ❌ 不用 |
| 看**下一次会发给模型的完整请求体** | ❌ 不用 |
| 改 Rust 代码 | ✅ **要** —— 核心是 `.so`，只能重编重装 |

也就是说：**提示词、工具描述、调试流程的迭代都不用重装**（提示词和工具描述现在都能
在运行时换，见 §3）；只有改 Rust 逻辑才要。

---

## 2. `/api/debug/*`：为了「看一眼现在什么情况」

前端不用这三个接口，它们是给调试端的。

### `GET /api/debug/state`

一次拿全：版本 / 平台 / 端口 / 设备名 / 有没有在跑回合 / 活跃会话（id、标题、
消息数、rev）/ 沙箱（rootfs、手机存储挂没挂）/ 模型 / 系统提示词（多少字符、
是不是被覆盖过）。

以前要拼这些得敲六个接口。

### `GET /api/debug/request`

**下一次会发给 provider 的完整请求体**，就是模型将看到的东西。

调提示词、查 prompt cache 的时候不用猜：

```sh
just phone
> /request
model = deepseek-v4.1-flash   messages = 7   tools = ['bash', 'edit']
stream_options = {"include_usage": true}
  [  0] system        630 B  你是运行在安卓手机上的操作助手：…
  [  1] user           12 B  在沙箱里建个 fizzbuzz
  [  2] assistant      44 B
  [  3] tool          128 B
  …
合计 4321 字符（历史越靠前越是被缓存命中的部分）
```

说不清「提示词到底发出去没有」的时候，看这个。

### `POST /api/debug/system-prompt`

运行时换一份系统提示词。**这是不重装 APK 调提示词的唯一办法**：

```sh
> /prompt -            # 然后粘贴/heredoc，Ctrl-D 结束
> /prompt 你是一个只用一句话回答的助手。
> /prompt              # 看当前是哪个
> /prompt!             # 恢复成编译进去的那版
```

要点：

- **只改内存**，重启 App 就回到 `core/src/agent.rs` 里的 `SYSTEM_PROMPT`；
- 换掉之后 `state.rs` 会 `log("warn", …)`，在 App 的日志面板里看得见 ——
  否则「为什么它的表现和代码里写的不一样」能查半天；
- **不动老会话的前缀**：新提示词作为一条 `system` 消息**追加在历史末尾**，
  老前缀原样保留、缓存照命中。这就是 `ensure_system_prompt` 那套机制
  （见 [docs/03-prompt-cache.md](03-prompt-cache.md) §3.1）。

在自己机器上验证一遍机制：

```sh
> /prompt 你是一个测试用的助手，只回答"好"。
> /request
  [  0] system        630 B  你是运行在安卓手机上的操作助手：…   ← 老的不动
  [  1] user            2 B  你好
  [  2] system         18 B  你是一个测试用的助手，只回答"好"。  ← 追加的
```

---

## 3. 工具描述怎么改（也要重装，但可以少重启几次）

工具表（`bash` / `edit` 的描述和 schema）是编译进 `.so` 的，改它要重装。
但它和系统提示词一样是**可缓存前缀的一部分**，所以改完先看 `/request`
确认工具表序列化没变过样子（`/request` 的 `tools` 字段就是当前表）。

改之前先量一下体量：

```sh
cd core && cargo run -q --example dump_prompt
```

它会打出 `SYSTEM_PROMPT` + 每个工具的 desc/schema 字符数（schema 按线上紧凑写法算）。
缓存读取只有输入价的 1/50，所以精简它的意义不是省钱，而是冷启动那一次、
上下文窗口占用、以及模型注意力被无关细节稀释。

---

## 4. 绕过模型直连（最常用的调试手段）

模型不听话、或者只是想确认环境对不对的时候，别跟它绕：

| 命令 | 作用 |
|---|---|
| `/bash <命令>` | 在沙箱里直接跑（走 `/api/linux`，和 `bash` 工具同一条路） |
| `/cat <沙箱路径>` | 看沙箱里的文件（走 `/sandbox/…`，和给用户看的链接同一条路） |
| `/dev {"type":"key","name":"back"}` | 直接发设备指令 |
| `/state` | 服务端状态 |
| `/new` `/use <id>` `/sessions` `/clear` | 会话管理 |
| `/wait` | 等服务端当前回合跑完 |

`bash` 工具的 cwd 是 `/workspace`，`/bash` 也一样：

```sh
> /bash ls -la /workspace; cat /etc/alpine-release
exit=0 0.31s
   │ total 8
   │ drwxr-xr-x 2 root root 4096 …
   │ 3.21.0
```

---

## 5. SSE 断了怎么办

手机是手机会干的事：WebView 被切后台、Wi-Fi 抖一下、进程被系统压一下，
**客户端这边的 SSE 都会断** —— 但服务端的回合不会停（前台服务保着进程，
`tx.send` 失败被忽略而已）。

`agentctl` 因此有两条兜底：

- 流断了会打一行 `⚠ 流断了（…）—— 服务端可能还在跑，等它`，然后轮询
  `/api/session` 直到 `busy=false`，把**服务端那份**最终回答打出来；
- `--json` 模式下兜底结果放在 `serverView` 字段里，`errors` 里会多一条
  `stream dropped`。

手动等的时候用 `/wait`。

---

## 6. 安全边界

服务和调试接口都只听 `127.0.0.1`，没开 `0.0.0.0`；从电脑访问必须显式
`adb forward`。

需要注意：**安卓上没有网络命名空间隔离，同一个设备上的别的 App 也能连
`127.0.0.1:<端口>`**。端口是每次启动随机分的，但这不构成安全边界。已有的
`POST /api/llm`（可以改 base_url 和 key）本来就是更大的面，所以调试接口没有
引入新的信任假设 —— 但如果以后要把核心做成给别人装的 App，这一整块
（`/api/llm` + `/api/debug/*`）都该加个 token。
