//! UniFFI 边界：给 Kotlin 用的接口（决策 D2）。
//!
//! 设计原则（见 `docs/01-tech-selection.md`）：
//!
//! - **正向**（Kotlin → Rust）：uniffi 自动生成 Kotlin 绑定
//! - **反向**（Rust → Kotlin）：`DeviceHost` callback interface，
//!   Kotlin 实现它，uniffi 自动生成反向调用代码 —— 我们**不手写 JNI**
//! - 跨边界一律用 JSON 字符串而不是细粒度强类型，这样 trait 能直接被
//!   uniffi 导出，不需要为每个方法设计跨语言类型映射
//!
//! Kotlin 侧的用法：
//!
//! ```kotlin
//! val core = AgentCore()                      // 触发 System.loadLibrary("agent_core")
//! core.log("info", "booting")
//! core.setDevice(DeviceHostImpl(service))
//! core.setupLinux(AndroidEnv(...))
//! val port = core.start(0)                    // 0 = 系统分配端口
//! webView.loadUrl("http://127.0.0.1:$port")
//! ```

use std::sync::Arc;

use crate::linux::SandboxConfig;
use crate::state::{BrowserApi, DeviceApi};
use crate::Core;

/// 统一的错误类型。`flat_error` 让 Kotlin 侧只拿到一个异常类 + 消息字符串，
/// 不用为每个变体生成嵌套类型。
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum CoreError {
    #[error("{message}")]
    Failed { message: String },
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        CoreError::Failed {
            // `{:#}` 带上 cause 链 —— 调试时这行差很多
            message: format!("{e:#}"),
        }
    }
}

/// 安卓侧的环境信息。Kotlin 在 boot 时一次性交进来。
#[derive(Debug, Clone, uniffi::Record)]
pub struct AndroidEnv {
    /// `<applicationInfo.nativeLibraryDir>` —— proot 二进制所在（可执行）
    pub native_lib_dir: String,
    /// `<context.filesDir>` —— rootfs 解压到这里
    pub files_dir: String,
    /// `<context.cacheDir>` —— proot 的 TMPDIR
    pub cache_dir: String,
    /// 系统 DNS。Alpine minirootfs 里没有 /etc/resolv.conf，
    /// 不写的话 `apk add` 会因为解析不了域名而报 "no such package"
    pub dns_servers: Vec<String>,
    /// 时区偏移（分钟）。写进沙箱的 TZ（POSIX 格式，符号与 UTC 相反）
    pub tz_offset_minutes: i32,
    /// 手机内部存储根目录（`/storage/emulated/0`）。有「所有文件访问权限」
    /// 才传，否则 None —— 沙箱的 /workspace 就退回 App 私有目录。
    pub shared_storage: Option<String>,
}

/// 设备能力接口，**由 Kotlin 实现**。
///
/// 实现者注意：`execute` 会被 Rust 从后台线程调用（`spawn_blocking`），
/// 所以实现里不能假设自己在主线程。无障碍 API 调用本身很快（几毫秒），
/// 直接同步做即可。
#[uniffi::export(callback_interface)]
pub trait DeviceHost: Send + Sync {
    /// 执行一条指令（`Command` 的 JSON），返回结果描述。
    ///
    /// 错误类型必须是 uniffi Error —— `Result<_, String>` 不行，
    /// bindgen 会报 `unknown throw type: Some(String)`。
    fn execute(&self, cmd_json: String) -> Result<String, CoreError>;

    /// 设备/实现名，仅用于展示
    fn name(&self) -> String;
}

/// 把 uniffi callback 适配成内部的 `DeviceApi`
struct HostAdapter(Box<dyn DeviceHost>);

impl DeviceApi for HostAdapter {
    fn execute(&self, cmd_json: &str) -> Result<String, String> {
        self.0
            .execute(cmd_json.to_string())
            .map_err(|e| e.to_string())
    }
    fn name(&self) -> String {
        self.0.name()
    }
}

