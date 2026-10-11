# 技术选型决策清单

> 状态：**已决策**（2026-10-10）→ 见 §7 决策结果、§8 最终依赖清单。
> §2–§6 保留为决策依据（含被否决的选项和理由）。
> 调研时间：2026-10-10

---

## 0. 目标形态（前提）

Kotlin（薄壳）+ Rust（核心）+ Web 前端（WebView），做一个跑在安卓手机上的 agent 工具，
能读写设备 UI、能接 LLM、能挂 MCP 工具。

**核心原则：不手写任何 LLM 协议、不手写流式协议、不手写反向 JNI。**

---

## 1. 环境现状

| 项 | 状态 |
|---|---|
| rustc / cargo | 1.98.0 ✅ |
| Rust Android targets | `aarch64` / `armv7` / `i686` / `x86_64-linux-android` 全已装 ✅ |
| `cargo-ndk` | ❌ **未安装**，需 `cargo install cargo-ndk`（4.1.2） |
| Android SDK | `/home/vihv/Android/Sdk`，platform `android-36`，build-tools 34/35/36 ✅ |
| NDK | **r27.0.12077973** ✅（注意：r27 的 16KB page size 是**选开**，见风险 R1） |
| Java | OpenJDK 21.0.12 ✅（AGP 8.x 可用） |
| Gradle | ❌ 系统无，用 wrapper 即可 |
| Node / Bun | v26.11.1 / 1.4.2 ✅ |
| 连接设备 | 无（`adb devices` 空） |

---

## 2. 架构级决策（先定这些，其余都能后改）

### D1 — Rust 核心以什么形式跑在设备上？

| | A. 进程内 `.so`（JNI 加载） | B. 进程外可执行文件 |
|---|---|---|
| 做法 | Kotlin `System.loadLibrary("agent_core")`，Rust 在后台线程起 tokio runtime + axum | Rust 编成可执行文件改名 `libagent_core.so` 放 jniLibs，`Runtime.exec()` 拉起，走 localhost socket |
| 启动/延迟 | 快，无 IPC | 慢一点，毫秒级 IPC |
| 体积 | 小 | +5~10MB（Go 更明显） |
| 权限 | 无额外 | 必须 `android:extractNativeLibs="true"`（Android 10+ 禁止从可写目录 exec） |
| 崩溃影响 | native panic 会带走 App | 核心崩了 App 还活着，可重启核心 |
| 调试 | 需 lldb 跨语言 | 最好：独立进程、可单独跑、可热替换 `.so` |
| 省电 | 单进程 | 多一个常驻进程 |

**倾向：A（进程内）**。理由：单进程省电、不需要 `extractNativeLibs`、日志统一、启动快。
缓解崩溃风险：`std::panic::set_hook` + `catch_unwind`，关键路径不 panic。
若你更看重"核心可热替换/崩溃隔离"，选 B，我按 B 设计也行。

---

### D2 — Rust 怎么反向调用 Kotlin（执行 tap / 读 UI 树）？

| | A. UniFFI callback interface | B. Kotlin 侧跑 localhost HTTP server | C. 手写反向 JNI |
|---|---|---|---|
| 做法 | Rust 定义 `trait DeviceApi`，`#[uniffi::export(callback_interface)]`，Kotlin 实现它 | Rust `reqwest` POST 到 Kotlin 的 NanoHTTPD/Ktor | `ndk-context` 拿 JavaVM + 缓存 GlobalRef + `GetMethodID` |
| 代码量 | 最少（自动生成） | ~120 行 Kotlin + 双向 JSON | 最多，且易错 |
| 延迟 | 最低（同进程调用） | 毫秒级 | 低 |
| 对称性 | 单向调用 + 回调 | 双向都是 HTTP/JSON，心智最统一 | 差 |
| 风险 | uniffi Kotlin 后端的 JNA/JNI 模式需确认（见 R4） | Kotlin 要维护一个 server | JNI 全局引用/线程 attach 易踩坑 |

**倾向：A**。这是 UniFFI 设计出来就是为了干这个的，且避免手写反向 JNI（你明确不想）。
若你偏好"两端都说同一种协议"的对称美，选 B。

---

### D3 — WebView 从哪加载前端？

