# 调试：别隔着手机屏幕测

> 核心是个**只听 `127.0.0.1` 的 HTTP 服务**（决策 D3）。所以从电脑上驱动它只需要
> 一条 `adb forward` —— 不用在手机小键盘上打字，也不用来回重装 APK。

```sh
just doctor           # 各部件体检（核心/模型/沙箱/设备/会话库）
just phone "在沙箱里建个 fizzbuzz 跑一下"
just phone            # 交互模式，/help 看命令
just open             # 把手机上的界面开到桌面浏览器（能用真 DevTools）
```

改完代码回到「可调状态」只要一条：

```sh
just restart          # 重装 → 重启 App → 重新打开无障碍服务 → 转发端口
```

`force-stop` 会解绑无障碍服务，重启后不重新挂上，设备指令那条路就是死的 ——
所以 `restart` 里带了 `dev_a11y_on`（它会保留你已有的其它无障碍服务）。

本机的核心（`just dev`）也是同一条路，只是不用 adb：

```sh
python3 scripts/agentctl.py "…"
python3 scripts/agentctl.py            # 交互模式
python3 scripts/agentctl.py --json "…" # 一整份机器可读转录，给脚本/我吃
```

---

## 1. 连上手机：无线（WiFi）就够

```sh
# 首次配对。地址和 6 位码都在手机上：
#   开发者选项 → 无线调试 → 使用配对码配对设备
just wifi-pair 192.168.0.34:41241 123456

# 之后每次
just wifi

# 或者经典办法：USB 插一次，之后就不用插了（自动取手机的 WiFi 地址）
just wifi-tcpip
```

配对过一次就不用再管了：`forward` / `phone` / `doctor` / `open` 都只依赖 adb，
**不关心底下是 USB 还是无线**。

两个实测到的坑：

- **发行版自带的 adb 可能没有 `adb mdns`**。这台机器上 `/usr/bin/adb`
  （37.0.0-android-tools）就没有，而无线调试的自动发现全靠
  `adb mdns services` 里那行 `_adb-tls-connect._tcp`。所以 `scripts/adb.sh`
  会优先用 SDK 里那份（`$ANDROID_HOME/platform-tools/adb`）。
  真正干活的是 adb **server**（5037 端口），客户端只是跟它说话 —— 别让旧客户端
  抢先把 server 起来；要覆盖就 `AGENT_ADB=/path/to/adb just wifi`。
- 无线下 `adb logcat` / `/proc/net/tcp` 都还能读（就是慢一点），所以端口发现照旧。

### 本机端口固定 18787

核心在设备侧 `bind 0`（每次重启都换端口），但**本机这一头固定 18787** ——
脚本、书签、肌肉记忆里要的是一个稳定地址：

```sh
base=$(scripts/phone-forward.sh)   # → http://127.0.0.1:18787
```

`HOST_PORT=9000` 可以改。设备侧那个随机端口怎么找，见下。

### 核心的接口要凭据

手机上任何有网络权限的 App 都能连 127.0.0.1，扫一遍端口就能找到核心、调
`/api/linux` 在沙箱里跑命令（沙箱挂着整个内部存储）。所以 `/api/*` 一律要凭据
（`core/src/state.rs` 的 `Auth`）：

- App 的 WebView：Kotlin 先打开 `/auth?t=<token>`，核心种一个 HttpOnly cookie
  再跳回首页。页面的 JS 读不到它。
- 沙箱里的 `agent` 命令：环境变量 `AGENT_TOKEN`，走 `x-agent-token` 头。
- 电脑上的脚本：`scripts/phone-token.sh` 拿（debug 包 `run-as` 读 `files/token`，
  release 包看 logcat 里的 `agent token: …`）。`just phone` / `just doctor` /
  `just open` 已经自己拿了；Vite 对着真机开发时 `AGENT_TOKEN=$(scripts/phone-token.sh)`。
- 沙箱文件走 `/sandbox/<sandboxKey>/…`：另一把钥匙（`/api/health` 里有），只能读沙箱文件。

凭据存在 `sessions.db` 里，跨重启不变。桌面的 `agent-dev` 默认不设防，
设了 `AGENT_TOKEN` 就和手机上一样。

### 核心的端口是随机分的，怎么找

核心 `bind 0` 让系统分配端口（硬编码端口在真机上可能被别的 App 占）。四种找法，
`scripts/phone-port.sh` 按可靠性依次试：

