# Android 壳

> 状态：**✅ 已在真机（Pixel 10 / Android 17 / API 37）完整验证**
> 验证日期：2026-10-10

---

## 1. 真机验证结果

| 能力 | 结果 |
|---|---|
| Rust 交叉编译 arm64-v8a + x86_64 | ✅ |
| JNA 加载 `libagent_core.so`（D1） | ✅ |
| UniFFI 正向调用（Kotlin → Rust） | ✅ |
| UniFFI 反向 callback（Rust → Kotlin，D2） | ✅ |
| WebView 加载 `http://127.0.0.1:<port>`（D3） | ✅ 明文例外够用（R2 解决） |
| 无障碍读 UI 树（真实 500 节点） | ✅ **后来删了**（见 §6 的注） |
| 无障碍坐标点击 / 滑动 / 系统按键 / 启动应用 | ✅ |
| **Alpine 沙箱跑起来**（proot + W^X 绕过） | ✅ 87ms 启动 |
| `apk add` 联网装包（jq + python3） | ✅ 32s / 50 MiB |
| `edit` 工具（unified diff） | ✅ |
| 真实 LLM（TLS + 流式 + reasoning） | ✅ |
| Prompt cache 命中率 | ✅ 73–87% |
| 完整 agent 循环（读界面 → 跑命令 → 汇报） | ✅ |

真机实测输出（agent 自己汇报的）：

```
Alpine 版本：3.21.0
架构：aarch64
CPU 核数：8
内存：总计 11.3G，可用约 2.6G
磁盘：228.4G，可用 110.8G
```

---

## 2. 目录

```
android/                          # Gradle 根
├── build.gradle.kts              # AGP 8.13.2 / Kotlin 2.4.21
├── gradle/wrapper/               # Gradle 8.14.6
└── app/
    ├── build.gradle.kts
    ├── proguard-rules.pro
    └── src/main/
        ├── AndroidManifest.xml
        ├── java/dev/vihv/agent/
        │   ├── AgentApp.kt                 Application + 核心生命周期
        │   ├── MainActivity.kt             WebView 壳
        │   ├── AgentAccessibilityService.kt 设备操控
        │   ├── DeviceHostImpl.kt           uniffi callback 实现
        │   ├── RootfsInstaller.kt          rootfs 解压（手写 tar）
        │   └── UiTree.kt                   无障碍树 → 扁平 JSON
        ├── java/uniffi/agent_core/         生成的绑定（gitignore）
        ├── jniLibs/                        交叉编译产物（gitignore）
        ├── assets/                         Alpine rootfs（gitignore）
        └── res/xml/{accessibility,network_security}_config.xml
```

Kotlin 总量 **~1150 行**（不含生成的绑定）。

---

## 3. 真机上踩到的 11 个坑

按发现顺序。**每一条都是实测出来的，不是推测。**

### 坑 1 — `PROOT_LOADER` 不设，proot 直接拒绝执行

**症状**
```
proot error: '/bin/sh' is not executable
```

**原因**（`deps/proot/src/execve/enter.c`）：

```c
loader_path = loader_path ?: getenv("PROOT_LOADER") ?: extract_loader(tracee, false);
```

不设 `PROOT_LOADER` 时 proot 会把**内嵌的 loader 解压到临时目录**再执行。
那个位置是 `app_data_file` 标签 —— W^X 拒绝执行 → 失败。

**解法**：把 loader 路径通过环境变量指到 `nativeLibraryDir`：

```rust
env.insert("PROOT_LOADER".into(), "<nativeLibDir>/libproot-loader.so");
env.insert("PROOT_LOADER_32".into(), "<nativeLibDir>/libproot-loader32.so");
```

### 坑 2 — proot 只认 `PROOT_TMP_DIR`，不认 `TMPDIR`

**症状**
```
proot warning: Unable to create temp directory for f2fs bug probe: Permission denied
```

**原因**（`deps/proot/src/path/temp.c`）：

```c
temp_directory = getenv("PROOT_TMP_DIR");
if (temp_directory == NULL) temp_directory = P_tmpdir;   // → /tmp
```