| | A. Rust 内置 axum 服务，指向 `http://127.0.0.1:PORT` | B. `WebViewAssetLoader` + Rust 只做 API | C. 前端放 assets，`file://` |
|---|---|---|---|
| 前端资源 | `rust-embed` 编进 `.so` | Android assets 目录 | Android assets 目录 |
| 同源 | ✅ 前端与 API 同源，无 CORS | ❌ 跨源，需 CORS 头 | — |
| secure context | ✅（能用 `crypto.subtle`、SW） | ✅ `https://appassets.androidplatform.net` | ❌ opaque origin，很多 API 不可用 |
| 明文流量 | 需加 localhost 例外 XML | 需加 localhost 例外 + CORS | 不需要 |
| 开发期 | 代理到 Vite dev server（`adb reverse`）即可热更 | 一样 | 麻烦 |

**倾向：A**。同源 + secure context + 开发期可代理 Vite，三件事一次解决。
明文例外约 10 行 XML，成本可忽略。

---

## 3. 库级决策

### D4 + D5 — LLM 协议层 & Agent Loop（**这两个绑在一起选**）

不想手写 OpenAI Chat/Responses 协议，所以有两个自洽的组合：

#### 组合 A「薄内核」
```
genai (provider 抽象)  +  自研 agent loop (~300-500 行)  +  rmcp (MCP)
```

- **genai**：多 provider **原生协议**（不是 OpenAI 兼容层套壳），SSE 流式，工具调用、
  `tool_choice`、内置工具（含 web search 映射到 Anthropic/OpenAI/Gemini）。
  覆盖 OpenAI / Anthropic / Gemini / xAI / Ollama / Groq / DeepSeek / Bedrock / Vertex / Cohere / Together / Fireworks / Nebius / GitHub Copilot …
- **自研 loop**：因为本项目的 UI 要**逐事件展示 tool call 并支持人工审批**，需要完全拦截事件流。
  框架会把 loop 包起来，反而挡路。
- 代价：重试、并行工具调用、上下文裁剪要自己写。

#### 组合 B「框架优先」
```
rig (provider + 类型安全工具 + loop)  +  rmcp (MCP)
```

- **rig** 0.44+：20+ provider 统一 API、类型安全工具、结构化输出，
  且**内置 `rmcp` feature** 直接对接官方 MCP SDK。
- 代价：rig API 迭代快（0.44 说明还很年轻），loop 控制权在框架手里，
  要把每个 tool call 事件转成前端协议需要绕。
- 好处：最快看到能跑的东西。

#### 补充件（任一组合都可加）
**async-openai** — 仅当需要 OpenAI **独有**能力时加：Responses API、Conversations、
Streaming events、Webhooks、Evals、Fine-tuning、Graders、Audio/Images 全套。
用 feature gate 隔离，默认不引。

> ❌ 已否决：手写 OpenAI Chat / Responses 协议。

**倾向：组合 A**。理由：审批流 + 事件流推送是这个工具的 UI 核心，loop 必须自己拿；
genai 的 provider 覆盖已经足够广。若你想先跑通再重构，选 B 也合理。

---

### D6 — 前端框架

| | A. React 19 + Vite + TS | B. Svelte 5 + Vite + TS | C. 无框架 |
|---|---|---|---|
| 聊天 UI | **assistant-ui** 开箱（流式、tool call 渲染、中断、附件） | 要自己写 | 自己写 |
| AI SDK 支持 | `@ai-sdk/react` 一等公民 | `@ai-sdk/svelte` 有 | — |
| 生态/组件 | shadcn/ui、Radix 海量 | 少一些 | — |
| 体积 | 较大 | 小 | 最小 |

**倾向：A**。手机 WebView 上体积差异不关键，而 assistant-ui 能省掉整个聊天 UI 的工作量。

### D7 — 前端 ↔ Rust 的流式协议

| | A. 采用 Vercel AI SDK 7 的 **UI Message Stream** 协议 | B. 自定义 SSE JSON |
|---|---|---|
| Rust 侧 | 手写一个 SSE 编码器（~150 行），按规范吐 `data: {...}` | 自己设计事件类型 |
| 前端 | `useChat` / assistant-ui 直接吃，零状态机 | 自己写流式状态机 |
| 现成语义 | text / reasoning / tool-input / tool-output / step / error / data-* 全都有 | 全要自己定义 |
| 规范 | 公开文档化，其他语言后端已有先例（FastAPI 示例） | — |

