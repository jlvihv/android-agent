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
pub mod db;
pub mod demo;
pub mod editor;
pub mod ffi;
pub mod http;
pub mod imaging;
pub mod linux;
pub mod llm;
pub mod mock;
pub mod models;
pub mod openai;
pub mod providers;
pub mod reader;
pub mod responses;
pub mod search;
pub mod sse;
pub mod state;

use std::sync::atomic::Ordering;
use std::sync::Arc;

pub use state::{AppState, Command, DeviceApi, Event, VERSION};
// proc-macro 模式的脚手架。必须在 crate 根调用一次，
// 之后 `uniffi-bindgen generate --library` 才能从 .so 里读出元数据。
uniffi::setup_scaffolding!();

/// 初始化日志。安卓上同时输出到 logcat，桌面上输出到 stdout。
pub fn init_tracing() {
    // rustls 的 crypto provider 必须在**建任何 reqwest client 之前**装好，
    // 否则 async-openai 的 `Client::with_config()` 里那句
    // `reqwest::Client::new()` 直接 panic，整个回合静默死掉。
    // 放在这里是因为桌面 / 安卓两条启动路径都会先调它。
    crate::llm::install_crypto_provider();

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
        // 这里**故意不装 tracing subscriber**：安卓上走 `tracing` 的 `log`
        // feature，事件直接交给上面那个 android_logger（见 Cargo.toml）。
        // 过滤粒度由 android_logger 的 level 决定
        // （AGENT_LOG=debug 或 setprop log.tag.agent-core DEBUG 都能调）。
        //
        // `tracing-subscriber` 因此是**非安卓依赖** —— 不然它会把
        // regex-automata/regex-syntax 一起编进 .so（安卓上根本用不到）。
    }

    #[cfg(not(target_os = "android"))]
    {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_env("AGENT_LOG")
            .unwrap_or_else(|_| EnvFilter::new("info,agent_core=debug,axum=info"));
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

/// 将附件字节写到 `<dir>/<id>`（阻塞 I/O 包一层 `spawn_blocking`）。
async fn write_attachment(
    dir: std::path::PathBuf,
    id: String,
    bytes: Vec<u8>,
) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("建附件目录失败: {e}"))?;
        std::fs::write(dir.join(&id), &bytes).map_err(|e| anyhow::anyhow!("写附件失败: {e}"))?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
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
        let dev = mock::MockDevice::new();
        core.state.set_device(dev);

        // 桌面：环境变量里配了 LLM 就当成一个已配置的服务商（没有库，只在内存里）
        if let Some(p) = crate::llm::Provider::from_env() {
            core.state.adopt_provider(&p);
            core.state.set_llm(p);
        }

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
        // 库里有 provider 配置和访问凭据；会话本身在 serve() 里后台读
        self.state.open_db(&dir);
    }

    pub fn set_linux_sandbox(&self, cfg: linux::SandboxConfig) {
        let sb = linux::LinuxSandbox::new(cfg);
        // 端口可能早就 bind 好了（安卓上 rootfs 解压完之后会**再调一次**这里，
        // 那是个新的 sandbox 实例）—— 不补这一下，沙箱里的 `agent` 命令
        // 就找不到核心。
        sb.set_agent_port(self.state.port.load(Ordering::Relaxed));
        sb.set_agent_token(self.state.auth().token);
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

    /// 同 `run_linux`，输出边跑边送进 `live`
    pub async fn run_linux_live(
        &self,
        script: &str,
        timeout: std::time::Duration,
        live: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> anyhow::Result<linux::ExecResult> {
        let sb = self
            .state
            .linux()
            .ok_or_else(|| anyhow::anyhow!("Linux 沙箱未初始化"))?;
        sb.run_live(script, timeout, Some(live)).await
    }

    /// 读沙箱里的文件（`read` 工具）。
    ///
    /// 图片（`ReadResult::Image`）已经解码/缩放好、可以直接内联；
    /// 其余是带 offset/limit/截断提示的文本。和 `edit_file` 一样是同步
    /// 本地 I/O，包一层 `spawn_blocking` 免得占住 tokio 的 worker（只有 2 个）。
    pub async fn read_any(
        &self,
        path: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<linux::ReadResult> {
        let sb = self
            .state
            .linux()
            .ok_or_else(|| anyhow::anyhow!("Linux 沙箱未初始化"))?;
        let path = path.to_string();
        tokio::task::spawn_blocking(move || sb.read_any(&path, offset, limit)).await?
    }

    /// 跑一个浏览器动作，返回结果 JSON。
    ///
    /// 三层保护，缺一层就可能把整个回合卡死：
    ///
    /// 1. `spawn_blocking`：Kotlin 侧要 post 到主线程等 WebView 做完（秒级），
    ///    直接在 tokio worker 上等会把（只有两个的）worker 占死。
    /// 2. 这里包一层 `timeout`：Kotlin 自己的硬超时是主要手段，但万一它没回来
    ///    （WebView 卡住/进程状态不对），agent 不能陪着一起挂。
    /// 3. 超时只意味着**不再等**，那个阻塞线程还会跑完 —— 可以接受，因为
    ///    Kotlin 实现里有自己的动作超时，不会无限跑下去。
    pub async fn browser(&self, action_json: &str) -> anyhow::Result<serde_json::Value> {
        let host = self
            .state
            .browser()
            .ok_or_else(|| anyhow::anyhow!("浏览器不可用（壳没接上）"))?;
        let action = action_json.to_string();
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(75),
            tokio::task::spawn_blocking(move || host.execute(&action)),
        )
        .await;
        let raw = match joined {
            Ok(join) => join
                .map_err(|e| anyhow::anyhow!("浏览器调用没能跑起来: {e}"))?
                .map_err(|e| anyhow::anyhow!("{e}"))?,
            Err(_) => anyhow::bail!(
                "浏览器动作超时（75s 没回来）—— 页面可能卡住了；重试或者换个做法"
            ),
        };
        Ok(serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({ "text": raw })))
    }

    /// 联网搜索。key 有就直连 Exa API，没有就走他们那个免 key 的公开 MCP
    /// 端点（按 IP 限流，见 `search` 模块）。
    pub async fn search_web(&self, query: &str, num: usize) -> anyhow::Result<crate::search::Found> {
        let key = crate::search::read_key(&self.state);
        crate::search::run(query, num, key.as_deref()).await
    }

    /// 把要内联给模型的图片字节落盘，返回绝对路径。
    ///
    /// **为什么落盘**：带图的消息进了历史之后，后面每一轮都要重发同一张图
    /// （prompt cache 靠字节一致）。消息本身只存路径，组装请求时再从盘上读 ——
    /// 否则 SQLite 里会不断累积 base64 大字符串，而且每轮还要重编码一遍。
    pub async fn save_image(&self, bytes: Vec<u8>, mime: String) -> anyhow::Result<std::path::PathBuf> {
        let dir = self
            .state
            .data_dir
            .read()
            .ok()
            .and_then(|g| g.clone())
            .map(|d| d.join("images"))
            .unwrap_or_else(|| std::env::temp_dir().join("android-agent-images"));
        // 写盘是阻塞 I/O，几 MB 也会占住一个 worker（runtime 只有 2 个）
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir).map_err(|e| anyhow::anyhow!("建图片目录失败: {e}"))?;
            let ext = match mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                "image/bmp" => "bmp",
                _ => "bin",
            };
            let path = dir.join(format!("{}.{ext}", uuid::Uuid::now_v7()));
            std::fs::write(&path, &bytes).map_err(|e| anyhow::anyhow!("写图片失败: {e}"))?;
            Ok(path)
        })
        .await?
    }

    /// 附件（界面上传的图片 / PDF）的落盘目录。
    fn attachments_dir(&self) -> std::path::PathBuf {
        self.state
            .data_dir
            .read()
            .ok()
            .and_then(|g| g.clone())
            .map(|d| d.join("attachments"))
            .unwrap_or_else(|| std::env::temp_dir().join("android-agent-attachments"))
    }

    /// 按 id 取已落盘的附件绝对路径。
    ///
    /// `id` 是磁盘文件名，所以**必须**先确认它是个裸文件名 —— 否则一个
    /// `../../llm.json` 就能把 App 私有目录里的 key 读出去。
    pub fn attachment_path(&self, id: &str) -> Option<std::path::PathBuf> {
        if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") {
            return None;
        }
        let path = self.attachments_dir().join(id);
        path.is_file().then_some(path)
    }

    /// 处理一个上传的附件并落盘，返回可直接进会话历史的 `Attachment`。
    ///
    /// 图片走 `read` 读图同一条缩放流水线；PDF 原样存（只限大小）。
    /// 两个“不是文件”的底线：图片必须能被解码，PDF 必须真的以 `%PDF` 开头。
    pub async fn store_attachment(
        &self,
        name: String,
        mime: String,
        bytes: Vec<u8>,
    ) -> anyhow::Result<crate::agent::Attachment> {
        use crate::agent::{Attachment, AttachmentKind};

        const MAX_PDF_BYTES: usize = 32 * 1024 * 1024;
        let dir = self.attachments_dir();
        let id = uuid::Uuid::now_v7().to_string();

        // 图片：按魔数认，不看客户端报的 mime（那是可以伪造的）
        if let Some(img) = crate::imaging::load_if_image_bytes(&bytes)? {
            let ext = match img.mime.as_str() {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                "image/bmp" => "bmp",
                _ => "bin",
            };
            let id = format!("{id}.{ext}");
            write_attachment(dir.clone(), id.clone(), img.bytes).await?;
            return Ok(Attachment {
                path: dir.join(&id).display().to_string(),
                id,
                mime: img.mime,
                name,
                kind: AttachmentKind::Image,
            });
        }

        // PDF：认魔数，别只看 mime
        anyhow::ensure!(bytes.starts_with(b"%PDF-"), "只支持图片和 PDF");
        anyhow::ensure!(
            bytes.len() <= MAX_PDF_BYTES,
            "PDF 有 {}MB，超过 {}MB 上限",
            bytes.len() / 1024 / 1024,
            MAX_PDF_BYTES / 1024 / 1024
        );
        let _ = mime; // mime 统一由服务端判定，不信客户端
        let id = format!("{id}.pdf");
        write_attachment(dir.clone(), id.clone(), bytes).await?;
        Ok(Attachment {
            path: dir.join(&id).display().to_string(),
            id,
            mime: "application/pdf".into(),
            name,
            kind: AttachmentKind::Pdf,
        })
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

        // 会话在后台从库里读（不卡 App 启动）。必须在服务起来之前调：
        // 它先占住会话的写锁，读完之前进来的请求排队等，而不是看到空列表。
        self.state.load_sessions();

        // 沙箱里的 `agent` 命令要靠端口找到核心（见 linux.rs 的 AGENT_CLI）
        if let Some(sb) = self.state.linux() {
            sb.set_agent_port(actual);
            sb.set_agent_token(self.state.auth().token);
        }

        // 后台把常用工具装上（rg/fd/jq/python3…）。幂等：装过就秒退。
        tokio::spawn(provision_tools(self.state.clone()));
        // 后台补一次模型能力判定（models.dev）。老 llm.json 里没有 PDF 能力，
        // 界面靠它决定要不要显示「文件」入口；目录有磁盘缓存，大多不联网。
        tokio::spawn(refresh_capabilities(self.state.clone()));

        let app = http::router(self.state.clone());
        let st = self.state.clone();
        tokio::spawn(async move {
            st.log("info", format!("http://127.0.0.1:{actual}"));
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("http server exited: {e}");
            }
        });

        // 会话落盘的定时器。
        //
        // 会话改动频繁（一轮对话几十次），不能改一次写一次；但也不能不写
        // —— 进程被 kill 就全没了。所以攒 2 秒写一次，写之前看 dirty 标志。
        let st = self.state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if st.sessions_dirty.swap(false, Ordering::Relaxed) {
                    // SQLite 写盘是阻塞 I/O。runtime 只有 2 个 worker，直接在
                    // 异步任务里写会占住一个，期间 SSE/HTTP 都变慢。
                    let res = tokio::task::block_in_place(|| st.save_sessions());
                    if let Err(e) = res {
                        // 没写成（比如会话正被占用）就留到下一轮再写
                        st.mark_sessions_dirty();
                        tracing::debug!("会话落盘推迟: {e:#}");
                    }
                }
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