Android 上 App 写不了 `/tmp`。

**解法**：两个都设 —— `TMPDIR` 给 guest 程序用，`PROOT_TMP_DIR` 给 proot 自己用。

### 坑 3 — proot 会检查执行位（我原本判断错了）

**症状**：修完坑 1、2 后仍然 `'/bin/sh' is not executable`。

**原因**（`deps/proot/src/path/path.c` 的 `which()`）：

```c
if (is_explicit && (statr.st_mode & S_IXUSR) == 0) {
    note(tracee, ERROR, USER, "'%s' is not executable", command);
    return -EACCES;
}
```

我原本以为不需要 chmod —— 理由是 loader 把二进制 **mmap** 进内存，而 mmap
只要求可读。理论上对，但 **proot 在 execve 之前自己先检查了执行位**。

**解法**：解压时从 tar 头里读 mode 并应用：

```kotlin
val mode = readOctal(header, 100, 8).toInt()
Files.setPosixFilePermissions(target.toPath(), toPerms(mode))
```

（`File.setExecutable` 只能设 owner 位，不够用。）

### 坑 4 — AAPT 把 `.tar.gz` 改名成 `.tar`

**实测**：`assets/alpine-minirootfs.tar.gz`（3.85 MB）进 APK 后变成
**`assets/alpine-minirootfs.tar`（8.47 MB，gzip 已被解掉）**。

**解法**：两个名字都试，并且**按 magic bytes 判断**而不是按文件名：

```kotlin
private fun isGzip(input: InputStream): Boolean {
    input.mark(4); val m = ByteArray(2); val n = input.read(m); input.reset()
    return n == 2 && (m[0].toInt() and 0xFF) == 0x1F && (m[1].toInt() and 0xFF) == 0x8B
}
```

### 坑 5 — `Application.onCreate` 抛异常 = 整个 App 崩

**症状**（真机 logcat）：

```
FATAL EXCEPTION: main
Caused by: android.app.ForegroundServiceStartNotAllowedException: startForegroundService() not allowed
    at AgentForegroundService$Companion.start(AgentApp.kt:170)
    at AgentApp.onCreate(AgentApp.kt:119)
```

Android 12+ 禁止从后台起前台服务。`Application.onCreate` 时 uid 状态还是
`CEM`，所以被拒。而 `onCreate` 里抛异常是致命的。

**解法**（两条都要）：
1. `Application.onCreate` 里**每一步都包 `runCatching`** —— 任何一步失败只降级，不致命
2. 前台服务改到 **`MainActivity.onResume`** 起（那时才真的进了前台）

### 坑 6 — DNS 拿到了 VPN 的，而 App 的流量不走 VPN

**症状**：`apk add` 报 `no such package`（看起来像软件源挂了）。

**根因**：`ConnectivityManager.activeNetwork` 返回的 DNS 是 Tailscale 的
`172.19.0.2` / `fdfe:dcba:9876::2`。App 不在 VPN 的允许列表里，所以那些地址不可达，
每条 DNS 查询都超时。

**解法**：遍历 `allNetworks`，**非 VPN 网络优先**，VPN 次之，公网兜底：

```kotlin
(nonVpn + vpn).distinct().take(3) + listOf("1.1.1.1", "8.8.8.8")
```

### 坑 7 — `prepare_rootfs` 被解压冲掉了

**症状**：`/etc/resolv.conf` 不存在。

**根因**：时序问题。`setupLinux()` 在 `Application.onCreate` 调，那时 rootfs
还没解压；`prepare_rootfs()` 建了 `/etc/resolv.conf`、`/root`、`/tmp`。
然后后台线程解压时 `deleteRecursively()` 整个目录重建 —— 全冲掉。

**解法**：解压完成后再调一次 `setupLinux()`（幂等）。

### 坑 8 — **R9 命中**：rustls-platform-verifier 在 Android 上 panic

**症状**：`/api/chat` 只发出 `start` / `start-step` 就断了，没有任何错误信息。

**为什么没错误信息**：panic 打到 **stderr**，而 Android 上 stderr 是丢掉的。

