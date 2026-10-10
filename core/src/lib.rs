//! android-agent 核心。
//!
//! 架构（见 docs/01-tech-selection.md §4）：
//!
//! ```text
//! WebView (Svelte)  ──同源 HTTP/WS──▶  axum  ◀──▶ AppState
//!                                        │
//!                            spawn_blocking
//!                                        ▼
//!                                 DeviceApi (Kotlin / mock)
//! ```
//!
//! 边界铁律：**native 侧不反调 Java**。设备能力由 `DeviceApi` trait 抽象，
//! 安卓上由 Kotlin 实现，通过 UniFFI callback interface 注入（阶段 2）。

pub mod agent;
pub mod assets;
pub mod demo;
pub mod editor;
pub mod ffi;
pub mod http;
pub mod linux;
pub mod llm;
pub mod mock;
pub mod sse;
pub mod state;

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub use state::{
    AppState, Command, DeviceApi, Event, Flags, Node, Screen, UiSnapshot, VERSION,
};
// proc-macro 模式的脚手架。必须在 crate 根调用一次，
// 之后 `uniffi-bindgen generate --library` 才能从 .so 里读出元数据。
uniffi::setup_scaffolding!();

/// 初始化日志。安卓上同时输出到 logcat，桌面上输出到 stdout。
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    // panic hook。
    //
    // 必须装：Rust 的 panic 默认打到 stderr，而 **Android 上 stderr 是丢掉的**。
    // 不装的话任务里 panic 了只会看到流莫名断掉，完全查不出原因
    // （实测踩过：只有 start/start-step 然后就 [DONE]）。
    std::panic::set_hook(Box::new(|info| {
        let msg = format!("RUST PANIC: {info}");
        #[cfg(target_os = "android")]
        log::error!("{msg}");
        #[cfg(not(target_os = "android"))]
        eprintln!("{msg}");
    }));

    let filter = EnvFilter::try_from_env("AGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,agent_core=debug,axum=info"));

    #[cfg(target_os = "android")]
    {
        // 默认 Info。之前写死 Trace 把 logcat 刷爆了 ——
        // tungstenite 每写一帧打十几行。
        // 要更细的日志用 `adb shell setprop log.tag.agent-core DEBUG`，
        // 或改 AGENT_LOG。
        let level = std::env::var("AGENT_LOG")
            .ok()
            .and_then(|v| match v.split(',').next().unwrap_or("").trim() {
                "trace" => Some(log::LevelFilter::Trace),
                "debug" => Some(log::LevelFilter::Debug),
                "warn" => Some(log::LevelFilter::Warn),
                "error" => Some(log::LevelFilter::Error),
                "info" => Some(log::LevelFilter::Info),
                _ => None,
            })
            .unwrap_or(log::LevelFilter::Info);

        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(level)
                .with_tag("agent-core"),
        );
        let _ = filter;
    }

    #[cfg(not(target_os = "android"))]
    {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }
}

/// 核心句柄。HTTP handler、UniFFI 导出、桌面 bin 都走这一层。
#[derive(Clone)]
pub struct Core {
    pub state: Arc<AppState>,
}

impl Default for Core {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    pub fn new() -> Self {
        Self {
            state: AppState::new(),
        }
    }

    /// 带上 mock 设备 —— 桌面开发和无设备调试用。
    pub fn with_mock() -> Self {
        let core = Self::new();
        core.state.mock.store(true, Ordering::Relaxed);
        let dev = mock::MockDevice::new(Arc::downgrade(&core.state));
        core.state.set_device(dev);

        // 桌面：如果环境里能找到 proot + rootfs，就把 Linux 沙箱也接上
        if let Some(sb) = linux::LinuxSandbox::from_env() {
            core.state.set_linux(Arc::new(sb));
        }
        core
    }

    /// 注入 Linux 沙箱（安卓上由 Kotlin 在 boot 时传入路径）。
    ///
    /// 会顺手 `prepare_rootfs()` —— 补上 `/etc/resolv.conf`、`/root`、`/tmp`。
    /// **漏掉这一步的后果**：`apk add` 因为 DNS 解析不了而报
    /// "no such package"（看起来像软件源挂了）；proot 的 f2fs 探测
    /// 因为 `/tmp` 不存在而报 permission denied。
    /// 设置持久化目录并从中恢复 LLM 配置。安卓上由 Kotlin 在 boot 时调用。
    pub fn set_data_dir(&self, dir: std::path::PathBuf) {
        if let Ok(mut g) = self.state.data_dir.write() {
            *g = Some(dir.clone());
        }
        self.state.load_llm(&dir);
    }

    pub fn set_linux_sandbox(&self, cfg: linux::SandboxConfig) {
        let sb = linux::LinuxSandbox::new(cfg);
        if let Err(e) = sb.prepare_rootfs() {
            self.push_log("warn", &format!("准备 rootfs 失败: {e:#}"));
        }
        self.state.set_linux(Arc::new(sb));
    }

