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
| 设备操控 | ✅ 真机读 UI 树 + 点击（内核已有；暂不暴露给模型，见下） |
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
│   async-openai (3 features) LLM，不手写协议                │
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

1. **不手写 LLM 协议** —— `async-openai`，`default-features = false` 只开 3 个 feature
2. **不手写流式协议** —— 用 AI SDK 的 UI Message Stream 规范
3. **不手写反向 JNI** —— UniFFI callback interface
4. **native 侧不反调 Java** —— Kotlin 把 UI 树拍平成 JSON 送进来
5. **Kotlin 壳不写业务逻辑** —— 目标 < 800 行，不随功能增长
6. **提示词前缀一个字都不能变** —— 见 [docs/03-prompt-cache.md](docs/03-prompt-cache.md)
7. **没有审批环节** —— 工具调用一律直接执行，服务端不保存「挂起等回复」的状态

## Agent 工具

只给模型两个工具：

| 工具 | 作用 |
|---|---|
| `bash` | 在 Alpine 沙箱里跑命令（可 `apk add`），cwd 是 `/workspace` |
| `edit` | 按原文精确替换编辑文件，返回 unified diff |

设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）**暂时不作为工具**暴露给模型。
内核那条路还在（Kotlin 拍 UI 树 → `/api/command` → 回推快照），
以后收进沙箱里的一个命令、由 `bash` 调用 —— 这样模型只需要理解 shell，
工具表也能一直保持只有两个。

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
| [docs/07-debugging.md](docs/07-debugging.md) | 从电脑上驱动真机调试：adb forward + agentctl + `/api/debug/*` |

## 脚本

| 命令 | 作用 |
|---|---|
| `just dev` | 起核心（桌面，mock 设备 + 本地 rootfs） |
| `just web-dev` | 起 Vite（`/api` 代理到核心） |
| `just test` | Rust 测试 |
| `just smoke` | 端到端：读界面 → 执行 → 复述 |
| `just android-assets` | 一次性：取 Alpine rootfs + proot 二进制 |
| `just android-debug` | 完整 Android 构建 → APK |
| `python3 scripts/agentctl.py "…"` | 给 agent 一句话并打印转录；不带参数进交互模式（`--json` 机器可读） |
| `just forward` | 把手机上核心的端口转发到本机（端口从 logcat 里捞） |
| `just phone "…"` | 对手机上跑着的核心说话（自动 forward），不用重装 APK |

## 构建 APK

```sh
just android-assets     # 一次性
just android-debug      # → android/app/build/outputs/apk/debug/app-debug.apk
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
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

安卓上 App 不继承 shell 环境变量，所以 LLM 配置走 `POST /api/llm`
（持久化到 `<filesDir>/llm.json`）。

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