**先加 panic hook**（本来就该有）：

```rust
std::panic::set_hook(Box::new(|info| {
    let msg = format!("RUST PANIC: {info}");
    #[cfg(target_os = "android")] log::error!("{msg}");
    #[cfg(not(target_os = "android"))] eprintln!("{msg}");
}));
```

**立刻看到真话**：

```
RUST PANIC: panicked at rustls-platform-verifier-0.7.1/src/android.rs:90:10:
Expect rustls-platform-verifier to be initialized
```

**根因**：reqwest 0.13 的 `rustls` feature 硬编码走平台验证器，而它在 Android 上
必须先 `init_with_env(env, context)`。拿这两个要手写 `JNI_OnLoad` + 反调
`ActivityThread.currentApplication()` —— 正是决策 D2 想避开的东西。

**解法**：用 `webpki-roots` 内嵌根证书，通过 `Client::with_http_client` 注入自建的
reqwest client，**完全绕开平台验证器**：

```rust
fn tls_config() -> anyhow::Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    Ok(rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth())
}

let http = reqwest::ClientBuilder::new()
    .tls_backend_preconfigured(Self::tls_config()?)
    .build()?;
Ok(Client::with_config(cfg).with_http_client(http))
```

**代价**：不认用户/企业自签 CA。要用企业 MITM 代理时得换回平台验证器 ——
那时才值得为它写那段 JNI。

### 坑 9 — `tls_backend_preconfigured` 自己会包一层 `Some`

**症状**：
```
builder error: Unknown TLS backend passed to `use_preconfigured_tls`
```

**根因**（reqwest 0.13.5 源码）：

```rust
pub fn tls_backend_preconfigured(mut self, tls: impl Any) -> ClientBuilder {
    let mut tls = Some(tls);                                   // ← 它自己包了
    if let Some(conn) = (&mut tls as &mut dyn Any)
        .downcast_mut::<Option<rustls::ClientConfig>>() {      // ← downcast 到 Option<T>
```

传 `Some(config)` 会变成 `Option<Option<..>>`，downcast 失败。
（reqwest 0.12 的 `use_preconfigured_tls` 是要包 `Some` 的 —— API 变了。）

**解法**：传裸值。

### 坑 10 — 日志级别写死 Trace 会刷爆 logcat

tungstenite 每写一帧打十几行。默认改 Info，可用 `AGENT_LOG` 覆盖。

### 坑 11 — `force-stop` 会解绑无障碍服务

每次重装/重启 App 后必须重新触发一次绑定，否则 `device: null`。
`scripts/dev.sh` 的 `dev_a11y_on` 处理了（注意保留用户已有的其它服务）。

---

## 4. 三个架构决策的落点

### D1（进程内 `.so`）

```kotlin
core = AgentCore()                    // System.loadLibrary("agent_core")
port = core.start(0u.toUShort())      // 后台线程 + 独立 tokio runtime，同步返回端口
```

端口传 0 让系统分配 —— 实测每次启动都不同（34155 / 43709 / 41071 …），
硬编码在真机上确实会被占。

### D2（UniFFI callback）

```kotlin
class DeviceHostImpl(private val service: AgentAccessibilityService) : DeviceHost {
    override fun execute(cmdJson: String): String { ... }
    override fun name() = "accessibility"
}
```

**零手写 JNI。** 边界是 JSON 字符串，按 `type` 分发。

⚠️ 一个 uniffi 限制：callback 的返回类型不能是 `Result<_, String>` ——
bindgen 会报 `unknown throw type: Some(String)`。必须用 `#[derive(uniffi::Error)]` 的枚举。

### D3（同源 localhost）

```kotlin
w.loadUrl("http://127.0.0.1:$port")
```

明文流量由 `res/xml/network_security_config.xml` 给 loopback 开例外。
**实测确认这个例外是必需的** —— 不给的话 WebView 会报 `ERR_CLEARTEXT_NOT_PERMITTED`。

---

## 5. 必须设对的开关

### `packaging.jniLibs.useLegacyPackaging = true`

