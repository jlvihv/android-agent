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

**崩溃策略：`panic = "abort"` —— panic 就崩，不兜。**

兜住的 panic 会退化成前端一句没头没尾的报错、或者干脆静默断流（`/api/chat` 只发出
`start`/`start-step` 就没了）：根因在 stderr 里，而**安卓的 stderr 是丢掉的** ——
那种失败比崩溃难查得多。所以装 panic hook 把根因写进 logcat，然后让它崩。

第三方解码器（`image` / `zune_jpeg` / `png`）也不给它开宽容区：它们的**解码 API
本来就是 `Result`**，畸形输入走 `Err`；会 panic 是它们的 bug。**对一个 bug 的正确
反应是暴露它然后修/换，不是盖毯子** —— 盖住之后，这类 bug 永远只在用户那里变成
一句看不懂的报错。

结论：`panic = "abort"`，`catch_unwind` 一律不用。真出现解码器 panic，logcat 里会有
`RUST PANIC` + 那个 crate 的源码路径（版本号就在路径里），照着去修、去换、或者自己写一个。

代价 / 收益：`.eh_frame` / `.eh_frame_hdr` / `.gcc_except_table` 和一堆清理代码全省掉，
`.so` 7.78MB → 6.72MB，打进 APK 少 ~388KB。`Mutex` 中毒也从此不可能发生。
同样的理由写在 `core/Cargo.toml` 的 `[profile.release]` 里。

若你更看重「核心可热替换/崩溃隔离」，选 B，我按 B 设计也行。

---

### D2 — Rust 怎么反向调用 Kotlin（执行设备指令）？

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

### D4 + D5 — LLM 协议层 & Agent Loop（**都是自研**）

**协议层自研** —— `openai.rs`（Chat Completions）+ `responses.rs`（Responses API），
**优先 Responses**，端点不支持就自动退回 Chat Completions 并记住
（`llm.rs` 的 `ApiStyle`/`Transport`）。不引 SDK 的理由：`async-openai` 的
`Client::with_config()` 会**先** `reqwest::Client::new()` 建一个默认连接、然后才轮到我们
`with_http_client` 覆盖；在 `rustls-no-provider` 下那个连接没有 crypto provider，
**直接 panic** —— 而它建的那个连接我们根本拿不到。而我们真正用到的只有「一个请求体 +
一条 SSE 流」，自己写少一个大依赖，而且每个字节都在我们手里（prompt cache 的前缀不变式
就靠这个，见 docs/03）。

**loop 也自研** —— `agent.rs`。UI 要逐事件展示 tool call，队列、停止、上下文都要自己控；
框架会把 loop 包起来，反而挡路。代价是重试、并行工具调用、上下文裁剪要自己写。

---

### D6 — 前端框架

| | A. React 19 + Vite + TS | B. Svelte 5 + Vite + TS | C. 无框架 |
|---|---|---|---|
| 聊天 UI | **assistant-ui** 开箱（流式、tool call 渲染、中断、附件） | 要自己写 | 自己写 |
| AI SDK 支持 | `@ai-sdk/react` 一等公民 | `@ai-sdk/svelte` 有 | — |
| 生态/组件 | shadcn/ui、Radix 海量 | 少一些 | — |
| 体积 | 较大 | 小 | 最小 |

**选 B（Svelte 5 + Vite + TS）**。手机 WebView 里体积和启动都值钱，而聊天 UI 就那么几个
组件，自己写（见 docs/05）比拖进 React + assistant-ui 那一串便宜。

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

**选 A**。协议公开且语义现成（工具输出、思考、步骤、`data-*` 都现成）。
（协议里还带 `tool-approval-request|response`，我们用不上 —— 工具一律直接执行 ——
但那些语义值这个价。）

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

**现在只出 arm64-v8a**（真机）。模拟器要 x86_64 时：
`AGENT_ABIS=arm64-v8a,x86_64 just emu-install` —— 多带一个 ABI 就是白送几 MB。

### D10 — UI 树快照格式（Kotlin → Rust）—— 现在没有这一条

读 UI 树的能力目前不在（见 D2 那节：native 侧只做不读界面的设备操作）。以后要做的话
就是「Kotlin 把无障碍树拍平成扁平 JSON 推给 Rust」，格式选型的结论在下面这张表。

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

| 项 | 选项 | 定了哪个 |
|---|---|---|
| 包管理 | bun（已装）/ npm | bun |
| 样式 | Tailwind v4 / CSS Modules | Tailwind v4 |
| 组件 | shadcn/ui / 手写 | **手写**（Svelte 侧，shadcn 是 React 的） |
| Markdown 渲染 | 现成库 / 自己写 | 自己写（`lib/markdown.ts`） |
| 事件推送 | WebSocket / SSE | 两个都用：`/api/ws`（事件、日志、设备状态）+ SSE（回合流） |
| 日志 | `tracing` + `android_logger` | 同一套宏同时输出 logcat 与 stdout |
| API key 存储 | 明文私有目录 / Android Keystore | 明文私有目录（库里的 `providers` 表） |
| 错误处理 | `thiserror`(lib) + `anyhow`(bin) | 惯例 |