规范要点（已核实）：
- Header：`x-vercel-ai-ui-message-stream: v1`
- SSE 行：`data: {"type":"text-delta","id":"...","delta":"Hello"}`
- 结尾：`data: [DONE]`
- 关键事件：`start` / `start-step` / `finish-step` / `finish` /
  `text-start|delta|end` / `reasoning-start|delta|end` /
  `tool-input-start|delta|available` / `tool-output-available` /
  `error` / `abort` / `data-*`

**倾向：A**。协议公开且语义现成（工具输出、思考、步骤、`data-*` 都现成），
前端换回官方 `@ai-sdk/svelte` 也不用改 Rust 侧。

> 当初选它的一个理由是协议自带 `tool-approval-request|response`。后来审批环节整个
> 删掉了（工具一律直接执行），但协议本身仍然值这个价。

### D8 — Rust ↔ Gradle 怎么串

| | A. 脚本驱动（justfile 调 `cargo ndk` → jniLibs） | B. 自定义 Gradle `Exec` task | C. `mozilla/rust-android-gradle` 插件 |
|---|---|---|---|
| 魔法 | 无 | 中 | 高 |
| 兼容风险 | 无 | 低 | ⚠️ 插件久未更新，AGP 8/9 兼容性未知 |
| CI | 最好写 | 一般 | 一般 |
| 增量构建 | 靠 cargo 自身 | 靠 cargo 自身 | 插件管 |

**倾向：A 为主 + B 作为 release 时的可选挂钩**。Gradle 只负责把 `jniLibs/` 打进去，不感知 Rust。

### D9 — 编译哪些 ABI

| ABI | 用途 | 建议 |
|---|---|---|
| `arm64-v8a` | 现代真机 | ✅ 必做 |
| `x86_64` | 模拟器 | ✅ 做（否则没法本地调） |
| `armeabi-v7a` | 老设备 | ⬜ 可跳过 |
| `x86` | 老模拟器 | ⬜ 跳过 |

**倾向：arm64-v8a + x86_64**。

### D10 — UI 树快照格式（Kotlin → Rust）

| | A. JSON（serde_json） | B. CBOR（ciborium） | C. Protobuf |
|---|---|---|---|
| 调试 | 最好（logcat 直接看） | 要工具 | 要工具 |
| 体积 | 大（节点几百个时明显） | 小 ~30% | 最小 |
| 代码 | 零成本 | 低 | 要 codegen |

**倾向：A + 剪枝**（只发可交互节点 + 文本 + 关键属性），先 JSON。
压测后再决定是否换 CBOR——接口留成 `serde` 泛型就能无痛切换。

### D11 — 本地推理（llama.cpp）现在做吗？

| | A. 现在不做，只留 trait | B. 现在就接 llama-cpp-2 |
|---|---|---|
| 体积 | 0 | +几十 MB |
| 编译时间 | 0 | 交叉编译 llama.cpp 很慢 |
| 风险 | 无 | `llama-cpp-rs` 明确声明**不遵守 semver**，要 pin 死版本 |

**倾向：A**。先跑通"云 LLM + 设备操作"，本地推理作为 `trait LlmBackend` 的一个实现后补。
（`llama-cpp-sys-2` 确实支持 Android NDK 交叉编译，有 `android-shared-stdcxx` feature，路是通的。）

### D12 — 其它小项

| 项 | 选项 | 倾向 |
|---|---|---|
| 包管理 | bun（已装）/ npm | bun |
| 样式 | Tailwind v4 / CSS Modules | Tailwind v4 |
| 组件 | shadcn/ui / 手写 | shadcn/ui（复制进仓库，无版本锁定） |
| Markdown 流式渲染 | streamdown / react-markdown + shiki | streamdown（专为流式设计） |
| 前端状态 | zustand / jotai | zustand |
| 事件推送（非流式） | WebSocket / SSE | WebSocket（agent 状态、日志、设备事件） |
| 日志 | `tracing` + `android_logger` | 同一套宏同时输出 logcat 与 stdout |
| API key 存储 | 明文私有目录 / Android Keystore | 先明文，后补 Keystore |
| 错误处理 | `thiserror`(lib) + `anyhow`(bin) | 惯例 |

