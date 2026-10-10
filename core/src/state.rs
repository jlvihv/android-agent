//! 全局状态与边界数据模型。
//!
//! 设计铁律（见 docs/01-tech-selection.md §4）：
//! native 侧不反调 Java。Kotlin 把 UI 树拍平成扁平 JSON 送进来，
//! Rust 只回传指令。所以这里所有跨边界类型都是 JSON 可序列化的。

use std::sync::atomic::{AtomicBool, AtomicU16};use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agent::SessionStore;
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
    /// 信任模式。
    ///
    /// **默认开启** —— agent 的所有操作直接放行，不弹审批。
    /// 改回逐个人工审批只需把 `new(false)` 改掉（或以后接个 UI 开关）。
    pub auto_approve: AtomicBool,
    /// LLM provider。
    ///
    /// 安卓上**没法用环境变量**（App 不继承 shell 环境），所以运行时
    /// 通过 `POST /api/llm` 注入，并持久化到 `<filesDir>/llm.json`。
    /// 桌面上启动时从环境变量播种。
    pub llm: RwLock<Option<Provider>>,
    /// 持久化目录（安卓上是 `<filesDir>`；桌面上为空则只存内存）
    pub data_dir: RwLock<Option<std::path::PathBuf>>,
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
            // 默认全部放行
            auto_approve: AtomicBool::new(true),
            llm: RwLock::new(None),
            data_dir: RwLock::new(None),
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

    pub fn llm(&self) -> Option<Provider> {
        self.llm.read().ok().and_then(|g| g.clone())
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

    pub fn auto_approve(&self) -> bool {
        self.auto_approve.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_auto_approve(&self, on: bool) {
        self.auto_approve
            .store(on, std::sync::atomic::Ordering::Relaxed);
        self.log(
            "info",
            if on {
                "信任模式：已开启，工具调用不再逐个审批"
            } else {
                "信任模式：已关闭"
            },
        );
    }

    pub fn set_linux(&self, sb: Arc<LinuxSandbox>) {
        let rootfs = sb.rootfs().display().to_string();
        if let Ok(mut g) = self.linux.write() {
            *g = Some(sb);
        }
        self.log("info", format!("Linux 沙箱就绪: {rootfs}"));
    }
}