| # | 办法 | 什么时候有用 |
|---|---|---|
| 1 | 命令行参数 / `$PHONE_PORT` | 手动兜底，永远优先 |
| 2 | `adb shell run-as <pkg> cat files/port` | App 启动时自己写的，最准最快 |
| 3 | `/proc/net/tcp` 里按 uid 筛 `127.0.0.1` 的 `LISTEN` | **不依赖 App 配合**，任何构建都能用 |
| 4 | `adb logcat` 里那行 `WebView 应指向 …` | 兜底 —— 环形缓冲会被冲掉（实测：App 明明跑着，logcat 里已经没有了） |

第 3 条是「刚装完还没写 port 文件」时的活路：

```sh
uid=$(adb shell cmd package list packages -U dev.vihv.agent | sed -n 's/.*uid:\([0-9]*\).*/\1/p')
adb shell cat /proc/net/tcp | awk -v u="$uid" '$4=="0A" && $8==u && $2 ~ /^0100007F:/ {print $2}'
# 0100007F:91CB → 端口 0x91CB = 37323
```

---

## 2. 什么能在电脑上调，什么必须在手机上

| 功能 | 从电脑上 | 怎么做 |
|---|---|---|
| 对话 / agent 循环 | ✅ | `just phone "…"` |
| 沙箱（`bash` 工具） | ✅ | `/bash <命令>` —— 真的在手机的 proot 里跑 |
| 改文件（`edit` 工具） | ✅ | `POST /api/edit`，或者让它自己调 |
| 设备指令 | ✅ | `/dev {"type":"key","name":"back"}`（坐标点击/滑动/按键/启动） |
| 系统提示词迭代 | ✅ | `/prompt …` |
| 看模型将看到什么 | ✅ | `/request` |
| 会话 / 持久化 | ✅ | `/sessions`，或 `run-as … cat files/sessions.db` 拉下来看 |
| 前端界面 | ✅ | `just open` → 桌面浏览器 + 真 DevTools；审 DOM 层级用 `just dom-audit`（§11） |
| 模型查看/操作自己 | ✅ | 沙箱里的 `agent` 命令（见 §8） |
| 改 Rust / 装包 | ⚠️ 要重装 | `just android-install` |
| 无障碍服务开关、存储权限 | ❌ | 只能在手机屏幕上点 |
| 切后台保活 / force-stop 后恢复 | ❌ | 手机上手动（核心的会话库可以直接看） |
| 触屏手势本身 | ❌ | 只能靠 `/dev` 里的坐标点击间接验证 |

### 发消息写进哪条会话：默认**不碰你的**

`agentctl` 默认用一条专用会话「调试（agentctl）」（没有就建一条，侧边栏里能看到），
**不会**写进你当前打开的那条 —— 这是踩过坑才加的：一次真机调试里，一句测试消息
被追加进了用户正在聊的会话。第一次创建时服务端会把它设成活动会话，客户端随后
把你原来那条换回去。

想写进当前会话得显式来：

```sh
just phone -- --session active "…"      # 写进你正在看的那条
just phone -- --session <会话 id> "…"    # 写进指定的那条
```

---

## 3. 一次体检：`just doctor`

各部件挨个查一遍，**只做无副作用的检查**（不按手机按键、不改文件、不装包），
所以随时能跑：

```
  ✅ 核心       v0.1.0 · android · 端口 37323
  ✅ 模型       opencode-go / deepseek-v4.1-flash
  ✅ 设备       accessibility
  ✅ 沙箱       /data/user/0/dev.vihv.agent/files/alpine-rootfs
  ✅ 跑命令      248ms · 'ok\n\n[exit 0]'
  ✅ 会话库      4 个会话
  ✅ 调试接口    可用 · 提示词 630 字符
           dataDir /data/user/0/dev.vihv.agent/files
```

