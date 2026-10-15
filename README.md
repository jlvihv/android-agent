# android-agent

跑在 Android 手机上的 agent 工具：**Kotlin 薄壳 + Rust 核心 + Web 前端（WebView）**。

Agent 拿到的能力：**读界面 / 操作界面 / 跑 Linux 命令 / 改文件**。

## 状态

**已在 Pixel 10 / Android 17 真机上完整验证。**

| 部分 | 状态 |
|---|---|
| `core/` Rust | ✅ 320 个测试全过，clippy 干净 |
| `web/` Svelte | ✅ 构建通过（169 KB JS / 61 KB gzip） |
| `android/` 壳 | ✅ **真机验证通过** |
| LLM | ✅ OpenCode Go / `deepseek-v4.1-flash`，真机跑通 |
| Linux 沙箱 | ✅ **Alpine 3.24 aarch64 在真机上跑起来**，`apk add` 可用；设置里能一键原地升级 Alpine |
| 桌面宿主模式 | ✅ `just dev-host`：不套沙箱，`bash`/`read`/`edit` 直接作用在这台机器上；**Linux 和 macOS** 都行（[docs/11](docs/11-host-mode.md)） |
| 出门在外连家里电脑 | ✅ 一台有公网 IP 的服务器当中继（`just relay-deploy root@IP`）；端到端 TLS，中继看不到内容（[docs/12](docs/12-relay.md)） |
| 换机器（谁连谁都行） | ✅ 侧栏底栏那个标签点开就是「选择机器」：**手机和电脑浏览器是同一份面板**，电脑也能连另一台电脑（手机连、电脑连都走核心里同一条隧道；页面拿不到任何机器的凭据；[docs/12](docs/12-relay.md)） |
| 桌面浏览器 | ✅ 核心**自己起**无头 Chrome（CDP），`agent browser` 在桌面上也能用（[docs/08](docs/08-browser.md) 第 15 节） |
| 设备操控 | ⚠️ 只做**不需要读界面**的：坐标点击 / 滑动 / 系统按键 / 启动应用 / 打开 URI |
| 手机系统能力 | ✅ 沙箱里的 `agent device\|clipboard\|volume\|brightness\|torch\|vibrate\|screenshot\|radios\|alarm\|location\|contacts\|calendar\|sms\|notify\|speak`（另加两段都能用的 `agent weather`）；**电脑上不列也不让跑**（[docs/13](docs/13-phone.md)） |
| Prompt cache | ✅ 命中 73–87% |
| 界面 | ✅ 对齐 Claude 移动端（神似，非像素级） |
| 多会话 | ✅ 侧边栏「最近对话」，标题自动生成 |
| 会话持久化 | ✅ SQLite（增量落盘），force-stop 后能恢复 |
| 后台运行 | ✅ 前台服务保活，切回来自动同步；通知栏**实时显示在跑什么**——本地核心和「连着的那台电脑」走同一条路（[docs/06](docs/06-persistence-and-background.md)） |

## 快速开始（桌面）

```sh
# 1. 准备 Linux 沙箱（x86_64，用于桌面开发）
mkdir -p .dev/linux && cd .dev/linux
curl -sSL -o alpine-minirootfs.tar.gz \
  https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-minirootfs-3.24.2-x86_64.tar.gz
mkdir -p rootfs && tar xzf alpine-minirootfs.tar.gz -C rootfs
cd ../..

# 2. 配 LLM（.env.local 已在 .gitignore 里）
cat > .env.local <<'EOF'
OPENCODE_BASE_URL=https://opencode.ai/zen/go/v1
OPENCODE_API_KEY=oc_sk_...
OPENCODE_MODEL=deepseek-v4.1-flash
EOF
chmod 600 .env.local

# 3. 构建前端 + 起核心
just web-build
just dev        # 沙箱里跑：bash 在 Alpine rootfs 里，改不到你的机器
# just dev-host  # 宿主模式：没有沙箱，bash / read / edit 直接作用在这台机器上（macOS 只有这个）

# 4. 另开一个终端，让 agent 干活
python3 scripts/agentctl.py "在沙箱里建一个 FizzBuzz 脚本，跑一下"
```

打开 <http://127.0.0.1:8787> 就是完整 App —— 和手机上 WebView 看到的是同一份东西。