---

## 4. 推荐组合一览（若全部采纳我的倾向）

```
┌──────────────────────────────────────────────────────────────┐
│ WebView  http://127.0.0.1:<随机端口>                          │
│   React 19 + Vite + TS + Tailwind v4 + shadcn/ui             │
│   assistant-ui + AI SDK 7 (@ai-sdk/react)                    │
│   ← SSE: x-vercel-ai-ui-message-stream: v1                    │
│   ← WS: agent 事件 / 日志 / 设备状态                           │
└──────────────────────────────────────────────────────────────┘
                              │ 同源 HTTP + WS
┌──────────────────────────────────────────────────────────────┐
│ Rust core (.so, 进程内, tokio + axum)                         │
│   axum 0.8 + tower-http      HTTP/SSE/WS                     │
│   rust-embed + mime_guess    前端资源编进 .so                  │
│   genai                      LLM provider（多协议原生）        │
│   自研 agent loop            事件流 / 审批 / 中断 / 预算        │
│   rmcp 2.x                   MCP client（官方 Rust SDK）       │
│   serde / tracing / thiserror                                │
│   uniffi 0.30+               正向绑定 + 反向 callback          │
└──────────────────────────────────────────────────────────────┘
                              │ UniFFI callback
┌──────────────────────────────────────────────────────────────┐
│ Kotlin 薄壳（目标 < 800 行，不随功能增长）                      │
│   MainActivity        WebView + 权限                          │
│   AgentAccessibilityService   读 UI 树 / 执行 tap·swipe·type  │
│   DeviceApiImpl       实现 UniFFI callback trait              │
│   AgentForegroundService      后台常驻 + 通知渠道               │
│   Application         启动时把 JavaVM/Context 交给 native      │
└──────────────────────────────────────────────────────────────┘
```

**"少量原生模块"到底是多少**——这些省不掉，因为是 Android 框架要求（系统靠反射实例化）：

| 必须存在 | 原因 | 估算行数 |
|---|---|---|
| `MainActivity` | 入口；`requestPermissions` 只能在 Activity 发起 | ~80 |
| `AgentAccessibilityService` | 操控其他 App 的**唯一**途径，必须继承该基类 | ~300 |
| `DeviceApiImpl` | 实现 Rust 定义的 callback trait | ~150 |
| `AgentForegroundService` + 通知渠道 | 后台常驻；Android 14+ 要 `foregroundServiceType` | ~120 |
| `Application` | 把 `JavaVM`/`Context` 存进 native 全局 | ~30 |
| `AndroidManifest.xml` + `accessibility_service_config.xml` | 组件与权限声明 | ~80 |
| **合计** | | **~760 行** |

**设计铁律**：native 侧**不写任何反调 Java 的代码**。
Kotlin 把 UI 树拍平成扁平 JSON 送进来，native 只回传指令（`tap(id)` / `swipe` / `type` / `back`）。

```
Kotlin Service ──[扁平 JSON UI 快照]──▶ Rust（决策/规划/推理）
      ▲                                      │
      └────────[指令: tap/swipe/type]────────┘
```

另外：`onAccessibilityEvent` 跑在**主线程**，绝对不能阻塞——native 侧收到立刻入队返回。

---

## 5. 版本锁定候选

| 组件 | 候选版本 | 备注 |
|---|---|---|
| rust | 1.98.0 | 已装 |
| cargo-ndk | 4.1.2 | 待装 |
| NDK | r27.0.12077973 | 已装；见 R1 |
| AGP | 8.x 最新 | 配 Java 21 |
| Kotlin | 2.x 最新 | |
| axum | 0.8 | |
| tokio | 1.x | |
| genai | 0.6.5 / 0.7.x | 0.7.0-beta 于 2026-09 前后发布，装之前看 crates.io 实际 |
| rig | 0.44.0 | 仅组合 B 需要 |
| rmcp | 2.x | 官方 `modelcontextprotocol/rust-sdk` |
| uniffi | 0.30+ | |
| llama-cpp-2 | pin 死 | 仅 D11 选 B 时；上游声明不遵守 semver |
| ai | 7.0.x | 2026-06-25 发布 |
| @ai-sdk/react | 4.x | |
| assistant-ui | 需 `ai@^7` + `@ai-sdk/react@^4` | |
| React | 19 | |
| Vite | 7 | |
| Tailwind | 4 | |

