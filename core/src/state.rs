//! 全局状态与边界数据模型。
//!
//! 设计铁律（见 docs/01-tech-selection.md §4）：
//! native 侧不反调 Java。Kotlin 把 UI 树拍平成扁平 JSON 送进来，
//! Rust 只回传指令。所以这里所有跨边界类型都是 JSON 可序列化的。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent::SessionStore;
use crate::db::Db;
use crate::linux::LinuxSandbox;
use crate::llm::Provider;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Rust → 设备侧：指令
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    TapXy { x: i32, y: i32 },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[serde(default = "default_swipe_ms")]
        ms: u32,
    },
    /// `back` / `home` / `recents` / `enter`
    Key { name: String },
    Launch { package: String },
}

fn default_swipe_ms() -> u32 {
    300
}

// ---------------------------------------------------------------------------
// 设备能力抽象（决策 D2 的落点）
// ---------------------------------------------------------------------------

/// 由 Kotlin 实现的能力接口。
///
/// **边界刻意用 `&str` JSON 而不是强类型**：这样这个 trait 可以直接
/// 用 `#[uniffi::export(callback_interface)]` 生成 Kotlin 接口，
/// 不需要为每个方法单独设计跨语言类型映射。
///
/// 实现方必须是非阻塞的（或自行转后台线程）—— Rust 侧会用
/// `spawn_blocking` 调用，但一次调用不应超过几百毫秒。
///
/// 现在模型**拿不到**设备工具，所以这条通路只有人在用（前端 / `/api/command`、
/// agentctl 的 `/dev`）。读界面那套（`Command::Snapshot` + `UiSnapshot`）跟着
/// 快照一起删了，以后作为沙箱里的一个命令回来。
pub trait DeviceApi: Send + Sync + 'static {
    /// 执行一条指令（`Command` 的 JSON），返回结果 JSON 字符串。
    fn execute(&self, cmd_json: &str) -> Result<String, String>;

    /// 设备/实现名，仅用于展示
    fn name(&self) -> String {
        "unknown".into()
    }
}

// ---------------------------------------------------------------------------
// Rust → 前端：事件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// 连接建立时立即推送
    Hello {
        port: u16,
        version: String,
        platform: String,
        mock: bool,
    },
    /// 设备连接状态变化
    Device {
        connected: bool,
        name: String,
        mock: bool,
    },
    Log {
        level: String,
        msg: String,
    },
    /// agent 事件。`chunk` 是 AI SDK UI Message Stream 的 chunk，原样透传
    /// （见 sse.rs）。前端不需要区分来源，统一走同一个 reducer。
    Agent { chunk: serde_json::Value },
    /// 指令执行回执
    Ack {
        id: String,
        ok: bool,
        detail: String,
    },
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// 落盘用的结构。和 `Provider` 分开是因为 `Provider::api_key` 标了
/// `skip_serializing`（防止通过任何序列化路径泄给前端）。
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedProvider {
    name: String,
    base_url: String,
    model: String,
    api_key: String,
    /// 老 llm.json 里没有这个字段 → 默认 true（模型能看图）
    #[serde(default = "crate::llm::default_supports_images")]
    supports_images: bool,
    /// 老 llm.json 里没有这个字段 → 默认 false（不知道就不显示文件入口）
    #[serde(default)]
    supports_files: bool,
    /// 协议偏好。老 llm.json 里没有 → Auto（优先 Responses）
    #[serde(default)]
    api_style: crate::llm::ApiStyle,
    /// 思考强度。老 llm.json 里没有 → 默认档（不发）
    #[serde(default)]
    reasoning_effort: Option<String>,
}

