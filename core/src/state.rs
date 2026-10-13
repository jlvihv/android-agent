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
    TapXy {
        x: i32,
        y: i32,
    },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[serde(default = "default_swipe_ms")]
        ms: u32,
    },
    /// `back` / `home` / `recents` / `enter`
    Key {
        name: String,
    },
    Launch {
        package: String,
    },
    /// 打开一个 URI：http(s)、App 深链（`weixin://`、`androidamap://`）、
    /// `intent://…` 都算。
    ///
    /// 沙箱里没有浏览器，但手机上有 —— 沙箱里的 `xdg-open` / `$BROWSER`
    /// （也就是 `agent-open`）最后就走到这里。跟 `Launch` 一样，**不设
    /// scheme 白名单**：这是 agent 自己的手机，它想开什么就开什么；
    /// 真打不开（没装那个 App、系统没 handler）由系统报错回来。
    Open {
        url: String,
    },
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

/// 平回开始/结束的通知方（由 Kotlin 实现）。
///
/// 只为一件事：**回合进行中把前台服务挂上**，让进程别被系统回收。
/// 以前是常驻服务 + 常驻通知，那个通知没有任何信息量；现在回合开始起、
/// 结束停，通知只在真有东西在跑的时候出现。
///
/// 为什么需要它：切到后台的回合一旦进程被杀，浏览器那只 WebView（页面、
/// 登录态）就没了，正在跑的 bash 也死。会话历史能告诉你“做到哪一步”。
///
/// 实现方**不能阻塞** —— 这是从 agent 的 tokio 线程上直接调的。
pub trait TurnApi: Send + Sync + 'static {
    /// `true` = 有回合在跑（可能不止一个会话）。实现应当幂等。
    fn turn(&self, active: bool);
}

// ---------------------------------------------------------------------------
// 浏览器能力（同样是决策 D2 的落点，但刻意和 DeviceApi 分开）
// ---------------------------------------------------------------------------

/// 由 Kotlin 实现：一个真 WebView 上的浏览器动作。
///
/// **为什么不挂在 `DeviceApi` 上**：浏览器不需要无障碍服务 —— 关掉无障碍
/// 也该能用，而设备那条路是 `onDeviceReady` 才注进来的（见 `AgentApp`）。
/// 两件事的可用条件不同，就不该共用一个开关。
///
/// 接口形状照抄 `DeviceApi`（JSON 进、JSON 出），加动作不用动 FFI 定义。
///
/// 与 `DeviceApi` 的两点不同：
///
/// - 实现方**会阻塞**（WebView 只能在主线程碰，Kotlin 侧要等它），所以
///   调用方必须走 `spawn_blocking`，见 `Core::browser`。
/// - 一次调用是**秒级**的（navigate 要等页面），不是几百毫秒。
pub trait BrowserApi: Send + Sync + 'static {
    /// 执行一个动作（`BrowserAction` 的 JSON），返回结果 JSON。
    ///
    /// 约定的返回形状（Kotlin 侧负责填）：
    /// `{"ok":bool, "text":…, "url":…, "title":…, "image":"<沙箱路径>", "error":…}`
    fn execute(&self, action_json: &str) -> Result<String, String>;

    /// 一句话说明自己是哪个后端（`/api/health`、体检用）。
    ///
    /// 手机上就是壳里那只 WebView，没什么可说的；桌面这边要说清连的是哪个
    /// Chrome 的调试端口 —— 连不上时那是唯一能查的线索。
    fn describe(&self) -> String {
        "WebView（壳）".to_string()
    }
}