/// 浏览器能力接口，**由 Kotlin 实现**。
///
/// 和 `DeviceHost` 是两条独立的路：这个不依赖无障碍服务，`AgentApp.onCreate`
/// 就注入了（而设备要等服务连上）。
///
/// 实现者注意两点：
///
/// - `execute` 会被 Rust 从 `spawn_blocking` 线程调用，**不是主线程**；
///   而 WebView 只能在主线程碰，所以实现里要自己 post 回主线程再等结果。
/// - 一次调用是**秒级**的（navigate 要等页面加载）。阻塞那个后台线程没问题，
///   但必须带硬超时 —— Rust 侧也有一个兜底（`Core::browser`）。
#[uniffi::export(callback_interface)]
pub trait BrowserHost: Send + Sync {
    /// 执行一个动作（JSON），返回结果 JSON。形状见 `BrowserApi::execute`。
    fn execute(&self, action_json: String) -> Result<String, CoreError>;
}

struct BrowserAdapter(Box<dyn BrowserHost>);

impl BrowserApi for BrowserAdapter {
    fn execute(&self, action_json: &str) -> Result<String, String> {
        self.0
            .execute(action_json.to_string())
            .map_err(|e| e.to_string())
    }
}

fn failed(e: impl std::fmt::Display) -> CoreError {
    CoreError::Failed {
        message: format!("{e:#}"),
    }
}

/// 一台配对过的电脑（给壳的，不带凭据）。
#[derive(uniffi::Record)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    /// 配了中继（出门在外也能连）
    pub relay: bool,
}

/// 核心句柄。Kotlin 持有它，生命周期跟 Application 一致。
#[derive(uniffi::Object)]
pub struct AgentCore {
    core: Core,
}

/// 不导出给 Kotlin 的小帮手。
impl AgentCore {
    /// 核心的 tokio runtime（`start` 之后才有）。从 Kotlin 线程上 `block_on` 用。
    fn rt(&self) -> Result<&tokio::runtime::Handle, CoreError> {
        self.core
            .state
            .rt
            .get()
            .ok_or_else(|| failed("核心还没启动"))
    }

    /// 机器记账（配对过哪些电脑、现在连的是哪台）。
    ///
    /// 它跟着库一起建（`AppState::open_db`），所以 `setup_linux` 之后才有 ——
    /// 在那之前 `None`。
    fn machines_store(&self) -> Result<Arc<crate::machines::Machines>, CoreError> {
        self.core
            .state
            .machines()
            .ok_or_else(|| failed("库还没打开"))
    }
}