---

## 4. 现在长这样

```
┌──────────────────────────────────────────────────────────────┐
│ WebView  http://127.0.0.1:<随机端口>                          │
│   Svelte 5 + Vite + TS + Tailwind v4（运行时零依赖）          │
│   自写 SSE 解析器（web/src/lib/sse.ts，见 §10）               │
│   ← SSE: x-vercel-ai-ui-message-stream: v1                    │
│   ← WS: agent 事件 / 日志 / 设备状态                           │
└──────────────────────────────────────────────────────────────┘
                              │ 同源 HTTP + WS
┌──────────────────────────────────────────────────────────────┐
│ Rust core (.so, 进程内, tokio + axum)                         │
│   axum 0.8 + tower-http      HTTP/SSE/WS                     │
│   rust-embed + mime_guess    前端资源编进 .so                  │
│   自研 LLM transport         openai.rs / responses.rs（D4）    │
│   自研 agent loop            事件流 / 队列 / 停止 / 上下文       │
│   rusqlite（bundled）        会话库 + 配置表（D3、docs/06）     │
│   serde / tracing / thiserror                                │
│   uniffi 0.30+               正向绑定 + 反向 callback（D2）     │
└──────────────────────────────────────────────────────────────┘
                              │ UniFFI callback
┌──────────────────────────────────────────────────────────────┐
│ Kotlin 壳（只放非它不可的平台能力）                              │
│   MainActivity                WebView + 面板 + 权限           │
│   AgentAccessibilityService   坐标点击·滑动·按键·启动应用      │
│   DeviceHostImpl              实现 UniFFI callback trait      │
│   AgentForegroundService      回合进行中才挂的前台服务          │
│   TurnStream                  订阅 /api/turns/stream（SSE）    │
│   BrowserController / …        agent 的 WebView + 注入的 JS    │
│   Application                 启动时把 JavaVM/Context 交给 native │
└──────────────────────────────────────────────────────────────┘
```

**"少量原生模块"到底是多少**——这些省不掉，因为是 Android 框架要求（系统靠反射实例化）：

| 必须存在 | 原因 |
|---|---|
| `MainActivity` | 入口；`requestPermissions` 只能在 Activity 发起 |
| `AgentAccessibilityService` | 操控其他 App 的**唯一**途径，必须继承该基类 |
| `DeviceHostImpl` | 实现 Rust 定义的 callback trait |
| `AgentForegroundService` + 通知渠道 | 回合期间保活；Android 14+ 要 `foregroundServiceType` |
| `Application` | 把 `JavaVM`/`Context` 存进 native 全局 |
| `AndroidManifest.xml` + `accessibility_service_config.xml` | 组件与权限声明 |

**设计铁律**：native 侧**不写任何反调 Java 的代码**。跨语言边界只有 JSON 字符串：
Rust 回传指令（`tap_xy` / `swipe` / `key` / `launch`），Kotlin 执行。

```
Rust（决策/规划/推理）──[指令 JSON]──▶ Kotlin Service（无障碍）
```

> 反方向那条（Kotlin 把 UI 树拍平成扁平 JSON 推给 Rust，D10）现在没有 ——
> 要做读 UI 树时再加。

另外：`onAccessibilityEvent` 跑在**主线程**，绝对不能阻塞 —— native 侧收到立刻入队返回。
（现在它什么都不做：读 UI 树那套不在，见 docs/04 §6。）

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
| rusqlite | 0.3x（`bundled`） | 会话库 / 配置表；`.so` 多 ~50 KB |
| uniffi | 0.30+ | Kotlin 后端只有 JNA（R4） |
| llama-cpp-2 | pin 死 | 仅 D11 选 B 时；上游声明不遵守 semver |
| Svelte | 5 | 前端 |
| Vite | 7 | |
| Tailwind | 4 | |

---

## 6. 曾经的风险项，现在的结论

| # | 结论 |
|---|---|
| R1 | ✅ NDK r27 的 16KB 对齐：`cargo-ndk` 4.x + NDK r27 默认就对齐到 `0x4000`，不用手动加 flag（docs/04 §8） |
| R2 | ✅ Android 9+ 禁明文 HTTP：`network_security_config.xml` 只给 loopback 开例外，真机验证通过（docs/04） |
| R4 | ✅ uniffi 0.32 的 Kotlin 后端**只有 JNA**（没 JNI 模式，也没开关）：多一个 `net.java.dev.jna:jna` 依赖 |
| R7 | ⚠️ Play 对 `AccessibilityService` 审核极严 —— 自用/侧载无影响；要上架得准备用途说明 |

---

## 7. 决策结果（2026-10-10 确认）

指导原则：**依赖越少越好**。