pub struct AppState {
    pub tx: broadcast::Sender<Event>,
    pub device: RwLock<Option<Arc<dyn DeviceApi>>>,
    pub port: AtomicU16,
    pub mock: AtomicBool,
    /// 会话历史。用 tokio 的 RwLock，因为 agent loop 会跨 await 持有。
    /// 多会话。侧边栏的「最近对话」就是它。
    pub chat: tokio::sync::RwLock<SessionStore>,
    /// Linux 沙箱。安卓上由 Kotlin 在 boot 时把 proot / rootfs 路径交进来；
    /// 桌面上由环境变量自动探测。
    pub linux: RwLock<Option<Arc<LinuxSandbox>>>,
    /// LLM provider。
    ///
    /// 安卓上**没法用环境变量**（App 不继承 shell 环境），所以运行时
    /// 通过 `POST /api/llm` 注入，并持久化到 `<filesDir>/llm.json`。
    /// 桌面上启动时从环境变量播种。
    pub llm: RwLock<Option<Provider>>,
    /// 持久化目录（安卓上是 `<filesDir>`；桌面上为空则只存内存）
    pub data_dir: RwLock<Option<std::path::PathBuf>>,
    /// 会话库。`set_data_dir` 时打开。
    pub db: RwLock<Option<Arc<Db>>>,
    /// 有回合正在跑。前端切回来时用它判断「还在进行中」还是「卡住了」。
    ///
    /// 注意：agent 循环**不依赖** SSE 客户端。客户端断开时 `tx.send`
    /// 失败但被忽略，循环照跑、工具照执行、LLM 照调 —— 所以 App 切到
    /// 后台 agent 是在继续干活的（前台服务保着进程）。
    pub turn_active: AtomicBool,
    /// 会话有改动，等着落盘。定时器每 2s 检查一次。
    ///
    /// 不用「改一次写一次」：一轮对话会产生几十次变更，每次都写盘太浪费。
    /// 也不能不写：进程被 kill 会话就没了（实测 force-stop 之后抽屉空了）。
    pub sessions_dirty: AtomicBool,
    /// 运行时覆盖的系统提示词（`POST /api/debug/system-prompt`）。
    ///
    /// `None` = 用编译进二进制的 `SYSTEM_PROMPT`。存在的理由就一个：
    /// **不重装 APK 就能对着真机调提示词** —— 改 Rust 常量要重编 .so、
    /// 重新打包安装，而提示词是迭代最频繁的东西。只改内存，重启即失效。
    pub system_prompt: RwLock<Option<String>>,
    /// 视觉能力的自动判定结果：`(base_url|model, 能不能看图)`。
    ///
    /// provider 换了（key 变了）就重算。判定来源见 `models` 模块。
    pub vision: RwLock<Option<(String, bool)>>,
    /// PDF / 文档输入能力的自动判定结果，和 `vision` 分开存。
    pub files: RwLock<Option<(String, bool)>>,
    /// 用户按了停止。回合循环每轮开始前 / 每个工具执行前都会看它。
    ///
    /// agent loop **没有轮数上限**，所以必须有一个真的能停下来、而不只是
    /// 断开前端连接的开关 —— 否则模型空转时只能在后台烧 token。
    pub abort_turn: AtomicBool,
    /// 配合 `abort_turn`：按停止时叫醒正在等的人（流式接收、跑着的 bash）。
    /// 只有标志位的话，停止要等到「这一步流完 / 这条命令跑完」才生效 ——
    /// 而 bash 的超时可以是 24 小时。
    pub abort_notify: tokio::sync::Notify,
    /// 已上传但还没随消息发出去的附件：`id → Attachment`。
    ///
    /// 上传和发送是两个请求，中间隔着一个用户可能改主意的间隔，所以需
    /// 要把“已经把字节存到磁盘上”这件事先记下来。发出去时按 id 取走。
    /// 附件一旦进了会话历史，`ChatMessage` 里就带着完整的 `Attachment`，
    /// 不靠这个 map 了。
    pub pending_attachments: Mutex<HashMap<String, crate::agent::Attachment>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(Self {
            tx,
            device: RwLock::new(None),
            port: AtomicU16::new(0),
            mock: AtomicBool::new(false),
            chat: tokio::sync::RwLock::new(SessionStore::default()),
            linux: RwLock::new(None),
            llm: RwLock::new(None),
            data_dir: RwLock::new(None),
            turn_active: AtomicBool::new(false),
            db: RwLock::new(None),
            sessions_dirty: AtomicBool::new(false),
            system_prompt: RwLock::new(None),
            vision: RwLock::new(None),
            files: RwLock::new(None),
            pending_attachments: Mutex::new(HashMap::new()),
            abort_turn: AtomicBool::new(false),
            abort_notify: tokio::sync::Notify::new(),
        })
    }

    /// 广播事件。没有订阅者时静默丢弃（这是正常状态，不是错误）。
    pub fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    pub fn log(&self, level: &str, msg: impl Into<String>) {
        let msg = msg.into();
        match level {
            "error" => tracing::error!("{msg}"),
            "warn" => tracing::warn!("{msg}"),
            "debug" => tracing::debug!("{msg}"),
            _ => tracing::info!("{msg}"),
        }
        self.emit(Event::Log {
            level: level.into(),
            msg,
        });
    }

    pub fn device(&self) -> Option<Arc<dyn DeviceApi>> {
        self.device.read().ok().and_then(|g| g.clone())
    }

    pub fn clear_device(&self) {
        if let Ok(mut g) = self.device.write() {
            *g = None;
        }
        self.emit(Event::Device {
            connected: false,
            name: String::new(),
            mock: false,
        });
    }

    pub fn set_device(&self, dev: Arc<dyn DeviceApi>) {
        let name = dev.name();
        let mock = self.mock.load(std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.device.write() {
            *g = Some(dev);
        }
        self.emit(Event::Device {
            connected: true,
            name,
            mock,
        });
    }

    pub fn linux(&self) -> Option<Arc<LinuxSandbox>> {
        self.linux.read().ok().and_then(|g| g.clone())
    }

    pub fn db(&self) -> Option<Arc<Db>> {
        self.db.read().ok().and_then(|g| g.clone())
    }

    /// 打开会话库并恢复会话。启动时调一次。
    pub fn open_db(&self, dir: &std::path::Path) {
        let path = dir.join("sessions.db");
        match Db::open(&path) {
            Ok(db) => {
                let db = Arc::new(db);
                match db.load() {
                    Ok(store) => {
                        let n = store.sessions.len();
                        // blocking_write 在异步上下文里会 panic。启动时是从
                        // Kotlin 主线程调进来的，没有 runtime —— 但还是挡一下。
                        if tokio::runtime::Handle::try_current().is_ok() {
                            tracing::warn!("在异步上下文里恢复会话，跳过");
                        } else {
                            *self.chat.blocking_write() = store;
                            if n > 0 {
                                self.log("info", format!("从 sessions.db 恢复 {n} 个会话"));
                            }
                        }
                    }
                    Err(e) => self.log("warn", format!("读取会话失败: {e:#}")),
                }
                if let Ok(mut g) = self.db.write() {
                    *g = Some(db);
                }
            }
            Err(e) => self.log("error", format!("打开 {} 失败: {e:#}", path.display())),
        }
    }

    pub fn mark_sessions_dirty(&self) {
        self.sessions_dirty.store(true, Ordering::Relaxed);
    }

    /// 当前生效的系统提示词。
    ///
    /// 默认是编译进去的 `SYSTEM_PROMPT`；被 `/api/debug/system-prompt` 覆盖过
    /// 就返回那一份。`Cow` 是为了没覆盖时**不拷贝**那个常量。
    pub fn system_prompt(&self) -> Cow<'static, str> {
        match self.system_prompt.read() {
            Ok(g) => match &*g {
                Some(s) => Cow::Owned(s.clone()),
                None => Cow::Borrowed(crate::agent::SYSTEM_PROMPT),
            },
            Err(_) => Cow::Borrowed(crate::agent::SYSTEM_PROMPT),
        }
    }

    pub fn system_prompt_overridden(&self) -> bool {
        self.system_prompt
            .read()
            .map(|g| g.is_some())
            .unwrap_or(false)
    }

    /// `None` = 恢复成编译进去的那版。
    pub fn set_system_prompt(&self, p: Option<String>) {
        if let Ok(mut g) = self.system_prompt.write() {
            *g = p;
        }
    }

    /// 会话落盘。定时器每 2s 调一次，只追加新增的消息。
    pub fn save_sessions(&self) -> anyhow::Result<()> {
        let Some(db) = self.db() else {
            return Ok(());
        };
        let store = self
            .chat
            .try_read()
            .map_err(|_| anyhow::anyhow!("会话被占用，跳过本次落盘"))?;
        db.sync(&store)
    }

    pub fn llm(&self) -> Option<Provider> {
        self.llm.read().ok().and_then(|g| g.clone())
    }

    /// 给所有会话换一个 provider session id。
    ///
    /// 换 provider / model 之后，旧会话的缓存前缀对不上新模型了：
    /// 继续用同一个 id，provider 侧要么白跑一次冷启动，要么命中不该命中的
    /// 缓存条目。历史被清空（`ChatSession::clear`）时会话自己会换。
    pub async fn rotate_provider_sessions(&self) {
        let mut store = self.chat.write().await;
        for s in &mut store.sessions {
            s.rotate_provider_session();
        }
        drop(store);
        self.mark_sessions_dirty();
    }

    /// 设置 provider 并落盘。
    ///
    /// ⚠️ 目前是**明文**存在 App 私有目录里（受 Android 沙箱 + SELinux 保护）。
    /// 下一步应该用 Android Keystore 加密 —— 见 docs/04-android-shell.md 的债。
    pub fn set_llm(&self, p: Provider, persist_dir: Option<&std::path::Path>) {
        self.persist_provider(&p, persist_dir);
        let name = p.name.clone();
        let model = p.model.clone();
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        self.log("info", format!("LLM 已配置: {name} / {model}"));
    }

    /// 只更新「能不能看图」并落盘（自动判定后调用），不重发配置日志。
    pub fn set_supports_images(&self, vision: bool, persist_dir: Option<&std::path::Path>) {
        let Some(mut p) = self.llm() else { return };
        if p.supports_images == vision {
            return;
        }
        p.supports_images = vision;
        self.persist_provider(&p, persist_dir);
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        tracing::debug!("视觉能力已记录: {vision}");
    }

    /// 只更新「能不能吃 PDF」并落盘（自动判定后调用），不重发配置日志。
    pub fn set_supports_files(&self, files: bool, persist_dir: Option<&std::path::Path>) {
        let Some(mut p) = self.llm() else { return };
        if p.supports_files == files {
            return;
        }
        p.supports_files = files;
        self.persist_provider(&p, persist_dir);
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        tracing::debug!("文档输入能力已记录: {files}");
    }

    fn persist_provider(&self, p: &Provider, persist_dir: Option<&std::path::Path>) {
        let Some(dir) = persist_dir else { return };
        let f = dir.join("llm.json");
        // 用独立的持久化结构，**不能**直接序列化 Provider ——
        // 它的 api_key 是 skip_serializing（防止泄给前端），
        // 直接存的话 key 就丢了。
        let rec = PersistedProvider {
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            model: p.model.clone(),
            api_key: p.api_key.clone(),
            supports_images: p.supports_images,
            supports_files: p.supports_files,
            api_style: p.api_style,
            reasoning_effort: p.reasoning_effort.clone(),
        };
        match serde_json::to_vec_pretty(&rec) {
            Ok(body) => {
                // 先写临时文件再 rename：直接覆盖时进程被杀，llm.json 就是半截，
                // 下次启动解析失败 —— API key 跟着没了。
                let tmp = dir.join("llm.json.tmp");
                if let Err(e) = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &f)) {
                    tracing::warn!("写 {} 失败: {e}", f.display());
                }
            }
            Err(e) => tracing::warn!("序列化 provider 失败: {e}"),
        }
    }

    /// 当前 provider 能不能看图：
    /// 缓存 → models.dev 自动判定 → 拿不准就保留原值。
    ///
    /// 判定结果会落盘，下次启动直接读，不用再联网。
    pub async fn supports_images_for(&self, provider: &Provider) -> bool {
        let key = vision_key(provider);
        if let Some((k, v)) = self.vision.read().ok().and_then(|g| g.clone()) {
            if k == key {
                return v;
            }
        }
        let dir = self.data_dir.read().ok().and_then(|g| g.clone());
        let resolved = crate::models::vision_for(provider, &provider.model, false, dir.as_deref()).await;
        let vision = resolved.unwrap_or(provider.supports_images);
        match resolved {
            Some(v) => tracing::info!("视觉能力自动判定: {} → {v}", provider.model),
            None => tracing::info!(
                "视觉能力查不到 {}（models.dev 没这个模型），沿用 {vision}",
                provider.model
            ),
        }
        if let Ok(mut g) = self.vision.write() {
            *g = Some((key.clone(), vision));
        }
        // 判定要联网（最多十几秒），这期间用户可能已经换了模型：只在当前
        // provider 还是**这一个**时才落盘，否则会把旧模型的能力写到新模型头上。
        if resolved.is_some() && self.llm().is_some_and(|cur| vision_key(&cur) == key) {
            self.set_supports_images(vision, dir.as_deref());
        }
        vision
    }

    /// 当前 provider 能不能吃 PDF / 文档输入：
    /// 缓存 → models.dev 自动判定 → 拿不准就保留原值（默认 false）。
    ///
    /// 和视觉能力同一个套路，但 PDF 默认关：没法确定时**不显示**文件入口，
    /// 总好过发一个保证 400 的请求。
    pub async fn supports_files_for(&self, provider: &Provider) -> bool {
        let key = files_key(provider);
        if let Some((k, v)) = self.files.read().ok().and_then(|g| g.clone()) {
            if k == key {
                return v;
            }
        }
        let dir = self.data_dir.read().ok().and_then(|g| g.clone());
        let resolved = crate::models::pdf_for(provider, &provider.model, false, dir.as_deref()).await;
        let files = resolved.unwrap_or(provider.supports_files);
        match resolved {
            Some(v) => tracing::info!("文档输入能力自动判定: {} → {v}", provider.model),
            None => tracing::info!(
                "文档输入能力查不到 {}（models.dev 没这个模型），沿用 {files}",
                provider.model
            ),
        }
        if let Ok(mut g) = self.files.write() {
            *g = Some((key.clone(), files));
        }
        if resolved.is_some() && self.llm().is_some_and(|cur| files_key(&cur) == key) {
            self.set_supports_files(files, dir.as_deref());
        }
        files
    }

    /// 用户请求停止当前回合（`POST /api/stop`）。
    pub fn request_abort(&self) {
        self.abort_turn
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.abort_notify.notify_waiters();
    }

    /// 等到用户按停止。已经按过就立刻返回。
    ///
    /// 先登记再看标志位：`notify_waiters` 不留许可，先看标志再登记的话，
    /// 两步之间按下的停止就丢了。
    pub async fn aborted(&self) {
        loop {
            let n = self.abort_notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.abort_requested() {
                return;
            }
            n.await;
        }
    }

    pub fn abort_requested(&self) -> bool {
        self.abort_turn
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 只看缓存/已存的值（同步，给组装请求和工具用，不做网络）。
    pub fn cached_supports_images(&self, provider: &Provider) -> bool {
        let key = vision_key(provider);
        if let Some((k, v)) = self.vision.read().ok().and_then(|g| g.clone()) {
            if k == key {
                return v;
            }
        }
        provider.supports_images
    }

    /// 同 `cached_supports_images`，但看的是 PDF / 文档输入。
    pub fn cached_supports_files(&self, provider: &Provider) -> bool {
        let key = files_key(provider);
        if let Some((k, v)) = self.files.read().ok().and_then(|g| g.clone()) {
            if k == key {
                return v;
            }
        }
        provider.supports_files
    }

    /// provider 的模型清单（带 vision 判定），给设置界面 / CLI 用。
    pub async fn list_models(
        &self,
        refresh: bool,
    ) -> anyhow::Result<Vec<crate::models::ModelInfo>> {
        let provider = self
            .llm()
            .or_else(Provider::from_env)
            .ok_or_else(|| anyhow::anyhow!("还没有配置 LLM"))?;
        let dir = self.data_dir.read().ok().and_then(|g| g.clone());
        crate::models::list(&provider, refresh, dir.as_deref()).await
    }

    /// 启动时从磁盘恢复
    pub fn load_llm(&self, dir: &std::path::Path) {
        let f = dir.join("llm.json");
        let Ok(body) = std::fs::read(&f) else { return };
        match serde_json::from_slice::<PersistedProvider>(&body) {
            Ok(rec) => {
                let name = rec.name.clone();
                let model = rec.model.clone();
                let mut p = Provider::compatible(rec.name, rec.base_url, rec.model, rec.api_key);
                p.supports_images = rec.supports_images;
                p.supports_files = rec.supports_files;
                p.api_style = rec.api_style;
                p.reasoning_effort = rec.reasoning_effort;
                // 恢复出来的值先当已知（不用等下一次请求再联网）
                if let Ok(mut g) = self.vision.write() {
                    *g = Some((vision_key(&p), rec.supports_images));
                }
                // 刻意**不**给 `files` 填缓存：老 llm.json 里没有这个字段，
                // 填了就会把「从未判定过」当成「已判定为 false」，界面就一直
                // 不显示文件入口。留给启动后的 `refresh_capabilities` 去查
                // models.dev（磁盘有目录缓存，大多不联网）。
                if let Ok(mut g) = self.llm.write() {
                    *g = Some(p);
                }
                self.log("info", format!("从 llm.json 恢复 LLM: {name} / {model}"));
            }
            Err(e) => self.log("warn", format!("llm.json 解析失败: {e}")),
        }
    }

    pub fn set_linux(&self, sb: Arc<LinuxSandbox>) {
        let rootfs = sb.rootfs().display().to_string();
        if let Ok(mut g) = self.linux.write() {
            *g = Some(sb);
        }
        self.log("info", format!("Linux 沙箱就绪: {rootfs}"));
    }
}

/// 视觉判定的缓存键：base_url + 模型名。任一变了就重算。
fn vision_key(p: &Provider) -> String {
    format!("{}|{}", p.base_url.trim_end_matches('/'), p.model)
}

/// 文档输入判定的缓存键，和视觉那套分开存。
fn files_key(p: &Provider) -> String {
    format!("{}|{}", p.base_url.trim_end_matches('/'), p.model)
}