---

## 6. 风险 / 待验证（动手前要实测的）

| # | 风险 | 验证方式 |
|---|---|---|
| ~~**R1**~~ ✅ **已验证通过** | NDK r27 的 16KB 对齐 —— `cargo-ndk` 4.x + NDK r27 默认就对齐到 `0x4000`，不需要手动加 flag | 实测见 docs/04-android-shell.md §8 |
| **R2** | Android 9+ 默认禁明文 HTTP。已加 `network_security_config.xml` 给 loopback 例外，**但真机行为未验证** | 上真机跑 |
| **R3** | `libc++_shared.so` 不一起打包会 `dlopen` 失败 | 用 `android-shared-stdcxx` 时确认 `jniLibs` 里有它 |
| ~~**R4**~~ ✅ **已确认** | uniffi 0.32 的 Kotlin 后端**只有 JNA**，没有 JNI 模式，也没有开关。代价：多一个 `net.java.dev.jna:jna:5.17.0@aar` 依赖 | 生成产物里是 `com.sun.jna.*` |
| **R5** | uniffi callback 从 tokio 异步上下文回调 Kotlin 的线程语义 | 小 demo 验证：Rust task → Kotlin 回调 → 返回，看是否死锁/线程错误 |
| **R6** | `onAccessibilityEvent` 主线程 + 高频（每秒几十次） | 压测：批量/节流后再过边界 |
| **R7** | Play 对 `AccessibilityService` 审核极严 | 自用/侧载无影响；上架需准备用途说明 |
| **R8** | rig 0.44 API 迭代快（仅组合 B） | 锁版本 + 少量封装隔离 |

---

## 7. 决策结果（2026-10-10 确认）

指导原则：**依赖越少越好**。

| # | 决策 | 结果 |
|---|---|---|
| D1 | Rust 运行形式 | **A 进程内 `.so`** |
| D2 | 反向通道 | **A UniFFI callback** |
| D3 | WebView 加载 | **A Rust axum + `127.0.0.1`** |
| D4+D5 | LLM / Agent | **`async-openai` + 自研 loop**（原组合 A 的 genai 也一并替换） |
| D6 | 前端 | **B Svelte 5**（不用 React / assistant-ui） |
| D7 | 流式协议 | **A AI SDK UI Message Stream**（保留规范；客户端见 §10） |
| D8 | Gradle 集成 | 脚本驱动 |
| D9 | ABI | arm64-v8a + x86_64 |
| D10 | UI 快照 | JSON + 剪枝 |
| D11 | 本地推理 | 先不做 |
| D12 | 其余 | bun / Tailwind v4 / **不用 shadcn** / markdown 后补 |
| — | `genai` | **砍掉** |
| — | `rig` | **砍掉** |
| — | `rmcp` / MCP | **砍掉**，但 `Tool` 做成 trait 留口子（后补 ~200 行 adapter） |

---

## 8. v1 最终依赖清单（最小化）

### 8.1 Rust core — 14 个直接依赖

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net"] }
axum = "0.8"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-openai = { version = "0.42", default-features = false,
                 features = ["rustls", "chat-completion", "responses"] }
