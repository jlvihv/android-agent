//! 全局状态与边界数据模型。
//!
//! 设计铁律（见 docs/01-tech-selection.md §4）：
//! native 侧不反调 Java。Kotlin 把 UI 树拍平成扁平 JSON 送进来，
//! Rust 只回传指令。所以这里所有跨边界类型都是 JSON 可序列化的。

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent::SessionStore;
use crate::db::Db;
use crate::linux::LinuxSandbox;
use crate::llm::Provider;
use crate::phone::PhoneApi;

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
    /// 一个回合此刻在干什么（见 `status::Snapshot`）。
    ///
    /// 它和其他事件走**同一条总线**（`/api/ws`，外加一条给壳用的 SSE：`/api/turns/stream`）。
    /// 所有订阅者（手机壳、前端、脚本）拿的是同一份 JSON（见 docs/06）：同进程的壳也
    /// 走这条路，不然「手机连到电脑上的核心」那条就什么都拿不到 —— 两套形状、
    /// 同一份状态语义要维护两遍。
    Turn {
        status: crate::status::Snapshot,
    },
    /// 有没有回合在跑（壳的前台服务起/停、界面上的「运行中」都跟它走）
    TurnActive {
        active: bool,
    },
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// 手机出门在外连这台电脑要的东西（见 `remote.rs` 和 docs/12-relay.md）。
///
/// 配对时（`/api/pair`）连同 token 一起给手机；配对过的手机在局域网里连上时也会
/// 再取一次（`/api/remote`）—— 中继是后来才配上的、或者换了服务器，都不用重新配对。
#[derive(Clone, Debug, Serialize)]
pub struct RemoteInfo {
    /// 这台电脑 TLS 证书的 SHA-256（十六进制）。手机经中继连过来时只认这一张：
    /// 中继只搬字节，冒充不了这台电脑
    pub fingerprint: String,
    /// 中继（没配就是 None，那就只能在局域网里用）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayAddr>,
}