默认（false）时 `.so` 留在 APK 里由 linker 直接 mmap，**磁盘上没有实体文件** ——
而我们要 **exec** `nativeLibraryDir` 里的 `libproot.so`。

AGP 会警告 Manifest 里的 `android:extractNativeLibs` 已被这个 DSL 取代，
所以只设 DSL，不写 Manifest 属性。

### `abiFilters = ["arm64-v8a", "x86_64"]`

arm64 是真机，x86_64 是模拟器。`armeabi-v7a` / `x86` 跳过（D9）。

### 前台服务类型

targetSdk 34+ 起 `startForeground` 必须带类型：

```kotlin
if (Build.VERSION.SDK_INT >= 34) {
    startForeground(ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
}
```

### 16KB page size（R1）—— 已自动满足

实测 `cargo-ndk` 4.1.2 + NDK r27 的产物已经对齐到 `0x4000`：

```
align: 0x4000
align: 0x8
```

不需要手动加 `-Wl,-z,max-page-size=16384`。`just android-libs` 里保留了这个检查。

### 预测性返回（预测性返回手势 / predictive back）

`android:enableOnBackInvokedCallback="true"`（Manifest，API 33+ 才看）。
开了之后系统不再调 `onBackPressed()`，返回事件走 `OnBackInvokedDispatcher`。

壳和页面之间靠一根细线（`MainActivity.BackHost`，`addJavascriptInterface` 成
`window.AgentHost`）：**页面只在真有覆盖层时**调 `setBackHandled(true)`，壳才去
注册回调。两个理由：

1. 注册了就接管手势，系统自己的预测动画不会跑（官方文档明确写了）—— 而退出
   App 的那个手势本来就该让系统画，所以在根页面上不注册
2. 只有页面知道覆盖层（抽屉/查看器/面板）开没开；壳这边 `web.canGoBack()`
   只能猜

注册的回调（API 34+ 用 `OnBackAnimationCallback`，跟手进度在它的
`onBackProgressed` 上；API 33 只有 `onBackInvoked`）把进度转给页面：

```
start / progress(0..1) / cancel / commit  →  window.__predictiveBack(...)
```

页面把它喂给栈顶覆盖层的位移（`web/src/lib/overlay.ts`），于是返回手势
划到哪，抽屉/查看器就跟着挪到哪；松手完成就顺势关掉，撤回就弹回。

### 真机验证（Pixel 10 / Android 17，API 37）

用 `adb shell input swipe` 从屏幕左边缘往里划（系统把它当真手势），
页面里装探针记录 `__predictiveBack` 的入参，壳里打了 logcat：

| 场景 | 结果 |
|---|---|
| 开抽屉 | `setBackHandled(true)` → logcat「注册系统返回回调」 |
| 抽屉上手势 | `start` + progress 0→0.71 共 60 帧 → `commit` → 抽屉关掉 |
| 查看器上手势 | progress 0→0.78 共 180 帧，中途截图：查看器被推出 45% 露出后面的对话 |
| 划出去再退回原处 | `cancel` → 查看器弹回原位，仍然开着 |
| 没有覆盖层时按返回 | 回到桌面（回调已注销，整个手势交给系统） |

`input swipe` 无论多短都会 commit（合成手势没有真正的手势取消），所以
cancel 那行是用 `input motionevent DOWN/MOVE…/UP` 划出去再退回原处验的。

一个反常量的地方：一次 1.5 秒的手势会送来 ~180 个 progress 回调（每秒 120
次，比屏幕刷新率还密）。页面侧收到就是设一个 transform，没有额外开销。

---

## 6. 设备操控的实现选择

| 指令 | 做法 | 为什么 |
|---|---|---|
| `tap_node` | 先按 `viewId` 找节点 `performAction(CLICK)`，失败退回**坐标手势** | `performAction` 不受滚动位置/状态栏高度影响；但很多控件没有 viewId |
| `tap_xy` / `swipe` | `dispatchGesture` | 需要 `canPerformGestures="true"` |
| `set_text` | `ACTION_FOCUS` + `ACTION_SET_TEXT` | 有些输入法要焦点才认 |
| `key` | `performGlobalAction` | back/home/recents |
| `launch` | `getLaunchIntentForPackage` + `FLAG_ACTIVITY_NEW_TASK` | 从 Service 启动必须带这个 flag |