/// 启动后补一次视觉 / PDF 能力判定。
///
/// 不在 `health` 里同步做（那会阻塞页面加载最多 15 秒），也不在
/// `load_llm` 里做（那时可能还没有 tokio runtime）。失败只记日志，
/// 调用方（`supports_*_for`）会退回落盘值。
async fn refresh_capabilities(st: Arc<AppState>) {
    let Some(p) = st.llm().or_else(crate::llm::Provider::from_env) else {
        return;
    };
    st.supports_images_for(&p).await;
    st.supports_files_for(&p).await;
}

/// 把常用工具装进沙箱（后台、幂等、出错只记日志，不挡启动）。
///
/// Alpine minirootfs 里只有 busybox：没有 python、没有 curl、没有 jq，
/// `grep` 也不认 `-P`。装这一次，模型后面就不用每个回合自己 `apk add`。
///
/// 安卓上这件事有坑：`serve()` 比 rootfs 解压**早**（解压在 Kotlin 的后台线程里），
/// 所以这里先等沙箱就绪再动手 —— 空壳上跑 `apk` 只会失败。
async fn provision_tools(st: Arc<AppState>) {
    for attempt in 0..90 {
        let Some(sb) = st.linux() else {
            if attempt == 0 {
                tracing::debug!("没有 Linux 沙箱，跳过工具预装");
            }
            return;
        };
        if !sb.is_ready() {
            // 安卓首次启动：rootfs 还在解压。等 2 秒再看（最多 3 分钟）
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        if sb.is_provisioned() {
            tracing::debug!("常用工具已就绪，跳过");
            return;
        }

        st.log("info", "正在给沙箱装常用工具（rg/fd/jq/git/python3…，首次几十秒）");
        match sb.provision().await {
            Ok(r) if r.code == 0 => {
                let last = r
                    .stdout
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .to_string();
                st.log(
                    "info",
                    format!(
                        "常用工具装好了（{:.0}s）{last}",
                        r.duration_ms as f64 / 1000.0
                    ),
                );
            }
            Ok(r) => {
                let tail: String = r.stdout.chars().rev().take(300).collect::<String>().chars().rev().collect();
                st.log(
                    "warn",
                    format!("装常用工具失败（exit {}）：{tail}", r.code),
                );
            }
            Err(e) => st.log("warn", format!("装常用工具失败：{e:#}")),
        }
        return;
    }
    tracing::warn!("等沙箱就绪超时，常用工具没装（可手动跑 agent-setup）");
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
    fn mock_device_takes_commands_and_reports_garbage() {
        let core = Core::with_mock();
        let dev = core.state.device().unwrap();
        assert!(dev.execute(r#"{"type":"key","name":"back"}"#).unwrap().contains("back"));
        assert!(dev.execute("not json").is_err(), "非法指令必须报错");
    }
}