**想直接用这台机器**（不套沙箱）：`just dev-host` —— `bash` / `read` / `edit`
改的就是本机真文件；cwd（`/workspace` 的落点）默认是这个仓库，
`AGENT_HOST_WORKSPACE="$PWD" just dev-host` 可以换成现在这个目录。
**这台机器上只跑一个核心**：已经有一个在跑时，新起的那个会直接拒绝并告诉你它的
pid（同时跑两个会静默地互相干扰），要两个就 `AGENT_MULTI=1`。
差别、代价和实现见 [docs/11-host-mode.md](docs/11-host-mode.md)。

**让核心常驻**（开机就在，不用手动开终端）：它是普通的 HTTP 服务，扔给 systemd
用户服务最省事 ——

```ini
# ~/.config/systemd/user/android-agent.service
[Service]
WorkingDirectory=%h/Code/project/android-agent
EnvironmentFile=-%h/Code/project/android-agent/.env.local   # 模型 / 中继都从这来
Environment=AGENT_LINUX_HOST=1
Environment=AGENT_HOST_WORKSPACE=%h/Code/project/android-agent
Environment=AGENT_PORT=8787
ExecStart=%h/Code/project/android-agent/core/target/release/agent-dev
Restart=always

[Install]
WantedBy=default.target
```

```sh
systemctl --user enable --now android-agent     # 开机自启（配上 loginctl enable-linger）
```

unit 里**不写模型配置**：核心自己认 `OPENCODE_*`（`AGENT_LLM_*` 优先、回退它，见
`core/src/llm.rs` 的 `from_env`），真实来源只有 `.env.local` 一个。要跑的是 release
二进制，所以改完代码先 `scripts/restart-core.sh`（它发现核心归 systemd 管时会用
`systemctl --user restart`）。

对**手机上**跑着的那个（不用重装 APK、也不用手点手机屏幕）：

```sh
just phone "在沙箱里建个 fizzbuzz 跑一下"    # 自动 adb forward 后连上去
just phone                                   # 交互模式，/help 看命令
```

细节见 [docs/07-debugging.md](docs/07-debugging.md)。

## 架构

```
┌──────────────────────────────────────────────────────────┐
│ WebView  http://127.0.0.1:<port>                          │
│   Svelte 5 + Vite + Tailwind v4                          │
│   ← SSE: AI SDK UI Message Stream (x-vercel-ai-…: v1)    │
│   ↔ WS:  事件总线                                          │
└──────────────────────────────────────────────────────────┘
                          │ 同源 HTTP + WS
┌──────────────────────────────────────────────────────────┐
│ Rust core (.so, 进程内, tokio + axum)                     │
│   axum + rust-embed        静态资源 + /api + /ws          │
│   openai.rs / responses.rs 自研 LLM 协议，不引 SDK           │
│   自研 agent loop           事件流 / 工具循环               │
│   linux.rs                  proot + Alpine 沙箱           │
│   editor.rs                 edit 工具（精确替换 + diff）    │
│   uniffi                    正向绑定 + 反向 callback        │
└──────────────────────────────────────────────────────────┘
                          │ UniFFI callback
┌──────────────────────────────────────────────────────────┐
│ Kotlin 壳（只做平台能力，不做业务逻辑）                    │
│   MainActivity / AccessibilityService / BrowserController  │
└──────────────────────────────────────────────────────────┘
```

## 设计铁律

1. **自研 LLM 协议，不引 SDK** —— `openai.rs`（Chat Completions）+ `responses.rs`
   （Responses API）。默认**优先 Responses**，端点不支持（404/405）自动退回
   Chat Completions 并记住。为什么要自研：`async-openai` 会在我们拿不到的地方
   建自己的 reqwest client，`rustls-no-provider` 下直接 panic（见下）
2. **不手写流式协议** —— 用 AI SDK 的 UI Message Stream 规范
3. **不手写反向 JNI** —— UniFFI callback interface
4. **native 侧不反调 Java** —— 跨语言边界只有 JSON 字符串，Rust 里没有一行 JNI
5. **Kotlin 壳不写业务逻辑** —— 只放非它不可的平台能力（WebView、无障碍、
   Intent、前台服务）。壳现在 ~5600 行，其中 `BrowserController.kt` 一个就 ~1500 行
   （一半是注入的 JS）。行数不当硬指标，但**判断该放哪边**的标准不变：能在 Rust
   里做的不放 Kotlin
6. **提示词前缀一个字都不能变** —— 见 [docs/03-prompt-cache.md](docs/03-prompt-cache.md)
7. **没有审批环节** —— 工具调用一律直接执行，服务端不保存「挂起等回复」的状态

## Agent 工具

给模型**三个**工具：