### 线程模型

`execute` 是从 Rust 的 `spawn_blocking` 线程进来的，而无障碍 API 必须在主线程调。
`onMain {}` 会 post 到主线程并**阻塞等结果**（8s 超时）。

### 事件节流（已随快照一起删掉）

`onAccessibilityEvent` 每秒可达几十次，原来用 250ms 的 debounce 合并。

> 读 UI 树那整套（`UiTree.kt`、推快照、按节点索引点击/输入）是给
> `device_snapshot` / `device_tap` 两个工具做地基的；工具删掉之后它跟着删了。
> 现在无障碍服务只剩不需要读界面的那几条：坐标点击、滑动、系统按键、启动应用。
> 以后做沙箱里的设备命令时会连同它一起重做。

### 实测效果（当时的记录）

模型读到节点 #14 是「📱 界面」标签页 → 点击 `(540,2259)` → 界面切换 →
再次快照确认（500 节点）。

---

## 7. 构建链

```sh
just android-assets      # 一次性：Alpine rootfs + proot 二进制
just android-debug       # bindings → 交叉编译 → web → APK
```

两个 `cargo` 命令（`uniffi-bindgen` 和 `cargo ndk`）**必须 cwd 在 crate 目录里** ——
它们内部都调 `cargo metadata`，用 `--manifest-path` 会失败。

`JAVA_HOME` 不信任环境变量（用户环境里常有个指向没装的 Android Studio 的坏值），
`scripts/jdk.sh` 自己探测。

### 真机调试

```sh
. scripts/dev.sh
dev_restart      # 重装 + 重启 + 转发端口
dev_a11y_on      # 开无障碍（保留已有的其它服务）
dev_llm          # 注入 .env.local 里的 LLM 配置
dev_shell 'uname -m'
```

安卓上 App **不继承 shell 环境变量**，所以 LLM 配置走运行时接口
`POST /api/llm`，存进 `sessions.db` 的 `meta` 表（`llm` 那一行；老版本的
`llm.json` 启动时自动迁进来，然后删掉文件）。

模型能不能看图不靠人配：`GET /api/models` 拉 provider 的 `/models` 清单，
再查 models.dev 目录（缓存 48h）里该模型的 `modalities.input`，自动得出。
`POST /api/model` 可在同一 provider 内换模型（自动重判）。

---

## 8. 还欠的债

1. **proot 是从本地 OpenMinis checkout 复制的二进制。**
   正式发布前必须自己编 —— provenance + 许可证（proot GPL-2.0，OpenMinis fork 也 GPL）。
   `just android-assets` 里标了警告。
2. **只出 arm64-v8a。** 要跑模拟器得把 `x86_64` 加回 `abiFilters` 和
   `android-libs` 的 `cargo ndk -t`（加回来真机就白带 4MB）。
3. ~~**`.so` 5.7 MB，`aws-lc-rs` 是大头。**~~ 已处理：
   - `aws-lc-rs` → `ring`（reqwest/async-openai 走 `rustls-no-provider`）
   - `tracing-subscriber` 改成非安卓依赖（它把 `regex` 也带进来了）
   - `android_logger` 关掉默认的 `regex` feature
   - `async-openai` 去掉没用的 `responses` feature
   现在 `.so` **7.06 MB**（比之前大是因为加了 `image` 解码器），release APK **7.1 MB**。
   再省就得砍 `image` 的 webp/gif/bmp（~0.3MB）或绕过 reqwest 的
   `rustls-platform-verifier`（~0.1MB）。
4. **API key 明文存在 `sessions.db`（`meta` 表）里。** 受 Android 沙箱保护，
   但下一步应该用 Android Keystore 加密。
5. ~~**没有 Web 端的设置界面。**~~ 已处理：设置页里按服务商填 API Key
   （见 docs/05-ui.md「设置页」），`POST /api/llm` 只留给脚本 / 桌面开发。