它只依赖 `/api/health`（旧构建也有）+ 一条 `echo ok`，其余是加分项 —— 所以对着一台
装了旧构建的手机跑，它会明确告诉你「这台核心没有 /api/debug/*」。

`just doctor --deep` 会另外**真跑一轮对话**（花 token），用来确认 LLM 那条链路。

---

## 4. 边界：什么不用重装，什么必须重装

| | 要重装 APK 吗 |
|---|---|
| 调**系统提示词** | ❌ 不用（`/api/debug/system-prompt`，见下） |
| 试沙箱、跑命令、看文件 | ❌ 不用 |
| 发设备指令 | ❌ 不用 |
| 看**下一次会发给模型的完整请求体** | ❌ 不用 |
| 改 Rust 代码 | ✅ **要** —— 核心是 `.so`，只能重编重装 |

也就是说：**提示词和调试流程的迭代都不用重装**；只有改 Rust 逻辑才要。
（改工具描述要重装 —— 工具表是编译进去的，但它和系统提示词一样属于「可缓存
前缀」，改完先用 `/request` 确认序列化没变样，见 §6。）

---

## 5. `/api/debug/*`：为了「看一眼现在什么情况」

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
model = deepseek-v4.1-flash   messages = 7   tools = ['read', 'bash', 'edit']
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

## 6. 工具描述怎么改（也要重装，但可以少重启几次）

工具表（`read` / `bash` / `edit` 的描述和 schema）是编译进 `.so` 的，改它要重装。
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

## 7. 绕过模型直连（最常用的调试手段）

模型不听话、或者只是想确认环境对不对的时候，别跟它绕：

| 命令 | 作用 |
|---|---|
| `/bash <命令>` | 在沙箱里直接跑（走 `/api/linux`，和 `bash` 工具同一条路） |
| `/cat <沙箱路径>` | 看沙箱里的文件（走 `/sandbox/<key>/…`，和给用户看的链接同一条路） |
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

## 8. 沙箱里的 `agent` 命令：让模型查看 / 操作自己

模型平时只看得到自己的上下文和历史，看不到「自己现在什么状态」。所以沙箱里放了
一个 `agent` 命令（`core/src/linux.rs` 的 `AGENT_CLI`，`prepare_rootfs` 写进
`/usr/local/bin/agent`），它就是核心那个 HTTP 服务的薄包装 —— 只用 busybox 的
`wget`，没有别的依赖：

```sh
agent help               # 看全部
agent state              # 我现在的状态
agent request            # 我下一次会收到的完整上下文（提示词 + 工具表 + 历史）
agent tools              # 我有哪些工具
agent sessions / session # 会话列表 / 当前这条会话的消息
agent doctor             # 各部件体检
agent setup [--force|--list|包…]   # 装/看常用工具
agent command '{"type":"key","name":"back"}'   # 操作这台手机
agent get /api/…         # 直接 GET 自己的任意接口（逃生口）
agent post /api/… '{…}'
```

三件设计上的事：

1. **端口靠环境变量注入**，不靠猜。`guest_env()` 里加 `AGENT_PORT` / `AGENT_BASE`，
   值来自 `LinuxSandbox::set_agent_port()` —— 由 `Core::serve` 在 bind 成功后调用。
   核心还没 bind 时不注入（`agent` 会明确报「AGENT_PORT 没设」而不是瞎连）。
2. **`/api/llm` 和 `/api/debug/system-prompt` 被挡住**（exit 3）。那两个是
   「改自己身份」（换模型/key/提示词），留给人在宿主机上用 `scripts/agentctl.py`。
   理由：模型会把网页、文件、命令输出读进上下文，一条提示注入就能让它顺手把
   自己换成别人的模型。读自己随便读，改自己得人来。
3. **`prepare_rootfs` 里写这个文件是幂等的**（内容没变就不碰），所以每次启动都会
   存在、也不会磨文件系统。`just test` 里有两条测试守着：文件在不在 + 可执行 +
   `sh -n` 语法 + `AGENT_PORT` 注入与否。

系统提示词里有一行告诉模型这个命令存在（`dump_prompt` 能看到）—— 否则等于没有。

### 顺带：沙箱里的工具是预装好的

Alpine minirootfs 里只有 busybox，所以 App 启动后会在后台跑一次 `agent-setup`
（同样 `prepare_rootfs` 写进去的脚本，清单在 `linux.rs` 的 `TOOLSET`）：

```sh
agent setup              # 幂等，装过就秒退
agent setup --list       # 看每个工具在不在
agent setup --force      # 重跑
agent setup <包…>        # 标准那套之外再加
```

预装的是 `bash curl ca-certificates grep findutils jq git ripgrep fd tree unzip
python3 openssl`（约 75MB，只在首次下载）。细节和三个坑（apk 退出码不可信、
标记要带版本、安卓上 serve 比 rootfs 解压早）见
[docs/02-linux-sandbox.md](02-linux-sandbox.md) 的「预装的工具」。

---

## 9. SSE 断了怎么办

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

## 10. 安全边界

服务和调试接口都只听 `127.0.0.1`，没开 `0.0.0.0`；从电脑访问必须显式
`adb forward`。

需要注意：**安卓上没有网络命名空间隔离，同一个设备上的别的 App 也能连
`127.0.0.1:<端口>`**。端口是每次启动随机分的，但这不构成安全边界。已有的
`POST /api/llm`（可以改 base_url 和 key）本来就是更大的面，所以调试接口没有
引入新的信任假设 —— 但如果以后要把核心做成给别人装的 App，这一整块
（`/api/llm` + `/api/debug/*`）都该加个 token。

---

## 11. 审前端 DOM：`just dom-audit`

找「多余的 HTML 层级」不用读源码猜 —— 用**真实渲染树**测。全本地，不碰手机、不花 token：

```sh
just dom-audit                  # 全量：会话区 + 各个面板
just dom-audit --no-panels      # 只看会话区
```

它起四个进程（跑完自动收，端口自动挑空闲的 —— dev 机器上常年躺着别人家的 5173）：

| 进程 | 端口 | 干什么 |
|---|---|---|
| 假 LLM | 8790 | `scripts/fixture-llm.ts`：给界面提供**受控内容**（先发一个真 `bash` 工具调用，再回一段 Markdown 夹具） |
| 核心 | 8791 | 独立实例，LLM 走 `AGENT_LLM_*` 环境变量（**只在内存里**，不读不写你的 sessions.db，也不动 8787 那个） |
| vite | 5173+ | 代理 `/api` 到核心；**改前端源码不用重编 Rust** |
| Chrome | 9222 | headless + CDP，被审的就是它这份渲染树 |

判据两道：

1. **静态筛候选**：只有一个元素子节点、没有自有文字、盒子与子元素**完全重合**、不是滚动容器、
   计算样式里没有任何有视觉或层叠上下文副作用的属性。
2. **拔掉测试**：把候选的 `display` 改成 `contents`（这一层不再产生盒子），比对**整页每个元素**
   的盒子 —— 一个像素都没动，才算真多余。

第二道不能省：静态看着「完全重合」的层，可能是父级 flex 的 item（被拉伸），或者子元素靠它的
`flex-1` 撑宽 —— 删了就变。实测：Markdown 列表项里那层 `div.flex.min-w-0.flex-1` 静态上完全
重合，拔掉后段落宽度 359 → 335.5px。**只有动态测能分辨。**

### 报告是「候选」，不是判决书

拔掉测试只对**当前状态**有效，而且它只看几何。下面三类它看不见，动手删之前必须回源码确认：

- 只承载**可继承**样式的层（font-size / color / leading）—— `display:contents` 不丢继承。
- 只当 JS 钩子的层（`bind:this` / `bind:clientWidth`）—— 比如 ThoughtLine 那两层 span。
- **语义必需**的层（`li` / `tr` / `td`）—— 几何上透明，但删了 DOM 就不合法。

反过来也一样：这一屏里惰性的层，换个内容可能就有用了（`#app` 那个 div 就时灵时不灵）。

### 改完前端，验证「布局一个像素都没动」

```sh
just dom-audit --snapshot /tmp/before.json   # 改之前
# …改代码…
just dom-audit --diff /tmp/before.json       # 改之后
```

快照按**文字**做锚点，不是按元素序号：删掉一层冗余 div，后面元素的下标会整体移一位，
按序号比全是假阳性；而冗余层自己没有文字，进不了快照，真正该在位的段落/列表项/按钮都带文字。

### 坑（踩过的）

- **别用核心自带的脚本化演示流审界面**（`POST /api/chat {"demo":true}`）：那条路**不落库**，
  而前端在流结束后会用服务端的会话视图覆盖本地消息（`loadMessages`）—— 演示内容会被立刻
  冲掉，界面回到空态。它只验协议链路。夹具要发**真的**工具调用，走真实 agent loop。
- **判据里不要看 `border*Style` / `outlineWidth`**：Tailwind preflight 把所有元素的
  `border-style` 设成 `solid`（宽度 0，不渲染），Chrome 的 `outline-width` 恒为 `3px`
  （`outline-style` 是 `none`，也不渲染）。带上这两个，**所有**元素都会被判成「有副作用」，
  报告直接归零。
- 夹具那个 `bash` 调用要真跑，得有 rootfs（`just linux-setup`）。没装也能跑完，
  只是工具行会显示执行失败。

---