#[uniffi::export]
impl AgentCore {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        crate::init_tracing();
        Arc::new(Self { core: Core::new() })
    }

    /// 绑定端口并开始服务（后台线程 + 独立 tokio runtime）。
    ///
    /// 传 0 让系统分配端口。**返回实际端口**，WebView 要指向它。
    ///
    /// 这个函数是同步的：它在内部起线程，等绑定成功就返回，
    /// 不会阻塞 Kotlin 的调用线程。
    pub fn start(&self, port: u16) -> Result<u16, CoreError> {
        let p = self.core.clone().serve_in_background(port)?;
        self.core
            .push_log("info", &format!("core 已启动 http://127.0.0.1:{p}"));
        Ok(p)
    }

    /// 本地 API 的凭据。WebView 要先打开 `/auth?t=<它>` 种上 cookie，
    /// 之后 `/api/…` 才放行（见 `state::Auth`）。`setup_linux` 之后才有值
    /// （它在会话库里，库是那时打开的）。
    pub fn token(&self) -> Option<String> {
        self.core.state.auth().token
    }

    pub fn port(&self) -> u16 {
        self.core.port()
    }

    /// 配对过的电脑（不带凭据）。面板自己不靠它 —— 面板问 `GET /api/machines`；
    /// 它是壳在「那台电脑不认我们的凭据了」时用的（要说出名字）。
    pub fn machines(&self) -> Vec<MachineInfo> {
        self.machines_store()
            .map(|m| m.list())
            .unwrap_or_default()
            .into_iter()
            .map(|m| MachineInfo {
                relay: m.relay.is_some(),
                id: m.id,
                name: m.name,
            })
            .collect()
    }

    /// 连上这台电脑，返回 **WebView 要加载的地址**（`http://localhost:<端口>/auth?t=…&k=…`）。
    ///
    /// 选路（局域网 / 中继）在核心里，见 `tunnel.rs`；只是在本机绑端口，不联网。
    /// 页面自己拿到的是另一个地址（不带凭据，见 `machines::Conn::page_url`）——
    /// 这一步是壳专用的。
    pub fn connect_machine(&self, id: String) -> Result<String, CoreError> {
        let m = self.machines_store()?;
        let rt = self.rt()?;
        rt.block_on(m.connect(&id))
            .map(|c| c.shell_url())
            .map_err(failed)
    }

    /// 上次连着的那台，接着连（壳启动时调）。返回 WebView 要加载的地址，没连过就是 `None`。
    pub fn resume_machine(&self) -> Option<String> {
        let m = self.machines_store().ok()?;
        let rt = self.rt().ok()?;
        rt.block_on(m.resume()).map(|c| c.shell_url())
    }

    /// 现在连的是哪台（页面让壳导航时，壳要拿它对上账；见 `Remote.navigate`）。
    pub fn current_machine_id(&self) -> Option<String> {
        self.machines_store().ok()?.current_id()
    }

    /// 现在这条隧道的地址（**壳那份**：带那台电脑的凭据）。壳只认这个地址和本机地址
    /// —— 页面（包括查看器里 AI 写的网页）给不了任意地址，见 docs/11。
    pub fn current_machine_url(&self) -> Option<String> {
        self.machines_store().ok()?.current().map(|(_, url, _)| url)
    }

    /// 断开（回到本机自己的核心）。
    pub fn disconnect_machine(&self) {
        if let Ok(m) = self.machines_store() {
            m.disconnect();
        }
    }

    /// 它不认我们的凭据了（换过库）：忘掉它，下次得重新配对。
    pub fn forget_machine(&self, id: String) {
        if let Ok(m) = self.machines_store() {
            m.forget(&id);
        }
    }

    /// mDNS 在局域网里看到了这台（壳的 `CoreDiscovery` 报上来）。
    ///
    /// 配没配对都报 —— 面板要把没配对的也列出来（点了输一次配对码就能用）。
    pub fn lan_found(&self, id: String, name: String, host: String, port: u16) {
        if let Ok(m) = self.machines_store() {
            m.lan_found(&id, &name, &host, port);
        }
    }

    /// 局域网里看不到它了（广播撤了 / 换了网）。
    pub fn lan_lost(&self, id: String) {
        if let Ok(m) = self.machines_store() {
            m.lan_lost(&id);
        }
    }

    /// 一次扫描结束了（面板关掉、[scanBriefly] 那 15 秒到了）：没配对过的那些忘掉。
    pub fn lan_reset(&self) {
        if let Ok(m) = self.machines_store() {
            m.lan_reset();
        }
    }

    /// 网络变了：正连着的那台重新先试局域网。
    pub fn network_changed(&self) {
        if let Ok(m) = self.machines_store() {
            m.network_changed();
        }
    }

    pub fn version(&self) -> String {
        crate::VERSION.to_string()
    }

    /// 页面里注入的 JS（`kind` 见 `browser_js::KINDS`，`params` 是 JSON 对象）。
    ///
    /// **手机上那份 JS 不在 Kotlin 里**：动作级的页面逻辑（骨架、正文、点击、
    /// 输入、按键、滚动、找元素、DOM 指纹、页面信息、`target=_blank` 修正）只有
    /// 核心这一份，桌面（CDP）用的是同一个 `build`。各写一份就会漂 ——
    /// 漂了的表现是「点不动 / 抽不出正文」，只有真机上才看得出来。
    pub fn browser_js(&self, kind: String, params: String) -> Result<String, CoreError> {
        crate::browser_js::build(&kind, &params).map_err(|e| CoreError::Failed {
            message: e.to_string(),
        })
    }

    /// 注入设备实现。无障碍服务连上之后调用。
    pub fn set_device(&self, host: Box<dyn DeviceHost>) {
        self.core.set_device(Arc::new(HostAdapter(host)));
    }

    /// 无障碍服务断开时调用
    pub fn clear_device(&self) {
        self.core.state.clear_device();
    }

    /// 注入浏览器实现。`AgentApp.onCreate` 里调（不依赖无障碍）。
    pub fn set_browser(&self, host: Box<dyn BrowserHost>) {
        self.core
            .state
            .set_browser(std::sync::Arc::new(BrowserAdapter(host)));
    }

    /// 配置 Linux 沙箱。
    ///
    /// 安卓上：
    /// - `proot` = `<nativeLibraryDir>/libproot.so`（伪装成 .so 才能被
    ///   APK 安装器解压到可执行目录）
    /// - `rootfs` = `<filesDir>/alpine-rootfs`
    pub fn setup_linux(&self, env: AndroidEnv) -> Result<(), CoreError> {
        // 顺便把持久化目录告诉核心：它打开会话库，读出 provider 配置和凭据
        self.core
            .set_data_dir(std::path::PathBuf::from(&env.files_dir));
        let rootfs = std::path::Path::new(&env.files_dir).join("alpine-rootfs");
        let native = std::path::Path::new(&env.native_lib_dir);
        let cfg = SandboxConfig {
            proot: native.join("libproot.so"),
            rootfs,
            binds: Vec::new(),
            tmp: Some(std::path::Path::new(&env.cache_dir).join("proot-tmp")),
            // ⚠️ 安卓上这两个必须指到 nativeLibraryDir。
            // 不设的话 proot 会把内嵌 loader 解到 app 临时目录，
            // 而那里是 app_data_file —— W^X 拒绝执行，最后报
            // `'/bin/sh' is not executable`。
            loader: Some(native.join("libproot-loader.so")),
            loader32: Some(native.join("libproot-loader32.so")),
            dns: env.dns_servers,
            tz_offset_minutes: Some(env.tz_offset_minutes),
            env: Default::default(),
            workspace: None,
            shared: None,
        }
        .with_shared_storage(env.shared_storage.as_deref().map(std::path::Path::new));
        self.core.set_linux_sandbox(cfg);
        Ok(())
    }

    /// 存储权限变了（用户刚在设置里打开「所有文件访问」）：只重配沙箱的
    /// 挂载，不碰别的。不能为此重新调 setup_linux —— 它会重开会话库、
    /// 用磁盘上的旧数据覆盖内存里的会话。
    pub fn set_shared_storage(&self, path: Option<String>) {
        let Some(sb) = self.core.state.linux() else {
            return;
        };
        // 宿主模式（桌面）没有手机存储可挂
        let Some(cfg) = sb.proot_config() else {
            return;
        };
        let cfg = cfg
            .clone()
            .with_shared_storage(path.as_deref().map(std::path::Path::new));
        if cfg.shared.is_some() == sb.has_shared_storage() {
            return;
        }
        self.core.set_linux_sandbox(cfg);
    }

    /// 推送一条日志（会出现在前端的「事件」页）
    pub fn log(&self, level: String, msg: String) {
        self.core.push_log(&level, &msg);
    }

    /// 清空会话历史
    pub fn reset_session(&self) {
        self.core.reset_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeHost;

    impl DeviceHost for FakeHost {
        fn execute(&self, cmd_json: String) -> Result<String, CoreError> {
            Ok(format!("handled {cmd_json}"))
        }
        fn name(&self) -> String {
            "fake".into()
        }
    }

    #[test]
    fn device_host_is_adapted_into_the_internal_trait() {
        let adapter = HostAdapter(Box::new(FakeHost));
        // 内部 trait 是同步的，callback 也是同步的
        assert_eq!(adapter.name(), "fake");
        assert!(adapter
            .execute(r#"{"type":"key","name":"back"}"#)
            .unwrap()
            .contains("back"));
    }

    #[test]
    fn anyhow_errors_keep_the_cause_chain_in_the_message() {
        let e: CoreError = anyhow::anyhow!("底层失败").context("外层操作").into();
        let msg = e.to_string();
        assert!(msg.contains("外层操作"), "{msg}");
        assert!(msg.contains("底层失败"), "{msg}");
    }

    #[test]
    fn constructor_does_not_panic_and_reports_a_version() {
        let core = AgentCore::new();
        assert!(!core.version().is_empty());
        assert_eq!(core.port(), 0, "还没 start 时端口应为 0");
    }
}