| # | 决策 | 结果 |
|---|---|---|
| D1 | Rust 运行形式 | **A 进程内 `.so`** |
| D2 | 反向通道 | **A UniFFI callback** |
| D3 | WebView 加载 | **A Rust axum + `127.0.0.1`** |
| D4+D5 | LLM / Agent | **协议自研（`openai.rs` / `responses.rs`）+ loop 自研（`agent.rs`）** |
| D6 | 前端 | **B Svelte 5**（不用 React / assistant-ui） |
| D7 | 流式协议 | **A AI SDK UI Message Stream**（保留规范；客户端见 §10） |
| D8 | Gradle 集成 | 脚本驱动 |
| D9 | ABI | **arm64-v8a**（真机）；模拟器要 x86_64 就 `AGENT_ABIS=arm64-v8a,x86_64 just emu-install` |
| D10 | UI 快照 | **没有**（读 UI 树那套随设备工具一起移除了） |
| D11 | 本地推理 | 先不做 |
| D12 | 其余 | bun / Tailwind v4 / **不用 shadcn** / markdown 后补 |
| — | `async-openai` | **砍掉**：协议自研（见 D4+D5） |
| — | `genai` | **砍掉** |
| — | `rig` | **砍掉** |
| — | `rmcp` / MCP | **砍掉** |

---

## 8. v1 最终依赖清单（最小化）

### 8.1 Rust core

只列关键的（完整清单在 `core/Cargo.toml`）：

```toml
[dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time", "net", "signal", "process", "io-util", "fs"] }
axum = { version = "0.8", features = ["ws"] }
serde = { version = "1", features = ["derive", "rc"] }
serde_json = "1"
rust-embed = "8"              # 前端嵌进 .so
uniffi = { version = "0.30", features = ["tokio"] }
tracing = { version = "0.1", features = ["log"] }
# 自己造 reqwest client，绕开 rustls-platform-verifier：
# Android 上它要求先 init_with_env(env, context)，否则第一次请求就 panic（见 llm.rs）
reqwest = { version = "0.13", default-features = false, features = ["rustls-no-provider", "json", "stream"] }
# ring 而不是 aws-lc-rs（后者是个完整 C 库），证书用内嵌的 webpki-roots
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }
webpki-roots = "1"
```

- **`rustls` 而非 `native-tls`** → 避免 openssl 交叉编译。这是 Android/Rust 上最容易卡死
  的地方，保持住。
- `tracing` 要开 `log` feature：安卓上只装了 `android_logger`（log 的 sink），没有这座桥
  的话我们所有的 `tracing::info!` 在 logcat 里全部看不到。

### 8.2 Web — Svelte 5，运行时零依赖

```jsonc
{
  "devDependencies": {
    "svelte": "^5",
    "vite": "^7",
    "@sveltejs/vite-plugin-svelte": "^6",
    "typescript": "^5",
    "svelte-check": "^4",
    "tailwindcss": "^4",
    "@tailwindcss/vite": "^4"
  }
}
```

全部是构建期依赖：`dependencies` 是空的 —— 流式解析器和聊天状态机自己写（见 §10）。
Tailwind v4 不需要 postcss config、不需要 autoprefixer。

### 8.3 Android

只有 AGP + Kotlin stdlib + JNA（uniffi 的 Kotlin 后端只有 JNA，见 R4）。

- **不需要 `androidx.webkit`** —— 不用 WebViewAssetLoader（D3 选了 A）

---

## 9. 这一版砍掉了什么，代价是什么

| 砍掉 | 收益 | 代价 | 能否后补 |
|---|---|---|---|
| `genai` | −1 个大依赖 | 只剩 OpenAI 协议 | 可加回，或加 provider adapter |
| `rig` | −1 个大框架 | loop 自己写（~400 行） | 不需要 |
| `rmcp` | −1 个依赖 | 没有 MCP | ✅ `Tool` trait 留口子，后补 ~200 行 |
| React + assistant-ui | −10 个以上的包，bundle 小很多 | 聊天 UI 自己写（~400 行 Svelte） | 不需要 |
| shadcn/ui + Radix | −20 个以上的包 | 组件自己写 | 可随时复制进来 |

### 代价：非 OpenAI 兼容的原生协议要自己写

协议层自研只讲 **OpenAI 协议**（Chat Completions + Responses）加自定义 base URL，
所以提供 **OpenAI 兼容端点**的都能用：

| ✅ 能用 | ❌ 不能用（要另写一个 transport） |
|---|---|
| OpenAI、DeepSeek、Kimi/Moonshot、通义 DashScope、智谱、SiliconFlow、OpenRouter、Groq、xAI、Together、Fireworks、vLLM、LM Studio、Ollama(`/v1`)、以及绝大多数中转站 | **Anthropic Claude**（原生 Messages API）<br>**Google Gemini**（原生 API） |

想用 Claude / Gemini：走 OpenRouter 等中转（OpenAI 兼容，最省事），或者给 `llm.rs`
加一个 transport。

---

## 10. 流式客户端：自己写（W2）

线上协议是 AI SDK 的 UI Message Stream（公开规范，本质就是 SSE + JSON 形状），
但客户端自己写（`web/src/lib/sse.ts`，~120 行 TS：SSE 解析 + 状态机）——
`dependencies` 因此是空的，也不用跟着 AI SDK 的版本走。协议本身简单得离谱：
`data: {"type": …}` 加一个 `data: [DONE]` 结尾。