    /// 清空会话历史（含待审批项）
    pub fn reset_session(&self) {
        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(h) => {
                let st = self.state.clone();
                h.spawn(async move {
                    let mut store = st.chat.write().await;
                    store.active_mut().clear();
                });
            }
            Err(_) => {
                // 没有运行时上下文（比如刚从 Kotlin 调进来）：
                // try_write 拿不到就算了，下次 /api/reset 会清
                if let Ok(mut store) = self.state.chat.try_write() {
                    store.active_mut().clear();
                }
            }
        }
        self.push_log("info", "会话已清空");
    }

    /// 在 Linux 沙箱里跑一段脚本。
    pub async fn run_linux(
        &self,
        script: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<linux::ExecResult> {
        let sb = self
            .state
            .linux()
            .ok_or_else(|| anyhow::anyhow!("Linux 沙箱未初始化"))?;
        sb.run(script, timeout).await
    }

    /// 编辑沙箱里的文件（`edit` 工具）。
    ///
    /// 读、改、写都同步且很快（都是本地文件），但包成 async 是为了
    /// 跟 `run_linux` 一样能被 `spawn_blocking` 之外的调用方直接 await。
    pub async fn edit_file(
        &self,
        path: &str,
        edits: &[editor::Edit],
    ) -> anyhow::Result<editor::EditOutcome> {
        let sb = self
            .state
            .linux()
            .ok_or_else(|| anyhow::anyhow!("Linux 沙箱未初始化"))?;
        let path = path.to_string();
        let edits = edits.to_vec();
        tokio::task::spawn_blocking(move || sb.edit_file(&path, &edits)).await?
    }

    pub fn port(&self) -> u16 {
        self.state.port.load(Ordering::Relaxed)
    }

    /// 注入设备实现（安卓上由 Kotlin 通过 UniFFI 传入）。
    pub fn set_device(&self, dev: Arc<dyn DeviceApi>) {
        self.state.set_device(dev);
    }

    /// 设备推送 UI 快照。`body` 是 `UiSnapshot` 的 JSON。
    pub fn push_snapshot(&self, body: &str) -> anyhow::Result<UiSnapshot> {
        http::store_snapshot(&self.state, body)
    }

    /// 设备推送日志。
    pub fn push_log(&self, level: &str, msg: &str) {
        self.state.log(level, msg);
    }

    /// 主动发一条指令给设备。
    pub async fn dispatch(&self, cmd: Command) -> serde_json::Value {
        http::dispatch(&self.state, cmd).await
    }

    /// 绑定端口并开始服务。返回实际端口（传 0 则由系统分配）。
    pub async fn serve(&self, port: u16) -> anyhow::Result<u16> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        let actual = listener.local_addr()?.port();
        self.state.port.store(actual, Ordering::Relaxed);

        let app = http::router(self.state.clone());
        let st = self.state.clone();
        tokio::spawn(async move {
            st.log("info", format!("http://127.0.0.1:{actual}"));
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("http server exited: {e}");
            }
        });

        Ok(actual)
    }

    /// 在后台线程起一个 tokio runtime 并开始服务。
    ///
    /// 安卓上从 JNI 调用时**不能**阻塞调用线程去 `block_on` —— 这是
    /// 给 Kotlin 用的入口：同步返回端口，服务在独立线程里跑。
    pub fn serve_in_background(self, port: u16) -> anyhow::Result<u16> {
        let (tx, rx) = std::sync::mpsc::channel::<Result<u16, String>>();
        let me = self.clone();

        std::thread::Builder::new()
            .name("agent-core-rt".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(Err(format!("runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(async move {
                    match me.serve(port).await {
                        Ok(p) => {
                            let _ = tx.send(Ok(p));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("bind: {e}")));
                            return;
                        }
                    }
                    // 服务跑在这个 runtime 里直到进程结束
                    std::future::pending::<()>().await;
                });
            })
            .map_err(|e| anyhow::anyhow!("spawn: {e}"))?;

        rx.recv()
            .map_err(|e| anyhow::anyhow!("runtime thread died: {e}"))?
            .map_err(|e| anyhow::anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_and_state_wire_up() {
        let core = Core::with_mock();
        assert_eq!(core.port(), 0);
        assert!(core.state.device().is_some());
    }

    #[test]
    fn mock_device_produces_a_snapshot() {
        let core = Core::with_mock();
        let dev = core.state.device().unwrap();
        dev.execute(r#"{"type":"snapshot"}"#).unwrap();
        let snap = core.state.latest_snapshot().expect("snapshot 应该已存入");
        assert!(snap.nodes.len() >= 4, "Home 屏应有 5 个节点");
        assert_eq!(snap.nodes[0].p, -1, "根节点 parent = -1");
    }

    #[test]
    fn push_snapshot_rejects_garbage() {
        let core = Core::new();
        assert!(core.push_snapshot("not json").is_err());
        assert!(core.push_snapshot(r#"{"ts":1,"nodes":[]}"#).is_ok());
    }
}