rust-embed = "8"
mime_guess = "2"
uniffi = { version = "0.30", features = ["tokio"] }
tracing = "0.1"
tracing-subscriber = "0.3"
android_logger = "0.14"
thiserror = "2"
anyhow = "1"
uuid = { version = "1", features = ["v7"] }
```

关键点（已核实）：

- `async-openai` 有 **57 个 feature flag，默认只开 `rustls`**。
  `default-features = false` 关掉全部，再显式加回 3 个。
- **`rustls` 而非 `native-tls`** → 避免 openssl 交叉编译。
  这是 Android/Rust 上最容易卡死的地方，务必保持。
- 不用显式引 `reqwest`：async-openai 的 `rustls` feature 已经带进来。
- `responses` feature = OpenAI 新一代 Responses API（含 Conversations、streaming events）。
  用不上就删掉，再省一点编译时间。

### 8.2 Web — Svelte 5，8 个包

```jsonc
{
  "dependencies": {
    "svelte": "^5",
    "ai": "^7",              // 见 §10 W1/W2，可省
    "@ai-sdk/svelte": "^4"   // 见 §10 W1/W2，可省
  },
  "devDependencies": {
    "vite": "^7",
    "@sveltejs/vite-plugin-svelte": "^6",
    "typescript": "^5",
    "svelte-check": "^4",
    "tailwindcss": "^4",
    "@tailwindcss/vite": "^4"
  }
}
```

Tailwind v4 比 v3 精简很多：不需要 postcss config、不需要 autoprefixer。

### 8.3 Android

只有 AGP + Kotlin stdlib。

- **不需要 `androidx.webkit`** —— 不用 WebViewAssetLoader（D3 选了 A）
- **不需要 JNA** —— uniffi 走 JNI 模式（见 R4）

---

## 9. 这一版砍掉了什么，代价是什么

| 砍掉 | 收益 | 代价 | 能否后补 |
|---|---|---|---|
| `genai` | −1 个大依赖 | 只剩 OpenAI 协议 | 可加回，或加 provider adapter |
| `rig` | −1 个大框架 | loop 自己写（~400 行） | 不需要 |
| `rmcp` | −1 个依赖 | 没有 MCP | ✅ `Tool` trait 留口子，后补 ~200 行 |
| React + assistant-ui | −10 个以上的包，bundle 小很多 | 聊天 UI 自己写（~400 行 Svelte） | 不需要 |
| shadcn/ui + Radix | −20 个以上的包 | 组件自己写 | 可随时复制进来 |

### ⚠️ 唯一需要你确认的真实代价

`async-openai` 只讲 **OpenAI 协议**。但它支持 `.with_api_base()`，
所以凡是提供 **OpenAI 兼容端点**的都能用：

| ✅ 能用 | ❌ 不能用（原生协议） |
|---|---|
| OpenAI、DeepSeek、Kimi/Moonshot、通义 DashScope、智谱、SiliconFlow、OpenRouter、Groq、xAI、Together、Fireworks、vLLM、LM Studio、Ollama(`/v1`)、以及绝大多数中转站 | **Anthropic Claude**（原生 Messages API）<br>**Google Gemini**（原生 API） |

想用 Claude / Gemini 的两条路：

1. 走 OpenRouter 等中转（OpenAI 兼容）← 最省事
2. 后期加 provider adapter（`trait LlmBackend`），换回 `genai` 或直连

> **如果你确定要直连 Claude / Gemini 原生协议，`async-openai` 就不够，
> 得换回 `genai`（用 1 个依赖换 3 家覆盖）。这条需要你明确。**

---

## 10. W1 / W2 — 流式客户端要不要装 AI SDK

线上协议无论选哪个都是 AI SDK 的 UI Message Stream（公开规范，本质就是 SSE + JSON 形状）。

| | W1. 装 `ai` + `@ai-sdk/svelte` | W2. 自己写解析器 |
|---|---|---|
| npm 包数 | +2 | +0 |
| 代码 | `Chat` 类开箱：消息状态 / 流式 / 中断 / 错误 / 重传 | ~120 行 TS：SSE 解析 + 状态机 |
| 风险 | 跟随 AI SDK 版本 | 自己维护 |

**倾向 W1**。它管的是"流式状态机"这类最容易写出 bug 的地方，2 个包换它值。
要极致精简的话 W2 完全可行——协议本身就是 `data: {"type": ...}` 加个 `data: [DONE]` 结尾。

---

## 11. 新增验证项

| # | 风险 | 验证方式 |
|---|---|---|
| **R9** | async-openai 默认 `rustls` feature → reqwest 0.13 使用 **`rustls-platform-verifier`** 读系统根证书。Android 上它需要 JavaVM | 进程内加载所以可调 `rustls_platform_verifier::android::init_hosted()`，**必须在第一个请求前调用**。嫌麻烦就换成内嵌 `webpki-roots` |
| **R10** | `@ai-sdk/svelte` 的 `Chat` 类是否依赖 SvelteKit？ | 目标是纯 Vite + Svelte，需实测 |
