//! 全局状态与边界数据模型。
//!
//! 设计铁律（见 docs/01-tech-selection.md §4）：
//! native 侧不反调 Java。Kotlin 把 UI 树拍平成扁平 JSON 送进来，
//! Rust 只回传指令。所以这里所有跨边界类型都是 JSON 可序列化的。

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent::SessionStore;
use crate::db::Db;
use crate::linux::LinuxSandbox;
use crate::llm::Provider;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// 设备侧 → Rust：UI 快照
// ---------------------------------------------------------------------------

/// 剪枝 + 扁平化后的 UI 快照（决策 D10）。
///
/// 树结构用 `parent` 索引表达，避免嵌套 JSON —— 无障碍树可以有几层深、
/// 几百个节点，嵌套结构在 Kotlin 侧序列化和 Rust 侧反序列化都更贵。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSnapshot {
    /// Unix 毫秒
    pub ts: u64,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub activity: String,
    #[serde(default)]
    pub screen: Screen,
    pub nodes: Vec<Node>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Screen {
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Node {
    /// 节点索引，也是指令里引用的句柄
    pub i: u32,
    /// 父节点索引；-1 表示根
    pub p: i32,
    /// 类名简写，如 `Button` / `TextView` / `RecyclerView`
    pub c: String,
    /// 可见文本
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub t: String,
    /// `viewIdResourceName`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// bounds `[left, top, right, bottom]`
    #[serde(default)]
    pub b: [i32; 4],
    /// 能力标志
    #[serde(default)]
    pub a: Flags,
    /// 状态描述，如 `checked=true`、`scrollable`、`password`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Flags {
    pub clickable: bool,
    pub long_clickable: bool,
    pub editable: bool,
    pub scrollable: bool,
    pub checkable: bool,
    pub focusable: bool,
    pub enabled: bool,
    pub selected: bool,
}

impl Flags {
    /// 是否值得保留在快照里（剪枝依据）。
    pub fn interesting(&self) -> bool {
        self.clickable
            || self.long_clickable
            || self.editable
            || self.scrollable
            || self.checkable
            || self.focusable
    }
}

// ---------------------------------------------------------------------------
// Rust → 设备侧：指令
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// 请求重新读取 UI 树
    Snapshot,
    /// 点击某个节点（由 Kotlin 侧解析成坐标 + performAction）
    TapNode { i: u32 },
    TapXy { x: i32, y: i32 },
    Swipe {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[serde(default = "default_swipe_ms")]
        ms: u32,
    },
    SetText { i: u32, text: String },
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
    /// 新的 UI 快照
    Snapshot { data: UiSnapshot },
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
}

pub struct AppState {
    pub tx: broadcast::Sender<Event>,
    pub snapshot: RwLock<Option<UiSnapshot>>,
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
}

impl AppState {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(Self {
            tx,
            snapshot: RwLock::new(None),
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

    pub fn latest_snapshot(&self) -> Option<UiSnapshot> {
        self.snapshot.read().ok().and_then(|g| g.clone())
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
        if let Some(dir) = persist_dir {
            let f = dir.join("llm.json");
            // 用独立的持久化结构，**不能**直接序列化 Provider ——
            // 它的 api_key 是 skip_serializing（防止泄给前端），
            // 直接存的话 key 就丢了。
            let rec = PersistedProvider {
                name: p.name.clone(),
                base_url: p.base_url.clone(),
                model: p.model.clone(),
                api_key: p.api_key.clone(),
            };
            match serde_json::to_vec_pretty(&rec) {
                Ok(body) => {
                    if let Err(e) = std::fs::write(&f, body) {
                        tracing::warn!("写 {} 失败: {e}", f.display());
                    }
                }
                Err(e) => tracing::warn!("序列化 provider 失败: {e}"),
            }
        }
        let name = p.name.clone();
        let model = p.model.clone();
        if let Ok(mut g) = self.llm.write() {
            *g = Some(p);
        }
        self.log("info", format!("LLM 已配置: {name} / {model}"));
    }

    /// 启动时从磁盘恢复
    pub fn load_llm(&self, dir: &std::path::Path) {
        let f = dir.join("llm.json");
        let Ok(body) = std::fs::read(&f) else { return };
        match serde_json::from_slice::<PersistedProvider>(&body) {
            Ok(rec) => {
                let name = rec.name.clone();
                let model = rec.model.clone();
                let p = Provider::compatible(rec.name, rec.base_url, rec.model, rec.api_key);
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