| 工具 | 作用 |
|---|---|
| `read` | 读文件：文本带 `offset` / `limit` / `numbered`（`numbered=true` 加 `cat -n` 同款行号；截断到 2000 行或 50KB），没读完时末尾写明整个文件多少行、多大、从哪续读；**图片缩放后直接发给模型看** |
| `bash` | 在 Alpine 沙箱里跑命令（可 `apk add`），cwd 是 `/workspace` |
| `edit` | 改文件：按原文精确替换，返回 unified diff；新建文件：只给 `content` |

设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）**不作为工具**暴露给模型 ——
它们在沙箱里（`agent command`）。浏览器和联网搜索也在沙箱里：唯一入口是
`agent browser` / `agent search`。

**判据、代价、以及「为什么只剩这三个」**：
[docs/10-tools-vs-sandbox.md](docs/10-tools-vs-sandbox.md)。

agent loop **没有轮数上限**：任务要多少轮就跑多少轮，唯一的出口是模型自己不再调工具，
或者用户按停止（`POST /api/stop`）。停止是真的停服务端循环 —— 前端断流循环是看不到的。
LLM 请求遇到临时故障（429 / 5xx / 断网 / 连上之后一个字没出就断了）会退避重试
三次（2s / 5s / 10s，界面上有提示）；已经流出内容才断的不重试（重来的话界面上是两份），
鉴权失败这类重试也没用的直接报错。

`bash` 的 `timeout`（秒）可选，**默认 60s**。下载 / `apk add` / 编译 / `git clone`
这类长任务由模型显式给足（上限 24h，估大不亏：命令一结束就返回）。超时被杀时结果里
会写明用了多少秒、默认值的话还会教它怎么给 —— 下一次它就知道要传了。
（必填的话每次都要估时间，而绝大多数命令几秒就完 —— 那是噪音。）

内核那条路（`/api/command` → 无障碍服务）只做**不需要读界面**的几条：
坐标点击、滑动、系统按键、启动应用。要读 UI 树（把无障碍树拍平成 JSON 推给 Rust）
得重做一遍 —— 做法还是「native 侧拍平推上来、Rust 回传指令」，加能力的步骤见
docs/10。

工具调用一律直接执行，没有审批环节 —— 服务端因此不持有「挂起等回复」的状态，
App 随时被杀都没关系。

## 文档

| 文件 | 内容 |
|---|---|
| [docs/01-tech-selection.md](docs/01-tech-selection.md) | 选型决策记录（含被否决的选项和理由） |
| [docs/02-linux-sandbox.md](docs/02-linux-sandbox.md) | **OpenMinis 怎么在 Android 上跑 Alpine**（W^X 问题与解法） |
| [docs/03-prompt-cache.md](docs/03-prompt-cache.md) | 提示词前缀不变式 + 命中率 |
| [docs/04-android-shell.md](docs/04-android-shell.md) | Android 壳：三个架构决策的落点、真机上的坑 |
| [docs/05-ui.md](docs/05-ui.md) | 界面：设计令牌、edge-to-edge、对话渲染 |
| [docs/06-persistence-and-background.md](docs/06-persistence-and-background.md) | 会话持久化（为什么是 SQLite）、后台运行与**实时更新的通知** |
| [docs/07-debugging.md](docs/07-debugging.md) | 无线调试 + agentctl + `/api/debug/*`：不重装 APK 也能调 |
| [docs/08-browser.md](docs/08-browser.md) | agent 的浏览器：无头 WebView、看板、截图 |
| [docs/09-search.md](docs/09-search.md) | 联网搜索：Exa 的两种接口、为什么不要钱 |
| [docs/10-tools-vs-sandbox.md](docs/10-tools-vs-sandbox.md) | **工具表为什么只有三个**：判据、代价、加能力的步骤 |
| [docs/11-host-mode.md](docs/11-host-mode.md) | **宿主模式**：桌面上不套沙箱，直接用这台机器（怎么做的、代价） |
| [docs/12-relay.md](docs/12-relay.md) | **换机器**：手机/电脑连到另一台机器上的核心（隧道、局域网/中继、页面不拿凭据、部署与验证） |
| [docs/13-phone.md](docs/13-phone.md) | **手机系统能力**：闹钟 / 短信 / 日历 / 联系人 / 定位 / 剪贴板 / 通知 / 朗读，以及「按机器显示不同子命令」与权限那套阻塞桥 |

## 脚本

