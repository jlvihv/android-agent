# android-agent

跑在 Android 手机上的 agent 工具：**Kotlin 薄壳 + Rust 核心 + Web 前端（WebView）**。

Agent 拿到的能力：**读界面 / 操作界面 / 跑 Linux 命令 / 改文件**。

## 状态

🟢 **核心已跑通并在桌面验证**（真机 Android 壳还没写）

**已在 Pixel 10 / Android 17 真机上完整验证。**

| 部分 | 状态 |
|---|---|
| `core/` Rust | ✅ 80 个测试全过，clippy 干净 |
| `web/` Svelte | ✅ 构建通过（57 KB JS / 21 KB gzip） |
| `android/` 壳 | ✅ **真机验证通过** |
| LLM | ✅ OpenCode Go / `deepseek-v4.1-flash`，真机跑通 |
| Linux 沙箱 | ✅ **Alpine 3.21 aarch64 在真机上跑起来**，`apk add` 可用 |
| 设备操控 | ⚠️ 只剩坐标点击 / 滑动 / 系统按键 / 启动应用（读 UI 树那套已删，见下） |
| Prompt cache | ✅ 实测命中 73–87% |
| 界面 | ✅ 对齐 Claude 移动端（神似，非像素级） |
| 多会话 | ✅ 侧边栏「最近对话」，标题自动生成 |
| 会话持久化 | ✅ SQLite（增量落盘），force-stop 后能恢复 |
| 后台运行 | ✅ 前台服务保活，切回来自动同步 |

## 快速开始（桌面）

```sh
# 1. 准备 Linux 沙箱（x86_64，用于桌面开发）
mkdir -p .dev/linux && cd .dev/linux
curl -sSL -o alpine-minirootfs.tar.gz \
  https://dl-cdn.alpinelinux.org/alpine/v3.21/releases/x86_64/alpine-minirootfs-3.21.0-x86_64.tar.gz
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
just dev

# 4. 另开一个终端，让 agent 干活
python3 scripts/agentctl.py "在沙箱里建一个 FizzBuzz 脚本，跑一下"
```

打开 <http://127.0.0.1:8787> 就是完整 App —— 和手机上 WebView 看到的是同一份东西。

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
│   uniffi (待接)             正向绑定 + 反向 callback        │
└──────────────────────────────────────────────────────────┘
                          │ UniFFI callback
┌──────────────────────────────────────────────────────────┐
│ Kotlin 薄壳（目标 < 800 行）                                │
│   MainActivity / AgentAccessibilityService / DeviceApiImpl │
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
5. **Kotlin 壳不写业务逻辑** —— 目标 < 800 行，不随功能增长
6. **提示词前缀一个字都不能变** —— 见 [docs/03-prompt-cache.md](docs/03-prompt-cache.md)
7. **没有审批环节** —— 工具调用一律直接执行，服务端不保存「挂起等回复」的状态

## Agent 工具

只给模型三个工具：

| 工具 | 作用 |
|---|---|
| `read` | 读文件：文本带 `offset` / `limit`（截断到 2000 行或 50KB）；**图片缩放后直接发给模型看** |
| `bash` | 在 Alpine 沙箱里跑命令（可 `apk add`），cwd 是 `/workspace` |
| `edit` | 按原文精确替换编辑文件，返回 unified diff |

设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）**不作为工具**暴露给模型。
以后收进沙箱里的一个命令、由 `bash` 调用 —— 这样模型只需要理解 shell。

agent loop **没有轮数上限**：任务要多少轮就跑多少轮，唯一的出口是模型自己不再调工具，
或者用户按停止（`POST /api/stop`）。停止是真的停服务端循环 —— 前端断流循环是看不到的。

`bash` 的 `timeout`（秒）是**必填**的，没有默认值 —— 逼模型先估算要跑多久，
下载 / `apk add` / 编译 / `git clone` 给足时间（上限 24h，估大不亏：命令一结束就返回）。
（之前默认 60s、上限 600s，长下载必然被半路杀掉。）

内核那条路（`/api/command` → 无障碍服务）现在只剩**不需要读界面**的几条：
坐标点击、滑动、系统按键、启动应用。读 UI 树那套（无障碍树拍平成 JSON 推上来）
跟 `device_snapshot` 一起删掉了 —— 它当时只为那两个工具服务。要做沙箱里的设备
命令时会连同它一起重做（`git log --diff-filter=D --name-only` 找得到）。

工具调用一律直接执行，没有审批环节 —— 服务端因此不持有「挂起等回复」的状态，
App 随时被杀都没关系。

## 文档

| 文件 | 内容 |
|---|---|
| [docs/01-tech-selection.md](docs/01-tech-selection.md) | 选型决策记录（含被否决的选项和理由） |
| [docs/02-linux-sandbox.md](docs/02-linux-sandbox.md) | **OpenMinis 怎么在 Android 上跑 Alpine**（W^X 问题与解法） |
| [docs/03-prompt-cache.md](docs/03-prompt-cache.md) | 提示词前缀不变式 + 实测命中率 |
| [docs/04-android-shell.md](docs/04-android-shell.md) | Android 壳：三个架构决策的落点、真机踩到的 11 个坑 |
| [docs/05-ui.md](docs/05-ui.md) | 界面：设计令牌、edge-to-edge、对话渲染 |
| [docs/06-persistence-and-background.md](docs/06-persistence-and-background.md) | 会话持久化（为什么是 SQLite）与后台运行 |
| [docs/07-debugging.md](docs/07-debugging.md) | 无线调试 + agentctl + `/api/debug/*`：不重装 APK 也能调 |

## 脚本

| 命令 | 作用 |
|---|---|
| `just dev` | 起核心（桌面，mock 设备 + 本地 rootfs） |
| `just web-dev` | 起 Vite（`/api` 代理到核心） |
| `just test` | Rust 测试 |
| `just smoke` | 端到端：一句话 → 工具执行 → 复述 |
| `just android-assets` | 一次性：取 Alpine rootfs + proot 二进制 |
| `just android-debug` | 完整 Android 构建 → APK |
| `just android-install` | 构建 + 装到连着的手机 |
| `python3 scripts/agentctl.py "…"` | 给 agent 一句话并打印转录；不带参数进交互模式（`--json` 机器可读） |

对着**手机上**跑着的核心（无线调试就够，不用插线、也不用手点手机屏幕）：

| 命令 | 作用 |
|---|---|
| `just wifi-pair <ip:port> <码>` | 首次配对（手机上开发者选项 → 无线调试） |
| `just wifi` / `just wifi-tcpip` | 之后每次连上（后者：USB 插一次，以后自动） |
| `just forward` | 把手机上核心的端口转发到本机 |
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
点哪家的模型就用哪家。配置存在 `sessions.db` 的 `meta` 表里（老版本的 `llm.json`
启动时自动迁进来再删掉）。脚本里还可以直接 `POST /api/llm`。

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