/// 换这台电脑挂的中继（实现在 `remote.rs`，手机上没有 —— 所以这里只是个接口）。
pub trait RelayControl: Send + Sync {
    /// 挂到这个中继上（立刻生效，并记下来，下次启动照样挂）。`Err` 是给人看的原因。
    fn attach(&self, relay: RelayAddr) -> Result<(), String>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayAddr {
    /// `IP:端口`
    pub addr: String,
    /// 中继的共享密钥（十六进制）。手机拨号要用它
    pub key: String,
}

/// 一条**排队等着交给模型**的用户消息（见 `AppState::queued`）。
///
/// 形状刻意和 `/api/chat` 的请求体对齐（`id` 是客户端给的那个，没给就现生成一个）
/// —— 界面上那条「排队中」的气泡、以及撤掉它（`/api/sessions/unqueue`）都要认
/// 同一个 id。
#[derive(Clone)]
pub struct QueuedMsg {
    pub id: String,
    pub text: String,
    /// 已经从 `pending_attachments` 里取出来的附件（排队期间留着，交给模型时用）
    pub attachments: Vec<crate::agent::Attachment>,
}

pub struct AppState {
    pub tx: broadcast::Sender<Event>,
    pub device: RwLock<Option<Arc<dyn DeviceApi>>>,
    /// 浏览器实现。和 `device` 分开：它的可用条件是无障碍无关的
    /// （见 `BrowserApi` 的说明）。
    pub browser: RwLock<Option<Arc<dyn BrowserApi>>>,
    /// 手机系统能力（闹钟 / 短信 / 日历 / 联系人 / 定位 / 剪贴板…）。
    /// 和上面两个都分开：它不依赖无障碍（`device`），也不依赖 WebView（`browser`），
    /// 但**只有手机上才有**（见 `phone` 模块）。桌面上一直是 `None`。
    pub phone: RwLock<Option<Arc<dyn PhoneApi>>>,
    /// **正在跑的**各会话最后一次的实时状态，key = sid。
    ///
    /// 只为一件事：有人刚订阅上来时补一帧「现在在跑什么」（见 `turn_snapshot`）。
    /// 回合结束（`end_turn`）就把那一行拿掉 —— 留着的话，每次重连都会把早就
    /// 完事的回合当成新消息重放一遍，壳就再发一次「已完成」通知。
    pub turn_status: Mutex<HashMap<String, crate::status::Snapshot>>,
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
    /// 远程 MCP 服务（配置 + 各自的会话 + 停用名单）。见 `mcp` 模块。
    pub mcp: crate::mcp::Mcp,
    /// 停用的 skill（目录名）。库里 `disabled` 表 kind = 'skill' 那几行
    disabled_skills: RwLock<std::collections::HashSet<String>>,
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
    /// 「我这台机器连过哪些电脑、现在连的是哪台」（见 `machines`）。
    ///
    /// 打开库时建（`open_db`）：配对过的机器、证书指纹、隧道钥匙都在库里。
    /// 页面上那个「选择机器」面板和隧道转发 `/api/machines*` 都问它 ——
    /// 手机上的壳只负责 mDNS 发现（`lan_found`）和导航。
    pub machines: std::sync::OnceLock<Arc<crate::machines::Machines>>,
    /// 出门在外连这台电脑要用的东西（证书指纹 + 中继地址），配对时和 `/api/remote`
    /// 交给手机。只有桌面宿主模式对局域网开放时才有（见 `remote.rs`）。
    pub remote: RwLock<Option<RemoteInfo>>,
    /// 运行时换中继（`POST /api/remote/relay`：手机把它知道的中继交给这台电脑）。
    /// 只有桌面宿主模式对局域网开放时才有（见 `remote.rs`）。
    pub relay_host: RwLock<Option<Arc<dyn RelayControl>>>,
    /// 局域网上的 TLS（手机 App 连这台电脑走它，见 `http::accept_conn`）。和中继那条用的是
    /// 同一张证书。没有 = 只有明文（手机自己的核心、只听本机的电脑）。
    pub lan_tls: std::sync::OnceLock<tokio_rustls::TlsAcceptor>,
    /// 核心跑在哪个 tokio runtime 上。手机的壳从 Kotlin 线程起中继隧道时要用
    /// （`ffi::AgentCore::start_tunnel`）—— runtime 本身归 `serve_in_background` 那个线程。
    pub rt: std::sync::OnceLock<tokio::runtime::Handle>,
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
    /// 排队等着交给模型的用户消息：会话 id → 那条队。
    ///
    /// **为什么在服务端排**：前端那个队列活在 WebView 里 —— App 切到后台、
    /// WebView 被冻住或回收，那条消息就永远发不出去了（排队 = 等一会儿再发同一个
    /// 请求，而「等」这件事前端没有能力保证）。这里排着的话，正在跑的那个回合
    /// 会在**步边界**把它取走交给模型（`agent::deliver_queued`），跟客户端
    /// 在不在、死没死都没关系。
    ///
    /// 取走的唯一入口是回合循环；没有回合在跑时由 `agent::continue_with_queued`
    /// 立刻开一个 —— 否则它们会一直等在那儿（界面上一直显示「排队中」）。
    pub queued: Mutex<HashMap<String, VecDeque<QueuedMsg>>>,
    /// 各会话**上一个回合**失败的原因（`/api/session` 的 `turnError`）。下一个回合
    /// 开始时清掉。为什么要存：排队消息接着开的回合没有人订阅它的流，不存的话
    /// 它的错误只进日志，界面上就是「发了没回音」。
    pub turn_errors: Mutex<HashMap<String, String>>,
    /// 本地 API 的访问凭据。见 `Auth`。
    pub auth: RwLock<Auth>,
    /// 会话有改动，等着落盘。定时器每 2s 检查一次。
    ///
    /// 不用「改一次写一次」：一轮对话会产生几十次变更，每次都写盘太浪费。
    /// 也不能不写：进程被 kill 会话就没了（force-stop 之后抽屉会空）。
    pub sessions_dirty: AtomicBool,
    /// 运行时覆盖的系统提示词（`POST /api/debug/system-prompt`）。
    ///
    /// `None` = 用编译进二进制的那份（`prompt::system_prompt`）。存在的理由就一个：
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
    /// 每个会话的 token 用量（输入框上那个上下文圆环）：库里 `usage` 表的内存缓存。
    ///
    /// `None` 也缓存（「这个会话一次都没调用过」）：不然没跑过的会话每次轮询都要查一次库。
    pub usage: Mutex<HashMap<String, Option<UsageStats>>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(Self {
            tx,
            device: RwLock::new(None),
            browser: RwLock::new(None),
            phone: RwLock::new(None),
            turn_status: Mutex::new(HashMap::new()),
            port: AtomicU16::new(0),
            mock: AtomicBool::new(false),
            chat: Arc::new(tokio::sync::RwLock::new(SessionStore::default())),
            linux: RwLock::new(None),
            llm: RwLock::new(None),
            providers: RwLock::new(Default::default()),
            mcp: Default::default(),
            disabled_skills: Default::default(),
            model_lists: Mutex::new(HashMap::new()),
            data_dir: RwLock::new(None),
            alpine: Default::default(),
            pairing: RwLock::new(None),
            machines: std::sync::OnceLock::new(),
            remote: RwLock::new(None),
            relay_host: RwLock::new(None),
            lan_tls: std::sync::OnceLock::new(),
            rt: std::sync::OnceLock::new(),
            trust_loopback: AtomicBool::new(false),
            sessions_loaded: AtomicBool::new(false),
            turns: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            turn_errors: Mutex::new(HashMap::new()),
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

    pub fn phone(&self) -> Option<Arc<dyn PhoneApi>> {
        self.phone.read().ok().and_then(|g| g.clone())
    }

    pub fn set_phone(&self, p: Arc<dyn PhoneApi>) {
        if let Ok(mut g) = self.phone.write() {
            *g = Some(p);
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

    /// 机器记账（配对过哪些电脑、现在连的是哪台）。没有库时为 `None`。
    pub fn machines(&self) -> Option<Arc<crate::machines::Machines>> {
        self.machines.get().cloned()
    }

    /// 打开会话库，读出 provider 配置和访问凭据。启动时调一次（主线程上）。
    ///
    /// **会话本身不在这里读**：历史一多（每条 bash 输出几十 KB），全读出来
    /// 要几百毫秒，而这是在 `Application.onCreate` 里 —— 卡的是 App 启动。
    /// 会话挪到 `load_sessions`，在后台读。
    pub fn open_db(self: &Arc<Self>, dir: &std::path::Path) {
        let path = dir.join("sessions.db");
        match Db::open(&path) {
            Ok(db) => {
                let db = Arc::new(db);
                self.load_llm(&db);
                self.load_providers(&db);
                match db.mcp_servers() {
                    Ok(list) => self.mcp.set_servers(list),
                    Err(e) => tracing::warn!("读 MCP 服务失败: {e:#}"),
                }
                match db.disabled() {
                    Ok(rows) => {
                        for (kind, name) in rows {
                            match kind.as_str() {
                                "mcp" => self.mcp.set_enabled(&name, false),
                                "skill" => {
                                    if let Ok(mut g) = self.disabled_skills.write() {
                                        g.insert(name);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => tracing::warn!("读停用名单失败: {e:#}"),
                }
                self.load_auth(&db);
                if let Ok(keys) = db.responses_unsupported() {
                    crate::llm::load_responses_unsupported(keys);
                }
                // 机器记账跟着库一起建：在它后面的机器列表、当前那台都落在库里
                let _ = self.machines.set(crate::machines::Machines::new(
                    db.clone(),
                    Arc::downgrade(self),
                ));
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
            let referenced = crate::gc::owned_files(&msgs).into_iter().collect();
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
        // 排队等着的那些也跟着走 —— 会话都没了，没人会去取它们
        self.drop_queued(id).await;
        self.clear_turn_error(id);
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
        self.clear_usage(&id);
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
        let db = self.db();
        let _ = tokio::task::spawn_blocking(move || {
            let msgs = if s.resident() {
                s.messages
            } else {
                db.and_then(|d| d.load_messages(&s.id).ok())
                    .unwrap_or_default()
            };
            let files = crate::gc::owned_files(&msgs);
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
        let key = crate::llm::responses_key(p);
        let res = tokio::task::spawn_blocking(move || db.add_responses_unsupported(&key)).await;
        if !matches!(res, Ok(Ok(()))) {
            tracing::warn!("记录「不支持 Responses」失败（下次启动会再探测一次）");
        }
    }

    /// 各家的 Key。库里还没有 `providers` 行时（桌面就是环境变量配的那一个），
    /// 把它记成对应那家（按 Base URL 认）已配置。
    fn load_providers(&self, db: &Db) {
        let store = db.providers().unwrap_or_else(|e| {
            tracing::warn!("读服务商的 Key 失败: {e:#}");
            Default::default()
        });
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
        if let Some(db) = self.db() {
            if let Err(e) = db.set_provider(id, creds.as_ref()) {
                tracing::warn!("保存服务商的 Key 失败: {e:#}");
            }
        }
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
        let db = self.db();
        let mut list = crate::models::list(provider, refresh, db.as_deref()).await?;
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
            if let Err(e) = db.set_llm(None) {
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
        self.clear_turn_error(sid);
        self.notify_turn();
        Some(t)
    }

    pub fn end_turn(&self, sid: &str) {
        if let Ok(mut g) = self.turns.lock() {
            g.remove(sid);
        }
        if let Ok(mut g) = self.turn_status.lock() {
            g.remove(sid);
        }
        self.notify_turn();
    }

    /// 把“还有没有回合在跑”告诉壳（前台服务跟这个走）。
    fn notify_turn(&self) {
        self.emit(Event::TurnActive {
            active: !self.busy_sessions().is_empty(),
        });
    }

    /// 把一份实时状态发到总线上（手机壳的通知栏、前端、脚本都在那儿听）。
    ///
    /// 没人听就是广播没人收，不是错误 —— 桌面上单独跑、没有订阅者时也一样发。
    pub fn push_turn_status(&self, status: crate::status::Snapshot) {
        if status.active {
            if let Ok(mut g) = self.turn_status.lock() {
                g.insert(status.sid.clone(), status.clone());
            }
        }
        self.emit(Event::Turn { status });
    }

    /// 给刚订阅上来的客户端补一帧「现在在跑什么」：先一句有没有回合在跑，再逐条状态。
    ///
    /// 为什么要补：订阅者可能在回合跑了一半才连上（手机壳刚起、刚从冻住里醒来、
    /// 网络刚回来），不补的话它得等下一次变化 —— 而一条 `bash` 可能几分钟不产生变化。
    /// 只补**在跑的**：完事的那一条是一次性的消息，重放就是重复通知。
    pub fn turn_snapshot(&self) -> Vec<Event> {
        let mut out = vec![Event::TurnActive {
            active: !self.busy_sessions().is_empty(),
        }];
        if let Ok(g) = self.turn_status.lock() {
            out.extend(g.values().cloned().map(|status| Event::Turn { status }));
        }
        out
    }

    pub fn is_busy(&self, sid: &str) -> bool {
        self.turns
            .lock()
            .map(|g| g.contains_key(sid))
            .unwrap_or(false)
    }

    // ---- 排队等着交给模型的用户消息（见 `queued` 字段） --------------------

    /// 排队等着交给模型的那些消息（`/api/session` 报给界面看）。
    pub fn queued_list(&self, sid: &str) -> Vec<QueuedMsg> {
        self.queued
            .lock()
            .map(|g| {
                g.get(sid)
                    .map(|q| q.iter().cloned().collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// 排进来一条（“这会儿正跑着，等模型空下来再给它”）。
    pub fn enqueue(&self, sid: &str, m: QueuedMsg) {
        if let Ok(mut g) = self.queued.lock() {
            g.entry(sid.to_string()).or_default().push_back(m);
        }
    }

    /// 取走**全部**排队的消息（回合循环在每个步边界调；没人取就一直是空的）。
    pub fn drain_queued(&self, sid: &str) -> Vec<QueuedMsg> {
        self.queued
            .lock()
            .ok()
            .and_then(|mut g| g.remove(sid))
            .map(|q| q.into_iter().collect())
            .unwrap_or_default()
    }

    /// 还有排队的吗（回合循环靠它在「模型不调工具了」那一刻决定要不要接着跑）。
    pub fn has_queued(&self, sid: &str) -> bool {
        self.queued
            .lock()
            .map(|g| g.get(sid).is_some_and(|q| !q.is_empty()))
            .unwrap_or(false)
    }

    /// 撤掉一条**还在排队**的（界面上面那个 ×）。已经交给模型的撤不掉 ——
    /// 那时它是历史了。它带的附件文件一起删（没人会再引用它们）。
    pub async fn unqueue(&self, sid: &str, id: &str) -> bool {
        let removed = {
            let Ok(mut g) = self.queued.lock() else {
                return false;
            };
            let Some(q) = g.get_mut(sid) else {
                return false;
            };
            let Some(i) = q.iter().position(|m| m.id == id) else {
                return false;
            };
            let m = q.remove(i);
            if q.is_empty() {
                g.remove(sid);
            }
            m
        };
        self.forget_queued_files(removed.into_iter().collect())
            .await;
        true
    }

    /// 整个会话的队列丢掉（删会话时用；不然那些消息永远没人取了），附件文件一起删。
    async fn drop_queued(&self, sid: &str) {
        let gone = self
            .queued
            .lock()
            .ok()
            .and_then(|mut g| g.remove(sid))
            .map(Vec::from)
            .unwrap_or_default();
        self.forget_queued_files(gone).await;
    }

    /// 排队消息带的附件文件。它们已经从 `pending_attachments` 里取出来了，又还没进
    /// 历史 —— 删会话时的 `forget_files` 和上传过期的清理都管不到。
    async fn forget_queued_files(&self, msgs: Vec<QueuedMsg>) {
        let msgs: Vec<crate::agent::ChatMessage> = msgs
            .into_iter()
            .filter(|m| !m.attachments.is_empty())
            .map(|m| crate::agent::ChatMessage::User {
                text: m.text,
                attachments: m.attachments,
            })
            .collect();
        if msgs.is_empty() {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || {
            crate::gc::remove_all(&crate::gc::owned_files(&msgs))
        })
        .await;
    }

    /// 记下这个会话的回合是怎么失败的（见 `turn_errors`）。
    pub fn set_turn_error(&self, sid: &str, msg: String) {
        if let Ok(mut g) = self.turn_errors.lock() {
            g.insert(sid.to_string(), msg);
        }
    }

    pub fn turn_error(&self, sid: &str) -> Option<String> {
        self.turn_errors.lock().ok()?.get(sid).cloned()
    }

    fn clear_turn_error(&self, sid: &str) {
        if let Ok(mut g) = self.turn_errors.lock() {
            g.remove(sid);
        }
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

    /// 记一次 LLM 调用的用量（`agent.rs` 每步结束时调）：进库（`usage` 表，一步一行，
    /// 拿来分析的），也加到内存里那份（输入框那个上下文圆环读它）。
    ///
    /// 库的读写都丢给阻塞池：库锁和定时落盘是同一把，落盘在写大事务时，
    /// 直接在这里等会占住一个 worker。
    ///
    /// 返回记上这一次之后的会话累计（`data-usage` 事件带给前端，前端不再自己累加）。
    pub async fn record_usage(
        &self,
        row: &crate::db::UsageRow,
        v: &crate::openai::UsageView,
    ) -> Option<UsageStats> {
        // 内存里还没有（重启后头一次）：先从库里接上之前的，**再**写这一行 —— 反过来就算两遍
        let base = self.usage_of(&row.session_id).await;
        let stats = self.usage.lock().ok().map(|mut g| {
            let s = g
                .entry(row.session_id.clone())
                .or_insert(base)
                .get_or_insert_with(UsageStats::default);
            s.add(v);
            s.clone()
        });
        let Some(db) = self.db() else { return stats };
        let row = row.clone();
        let r = tokio::task::spawn_blocking(move || db.add_usage(&row)).await;
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("用量没记进库：{e:#}"),
            Err(e) => tracing::warn!("用量没记进库：{e}"),
        }
        stats
    }

    /// 这个会话的用量（上次清空之后；没跑过就是 `None`）。内存里没有就去库里读 ——
    /// 核心重启之后圆环接着显示，不用等下一次调用。
    pub async fn usage_of(&self, sid: &str) -> Option<UsageStats> {
        if let Some(u) = self.usage.lock().ok()?.get(sid) {
            return u.clone();
        }
        let u = match self.db() {
            Some(db) => {
                let sid = sid.to_string();
                tokio::task::spawn_blocking(move || db.session_usage(&sid))
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .map_err(|e| tracing::warn!("读用量失败：{e:#}"))
                    .ok()
                    .flatten()
            }
            None => None,
        };
        // 读库那会儿 `record_usage` 可能已经记上了：以内存里的为准
        let mut g = self.usage.lock().ok()?;
        g.entry(sid.to_string()).or_insert(u).clone()
    }

    /// 会话没了：内存里那份扔掉。库里的行**留着**（分析用，见 db.rs 的 `usage` 表）。
    pub fn forget_usage(&self, sid: &str) {
        if let Ok(mut g) = self.usage.lock() {
            g.remove(sid);
        }
    }

    /// 会话清空了：之前的用量不再算「当前上下文」。库里的行标成清掉了（不删）。
    pub fn clear_usage(&self, sid: &str) {
        // 先改库再清内存：反过来的话，中间插进来的 `usage_of` 会把库里的旧值缓存回去
        if let Some(db) = self.db() {
            if let Err(e) = db.clear_usage(sid) {
                tracing::warn!("标记用量失败：{e:#}");
            }
        }
        self.forget_usage(sid);
    }

    pub fn mark_sessions_dirty(&self) {
        self.sessions_dirty.store(true, Ordering::Relaxed);
    }

    /// 当前生效的系统提示词 = 基础那份 + 这台机器上装了什么（skill 索引）。
    ///
    /// 基础那份默认是编译进去的（按平台各一份，见 `prompt::system_prompt`），被
    /// `/api/debug/system-prompt` 覆盖过就是那一份。后面拼的部分是**动态**的，但只随
    /// 「装了什么」变，不随时间、屏幕、会话变：装的东西不变时，每个会话、每一轮拿到的
    /// 都是同一串字节，前缀缓存照常命中（见 `skills` 模块）。
    /// `Cow` 是为了什么都没装、也没覆盖时**不拷贝**那个常量。
    pub fn system_prompt(&self) -> Cow<'static, str> {
        let base: Cow<'static, str> = match self.system_prompt.read() {
            Ok(g) => match &*g {
                Some(s) => Cow::Owned(s.clone()),
                None => Cow::Borrowed(self.builtin_prompt()),
            },
            Err(_) => Cow::Borrowed(self.builtin_prompt()),
        };
        let extras = self.installed_prompt();
        if extras.is_empty() {
            base
        } else {
            Cow::Owned(format!("{base}\n\n{extras}"))
        }
    }

    /// 拼在系统提示词后面那段：MCP 那句（**没接也在**，说怎么接）+ skill 索引（内置的
    /// create-skill 一直在，它教模型怎么装别的）。
    fn installed_prompt(&self) -> String {
        let mut parts = vec![crate::mcp::prompt_line(&self.mcp.enabled_servers())];
        let skills = crate::skills::index(&self.skills());
        if !skills.is_empty() {
            parts.push(skills);
        }
        parts.join("\n")
    }

    /// 存一个 MCP 服务（库 + 内存）。同名的就是改：旧会话作废，下次重新握手。
    /// **不动启用状态**：开关是用户的，改配置不该顺手把它打开（新的默认启用）。
    pub async fn put_mcp_server(&self, srv: crate::mcp::Server) -> anyhow::Result<()> {
        if let Some(db) = self.db() {
            db.set_mcp_server(&srv.name, Some(&srv))?;
        }
        let name = srv.name.clone();
        let mut list: Vec<_> = self.mcp.servers().into_iter().filter(|s| s.name != name).collect();
        list.push(srv);
        self.mcp.set_servers(list);
        self.mcp.forget_session(&name).await;
        Ok(())
    }

    /// 删一个 MCP 服务。返回原来有没有。
    pub async fn remove_mcp_server(&self, name: &str) -> anyhow::Result<bool> {
        let list = self.mcp.servers();
        let had = list.iter().any(|s| s.name == name);
        if let Some(db) = self.db() {
            db.set_mcp_server(name, None)?;
            db.set_disabled("mcp", name, false)?;
        }
        self.mcp.set_enabled(name, true);
        self.mcp
            .set_servers(list.into_iter().filter(|s| s.name != name).collect());
        self.mcp.forget_session(name).await;
        Ok(had)
    }

    /// 启用 / 停用一个 MCP 服务（停用的不进提示词，模型调它会被拒）。
    pub async fn set_mcp_enabled(&self, name: &str, enabled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.mcp.servers().iter().any(|s| s.name == name),
            "没有叫 {name} 的 MCP 服务"
        );
        if let Some(db) = self.db() {
            db.set_disabled("mcp", name, !enabled)?;
        }
        self.mcp.set_enabled(name, enabled);
        self.mcp.forget_session(name).await;
        Ok(())
    }

    /// 启用 / 停用一个 skill（按目录名）。停用的文件留着，只是不进提示词的索引。
    pub fn set_skill_enabled(&self, dir: &str, enabled: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.skills().iter().any(|s| s.dir == dir),
            "没有叫 {dir} 的 skill"
        );
        if let Some(db) = self.db() {
            db.set_disabled("skill", dir, !enabled)?;
        }
        if let Ok(mut g) = self.disabled_skills.write() {
            if enabled {
                g.remove(dir);
            } else {
                g.insert(dir.to_string());
            }
        }
        Ok(())
    }

    /// 这台机器上装的 skill，带上启用状态（每次现扫：一个目录几十个 `SKILL.md`，
    /// 比缓存失效的麻烦便宜）。
    pub fn skills(&self) -> Vec<crate::skills::Skill> {
        let Some(sb) = self.linux() else {
            return Vec::new();
        };
        let (host, guest) = sb.skills_dir();
        let off = self.disabled_skills.read().map(|g| g.clone()).unwrap_or_default();
        crate::skills::scan(&host, &guest)
            .into_iter()
            .map(|mut s| {
                s.enabled = !off.contains(&s.dir);
                s
            })
            .collect()
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
        // 读锁只拿到算完计划（比对 + 序列化）：写盘的时候回合照样能往会话里追加
        let plan = {
            let store = self
                .chat
                .try_read()
                .map_err(|_| anyhow::anyhow!("会话被占用，跳过本次落盘"))?;
            db.plan(&store)?
        };
        match plan {
            Some(p) => db.write(p),
            None => Ok(()),
        }
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

    /// provider 配置存进库的 `llm` 表（桌面开发没有库，只留在内存里）。
    ///
    /// 用 `LlmConfig` 而不是直接存 `Provider`：后者的 api_key 是 skip_serializing
    /// （防止泄给前端），一不小心就存丢了。
    fn persist_provider(&self, p: &Provider) {
        let Some(db) = self.db() else { return };
        let rec = crate::db::LlmConfig {
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            model: p.model.clone(),
            api_key: p.api_key.clone(),
            supports_images: p.supports_images,
            supports_files: p.supports_files,
            api_style: p.api_style,
            reasoning_effort: p.reasoning_effort.clone(),
        };
        if let Err(e) = db.set_llm(Some(&rec)) {
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
        let db = self.db();
        let resolved =
            crate::models::vision_for(provider, &provider.model, false, db.as_deref()).await;
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
        let db = self.db();
        let resolved =
            crate::models::pdf_for(provider, &provider.model, false, db.as_deref()).await;
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
        let db = self.db();
        crate::models::list(&provider, refresh, db.as_deref()).await
    }

    /// 启动时从库里恢复 provider。
    fn load_llm(&self, db: &Db) {
        match db.llm() {
            Ok(Some(rec)) => {
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
            Ok(None) => {}
            Err(e) => self.log("warn", format!("读取 provider 配置失败: {e:#}")),
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

/// 随机凭据：122 位随机数的十六进制（UUID v4 去掉版本位之外全是随机的）。
///
/// 访问凭据、沙箱钥匙、隧道钥匙都从这来。
pub(crate) fn random_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 两个字符串是否相等，耗时和内容无关（别让比较的快慢泄露凭据的前缀）。
///
/// 比凭据的地方全用这一个（`http.rs` 的鉴权、`tunnel.rs` 的隧道钥匙）。
pub(crate) fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
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
/// `last` 是「**现在这个上下文有多大**」：一次请求发出去的 `input` 就是当时
/// 整段历史 + 系统提示词 + 工具定义的大小，所以它比累计值更能说明「还剩多少余量」。
///
/// 桶是**互斥**的（见 `openai::UsageView`）：`input = uncached_input +
/// cache_read + cache_write`，加起来不会重复。归一（含命中率）只在
/// `Usage::view()` 里做一次。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageStats {
    /// 最近一次 LLM 调用（已归一化）
    pub last: Option<crate::openai::UsageView>,
    /// 本会话累计（同一个互斥桶口径）
    pub input: u64,
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
    pub calls: u64,
    /// 报了缓存字段的那些调用的输入之和 —— 累计命中率的分母（见
    /// `openai::cache_hit_ratio`）。provider 没报缓存的调用不进这里。
    pub cache_reported_input: u64,
    /// 本会话累计命中率（核心算好，前端只读）。一次都没报过缓存是 `None`
    pub cache_hit_ratio: Option<f64>,
}

impl UsageStats {
    fn add(&mut self, v: &crate::openai::UsageView) {
        self.last = Some(v.clone());
        if v.cache_hit_ratio.is_some() {
            self.cache_reported_input += v.input as u64;
        }
        self.input += v.input as u64;
        self.uncached_input += v.uncached_input as u64;
        self.cache_read += v.cache_read as u64;
        self.cache_write += v.cache_write as u64;
        self.output += v.output as u64;
        self.reasoning += v.reasoning as u64;
        self.calls += 1;
        self.cache_hit_ratio =
            crate::openai::cache_hit_ratio(self.cache_read, self.cache_reported_input);
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
        // 后面拼的那段（MCP / skill）不在这里比：只比基础那份选得对不对
        let st = AppState::new();
        assert!(
            st.system_prompt()
                .starts_with(crate::agent::system_prompt(crate::platform::Platform::Android)),
            "还没有沙箱时（安卓 boot 阶段 / 纯 Rust 环境）用手机那份"
        );

        st.set_linux(Arc::new(LinuxSandbox::host(std::env::temp_dir())));
        assert!(st
            .system_prompt()
            .starts_with(crate::agent::system_prompt(crate::platform::Platform::host())));

        // 运行时覆盖照旧最高优先（`/api/debug/system-prompt` 调提示词那条路）
        st.set_system_prompt(Some("改过的".into()));
        assert!(st.system_prompt().starts_with("改过的\n\n"));
    }

    /// 配置按列存在表里：改模型、改 Key 都落在表里，重开还在，凭据也不变。
    #[test]
    fn settings_live_in_tables_and_survive_a_restart() {
        let dir = temp_dir("settings");
        let st = AppState::new();
        st.open_db(&dir);
        st.set_llm(Provider::compatible("go", "https://x/v1", "m1", "sk-1"));
        assert_eq!(st.llm().unwrap().model, "m1");
        st.adopt_provider(&st.llm().unwrap());
        assert!(
            st.provider_creds("custom").is_some(),
            "在用的那家记成已配置"
        );

        // 改模型 → 存进表；重开之后还在，凭据也不变
        let mut p2 = st.llm().unwrap();
        p2.model = "m2".into();
        st.set_llm(p2);
        let token = st.auth().token;
        let key = st.auth().sandbox_key;
        drop(st);

        let st = AppState::new();
        st.open_db(&dir);
        assert_eq!(st.llm().unwrap().model, "m2");
        assert!(st.provider_creds("custom").is_some());
        assert_eq!(st.auth().token, token, "凭据跨重启不变，调试脚本拿一次就够");
        assert_eq!(st.auth().sandbox_key, key);
        assert!(token.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 装了 skill，系统提示词就带上它的索引（拼在基础那份后面）；没变时每次拿到的
    /// 是同一串字节（前缀缓存靠这个）。没有沙箱 / 没装就是基础那份本身。
    #[test]
    fn installed_skills_are_indexed_in_the_system_prompt() {
        let root = std::env::temp_dir().join(format!("agent-skill-state-{}", uuid::Uuid::now_v7()));
        let st = AppState::new();
        st.set_linux(Arc::new(crate::linux::LinuxSandbox::new(
            crate::linux::SandboxConfig {
                proot: "/nonexistent/proot".into(),
                rootfs: root.clone(),
                binds: vec![],
                tmp: None,
                dns: vec![],
                tz_offset_minutes: None,
                loader: None,
                loader32: None,
                env: Default::default(),
                workspace: Some(root.join("ws")),
                shared: None,
            },
        )));
        let bare = st.system_prompt().into_owned();
        assert!(bare.contains("agent mcp add"), "没接也要说怎么接：{bare}");
        assert!(!bare.contains("技能（skill）"), "一个 skill 都没有就不列：{bare}");

        std::fs::create_dir_all(root.join("ws/skills/pdf")).unwrap();
        std::fs::write(
            root.join("ws/skills/pdf/SKILL.md"),
            "---\nname: pdf\ndescription: 处理 PDF\n---\n正文不进提示词\n",
        )
        .unwrap();
        let p = st.system_prompt().into_owned();
        assert!(p.starts_with(&bare), "装之前那份原样在前面（只在末尾多出索引）");
        assert!(p.contains("技能（skill）"), "{p}");
        assert!(p.contains("pdf：处理 PDF（/workspace/skills/pdf/SKILL.md）"), "{p}");
        assert!(!p.contains("正文不进提示词"));
        assert_eq!(st.system_prompt(), p, "没变就逐字节一样");

        // 停用：文件留着、设置页看得到，但不进提示词；再打开就回来
        st.set_skill_enabled("pdf", false).unwrap();
        assert!(!st.skills()[0].enabled);
        assert_eq!(st.system_prompt(), bare, "停用了就和没装一样");
        st.set_skill_enabled("pdf", true).unwrap();
        assert_eq!(st.system_prompt(), p);
        assert!(st.set_skill_enabled("nope", false).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn usage_is_per_session_and_accumulates() {
        use crate::openai::{PromptTokensDetails, Usage};
        let st = AppState::new();
        assert!(st.usage_of("a").await.is_none(), "没跑过就是 None");

        let u1 = Usage {
            prompt_tokens: 1000,
            completion_tokens: 50,
            total_tokens: 1050,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(800),
            }),
            ..Default::default()
        };
        st.record_usage(&usage_row("a"), &u1.view()).await;
        let s = st.usage_of("a").await.unwrap();
        // 互斥桶：input 含缓存，uncached + cache_read = input
        assert_eq!(
            (
                s.input,
                s.uncached_input,
                s.cache_read,
                s.cache_write,
                s.output,
                s.calls
            ),
            (1000, 200, 800, 0, 50, 1)
        );
        let last = s.last.as_ref().unwrap();
        assert_eq!(
            (last.input, last.uncached_input, last.cache_read),
            (1000, 200, 800)
        );
        assert_eq!(last.cache_hit_ratio, Some(0.8));

        // 第二步：last 换成新的，累计往上加。DeepSeek 系那套字段名也要认
        let u2 = Usage {
            prompt_tokens: 1500,
            completion_tokens: 20,
            prompt_cache_hit_tokens: Some(1400),
            ..Default::default()
        };
        st.record_usage(&usage_row("a"), &u2.view()).await;
        let s = st.usage_of("a").await.unwrap();
        assert_eq!(
            (s.input, s.uncached_input, s.cache_read, s.output, s.calls),
            (2500, 300, 2200, 70, 2)
        );
        assert_eq!(s.last.as_ref().unwrap().input, 1500);

        assert!(st.usage_of("b").await.is_none(), "另外那个会话不受影响");

        // 会话删了 / 被清空 → 它的用量不该留着
        st.forget_usage("a");
        assert!(st.usage_of("a").await.is_none());
    }

    fn usage_row(sid: &str) -> crate::db::UsageRow {
        crate::db::UsageRow {
            session_id: sid.into(),
            provider: "p".into(),
            model: "m".into(),
            ..Default::default()
        }
    }

    /// 用量进库：核心重启（同一个库、新的 AppState）之后圆环接着显示，接着累加时
    /// 不把库里那几步再算一遍；清空会话之后从零算。
    #[tokio::test]
    async fn usage_survives_a_restart_and_clearing_starts_from_zero() {
        use crate::openai::Usage;
        let db = Arc::new(Db::in_memory().unwrap());
        let with_db = || {
            let st = AppState::new();
            *st.db.write().unwrap() = Some(db.clone());
            st
        };
        let step = |input: u32, output: u32| {
            let u = Usage {
                prompt_tokens: input,
                completion_tokens: output,
                ..Default::default()
            };
            let row = crate::db::UsageRow {
                input,
                output,
                ..usage_row("a")
            };
            (row, u)
        };

        let st = with_db();
        let (r, u) = step(1000, 50);
        st.record_usage(&r, &u.view()).await;
        let (r, u) = step(1500, 20);
        st.record_usage(&r, &u.view()).await;

        // 「重启」：内存里什么都没有，从库里接上
        let st = with_db();
        let s = st.usage_of("a").await.expect("库里有");
        assert_eq!((s.input, s.output, s.calls), (2500, 70, 2));
        assert_eq!(s.last.as_ref().unwrap().input, 1500);

        // 重启后**没先问过** usage_of 就来了一步：也得先接上库里的，再加这一步
        let st = with_db();
        let (r, u) = step(2000, 10);
        st.record_usage(&r, &u.view()).await;
        let s = st.usage_of("a").await.unwrap();
        assert_eq!((s.input, s.calls), (4500, 3), "不多算、不漏算");

        // 清空会话：圆环从零开始；重启之后也一样
        st.clear_usage("a");
        assert!(st.usage_of("a").await.is_none());
        assert!(with_db().usage_of("a").await.is_none());
        let (r, u) = step(300, 5);
        st.record_usage(&r, &u.view()).await;
        let s = with_db().usage_of("a").await.unwrap();
        assert_eq!((s.input, s.calls), (300, 1));
    }

    /// 累计命中率的分母只算**报了缓存字段**的调用：没报的那次输入进了分母、命中却是 0，
    /// 会把命中率白白压低。内存累加和从库里重建（重启后）两条路得到同一个数。
    #[tokio::test]
    async fn cumulative_hit_ratio_ignores_calls_that_did_not_report_cache() {
        use crate::openai::{PromptTokensDetails, Usage};
        let db = Arc::new(Db::in_memory().unwrap());
        let with_db = || {
            let st = AppState::new();
            *st.db.write().unwrap() = Some(db.clone());
            st
        };
        let st = with_db();
        assert_eq!(UsageStats::default().cache_hit_ratio, None);

        // 报了：1000 里命中 900
        let reported = Usage {
            prompt_tokens: 1000,
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(900),
            }),
            ..Default::default()
        };
        let row = crate::db::UsageRow {
            input: 1000,
            cached: Some(900),
            ..usage_row("a")
        };
        let s = st.record_usage(&row, &reported.view()).await.unwrap();
        assert_eq!(s.cache_hit_ratio, Some(0.9));

        // 没报：3000 的输入不该把命中率拉到 900 / 4000
        let silent = Usage {
            prompt_tokens: 3000,
            ..Default::default()
        };
        let row = crate::db::UsageRow {
            input: 3000,
            ..usage_row("a")
        };
        let s = st.record_usage(&row, &silent.view()).await.unwrap();
        assert_eq!((s.input, s.cache_reported_input), (4000, 1000));
        assert_eq!(s.cache_hit_ratio, Some(0.9));

        // 重启之后从库里重建：同一个数
        let s = with_db().usage_of("a").await.unwrap();
        assert_eq!((s.input, s.cache_reported_input), (4000, 1000));
        assert_eq!(s.cache_hit_ratio, Some(0.9));
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
                .push(crate::agent::ChatMessage::user("留着"));
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
                    .push(crate::agent::ChatMessage::user(format!("第{i}个")));
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
            .push(crate::agent::ChatMessage::user("没落盘"));
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
            crate::agent::ChatMessage::User {
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

    fn snap(sid: &str, active: bool) -> crate::status::Snapshot {
        crate::status::Snapshot {
            sid: sid.into(),
            title: String::new(),
            phase: if active {
                crate::status::Phase::Thinking
            } else {
                crate::status::Phase::Done
            },
            tool: None,
            detail: None,
            started_at_ms: 1,
            active,
            finished_at_ms: (!active).then_some(2),
            ok: true,
            stopped: false,
        }
    }

    /// 回合状态是**总线上的事件**。这里钉三件事：实时广播、晚连上来的订阅者能补到
    /// 当前状态、以及**完事的回合不重放**（重放 = 壳每次重连都再发一次「已完成」）。
    #[test]
    fn turn_status_is_broadcast_and_only_live_turns_are_replayed() {
        let st = AppState::new();
        let mut rx = st.tx.subscribe();
        let turn = st.begin_turn("s1").unwrap();

        st.push_turn_status(snap("s1", true));
        // 注意 `begin_turn` 自己也会发一条 `TurnActive`（起前台服务用），所以要挑出来看
        let mut told = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::Turn { .. }) {
                told = true;
            }
        }
        assert!(told, "状态要发到总线上");

        let snap_now = st.turn_snapshot();
        assert!(
            snap_now
                .iter()
                .any(|e| matches!(e, Event::TurnActive { active: true })),
            "{snap_now:?}"
        );
        assert_eq!(
            snap_now
                .iter()
                .filter(|e| matches!(e, Event::Turn { .. }))
                .count(),
            1
        );

        // 回合结束：结束那一条照样广播，但之后连上来的不再补到它
        st.push_turn_status(snap("s1", false));
        st.end_turn("s1");
        let mut idle = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, Event::TurnActive { active: false }) {
                idle = true;
            }
        }
        assert!(idle, "回合结束要告诉订阅者");
        assert!(
            !st.turn_snapshot()
                .iter()
                .any(|e| matches!(e, Event::Turn { .. })),
            "完事的回合不该重放"
        );
        drop(turn);
    }
}