| 命令 | 作用 |
|---|---|
| `just dev` | 起核心（桌面，mock 设备 + 本地 rootfs） |
| `just dev-host` | 起核心（桌面**宿主模式**：没有沙箱，直接跑在这台机器上） |
| `just web-dev` | 起 Vite（`/api` 代理到核心） |
| `just test` | Rust 测试 |
| `just smoke` | 端到端：一句话 → 工具执行 → 复述 |
| `just android-assets` | 一次性：取 Alpine rootfs + proot 二进制 |
| `just android-debug` | 完整 Android 构建 → APK |
| `just android-install` | 构建 + 装到连着的手机 |
| `python3 scripts/agentctl.py "…"` | 给 agent 一句话并打印转录；不带参数进交互模式（`--json` 机器可读） |
| `scripts/restart-core.sh` | **重启电脑上那个核心**（改完核心代码要生效时）。先编译、等回合答完、才杀；从 agent 里跑会自己脱离会话，之后顺手把手机 App 叫起来。核心若归 systemd 用户服务管（`android-agent.service`）就改用 `systemctl --user restart` |

对着**手机上**跑着的核心（无线调试就够，不用插线、也不用手点手机屏幕）：

| 命令 | 作用 |
|---|---|
| `just wifi-pair <ip:port> <码>` | 首次配对（手机上开发者选项 → 无线调试） |
| `just wifi` / `just wifi-tcpip` | 之后每次连上（后者：USB 插一次，以后自动） |
| `just forward` | 把手机上核心的端口转发到本机（App 不在前台就顺手叫到前台） |
| `just wake [on\|off]` | 手机那头会不会被系统冻住：看状态 / 加 doze 白名单 + 叫到前台 |
| `just doctor [--deep]` | 各部件体检：核心/模型/沙箱/设备/会话库 |
| `just phone "…"` | 和手机上的 agent 说话 |
| `just open` | 把手机上的界面开到桌面浏览器（真 DevTools） |

## 构建 APK

```sh
just android-assets     # 一次性
just android-debug      # → android/app/build/outputs/apk/debug/app-debug.apk
just android-install     # 或 adb install -r android/app/build/outputs/apk/debug/app-debug.apk
just android-log        # 看 agent 的日志
```

首次启动会跳到无障碍设置页 —— 开启后 agent 才能读界面和点击。

## 真机调试

```sh
. scripts/dev.sh
dev_restart      # 重装 + 重启 + 转发端口
dev_a11y_on      # 开无障碍（保留已有的其它服务）
dev_llm          # 注入 .env.local 里的 LLM 配置
dev_shell 'uname -m'
```

安卓上 App 不继承 shell 环境变量，所以 LLM 在 App 的**设置页**里配：内置
OpenAI / DeepSeek / OpenCode Go / OpenCode Zen，只填 API Key，模型清单自动拉；
另有「其他（OpenAI 兼容）」可以填 Base URL。几家可以同时配好，在选择模型面板里
点哪家的模型就用哪家。配置按列存在 `sessions.db` 的表里（`llm` / `providers`，不用 JSON）。脚本里还可以直接 `POST /api/llm`。

模型能不能看图**自动判定**，不用配：`GET /api/models` 拉 provider 的模型清单，
再查 [models.dev](https://models.dev) 目录里该模型的 `modalities.input` 有没有
`image`。判定结果跟着 provider 配置落盘，下次启动直接用。`read` 读到的图片会作为
`image_url` 发给能看图的模型；查不到（目录里没这个模型）就沿用上次的值。

PDF / 文档输入同理，看的是 `modalities.input` 里的 `pdf`（`/api/health` 的
`capabilities.pdf`）。**只有它为真时，输入卡片的「+」里才会出现「文件」入口** ——
不支持 `file` 内容块的端点接了 PDF 会直接 400。

界面里点输入卡片的模型药丸 = **选择模型**面板：上面是 `GET /api/models` 拉回来的
清单（不写描述），上面一行「模型提供商」切换看哪家的模型（只列配好了 Key 的），下面一行是「思考强度」。思考强度选完跟着 provider 配置存进库，
发请求时映射成 `reasoning_effort`（Chat Completions）或 `reasoning.effort`
（Responses）；**默认档一个字段都不发**（严格网关会 400）。

## 环境

| 项 | 版本 |
|---|---|
| rustc / cargo | 1.98.0 |
| Android SDK | `~/Android/Sdk`（platform android-36） |
| NDK | r27.0.12077973 |
| Java | OpenJDK 21 |
| Bun | 1.4.2 |
| cargo-ndk | 4.1.2 |
| just | 已装 |
| 桌面 proot | `proot-git`（archlinuxcn） |
| JNA | 5.17.0（唯一一个 Android 依赖） |
