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
use crate::state::DeviceApi;
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

/// 核心句柄。Kotlin 持有它，生命周期跟 Application 一致。
#[derive(uniffi::Object)]
pub struct AgentCore {
    core: Core,
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

    pub fn port(&self) -> u16 {
        self.core.port()
    }

    pub fn version(&self) -> String {
        crate::VERSION.to_string()
    }

    /// 注入设备实现。无障碍服务连上之后调用。
    pub fn set_device(&self, host: Box<dyn DeviceHost>) {
        self.core.set_device(Arc::new(HostAdapter(host)));
    }

    /// 无障碍服务断开时调用
    pub fn clear_device(&self) {
        self.core.state.clear_device();
    }

    /// 配置 Linux 沙箱。
    ///
    /// 安卓上：
    /// - `proot` = `<nativeLibraryDir>/libproot.so`（伪装成 .so 才能被
    ///   APK 安装器解压到可执行目录）
    /// - `rootfs` = `<filesDir>/alpine-rootfs`
    pub fn setup_linux(&self, env: AndroidEnv) -> Result<(), CoreError> {
        // 顺便把持久化目录告诉核心，它会从 llm.json 恢复 provider
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
        let cfg = sb
            .config()
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
        assert!(adapter.execute(r#"{"type":"key","name":"back"}"#).unwrap().contains("back"));
    }

    #[test]
    fn anyhow_errors_keep_the_cause_chain_in_the_message() {
        let e: CoreError = anyhow::anyhow!("底层失败")
            .context("外层操作")
            .into();
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