// ---------------------------------------------------------------------------

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
    /// 浏览器实现。和 `device` 分开：它的可用条件是无障碍无关的
    /// （见 `BrowserApi` 的说明）。
    pub browser: RwLock<Option<Arc<dyn BrowserApi>>>,
    /// 回合开始/结束的接收方（前台服务用）。见 `TurnApi`。
    pub turn: RwLock<Option<Arc<dyn TurnApi>>>,
    pub port: AtomicU16,
    pub mock: AtomicBool,
    /// 会话历史。用 tokio 的 RwLock，因为 agent loop 会跨 await 持有。
    /// 多会话。侧边栏的「最近对话」就是它。
    ///
    /// 包一层 Arc 是为了启动时能拿一个 owned 写锁交给后台任务：会话从库里
    /// 读出来之前，所有要读会话的请求都排队等它（见 `load_sessions`）。
    pub chat: Arc<tokio::sync::RwLock<SessionStore>>,
    /// Linux 沙箱。安卓上由 Kotlin 在 boot 时把 proot / rootfs 路径交进来；
    /// 桌面上由环境变量自动探测。
    pub linux: RwLock<Option<Arc<LinuxSandbox>>>,
    /// LLM provider。
    ///
    /// 安卓上**没法用环境变量**（App 不继承 shell 环境），所以运行时
    /// 通过 `POST /api/llm` 注入，存在会话库的 `meta` 表里（`llm` 这一行）。
    /// 桌面上启动时从环境变量播种。
    pub llm: RwLock<Option<Provider>>,
    /// 每家服务商存下来的 Key（`meta` 表的 `providers`）。见 `providers` 模块。
    ///
    /// `llm` 是「当前在用的那一个」（含模型），这里是「配好了的全部」：
    /// 选择模型面板把这几家的模型都列出来，点哪个就切到哪家。
    pub providers: RwLock<crate::providers::Store>,
    /// 各家的模型清单：服务商 id → (拉的时间, 清单)。
    ///
    /// 打开选择模型面板要拉好几家的 `/models`，每次都等网络太慢；
    /// 清单十分钟内不会变，换 Key 时清掉。
    pub model_lists: Mutex<HashMap<String, (std::time::Instant, Vec<crate::models::ModelInfo>)>>,
    /// 持久化目录（安卓上是 `<filesDir>`；桌面上为空则只存内存）
    pub data_dir: RwLock<Option<std::path::PathBuf>>,
    /// 沙箱的 Alpine 升级（设置页里点的那个，见 `alpine`）
    pub alpine: crate::alpine::Upgrader,
    /// 局域网配对码（只有桌面宿主模式对局域网开放时才有，见 `desktop::Pairing`）
    pub pairing: RwLock<Option<Arc<crate::desktop::Pairing>>>,
    /// 本机（loopback）来的请求不要凭据。**只有桌面**会打开（`agent-dev` 的 main）：
    /// 对局域网开放时凭据是给手机的，电脑自己用浏览器照旧免登录。手机上永远是
    /// false —— 那里别的 App 也能连 127.0.0.1。
    pub trust_loopback: AtomicBool,
    /// 会话库。`set_data_dir` 时打开。
    pub db: RwLock<Option<Arc<Db>>>,
    /// 会话已经从库里读进内存了。
    ///
    /// 读之前（或者读失败了）**绝不能落盘**：`Db::sync` 会把内存里没有的
    /// 会话从库里删掉 —— 拿一个空的 store 去同步，等于清空全部历史。
    pub sessions_loaded: AtomicBool,
    /// 正在跑的回合：会话 id → 这个回合的停止开关。
    ///
    /// 按会话分开：可以同时在几个会话里各跑一个回合，停止也只停那一个。
    /// 同一个会话同时只能有一个（两个回合往同一段历史里交错追加就乱了）。
    ///
    /// 注意：agent 循环**不依赖** SSE 客户端。客户端断开时 `tx.send`
    /// 失败但被忽略，循环照跑、工具照执行、LLM 照调 —— 所以 App 切到
    /// 后台、或者切到别的会话，agent 都在继续干活（前台服务保着进程）。
    pub turns: Mutex<HashMap<String, Arc<Turn>>>,
    /// 本地 API 的访问凭据。见 `Auth`。
    pub auth: RwLock<Auth>,
    /// 会话有改动，等着落盘。定时器每 2s 检查一次。
    ///
    /// 不用「改一次写一次」：一轮对话会产生几十次变更，每次都写盘太浪费。
    /// 也不能不写：进程被 kill 会话就没了（实测 force-stop 之后抽屉空了）。
    pub sessions_dirty: AtomicBool,
    /// 运行时覆盖的系统提示词（`POST /api/debug/system-prompt`）。
    ///
    /// `None` = 用编译进二进制的那份（`agent::system_prompt`）。存在的理由就一个：
    /// **不重装 APK 就能对着真机调提示词** —— 改 Rust 常量要重编 .so、
    /// 重新打包安装，而提示词是迭代最频繁的东西。只改内存，重启即失效。
    pub system_prompt: RwLock<Option<String>>,
    /// 视觉能力的自动判定结果：`(base_url|model, 能不能看图)`。
    ///
    /// provider 换了（key 变了）就重算。判定来源见 `models` 模块。
    pub vision: RwLock<Option<(String, bool)>>,
    /// PDF / 文档输入能力的自动判定结果，和 `vision` 分开存。
    pub files: RwLock<Option<(String, bool)>>,
    /// 已上传但还没随消息发出去的附件：`id → Attachment`。
    ///
    /// 上传和发送是两个请求，中间隔着一个用户可能改主意的间隔，所以需
    /// 要把“已经把字节存到磁盘上”这件事先记下来。发出去时按 id 取走。
    /// 附件一旦进了会话历史，`ChatMessage` 里就带着完整的 `Attachment`，
    /// 不靠这个 map 了。
    /// 值里带上传时刻：放太久没发的会被清掉（`add_pending_attachment`）。
    pub pending_attachments: Mutex<HashMap<String, (std::time::Instant, crate::agent::Attachment)>>,
    /// 每个会话的 token 用量（输入框上那个上下文圆环）。**只在内存里**。
    ///
    /// 不落盘：它是「现在这个上下文用了多少」的仪表盘，重启后从下一次
    /// LLM 调用重新数就行 —— 为它去动会话的存储格式不值当。
    pub usage: Mutex<HashMap<String, UsageStats>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(Self {
            tx,
            device: RwLock::new(None),
            browser: RwLock::new(None),
            turn: RwLock::new(None),
            port: AtomicU16::new(0),
            mock: AtomicBool::new(false),
            chat: Arc::new(tokio::sync::RwLock::new(SessionStore::default())),
            linux: RwLock::new(None),
            llm: RwLock::new(None),
            providers: RwLock::new(Default::default()),
            model_lists: Mutex::new(HashMap::new()),
            data_dir: RwLock::new(None),
            alpine: Default::default(),
            pairing: RwLock::new(None),
            trust_loopback: AtomicBool::new(false),
            sessions_loaded: AtomicBool::new(false),
            turns: Mutex::new(HashMap::new()),
            auth: RwLock::new(Auth {
                token: None,
                sandbox_key: random_secret(),
            }),
            db: RwLock::new(None),
            sessions_dirty: AtomicBool::new(false),
            system_prompt: RwLock::new(None),
            vision: RwLock::new(None),
            files: RwLock::new(None),
            pending_attachments: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
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

    pub fn browser(&self) -> Option<Arc<dyn BrowserApi>> {
        self.browser.read().ok().and_then(|g| g.clone())
    }

    pub fn set_browser(&self, b: Arc<dyn BrowserApi>) {
        if let Ok(mut g) = self.browser.write() {
            *g = Some(b);
        }
    }

    pub fn set_turn_host(&self, t: Arc<dyn TurnApi>) {
        if let Ok(mut g) = self.turn.write() {
            *g = Some(t);
        }
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

    /// 打开会话库，读出 provider 配置和访问凭据。启动时调一次（主线程上）。
    ///
    /// **会话本身不在这里读**：历史一多（每条 bash 输出几十 KB），全读出来
    /// 要几百毫秒，而这是在 `Application.onCreate` 里 —— 卡的是 App 启动。
    /// 会话挪到 `load_sessions`，在后台读。
    pub fn open_db(&self, dir: &std::path::Path) {
        let path = dir.join("sessions.db");
        match Db::open(&path) {
            Ok(db) => {
                let db = Arc::new(db);
                self.load_llm(&db, dir);
                self.load_providers(&db);
                self.load_auth(&db);
                if let Ok(Some(raw)) = db.get_meta(RESPONSES_UNSUPPORTED) {
                    let keys: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
                    crate::llm::load_responses_unsupported(keys);
                }
                if let Ok(mut g) = self.db.write() {
                    *g = Some(db);
                }
            }
            Err(e) => self.log("error", format!("打开 {} 失败: {e:#}", path.display())),
        }
    }

    /// 在后台把会话从库里读进内存。`serve()` 起服务**之前**调。
    ///
    /// 同步拿下会话的写锁再交给后台任务：服务起来之后、读完之前进来的请求
    /// 都会在锁上排队，而不是看到一个空的会话列表。
    pub fn load_sessions(self: &Arc<Self>) {
        let Some(db) = self.db() else {
            // 没有库（桌面开发）：没什么要读的，内存里就是全部
            self.sessions_loaded.store(true, Ordering::Relaxed);
            return;
        };
        let Ok(mut guard) = self.chat.clone().try_write_owned() else {
            tracing::warn!("会话被占用，没法在后台加载");
            return;
        };
        let st = self.clone();
        // 清扫只动这个时刻之前写的文件（见 `gc::sweep`）
        let started = std::time::SystemTime::now();
        tokio::spawn(async move {
            let db2 = db.clone();
            match tokio::task::spawn_blocking(move || db2.load()).await {
                Ok(Ok(store)) => {
                    let n = store.sessions.len();
                    *guard = store;
                    drop(guard);
                    st.sessions_loaded.store(true, Ordering::Relaxed);
                    if n > 0 {
                        st.log("info", format!("从 sessions.db 恢复 {n} 个会话"));
                    }
                    st.sweep_files(db, started).await;
                }
                Ok(Err(e)) => st.log(
                    "error",
                    format!("读取会话失败，本次运行不落盘（免得覆盖掉库里的历史）: {e:#}"),
                ),
                Err(e) => st.log("error", format!("读取会话的任务挂了: {e}")),
            }
        });
    }

    /// 启动清扫：删掉没有任何会话引用的图片副本 / 附件 / 截图 / 溢出输出。
    async fn sweep_files(&self, db: Arc<Db>, before: std::time::SystemTime) {
        let data = self.data_dir.read().ok().and_then(|g| g.clone());
        let dirs = self.side_dirs();
        let res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
            let msgs = db.messages_with_files()?;
            let referenced = crate::gc::owned_files(&msgs, &dirs).into_iter().collect();
            Ok(crate::gc::sweep(
                data.as_deref(),
                &dirs,
                &referenced,
                before,
            ))
        })
        .await;
        match res {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("清掉 {n} 个没人引用的文件"),
            Ok(Err(e)) => tracing::warn!("没法确定哪些文件还在用，这次不清扫: {e:#}"),
            Err(e) => tracing::warn!("清扫任务挂了: {e}"),
        }
    }

    // —— 会话的消息：按需读进来、用完放掉 ——

    /// 保证这个会话的消息在内存里（不在就从库里读回来）。
    ///
    /// 没有这个会话不算错（调用方自己报）；库里那份读不出来才报错 —— 这时
    /// 会话保持「不在内存里」，库里的行一条都不会被动到。
    pub async fn ensure_loaded(&self, id: &str) -> anyhow::Result<()> {
        {
            let store = self.chat.read().await;
            match store.get(id) {
                Some(s) if !s.resident() => {}
                _ => return Ok(()),
            }
        }
        let db = self
            .db()
            .ok_or_else(|| anyhow::anyhow!("会话 {id} 的消息不在内存里，又没有会话库"))?;
        let sid = id.to_string();
        let msgs = tokio::task::spawn_blocking(move || db.load_messages(&sid))
            .await?
            .map_err(|e| anyhow::anyhow!("读会话 {id} 失败: {e:#}"))?;
        // 读的这一会儿会话只可能改了会话头（消息要改得先读进来），装回去就行；
        // 期间被清空过的话它已经在内存里了，`install` 不会动它
        if let Some(s) = self.chat.write().await.get_mut(id) {
            s.install(msgs);
        }
        Ok(())
    }

    /// 把用不着的会话的消息放掉（落盘定时器每次写完盘调一次）。
    ///
    /// 只放这样的：不是当前会话、没有回合在跑、刚读进来超过一分钟、而且
    /// **内存里的样子已经全部落盘**（放掉的东西必须能读回来）。
    pub fn evict_idle(&self) -> usize {
        const KEEP_FOR: std::time::Duration = std::time::Duration::from_secs(60);
        let Some(db) = self.db() else { return 0 };
        if !self.sessions_loaded.load(Ordering::Relaxed) {
            return 0;
        }
        let busy = self.busy_sessions();
        let Ok(mut store) = self.chat.try_write() else {
            return 0;
        };
        let active = store.active.clone();
        let mut n = 0;
        for s in &mut store.sessions {
            let idle = s.resident()
                && s.has_content()
                && s.id != active
                && !busy.contains(&s.id)
                && s.loaded_at.is_none_or(|t| t.elapsed() >= KEEP_FOR);
            if idle && db.is_synced(s) {
                s.evict();
                n += 1;
            }
        }
        n
    }

    /// 删一个会话：先停它的回合，再删，再清它引用的文件。
    ///
    /// 不先停的话，回合还在后台跑（一条 `timeout: 3600` 的 bash 会一直跑完），
    /// 结果写进一个已经不存在的会话里。
    pub async fn delete_session(&self, id: &str) {
        self.request_abort(id);
        self.forget_usage(id);
        let gone = self.chat.write().await.delete(id);
        self.mark_sessions_dirty();
        if let Some(s) = gone {
            self.forget_files(s).await;
        }
    }

    /// 清空当前会话（`/api/reset`）。有回合在跑就拒绝：回合跑到一半把历史
    /// 抽空，后面追加进来的工具结果就没有对应的调用了，这条会话从此每次都 400。
    pub async fn reset_active(&self) -> Result<(), &'static str> {
        let Some(id) = self.chat.read().await.active_ref().map(|s| s.id.clone()) else {
            return Ok(());
        };
        // 读进来只是为了知道要清哪些文件；读不出来也照样清空
        let _ = self.ensure_loaded(&id).await;
        let old = {
            let mut store = self.chat.write().await;
            // 在会话锁里判断：拿着锁的这段时间，新回合碰不到这条历史
            if self.is_busy(&id) {
                return Err("这个会话还有回合在运行，先停止它或等它结束");
            }
            let Some(s) = store.get_mut(&id) else {
                return Ok(());
            };
            let old = std::mem::take(&mut s.messages);
            s.clear();
            old
        };
        self.mark_sessions_dirty();
        // 历史清空了，用量里那个「当前上下文」也就不成立了
        self.forget_usage(&id);
        let mut gone = crate::agent::ChatSession::new();
        gone.messages = old;
        self.forget_files(gone).await;
        Ok(())
    }

    /// 我们自己的副产物落在哪（截图 / 溢出输出）。沙箱那边按形态给（见
    /// `LinuxSandbox::side_dirs`）；没有沙箱就是空的（没地方写这些东西）。
    pub fn side_dirs(&self) -> crate::gc::SideDirs {
        self.linux().map(|l| l.side_dirs()).unwrap_or_default()
    }

    /// 清掉一个（已经从 store 里拿出来的）会话引用的文件。消息不在内存里的
    /// 从库里读（赶在落盘把那些行删掉之前；没赶上就留给启动清扫）。
    async fn forget_files(&self, s: crate::agent::ChatSession) {
        let dirs = self.side_dirs();
        let db = self.db();
        let _ = tokio::task::spawn_blocking(move || {
            let msgs = if s.resident() {
                s.messages
            } else {
                db.and_then(|d| d.load_messages(&s.id).ok())
                    .unwrap_or_default()
            };
            let files = crate::gc::owned_files(&msgs, &dirs);
            let n = crate::gc::remove_all(&files);
            if n > 0 {
                tracing::debug!("会话 {} 的 {n} 个文件已清掉", s.id);
            }
        })
        .await;
    }

    /// 记下一个刚上传、还没随消息发出去的附件；顺手清掉放了太久的。
    ///
    /// 上传了不发（用户改主意、页面刷新）的附件没人会来取，不清的话 map 和
    /// 磁盘上的文件都只增不减。一小时足够「上传 → 打字 → 发送」。
    pub fn add_pending_attachment(&self, a: crate::agent::Attachment) {
        const TTL: std::time::Duration = std::time::Duration::from_secs(3600);
        let mut stale = Vec::new();
        if let Ok(mut pending) = self.pending_attachments.lock() {
            pending.retain(|_, (at, old)| {
                let keep = at.elapsed() < TTL;
                if !keep {
                    stale.push(std::path::PathBuf::from(&old.path));
                }
                keep
            });
            pending.insert(a.id.clone(), (std::time::Instant::now(), a));
        }
        crate::gc::remove_all(&stale);
    }

    /// 记下「这个端点不支持 Responses API」并落盘。
    ///
    /// 只放内存的话，每次重启后的第一个请求都要先白白失败一次再退回 Chat
    /// Completions。是新记下的才写库（一个端点一辈子写一次）。
    pub async fn remember_responses_unsupported(&self, p: &Provider) {
        if !crate::llm::mark_responses_unsupported(p) {
            return;
        }
        let Some(db) = self.db() else { return };
        let body = serde_json::to_string(&crate::llm::responses_unsupported_list())
            .unwrap_or_else(|_| "[]".into());
        let res =
            tokio::task::spawn_blocking(move || db.set_meta(RESPONSES_UNSUPPORTED, &body)).await;
        if !matches!(res, Ok(Ok(()))) {
            tracing::warn!("记录「不支持 Responses」失败（下次启动会再探测一次）");
        }
    }

    /// 各家的 Key。老版本只有一个 provider（`llm`）：库里还没有 `providers`
    /// 时，把它记成对应那家（按 Base URL 认）已配置。
    fn load_providers(&self, db: &Db) {
        let store: crate::providers::Store = match db.get_meta("providers") {
            Ok(Some(s)) => serde_json::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("providers 解析失败: {e}");
                Default::default()
            }),
            _ => Default::default(),
        };
        if let Ok(mut g) = self.providers.write() {
            *g = store;
        }
        if self.providers.read().map(|g| g.is_empty()).unwrap_or(false) {
            if let Some(p) = self.llm() {
                self.adopt_provider(&p);
            }
        }
    }

    /// 把一个已经在用的 provider 记成「这家已配置」（迁移老配置 / 桌面环境变量）。
    pub fn adopt_provider(&self, p: &Provider) {
        let id = crate::providers::identify(p);
        let creds = crate::providers::Creds {
            api_key: p.api_key.clone(),
            base_url: if id == crate::providers::CUSTOM {
                p.base_url.clone()
            } else {
                String::new()
            },
        };
        self.set_provider_creds(id, Some(creds));
    }

    pub fn provider_creds(&self, id: &str) -> Option<crate::providers::Creds> {
        self.providers.read().ok()?.get(id).cloned()
    }

    /// 存 / 删一家的 Key（`None` = 删），落库，并清掉它的模型清单缓存。
    pub fn set_provider_creds(&self, id: &str, creds: Option<crate::providers::Creds>) {
        if let Ok(mut g) = self.providers.write() {
            match creds {
                Some(c) => {
                    g.insert(id.to_string(), c);
                }
                None => {
                    g.remove(id);
                }
            }
        }
        if let Ok(mut g) = self.model_lists.lock() {
            g.remove(id);
        }
        let Some(db) = self.db() else { return };
        let body = self
            .providers
            .read()
            .ok()
            .and_then(|g| serde_json::to_string(&*g).ok());
        if let Some(body) = body {
            if let Err(e) = db.set_meta("providers", &body) {
                tracing::warn!("保存服务商的 Key 失败: {e:#}");
            }
        }
    }

    /// 一家的可对话模型清单（带缓存）。`refresh` = 不要缓存。
    pub async fn models_of(
        &self,
        id: &str,
        provider: &Provider,
        refresh: bool,
    ) -> anyhow::Result<Vec<crate::models::ModelInfo>> {
        const TTL: std::time::Duration = std::time::Duration::from_secs(600);
        if !refresh {
            let hit = self.model_lists.lock().ok().and_then(|g| {
                g.get(id)
                    .filter(|(at, _)| at.elapsed() < TTL)
                    .map(|(_, v)| v.clone())
            });
            if let Some(v) = hit {
                return Ok(v);
            }
        }
        let dir = self.data_dir.read().ok().and_then(|g| g.clone());
        let mut list = crate::models::list(provider, refresh, dir.as_deref()).await?;
        list.retain(|m| crate::providers::is_chat_model(&m.id));
        if let Ok(mut g) = self.model_lists.lock() {
            g.insert(id.to_string(), (std::time::Instant::now(), list.clone()));
        }
        Ok(list)
    }

    /// 不再有当前 provider（删掉了最后一个配好的服务商）。
    pub fn clear_llm(&self) {
        if let Ok(mut g) = self.llm.write() {
            *g = None;
        }
        if let Some(db) = self.db() {
            if let Err(e) = db.delete_meta("llm") {
                tracing::warn!("清掉 provider 配置失败: {e:#}");
            }
        }
    }

    /// 访问凭据：库里有就用，没有就生成一份存进去（跨重启不变，调试脚本
    /// 拿一次就够）。
    fn load_auth(&self, db: &Db) {
        let get_or_make = |k: &str| -> String {
            match db.get_meta(k) {
                Ok(Some(v)) if !v.is_empty() => v,
                _ => {
                    let v = random_secret();
                    if let Err(e) = db.set_meta(k, &v) {
                        tracing::warn!("保存 {k} 失败: {e:#}");
                    }
                    v
                }
            }
        };
        let auth = Auth {
            token: Some(get_or_make("token")),
            sandbox_key: get_or_make("sandbox_key"),
        };
        if let Ok(mut g) = self.auth.write() {
            *g = auth;
        }
    }

    /// 这个核心实例的 id（存在库里，重启不变）。手机按它记住配对过的电脑 ——
    /// IP 会变（DHCP），id 不变。没库的话每次启动现编一个。
    pub fn instance_id(&self) -> String {
        if let Some(db) = self.db() {
            if let Ok(Some(v)) = db.get_meta("instance_id") {
                if !v.is_empty() {
                    return v;
                }
            }
            let v = random_secret();
            if let Err(e) = db.set_meta("instance_id", &v) {
                tracing::warn!("保存 instance_id 失败: {e:#}");
            }
            return v;
        }
        static EPHEMERAL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        EPHEMERAL.get_or_init(random_secret).clone()
    }

    pub fn auth(&self) -> Auth {
        self.auth
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| Auth {
                token: Some(random_secret()),
                sandbox_key: random_secret(),
            })
    }

    /// 桌面开发用：`AGENT_TOKEN` 设了就要求凭据，没设就不设防。
    pub fn set_token(&self, token: Option<String>) {
        if let Ok(mut g) = self.auth.write() {
            g.token = token.filter(|t| !t.is_empty());
        }
    }

    // —— 回合 ——

    /// 在这个会话上开一个回合。已经有一个在跑就返回 None。
    pub fn begin_turn(&self, sid: &str) -> Option<Arc<Turn>> {
        let mut g = self.turns.lock().ok()?;
        if g.contains_key(sid) {
            return None;
        }
        let t = Arc::new(Turn::default());
        g.insert(sid.to_string(), t.clone());
        drop(g);
        self.notify_turn();
        Some(t)
    }

    pub fn end_turn(&self, sid: &str) {
        if let Ok(mut g) = self.turns.lock() {
            g.remove(sid);
        }
        self.notify_turn();
    }

    /// 把“还有没有回合在跑”告诉壳（前台服务跟这个走）。
    fn notify_turn(&self) {
        let Some(h) = self.turn.read().ok().and_then(|g| g.clone()) else {
            return;
        };
        h.turn(!self.busy_sessions().is_empty());
    }

    pub fn is_busy(&self, sid: &str) -> bool {
        self.turns
            .lock()
            .map(|g| g.contains_key(sid))
            .unwrap_or(false)
    }

    pub fn busy_sessions(&self) -> Vec<String> {
        self.turns
            .lock()
            .map(|g| g.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// 停这个会话正在跑的回合。返回有没有在跑。
    pub fn request_abort(&self, sid: &str) -> bool {
        let t = self.turns.lock().ok().and_then(|g| g.get(sid).cloned());
        match t {
            Some(t) => {
                t.request_abort();
                true
            }
            None => false,
        }
    }

    /// 记一次 LLM 调用的用量（`agent.rs` 每步结束时调）。
    pub fn record_usage(&self, sid: &str, u: &crate::openai::Usage) {
        if let Ok(mut g) = self.usage.lock() {
            g.entry(sid.to_string()).or_default().add(u);
        }
    }

    /// 这个会话的用量（没跑过就是 `None`）。
    pub fn usage_of(&self, sid: &str) -> Option<UsageStats> {
        self.usage.lock().ok().and_then(|g| g.get(sid).cloned())
    }

    /// 会话没了 / 被清空：它的用量也就不成立了。
    pub fn forget_usage(&self, sid: &str) {
        if let Ok(mut g) = self.usage.lock() {
            g.remove(sid);
        }
    }

    pub fn mark_sessions_dirty(&self) {
        self.sessions_dirty.store(true, Ordering::Relaxed);
    }

    /// 当前生效的系统提示词。
    ///
    /// 默认是编译进去的那份；被 `/api/debug/system-prompt` 覆盖过就返回那一份。
    /// `Cow` 是为了没覆盖时**不拷贝**那个常量。
    ///
    /// 编译进去的按平台各一份（`platform()`，见 `agent::system_prompt`）。
    /// 同一个实例上不会中途换 —— 前缀缓存的一个字节都不能变。
    pub fn system_prompt(&self) -> Cow<'static, str> {
        match self.system_prompt.read() {
            Ok(g) => match &*g {
                Some(s) => Cow::Owned(s.clone()),
                None => Cow::Borrowed(self.builtin_prompt()),
            },
            Err(_) => Cow::Borrowed(self.builtin_prompt()),
        }
    }

    /// 编译进去的那份提示词（没被运行时覆盖时用）。
    fn builtin_prompt(&self) -> &'static str {
        crate::agent::system_prompt(self.platform())
    }

    /// 模型跑在哪种机器上 —— 提示词和工具表按它选。
    ///
    /// 看沙箱的形态：宿主模式就是这个二进制的桌面系统；proot 沙箱、或者
    /// 还没有沙箱（安卓 boot 阶段 / 纯 Rust 环境）都按手机算。同一个实例上
    /// 不会中途变（沙箱只在启动时设一次），前缀缓存的一个字节都不会动。
    pub fn platform(&self) -> crate::platform::Platform {
        let host = self
            .linux
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.is_host()))
            .unwrap_or(false);
        if host {
            crate::platform::Platform::host()
        } else {
            crate::platform::Platform::Android
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
        if !self.sessions_loaded.load(Ordering::Relaxed) {
            return Ok(());
        }
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
    pub fn set_llm(&self, p: Provider) {
        self.persist_provider(&p);
        let name = p.name.clone();
        let model = p.model.clone();
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        self.log("info", format!("LLM 已配置: {name} / {model}"));
    }

    /// 只更新「能不能看图」并落盘（自动判定后调用），不重发配置日志。
    pub fn set_supports_images(&self, vision: bool) {
        let Some(mut p) = self.llm() else { return };
        if p.supports_images == vision {
            return;
        }
        p.supports_images = vision;
        self.persist_provider(&p);
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        tracing::debug!("视觉能力已记录: {vision}");
    }

    /// 只更新「能不能吃 PDF」并落盘（自动判定后调用），不重发配置日志。
    pub fn set_supports_files(&self, files: bool) {
        let Some(mut p) = self.llm() else { return };
        if p.supports_files == files {
            return;
        }
        p.supports_files = files;
        self.persist_provider(&p);
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        tracing::debug!("文档输入能力已记录: {files}");
    }

    /// provider 配置存进会话库的 `meta` 表（桌面开发没有库，只留在内存里）。
    ///
    /// 一行 UPSERT 是原子的：原来写 llm.json 时进程被杀，文件就是半截，
    /// 下次启动解析失败，API key 跟着没了。
    fn persist_provider(&self, p: &Provider) {
        let Some(db) = self.db() else { return };
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
        let res = serde_json::to_string(&rec)
            .map_err(anyhow::Error::from)
            .and_then(|body| db.set_meta("llm", &body));
        if let Err(e) = res {
            tracing::warn!("保存 provider 失败: {e:#}");
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
        let resolved =
            crate::models::vision_for(provider, &provider.model, false, dir.as_deref()).await;
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
            self.set_supports_images(vision);
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
        let resolved =
            crate::models::pdf_for(provider, &provider.model, false, dir.as_deref()).await;
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
            self.set_supports_files(files);
        }
        files
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

    /// 启动时从库里恢复 provider。
    ///
    /// 老版本存在 `<filesDir>/llm.json`：库里还没有时把它搬进来，然后删掉
    /// 文件 —— 明文的 key 不该在两个地方各留一份。
    fn load_llm(&self, db: &Db, dir: &std::path::Path) {
        let raw = match db.get_meta("llm") {
            Ok(Some(s)) => Some(s),
            Ok(None) => migrate_llm_json(db, dir),
            Err(e) => {
                self.log("warn", format!("读取 provider 配置失败: {e:#}"));
                None
            }
        };
        let Some(raw) = raw else { return };
        match serde_json::from_str::<PersistedProvider>(&raw) {
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
                // 刻意**不**给 `files` 填缓存：老配置里没有这个字段，
                // 填了就会把「从未判定过」当成「已判定为 false」，界面就一直
                // 不显示文件入口。留给启动后的 `refresh_capabilities` 去查
                // models.dev（磁盘有目录缓存，大多不联网）。
                if let Ok(mut g) = self.llm.write() {
                    *g = Some(p);
                }
                self.log("info", format!("恢复 LLM 配置: {name} / {model}"));
            }
            Err(e) => self.log("warn", format!("provider 配置解析失败: {e}")),
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

/// 老版本的 `llm.json` → 库。成功搬进去才删文件；返回搬进去的内容。
fn migrate_llm_json(db: &Db, dir: &std::path::Path) -> Option<String> {
    let f = dir.join("llm.json");
    let body = std::fs::read_to_string(&f).ok()?;
    // 坏文件不搬（搬进去也解析不了），留着给人看
    serde_json::from_str::<PersistedProvider>(&body).ok()?;
    match db.set_meta("llm", &body) {
        Ok(()) => {
            let _ = std::fs::remove_file(&f);
            let _ = std::fs::remove_file(dir.join("llm.json.tmp"));
            tracing::info!("llm.json 已迁移进 sessions.db");
            Some(body)
        }
        Err(e) => {
            tracing::warn!("迁移 llm.json 失败（下次启动再试）: {e:#}");
            Some(body)
        }
    }
}

/// `meta` 表里那一行：已知不支持 Responses API 的 `base_url|model`（JSON 数组）
const RESPONSES_UNSUPPORTED: &str = "responses_unsupported";

/// 随机凭据：122 位随机数的十六进制（UUID v4 去掉版本位之外全是随机的）。
fn random_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 本地 API 的访问凭据。
///
/// 核心只听 127.0.0.1，但**手机上任何一个有网络权限的 App 都能连
/// 127.0.0.1**，扫一遍端口就能找到我们 —— 然后调 `/api/linux` 在沙箱里跑
/// 命令，而沙箱挂着整个内部存储（我们有「所有文件访问」权限）。所以：
///
/// - `token`：`/api/*` 要它。App 的 WebView 走 cookie（`/auth?t=` 种下，
///   HttpOnly），沙箱里的 `agent` 命令和电脑上的调试脚本走 `x-agent-token`
///   头。`None` = 不设防（桌面开发没设 `AGENT_TOKEN` 时）。
/// - `sandbox_key`：沙箱文件走 `/sandbox/<key>/…`。单独一把，因为沙箱里
///   生成的网页在 opaque origin 里，带不上 cookie；放在路径里，页面的相对
///   路径（css/js/图片）自然继承。这把钥匙页面自己看得到，所以它**只**能
///   读沙箱文件，开不了 `/api`。
#[derive(Clone, Debug)]
pub struct Auth {
    pub token: Option<String>,
    pub sandbox_key: String,
}

/// 一个会话的 token 用量 —— 输入框上那个上下文圆环和它点开的面板。
///
/// `last` 是「**现在这个上下文有多大**」：一次请求发出去的 `prompt_tokens`
/// 就是当时整段历史 + 系统提示词 + 工具定义的大小，所以它比累计值更能说明
/// 「还剩多少余量」。
///
/// 缓存字段有**两套写法**（OpenAI 系 / DeepSeek 系），归一在
/// `Usage::cached_tokens()` 里。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageStats {
    /// 最近一次 LLM 调用
    pub last: Option<crate::openai::Usage>,
    /// 本会话累计
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    pub calls: u64,
}

impl UsageStats {
    fn add(&mut self, u: &crate::openai::Usage) {
        self.last = Some(u.clone());
        self.input += u.prompt_tokens as u64;
        self.output += u.completion_tokens as u64;
        self.cached += u.cached_tokens().unwrap_or(0) as u64;
        self.calls += 1;
    }
}

/// 一个正在跑的回合的停止开关。
///
/// agent loop **没有轮数上限**，所以必须有一个真的能停下来、而不只是
/// 断开前端连接的开关 —— 否则模型空转时只能在后台烧 token。
#[derive(Default)]
pub struct Turn {
    abort: AtomicBool,
    /// 按停止时叫醒正在等的人（流式接收、跑着的 bash）。只有标志位的话，
    /// 停止要等到「这一步流完 / 这条命令跑完」才生效 —— 而 bash 的超时
    /// 可以是 24 小时。
    notify: tokio::sync::Notify,
}

impl Turn {
    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub fn abort_requested(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// 等到用户按停止。已经按过就立刻返回。
    ///
    /// 先登记再看标志位：`notify_waiters` 不留许可，先看标志再登记的话，
    /// 两步之间按下的停止就丢了。
    pub async fn aborted(&self) {
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.abort_requested() {
                return;
            }
            n.await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agent-state-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 编译进去的系统提示词跟着沙箱形态走：安卓/沙箱一份，桌面宿主模式一份。
    ///
    /// 两份都是常量，所以同一个实例上**不会**中途换（前缀缓存一个字节都不能变）；
    /// 这里只是把「哪份对应哪种形态」钉住。
    #[test]
    fn the_builtin_prompt_follows_the_sandbox_flavour() {
        let st = AppState::new();
        assert_eq!(
            st.system_prompt(),
            crate::agent::system_prompt(crate::platform::Platform::Android),
            "还没有沙箱时（安卓 boot 阶段 / 纯 Rust 环境）用手机那份"
        );

        st.set_linux(Arc::new(LinuxSandbox::host(std::env::temp_dir())));
        assert_eq!(
            st.system_prompt(),
            crate::agent::system_prompt(crate::platform::Platform::host())
        );

        // 运行时覆盖照旧最高优先（`/api/debug/system-prompt` 调提示词那条路）
        st.set_system_prompt(Some("改过的".into()));
        assert_eq!(st.system_prompt(), "改过的");
    }

    /// 老版本把 provider（含明文 key）存在 llm.json：搬进库，然后删掉文件。
    #[test]
    fn llm_json_moves_into_the_database_and_the_file_goes_away() {
        let dir = temp_dir("migrate");
        std::fs::write(
            dir.join("llm.json"),
            r#"{"name":"go","base_url":"https://x/v1","model":"m1","api_key":"sk-1"}"#,
        )
        .unwrap();

        let st = AppState::new();
        st.open_db(&dir);
        let p = st.llm().expect("从 llm.json 恢复出 provider");
        assert_eq!((p.model.as_str(), p.api_key.as_str()), ("m1", "sk-1"));
        assert!(
            !dir.join("llm.json").exists(),
            "明文 key 不该再留一份在文件里"
        );
        // 老配置记成「其他」那家已配置，设置页里看得到
        let c = st
            .provider_creds("custom")
            .expect("老配置要出现在设置页的服务商里");
        assert_eq!(
            (c.api_key.as_str(), c.base_url.as_str()),
            ("sk-1", "https://x/v1")
        );

        // 改模型 → 存进库；重开之后还在，凭据也不变
        let mut p2 = p.clone();
        p2.model = "m2".into();
        st.set_llm(p2);
        let token = st.auth().token;
        let key = st.auth().sandbox_key;
        drop(st);

        let st = AppState::new();
        st.open_db(&dir);
        assert_eq!(st.llm().unwrap().model, "m2");
        assert_eq!(st.auth().token, token, "凭据跨重启不变，调试脚本拿一次就够");
        assert_eq!(st.auth().sandbox_key, key);
        assert!(token.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_is_per_session_and_accumulates() {
        use crate::openai::{PromptTokensDetails, Usage};
        let st = AppState::new();
        assert!(st.usage_of("a").is_none(), "没跑过就是 None");

        let u1 = Usage {
            prompt_tokens: 1000,
            completion_tokens: 50,
            total_tokens: 1050,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(800),
            }),
            ..Default::default()
        };
        st.record_usage("a", &u1);
        let s = st.usage_of("a").unwrap();
        assert_eq!((s.input, s.output, s.cached, s.calls), (1000, 50, 800, 1));
        assert_eq!(s.last.as_ref().unwrap().prompt_tokens, 1000);

        // 第二步：last 换成新的，累计往上加
        let u2 = Usage {
            prompt_tokens: 1500,
            completion_tokens: 20,
            // DeepSeek 系那套写法，两套都要认
            prompt_cache_hit_tokens: Some(1400),
            ..Default::default()
        };
        st.record_usage("a", &u2);
        let s = st.usage_of("a").unwrap();
        assert_eq!((s.input, s.output, s.cached, s.calls), (2500, 70, 2200, 2));
        assert_eq!(s.last.as_ref().unwrap().prompt_tokens, 1500);

        assert!(st.usage_of("b").is_none(), "另外那个会话不受影响");

        // 会话删了 / 被清空 → 它的用量不该留着
        st.forget_usage("a");
        assert!(st.usage_of("a").is_none());
    }

    #[test]
    fn turns_are_per_session() {
        let st = AppState::new();
        let a = st.begin_turn("a").unwrap();
        assert!(st.begin_turn("a").is_none(), "同一个会话不能同时跑两个回合");
        let b = st.begin_turn("b").unwrap();

        assert!(st.request_abort("a"));
        assert!(a.abort_requested());
        assert!(!b.abort_requested(), "停 a 不能把 b 也停了");

        st.end_turn("a");
        assert!(!st.is_busy("a"));
        assert!(st.is_busy("b"));
        assert!(!st.request_abort("a"), "没在跑的会话：停止什么也不做");
    }

    /// 会话还没从库里读出来时绝不能落盘：`sync` 会把内存里没有的会话从
    /// 库里删掉，拿空 store 去同步等于清空全部历史。
    #[tokio::test]
    async fn nothing_is_saved_before_the_sessions_are_loaded() {
        let dir = temp_dir("load");
        {
            let st = AppState::new();
            st.open_db(&dir);
            st.sessions_loaded.store(true, Ordering::Relaxed);
            let mut store = st.chat.write().await;
            let id = store.create();
            store
                .get_mut(&id)
                .unwrap()
                .messages
                .push(crate::agent::ChatMessage::User("留着".into()));
            drop(store);
            st.save_sessions().unwrap();
        }

        let st = AppState::new();
        st.open_db(&dir);
        // 还没 load_sessions：内存是空的，这时落盘必须什么都不做
        st.save_sessions().unwrap();
        st.load_sessions();
        // 后台读完之前，读会话的人排队等，不会看到空列表
        let store = st.chat.read().await;
        assert_eq!(store.sessions.len(), 1);
        assert!(st.sessions_loaded.load(Ordering::Relaxed));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个开着库、读过会话的 AppState，外加 n 个各有一句话的会话（最后一个是活动的）。
    async fn state_with_sessions(
        tag: &str,
        n: usize,
    ) -> (Arc<AppState>, Vec<String>, std::path::PathBuf) {
        let dir = temp_dir(tag);
        let st = AppState::new();
        st.open_db(&dir);
        st.sessions_loaded.store(true, Ordering::Relaxed);
        let mut ids = Vec::new();
        {
            let mut store = st.chat.write().await;
            for i in 0..n {
                let id = store.create();
                store
                    .get_mut(&id)
                    .unwrap()
                    .messages
                    .push(crate::agent::ChatMessage::User(format!("第{i}个")));
                ids.push(id);
            }
        }
        st.save_sessions().unwrap();
        (st, ids, dir)
    }

    /// 落过盘、不活跃、没在跑的会话放掉消息；要用时 `ensure_loaded` 原样读回来。
    /// 活动的、在跑的、没落盘的都不放。
    #[tokio::test]
    async fn idle_sessions_are_evicted_and_read_back_on_demand() {
        let (st, ids, dir) = state_with_sessions("evict", 3).await;
        let (a, b, active) = (&ids[0], &ids[1], &ids[2]);
        st.begin_turn(b).unwrap();
        // a 有一条还没落盘的消息：不能放
        st.chat
            .write()
            .await
            .get_mut(a)
            .unwrap()
            .messages
            .push(crate::agent::ChatMessage::User("没落盘".into()));
        assert_eq!(st.evict_idle(), 0);

        st.save_sessions().unwrap();
        assert_eq!(st.evict_idle(), 1, "只有 a：b 在跑，另一个是当前会话");
        {
            let store = st.chat.read().await;
            assert!(!store.get(a).unwrap().resident());
            assert!(store.get(b).unwrap().resident());
            assert!(store.get(active).unwrap().resident());
        }

        st.ensure_loaded(a).await.unwrap();
        let store = st.chat.read().await;
        let s = store.get(a).unwrap();
        assert!(s.resident());
        let texts: Vec<&str> = s.messages.iter().filter_map(|m| m.user_text()).collect();
        assert_eq!(texts, vec!["第0个", "没落盘"]);
        drop(store);
        // 刚读回来的不马上又放掉
        assert_eq!(st.evict_idle(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删会话先停它的回合，再把它引用的文件（附件、图片副本）一起清掉。
    #[tokio::test]
    async fn deleting_a_session_stops_its_turn_and_removes_its_files() {
        let (st, ids, dir) = state_with_sessions("delete", 1).await;
        let id = &ids[0];
        let att = dir.join("a.png");
        std::fs::write(&att, b"x").unwrap();
        st.chat.write().await.get_mut(id).unwrap().messages.push(
            crate::agent::ChatMessage::UserFiles {
                text: String::new(),
                attachments: vec![crate::agent::Attachment {
                    id: "a.png".into(),
                    path: att.display().to_string(),
                    mime: "image/png".into(),
                    name: "a.png".into(),
                    kind: crate::agent::AttachmentKind::Image,
                }],
            },
        );
        let turn = st.begin_turn(id).unwrap();

        st.delete_session(id).await;
        assert!(turn.abort_requested(), "回合要停");
        assert!(st.chat.read().await.get(id).is_none());
        assert!(!att.exists(), "附件要跟着删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回合跑到一半不许清空：后面追加的工具结果会没有对应的调用，会话从此 400。
    #[tokio::test]
    async fn resetting_a_busy_session_is_refused() {
        let (st, ids, dir) = state_with_sessions("reset", 1).await;
        let id = &ids[0];
        st.begin_turn(id).unwrap();
        assert!(st.reset_active().await.is_err());
        assert_eq!(st.chat.read().await.get(id).unwrap().messages.len(), 1);

        st.end_turn(id);
        st.reset_active().await.unwrap();
        assert!(st.chat.read().await.get(id).unwrap().messages.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上传了不发的附件：放过一小时，下一次上传时清掉（map 和文件都清）。
    #[test]
    fn stale_pending_attachments_are_dropped() {
        let dir = temp_dir("pending");
        let st = AppState::new();
        let file = |n: &str| {
            let p = dir.join(n);
            std::fs::write(&p, b"x").unwrap();
            crate::agent::Attachment {
                id: n.into(),
                path: p.display().to_string(),
                mime: "application/pdf".into(),
                name: n.into(),
                kind: crate::agent::AttachmentKind::Pdf,
            }
        };
        st.add_pending_attachment(file("old.pdf"));
        // 把它的时间拨回两小时前
        st.pending_attachments
            .lock()
            .unwrap()
            .get_mut("old.pdf")
            .unwrap()
            .0 -= std::time::Duration::from_secs(7200);
        st.add_pending_attachment(file("new.pdf"));

        let pending = st.pending_attachments.lock().unwrap();
        assert!(!pending.contains_key("old.pdf") && pending.contains_key("new.pdf"));
        assert!(!dir.join("old.pdf").exists() && dir.join("new.pdf").exists());
        drop(pending);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
