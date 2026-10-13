//! 桌面（Linux / macOS）上的浏览器：**无头 Chrome + CDP**。
//!
//! 手机那边的浏览器是壳里的一只无头 `WebView`（`BrowserController.kt`）。
//! 桌面上没有壳，但 `BrowserApi` 本来就是个 trait —— 这是它的第三个实现
//! （前两个：Kotlin 的 WebView、测试里的 Echo/RecordingBrowser）。
//!
//! ## 为什么是 CDP，不是嵌一个 WebView
//!
//! Linux 上能嵌的 WebView 不止一个（WebKitGTK / CEF / QtWebEngine / Electron），
//! 但都不划算：
//!
//! - 它们各自要把一个 C/C++ 运行时（+ GLib 主循环、或几十 MB 的 CEF 包）链进
//!   `.so`，还要装一堆 `-dev` 包才能构建；
//! - WebKitGTK 是 **WebKit**，而安卓的 WebView 是 **Chromium** —— 注入的 JS
//!   在两边不保证同一种行为；
//! - 而 CDP 是「WebSocket + JSON」，引擎和安卓那边同为 Chromium，
//!   **不需要任何新依赖**（`tungstenite` 早就因为 axum 在依赖树里了，
//!   见 `Cargo.toml` 里那条注释）。
//!
//! 这也和项目既有的取舍一致：LLM 协议自己写（不用 SDK），CDP 也自己写。
//!
//! ## Chrome 由**核心自己起**
//!
//! 用的人不该先另开一个终端去起浏览器。第一次用到它的时候，核心拉起一个无头
//! Chrome：当场挑一个空闲端口（不是写死 9222 —— 同一台机器上跑两个核心不会互相
//! 抢）、独立的 profile、日志落在 `<profile>/chrome.log`。生命周期跟着核心：
//! 正常退出时 `Drop` 杀掉它，被 SIGKILL 的话下次启动时 `sweep_orphan_chromes`
//! 收掉上次留下的。
//!
//! 要**用你自己那个浏览器**（能看着它动）或者接到别的机器上：
//! `AGENT_CDP=http://127.0.0.1:9222`（自己起一个带 `--remote-debugging-port` 的）；
//! `AGENT_CDP=off` 是不要浏览器；找不到 Chrome 时用 `AGENT_CHROME` 指可执行文件。
//! 连不上时报的错里会写清楚这两条路。
//!
//! ## 注入的 JS 只有一份
//!
//! 页面里那段逻辑（骨架 / 正文 / 点击 / 输入 / 按键 / 滚动 / 找元素 / DOM 指纹 /
//! 页面信息 / `target=_blank` 修正）在 `crate::browser_js`，手机上的 WebView
//! 也是从那儿取（uniffi 的 `AgentCore.browserJs`）—— 见 docs/08 第 15 节。
//! 动作**名字**这一层也有测试盯着：`ACTIONS` 和沙箱里 `agent browser` 的 case 表
//! （`agent_cli_and_cdp_agree_on_the_action_list`）。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use crate::gc::ShotDir;
use crate::state::BrowserApi;

/// 一次 `Runtime.evaluate` 的预算。健康的页面毫秒级就回来。
const JS_TIMEOUT: Duration = Duration::from_secs(15);

/// 一般 CDP 命令（attach / 建标签 / 截图）的预算。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 等自己起的 Chrome 把调试端口听上：冷启动一般 < 1s，慢机器给足
const CHROME_BOOT_TIMEOUT: Duration = Duration::from_secs(20);

/// navigate 等 `Page.loadEventFired` 的上限。等不到**不算失败**（SPA 常常不触发）
const NAV_TIMEOUT: Duration = Duration::from_secs(25);

/// 页面安定判定：指纹多久没变就算稳了；总预算多少。
const SETTLE_MS: Duration = Duration::from_millis(300);
const SETTLE_BUDGET: Duration = Duration::from_secs(4);

/// 骨架树给模型的文本上限（它会进历史，按钱算）
const BACKBONE_MAX_CHARS: usize = 12_000;

/// 正文 / JS 返回值上限
const TEXT_MAX_CHARS: usize = 12_000;

/// 标签页上限（和 Kotlin 一致：三页以上模型自己就转不过来了）
const MAX_TABS: usize = 4;

/// 截图质量。截图进的是模型上下文，不是相册。
const SHOT_QUALITY: u32 = 80;

/// UA / 视口两个档位。**两个必须一起换**：只换 UA 不换视口的话，
/// 桌面版布局塞进手机宽度里，排版比移动版还乱。
const DESKTOP_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";
const MOBILE_UA: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/134.0.0.0 Mobile Safari/537.36";

/// 支持的动作。**和沙箱里 `agent browser` 的 case 表必须一致** ——
/// 漂了就会出现「CLI 发得出去、后端不认」的静默失败（`new_tab` 那次把 URL
/// 吃掉就是这个类别）。测试 `agent_cli_and_cdp_agree_on_the_action_list` 盯着。
pub const ACTIONS: &[&str] = &[
    "navigate",
    "get_backbone",
    "click",
    "type",
    "get_text",
    "get_readable",
    "wait_for_dom_stable",
    "scroll",
    "screenshot",
    "back",
    "press",
    "find_elements",
    "set_user_agent",
    "js",
    "new_tab",
    "list_tabs",
    "switch_tab",
    "close_tab",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Desktop,
    Mobile,
}

impl Profile {
    fn ua(self) -> &'static str {
        match self {
            Profile::Desktop => DESKTOP_UA,
            Profile::Mobile => MOBILE_UA,
        }
    }

    fn viewport(self) -> (u32, u32, f64, bool) {
        // (宽, 高, deviceScaleFactor, mobile)
        match self {
            Profile::Desktop => (1180, 820, 1.0, false),
            Profile::Mobile => (412, 915, 2.6, true),
        }
    }
}

/// 无头 Chrome 上的一个浏览器。
///
/// 两种来源：
/// - **自己起**（默认）：第一次要动作时拉起一个无头 Chrome（挑一个空闲端口、
///   独立 profile），进程跟着核心一起死。**这才是常规路径** —— 用的人不该先
///   去另开一个终端起浏览器。
/// - **连别人的**：`AGENT_CDP=http://127.0.0.1:9222` 指到已经在跑的那个上
///   （比如你自己那个带 `--remote-debugging-port` 的可见 Chrome，能看着它动）。
pub struct CdpBrowser {
    /// `AGENT_CDP` 指的地址；自己起的时候是空的（端口起来才知道）
    endpoint: String,
    /// 自己起 Chrome 吗
    managed: bool,
    /// 截图落哪（宿主目录 + 历史里存的前缀）
    shots: ShotDir,
    /// 用户按了停止：正在跑的动作在下一个检查点退出
    cancelled: AtomicBool,
    inner: Mutex<Inner>,
}

/// 自己起的那个 Chrome 进程。`Drop` 就杀掉它 —— 核心退出 / 浏览器被换掉时
/// 不留孤儿进程。
struct Chrome {
    child: std::process::Child,
    /// 用的哪个 profile（日志里报出来，排查「登录态怎么没了」时要用）
    profile: PathBuf,
    /// Chrome 自己的 stdout/stderr 落在这儿（起不来时它是唯一线索）
    log: PathBuf,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 连接 + 标签页。整个 `execute` 期间被 Mutex 串住：两个动作交叉执行会让
/// `navigate A → click` 点到别的页面上（和 Kotlin 那个 `lock` 同一个理由）。
struct Inner {
    conn: Option<Conn>,
    /// 自己起的 Chrome（连别人的时候是 None）
    chrome: Option<Chrome>,
    /// 实际连的地址（自己起的话 = 起来之后才知道的那个端口）
    endpoint: String,
    tabs: Vec<Tab>,
    current: usize,
    next_id: i32,
    profile: Profile,
}

struct Tab {
    /// 模型看到的编号（1 起，不随关闭重排 —— `switch_tab` 用的是它）
    id: i32,
    target: String,
    session: String,
    url: String,
    title: String,
}

impl CdpBrowser {
    /// 连一个**已经在跑**的 Chrome（`AGENT_CDP` 指的那个）
    pub fn connect_to(endpoint: String, shots: ShotDir) -> Self {
        Self {
            inner: Mutex::new(Inner {
                conn: None,
                chrome: None,
                endpoint: endpoint.clone(),
                tabs: Vec::new(),
                current: 0,
                next_id: 1,
                profile: Profile::Desktop,
            }),
            endpoint,
            managed: false,
            shots,
            cancelled: AtomicBool::new(false),
        }
    }

    /// 自己起一个无头 Chrome（端口起来之后才知道，所以先空着）
    pub fn managed(shots: ShotDir) -> Self {
        Self {
            endpoint: String::new(),
            managed: true,
            shots,
            cancelled: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                conn: None,
                chrome: None,
                endpoint: String::new(),
                tabs: Vec::new(),
                current: 0,
                next_id: 1,
                profile: Profile::Desktop,
            }),
        }
    }

    /// 连的是哪个地址（启动日志 / 体检用）。自己起的时候要等它起来才有值。
    pub fn endpoint(&self) -> String {
        if self.managed {
            self.inner
                .lock()
                .ok()
                .map(|g| g.endpoint.clone())
                .unwrap_or_default()
        } else {
            self.endpoint.clone()
        }
    }

    /// 从环境变量构造（桌面用）。
    ///
    /// - 没设 `AGENT_CDP`：**自己起**一个无头 Chrome（常规路径）
    /// - `AGENT_CDP=http://…`：连那个已经在跑的（自己的可见 Chrome 也行）
    /// - `AGENT_CDP=off` / `0` / `no`：不要浏览器
    pub fn from_env(shots: ShotDir) -> Option<Self> {
        let ep = std::env::var("AGENT_CDP")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match ep.as_deref() {
            Some("off") | Some("0") | Some("no") => None,
            Some(url) => Some(Self::connect_to(url.to_string(), shots)),
            None => Some(Self::managed(shots)),
        }
    }

    fn dispatch(&self, g: &mut Inner, name: &str, a: &Value) -> Result<Value, String> {
        // 先认名字：不认识的动作不该顺手去开标签（那会先去连 Chrome，
        // 报出来的是一句「连不上」，把「动作写错了」这件事盖掉）
        if !ACTIONS.contains(&name) {
            return Err(format!("未知动作: {name}（支持：{}）", ACTIONS.join(" ")));
        }
        // 需要页面的动作：没有标签就先开一只（空白页）
        if name != "list_tabs" && name != "new_tab" && g.tabs.is_empty() {
            self.open_tab(g, None)?;
        }
        match name {
            "navigate" => {
                let url = a.get("url").and_then(|v| v.as_str()).unwrap_or("").trim();
                if url.is_empty() {
                    return Err("缺少参数 url".into());
                }
                let target =
                    normalize_url(url).ok_or_else(|| format!("只支持 http/https：{url}"))?;
                let idx = g.current;
                let session = g.tabs[idx].session.clone();
                let conn = self.ensure_conn(g)?;
                conn.drain_events();
                conn.call(Some(&session), "Page.navigate", json!({ "url": target }))?;
                let loaded = conn.wait_event(&session, "Page.loadEventFired", NAV_TIMEOUT)?;
                self.await_dom_stable(conn, &session, SETTLE_BUDGET)?;
                install_target_fix(conn, &session);
                let info = page_info(conn, &session)?;
                let real = info
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&target)
                    .to_string();
                let title = info
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.remember(g, real.clone(), title.clone());
                let mut text = format!("已打开 {real}");
                if !title.is_empty() {
                    text.push_str(&format!("\n标题：{title}"));
                }
                if !loaded {
                    // SPA 经常不触发 loadEventFired；不是失败，如实说
                    text.push_str("\n（页面可能还在加载）");
                }
                Ok(ok(&text, Some(real), Some(title)))
            }
            "get_backbone" => {
                let depth = a
                    .get("max_depth")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(5)
                    .clamp(1, 10);
                let js = page_js("backbone", json!({ "max_depth": depth }))?;
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                let mut text = out
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.chars().count() > BACKBONE_MAX_CHARS {
                    text = clip(&text, BACKBONE_MAX_CHARS);
                    text.push_str("\n…（树太长，已截断；用 selector 缩小范围或调小 max_depth）");
                }
                let nodes = out.get("nodes").and_then(|v| v.as_i64()).unwrap_or(0);
                let d = out.get("depth").and_then(|v| v.as_i64()).unwrap_or(0);
                Ok(ok(
                    &format!("页面结构（{nodes} 个节点，深度 {d}）：\n{text}"),
                    None,
                    None,
                ))
            }
            "click" => {
                let sel = a.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                let js = if !sel.is_empty() {
                    page_js("click_sel", json!({ "selector": sel }))?
                } else if let (Some(x), Some(y)) = (
                    a.get("x").and_then(|v| v.as_i64()),
                    a.get("y").and_then(|v| v.as_i64()),
                ) {
                    page_js("click_xy", json!({ "x": x, "y": y }))?
                } else {
                    return Err("click 需要 selector，或者 x + y".into());
                };
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                self.settle(conn, &session)?;
                let tag = out.get("tag").and_then(|v| v.as_str()).unwrap_or("");
                let text = clip(out.get("text").and_then(|v| v.as_str()).unwrap_or(""), 80);
                let matched = out.get("matched").and_then(|v| v.as_i64()).unwrap_or(1);
                let extra = if matched > 1 {
                    format!("（selector 匹配到 {matched} 个，点的是第一个）")
                } else {
                    String::new()
                };
                let info = page_info(conn, &session)?;
                Ok(with_page_info(
                    ok(&format!("已点击 <{tag}> {text}{extra}"), None, None),
                    &info,
                ))
            }
            "type" => {
                let sel = a.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                if sel.is_empty() {
                    return Err("缺少参数 selector".into());
                }
                let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js("type", json!({ "selector": sel, "text": text }))?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                self.settle(conn, &session)?;
                let n = out.get("length").and_then(|v| v.as_i64()).unwrap_or(0);
                let matched = out.get("matched").and_then(|v| v.as_i64()).unwrap_or(1);
                let extra = if matched > 1 {
                    format!("（selector 匹配到 {matched} 个，填的是第一个）")
                } else {
                    String::new()
                };
                Ok(ok(
                    &format!("已往 {sel} 输入 {n} 个字符{extra}"),
                    None,
                    None,
                ))
            }
            "get_text" => {
                let sel = a.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js("get_text", json!({ "selector": sel }))?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                let mut text = out
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let total = out.get("length").and_then(|v| v.as_i64()).unwrap_or(0);
                if text.chars().count() > TEXT_MAX_CHARS {
                    text = clip(&text, TEXT_MAX_CHARS);
                }
                let head = if sel.is_empty() {
                    format!("正文（{total} 字）：")
                } else {
                    format!("{sel} 的文本（{total} 字）：")
                };
                Ok(ok(&format!("{head}\n{text}"), None, None))
            }
            "get_readable" => {
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js("readable", json!({}))?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                let mut text = out
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if text.chars().count() > TEXT_MAX_CHARS {
                    text = clip(&text, TEXT_MAX_CHARS);
                }
                let src = out.get("source").and_then(|v| v.as_str()).unwrap_or("");
                let title = out.get("title").and_then(|v| v.as_str()).unwrap_or("");
                Ok(ok(
                    &format!("正文（{src}）：\n{title}\n\n{text}"),
                    None,
                    None,
                ))
            }
            "wait_for_dom_stable" => {
                let secs = a
                    .get("timeout")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(10)
                    .max(1);
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let sig_js = page_js("dom_sig", json!({}))?;
                let before = eval_str(conn, &session, &sig_js)?;
                self.await_dom_stable(conn, &session, Duration::from_secs(secs as u64))?;
                let after = eval_str(conn, &session, &sig_js)?;
                let moved = before != after;
                Ok(ok(
                    if moved {
                        "页面已经安定（内容有更新）"
                    } else {
                        "页面已经安定（内容没变）"
                    },
                    None,
                    None,
                ))
            }
            "scroll" => {
                let dir = a
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("down");
                let amount = a.get("amount").and_then(|v| v.as_i64()).unwrap_or(600);
                let sel = a.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js(
                    "scroll",
                    json!({ "direction": dir, "amount": amount, "selector": sel }),
                )?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                let where_ = out
                    .get("element")
                    .and_then(|v| v.as_str())
                    .unwrap_or("window");
                let y = out.get("scrollY").and_then(|v| v.as_i64()).unwrap_or(0);
                let h = out
                    .get("scrollHeight")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let done = out
                    .get("scrolled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(ok(
                    &format!(
                        "{}滚动 {where_}：y={y} / {h}",
                        if done { "" } else { "没能" }
                    ),
                    None,
                    None,
                ))
            }
            "screenshot" => {
                let full = a
                    .get("full_page")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let r = conn.call(
                    Some(&session),
                    "Page.captureScreenshot",
                    json!({
                        "format": "jpeg",
                        "quality": SHOT_QUALITY,
                        "captureBeyondViewport": full,
                    }),
                )?;
                let b64 = r
                    .get("data")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "截图没有数据".to_string())?;
                let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map_err(|e| format!("截图解码失败: {e}"))?;
                std::fs::create_dir_all(&self.shots.host)
                    .map_err(|e| format!("建截图目录失败: {e}"))?;
                let name = format!("shot-{}.jpg", uuid::Uuid::now_v7());
                let path = self.shots.host.join(&name);
                std::fs::write(&path, &bytes).map_err(|e| format!("写截图失败: {e}"))?;
                let guest = format!("{}/{}", self.shots.guest.trim_end_matches('/'), name);
                let info = page_info(conn, &session)?;
                let mut out = json!({
                    "ok": true,
                    "image": guest,
                    "text": format!(
                        "已截图（{}，{} KB）",
                        if full { "整页" } else { "当前视口" },
                        bytes.len() / 1024
                    ),
                });
                out = with_page_info(out, &info);
                Ok(out)
            }
            "back" => {
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let h = conn.call(Some(&session), "Page.getNavigationHistory", json!({}))?;
                let idx = h.get("currentIndex").and_then(|v| v.as_i64()).unwrap_or(0);
                let entries = h
                    .get("entries")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if idx <= 0 || entries.is_empty() {
                    return Err("已经是第一页了，退不了".into());
                }
                let entry = entries
                    .get((idx - 1) as usize)
                    .and_then(|e| e.get("id"))
                    .cloned()
                    .ok_or_else(|| "拿不到上一条历史".to_string())?;
                conn.drain_events();
                conn.call(
                    Some(&session),
                    "Page.navigateToHistoryEntry",
                    json!({ "entryId": entry }),
                )?;
                conn.wait_event(&session, "Page.loadEventFired", NAV_TIMEOUT)?;
                self.await_dom_stable(conn, &session, SETTLE_BUDGET)?;
                install_target_fix(conn, &session);
                let info = page_info(conn, &session)?;
                let url = info
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.remember(g, url.clone(), String::new());
                Ok(with_page_info(
                    ok(&format!("已后退到 {url}"), None, None),
                    &info,
                ))
            }
            "press" => {
                let key = a.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js("press", json!({ "key": key }))?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                self.settle(conn, &session)?;
                let acted = out.get("acted").and_then(|v| v.as_str()).unwrap_or("");
                Ok(ok(
                    &format!(
                        "已按 {key}{}",
                        if acted.is_empty() {
                            String::new()
                        } else {
                            format!("（{acted}）")
                        }
                    ),
                    None,
                    None,
                ))
            }
            "find_elements" => {
                let sel = a.get("selector").and_then(|v| v.as_str()).unwrap_or("");
                if sel.is_empty() {
                    return Err("缺少参数 selector".into());
                }
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                let js = page_js("find_elements", json!({ "selector": sel }))?;
                let out = eval_json(conn, &session, &js, JS_TIMEOUT)?;
                if let Some(e) = err_of(&out) {
                    return Err(e);
                }
                let count = out.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                let items = out
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut text = format!("匹配到 {count} 个（列前 {} 个）：", items.len());
                for it in &items {
                    text.push_str(&format!(
                        "\n  [{}] <{}> {}  {}x{} @ {},{} {}",
                        it.get("i").and_then(|v| v.as_i64()).unwrap_or(0),
                        it.get("tag").and_then(|v| v.as_str()).unwrap_or(""),
                        clip(it.get("text").and_then(|v| v.as_str()).unwrap_or(""), 60),
                        it.get("w").and_then(|v| v.as_i64()).unwrap_or(0),
                        it.get("h").and_then(|v| v.as_i64()).unwrap_or(0),
                        it.get("x").and_then(|v| v.as_i64()).unwrap_or(0),
                        it.get("y").and_then(|v| v.as_i64()).unwrap_or(0),
                        if it.get("visible").and_then(|v| v.as_bool()).unwrap_or(false) {
                            "可见"
                        } else {
                            "不可见"
                        },
                    ));
                }
                Ok(ok(&text, None, None))
            }
            "set_user_agent" => {
                let want = match a.get("user_agent").and_then(|v| v.as_str()) {
                    Some("mobile") => Profile::Mobile,
                    Some("desktop") => Profile::Desktop,
                    other => {
                        return Err(format!(
                            "user_agent 只能是 mobile 或 desktop，给的是 {other:?}"
                        ))
                    }
                };
                g.profile = want;
                let sessions: Vec<String> = g.tabs.iter().map(|t| t.session.clone()).collect();
                let current = g.current;
                let conn = self.ensure_conn(g)?;
                for s in &sessions {
                    apply_profile(conn, s, want)?;
                }
                conn.drain_events();
                if let Some(s) = sessions.get(current) {
                    // 换视口/UA 必须重载才生效（页面已经按旧值渲染过了）
                    conn.call(Some(s), "Page.reload", json!({}))?;
                    conn.wait_event(s, "Page.loadEventFired", NAV_TIMEOUT)?;
                    self.await_dom_stable(conn, s, SETTLE_BUDGET)?;
                    install_target_fix(conn, s);
                }
                Ok(ok(
                    &format!(
                        "已切到{}版（UA + 视口一起换，页面已重载）",
                        if want == Profile::Mobile {
                            "移动"
                        } else {
                            "桌面"
                        }
                    ),
                    None,
                    None,
                ))
            }
            "js" => {
                let script = a.get("script").and_then(|v| v.as_str()).unwrap_or("");
                if script.trim().is_empty() {
                    return Err("缺少参数 script".into());
                }
                let session = self.session(g)?;
                let conn = self.ensure_conn(g)?;
                // 包一层 async IIFE：`await fetch(...)` 是这个动作最值钱的用法。
                // CDP 的 `awaitPromise` 会等它，不用像 Kotlin 那边 kick + 轮询。
                let expr = format!("(async function() {{\n{script}\n}})()");
                let v = conn.eval(&session, &expr, JS_TIMEOUT)?;
                let body = match &v {
                    Value::Null => "（没有返回值）".to_string(),
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let mut body = body;
                if body.chars().count() > TEXT_MAX_CHARS {
                    body = clip(&body, TEXT_MAX_CHARS);
                    body.push_str("\n…（已截断）");
                }
                Ok(ok(&format!("JS 返回：\n{body}"), None, None))
            }
            "new_tab" => {
                let url = a
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                if g.tabs.len() >= MAX_TABS {
                    return Err(format!(
                        "标签页已经到上限（{MAX_TABS} 只）了，先 `agent browser close_tab` 关一只"
                    ));
                }
                self.open_tab(g, url.as_deref())?;
                let t = &g.tabs[g.current];
                Ok(ok(
                    &format!(
                        "已开第 {} 只标签（共 {} 只）：{}",
                        t.id,
                        g.tabs.len(),
                        if t.url.is_empty() {
                            "空白页"
                        } else {
                            &t.url
                        }
                    ),
                    None,
                    None,
                ))
            }
            "list_tabs" => {
                if g.tabs.is_empty() {
                    return Ok(ok(
                        "还没有标签页（navigate 或 new_tab 会开一只）",
                        None,
                        None,
                    ));
                }
                let mut text = format!("共 {} 只标签：", g.tabs.len());
                for (i, t) in g.tabs.iter().enumerate() {
                    text.push_str(&format!(
                        "\n  [{}]{} {}",
                        t.id,
                        if i == g.current { " ← 当前" } else { "" },
                        if t.url.is_empty() {
                            "空白页".to_string()
                        } else {
                            format!("{} {}", t.title, t.url).trim().to_string()
                        }
                    ));
                }
                Ok(ok(&text, None, None))
            }
            "switch_tab" => {
                let want = a.get("tab").and_then(|v| v.as_i64()).unwrap_or(-1);
                let Some(i) = g.tabs.iter().position(|t| t.id as i64 == want) else {
                    return Err(format!(
                        "没有编号 {want} 的标签（现有的：{}）",
                        g.tabs
                            .iter()
                            .map(|t| t.id.to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                };
                g.current = i;
                let t = &g.tabs[i];
                Ok(ok(
                    &format!(
                        "已切到第 {} 只：{}",
                        t.id,
                        if t.url.is_empty() {
                            "空白页"
                        } else {
                            &t.url
                        }
                    ),
                    None,
                    None,
                ))
            }
            "close_tab" => {
                if g.tabs.is_empty() {
                    return Err("没有标签页可关".into());
                }
                let want = a.get("tab").and_then(|v| v.as_i64());
                let i = match want {
                    Some(n) => g
                        .tabs
                        .iter()
                        .position(|t| t.id as i64 == n)
                        .ok_or_else(|| format!("没有编号 {n} 的标签"))?,
                    None => g.current,
                };
                let target = g.tabs[i].target.clone();
                let id = g.tabs[i].id;
                let conn = self.ensure_conn(g)?;
                conn.call(None, "Target.closeTarget", json!({ "targetId": target }))?;
                g.tabs.remove(i);
                g.current = g.current.min(g.tabs.len().saturating_sub(1));
                Ok(ok(
                    &format!("已关掉第 {id} 只（还剩 {} 只）", g.tabs.len()),
                    None,
                    None,
                ))
            }
            // 上面 `ACTIONS` 已经拦过一遍；走到这儿说明两边对不上了
            other => Err(format!("未知动作: {other}（支持：{}）", ACTIONS.join(" "))),
        }
    }

    /// 当前标签的 session（没有标签就是错误 —— 调用前已经保证开过一只）
    fn session(&self, g: &Inner) -> Result<String, String> {
        g.tabs
            .get(g.current)
            .map(|t| t.session.clone())
            .ok_or_else(|| "还没有打开任何页面".to_string())
    }

    /// 记下当前页的 url / 标题（`list_tabs` 用）
    fn remember(&self, g: &mut Inner, url: String, title: String) {
        if let Some(t) = g.tabs.get_mut(g.current) {
            t.url = url;
            if !title.is_empty() {
                t.title = title;
            }
        }
    }

    /// 开一只标签并切过去。`url` 给了就导航（等加载完成）。
    fn open_tab(&self, g: &mut Inner, url: Option<&str>) -> Result<(), String> {
        let profile = g.profile;
        let id = g.next_id;
        g.next_id += 1;
        let conn = self.ensure_conn(g)?;
        let r = conn.call(None, "Target.createTarget", json!({ "url": "about:blank" }))?;
        let target = r
            .get("targetId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "createTarget 没给 targetId".to_string())?
            .to_string();
        let r = conn.call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
        )?;
        let session = r
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "attachToTarget 没给 sessionId".to_string())?
            .to_string();
        conn.call(Some(&session), "Page.enable", json!({}))?;
        apply_profile(conn, &session, profile)?;
        // 导航先做（还要用 `conn`），**最后**才登记标签 —— 中途借用 `g.tabs`
        // 会把上面那个 `&mut Conn` 的借用打断
        let landed = match url {
            Some(u) => {
                let target_url =
                    normalize_url(u).ok_or_else(|| format!("只支持 http/https：{u}"))?;
                conn.drain_events();
                conn.call(
                    Some(&session),
                    "Page.navigate",
                    json!({ "url": target_url }),
                )?;
                conn.wait_event(&session, "Page.loadEventFired", NAV_TIMEOUT)?;
                self.await_dom_stable(conn, &session, SETTLE_BUDGET)?;
                install_target_fix(conn, &session);
                let info = page_info(conn, &session)?;
                Some((
                    info.get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&target_url)
                        .to_string(),
                    info.get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                ))
            }
            None => None,
        };
        g.tabs.push(Tab {
            id,
            target,
            session,
            url: String::new(),
            title: String::new(),
        });
        g.current = g.tabs.len() - 1;
        if let Some((real, title)) = landed {
            self.remember(g, real, title);
        }
        Ok(())
    }

    /// 动作之后等页面安定（SPA 的渲染大多在这一小段里出来）
    fn settle(&self, conn: &mut Conn, session: &str) -> Result<(), String> {
        self.await_dom_stable(conn, session, SETTLE_BUDGET)
    }

    /// 轮询 DOM 指纹，直到它连续 `SETTLE_MS` 没变（或到 `budget` 用完）。
    fn await_dom_stable(
        &self,
        conn: &mut Conn,
        session: &str,
        budget: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        let dom_sig = page_js("dom_sig", json!({}))?;
        let mut last = String::new();
        let mut stable_since = Instant::now();
        loop {
            let sig = eval_str(conn, session, &dom_sig)?;
            if sig != last {
                last = sig;
                stable_since = Instant::now();
            } else if stable_since.elapsed() >= SETTLE_MS {
                return Ok(());
            }
            if Instant::now() >= deadline || self.cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(120));
        }
    }
}

impl BrowserApi for CdpBrowser {
    fn describe(&self) -> String {
        let ep = self.endpoint();
        if self.managed {
            if ep.is_empty() {
                "CDP（自己起的无头 Chrome，还没起 —— 第一次动作用到它时拉起来）".to_string()
            } else {
                format!("CDP（自己起的无头 Chrome）{ep}")
            }
        } else {
            format!("CDP（连别人起的）{ep}")
        }
    }

    fn execute(&self, action_json: &str) -> Result<String, String> {
        let a: Value =
            serde_json::from_str(action_json).map_err(|e| format!("浏览器动作不是 JSON: {e}"))?;
        let name = a.get("action").and_then(|v| v.as_str()).unwrap_or("");
        // 「别跑了」：Rust 侧放弃这次调用时发的（见 `CancelBrowserOnDrop`）。
        // 不排队、不改状态 —— 只立个旗子，正在跑的动作在下一个检查点退出。
        if name == "__cancel" {
            self.cancelled.store(true, Ordering::Relaxed);
            return Ok(json!({ "ok": true, "text": "已请求取消" }).to_string());
        }
        self.cancelled.store(false, Ordering::Relaxed);
        let mut g = self
            .inner
            .lock()
            .map_err(|_| "浏览器状态被锁坏了（上一个动作 panic 过？）".to_string())?;
        let mut res = self.dispatch(&mut g, name, &a);
        // 连接断了：重连（标签页重新挂上）再试一次。只试一次 —— 再断就是真有问题
        if g.conn.as_ref().is_some_and(|c| c.dead) {
            tracing::info!("和 Chrome 的连接断了，重连");
            res = match self.reconnect(&mut g) {
                Ok(()) => self.dispatch(&mut g, name, &a),
                Err(e) => Err(format!("和浏览器的连接断了，重连也失败：{e}")),
            };
        }
        let out = match res {
            Ok(v) => v,
            Err(e) => json!({ "ok": false, "text": e }),
        };
        Ok(out.to_string())
    }
}

// ---------------------------------------------------------------------------
// 连接
// ---------------------------------------------------------------------------

/// 一条到 Chrome 的 WebSocket 连接。
///
/// 读超时是**故意的**：CDP 的等待（load 事件、DOM 安定）不能无限挂住，
/// 而且超时正是轮询的节拍。`tungstenite` 在 WouldBlock 时会留着半个帧的缓冲，
/// 所以重试 `read` 是安全的。
struct Conn {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: i64,
    /// 不是回包的消息（事件）先放这儿，`wait_event` 来取。
    events: Vec<Value>,
    /// 连接已经断了（收发碰到真的 IO 错误 / 对面 Close）。`execute` 看到它就
    /// 重连一次（见 `reconnect`）—— 不然之后每个动作都报同一句 Broken pipe，
    /// 只能重启核心。实测：Mac 上空闲半小时后就这么断过一次，Chrome 本身还活着。
    dead: bool,
}

impl Conn {
    fn connect(endpoint: &str) -> Result<Self, String> {
        let version = http_get_json(endpoint, "/json/version").map_err(|e| {
            format!(
                "连不上 Chrome（{endpoint}）：{e}。\
                 没设 AGENT_CDP 时是我们自己起的那个没起来（看日志里那条 \
                 「自己起了无头 Chrome」，或者 `AGENT_CHROME` 指对可执行文件）；\
                 设了 AGENT_CDP 时是那个地址不对 / 那个 Chrome 没开调试端口"
            )
        })?;
        let ws_url = version
            .get("webSocketDebuggerUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("{endpoint}/json/version 里没有 webSocketDebuggerUrl"))?
            .to_string();
        let (ws, _) = tungstenite::connect(ws_url.as_str())
            .map_err(|e| format!("连不上 Chrome 的 CDP（{ws_url}）：{e}"))?;
        if let MaybeTlsStream::Plain(tcp) = ws.get_ref() {
            tcp.set_read_timeout(Some(Duration::from_millis(200)))
                .map_err(|e| format!("设置读超时失败: {e}"))?;
        }
        Ok(Self {
            ws,
            next_id: 1,
            events: Vec::new(),
            dead: false,
        })
    }

    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        if let Err(e) = self.ws.send(Message::Text(msg.to_string().into())) {
            self.dead = true;
            return Err(format!("往 CDP 发 {method} 失败: {e}"));
        }
        Ok(id)
    }

    /// 读一条消息；`None` = 读超时（还没消息来）。
    fn read_msg(&mut self) -> Result<Option<Value>, String> {
        match self.ws.read() {
            Ok(Message::Text(t)) => serde_json::from_str(t.as_str())
                .map(Some)
                .map_err(|e| format!("CDP 回了非 JSON: {e}")),
            Ok(Message::Binary(b)) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| format!("CDP 回了非 JSON（binary）: {e}")),
            Ok(Message::Close(_)) => {
                self.dead = true;
                Err("CDP 连接被关掉了（Chrome 退出了？）".into())
            }
            Ok(_) => Ok(None), // Ping / Pong / Frame
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => {
                self.dead = true;
                Err(format!("读 CDP 失败: {e}"))
            }
        }
    }

    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.call_within(session, method, params, CALL_TIMEOUT)
    }

    fn call_within(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.send(session, method, params)?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(format!("CDP {method} 超时（{}s）", timeout.as_secs()));
            }
            let Some(v) = self.read_msg()? else { continue };
            if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                if let Some(err) = v.get("error") {
                    let msg = match err.get("message").and_then(|m| m.as_str()) {
                        Some(m) => m.to_string(),
                        None => err.to_string(),
                    };
                    return Err(format!("CDP {method} 报错: {msg}"));
                }
                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
            }
            // 不是我们的回包：事件，留着给 `wait_event`。
            // （连接被 Mutex 串住，同时只有一条命令在飞，所以不会丢别人的回包。）
            self.events.push(v);
        }
    }

    /// 把攒着的事件扔掉。**导航之前必须清一次** —— 否则上一次的
    /// `loadEventFired` 会把这一次的等待直接骗过去。
    fn drain_events(&mut self) {
        self.events.clear();
    }

    /// 等一个事件（比如 `Page.loadEventFired`）。等不到返回 false（不是错误）。
    fn wait_event(
        &mut self,
        session: &str,
        method: &str,
        timeout: Duration,
    ) -> Result<bool, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(pos) = self.events.iter().position(|e| {
                e.get("method").and_then(|m| m.as_str()) == Some(method)
                    && e.get("sessionId").and_then(|s| s.as_str()) == Some(session)
            }) {
                self.events.remove(pos);
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let Some(v) = self.read_msg()? else { continue };
            self.events.push(v);
        }
    }

    /// 跑一段 JS，返回它的值（`returnByValue` + `awaitPromise`）。
    fn eval(&mut self, session: &str, expr: &str, timeout: Duration) -> Result<Value, String> {
        let r = self.call_within(
            Some(session),
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true,
            }),
            timeout,
        )?;
        // `call` 已经把外层 `result` 剥掉了，这里拿到的就是 `Runtime.evaluate`
        // 的结果对象：`{result: {type, value}, exceptionDetails?}`
        if let Some(ex) = r.get("exceptionDetails") {
            let what = ex
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(|d| d.as_str())
                .or_else(|| ex.get("text").and_then(|t| t.as_str()))
                .unwrap_or("未知错误");
            return Err(format!("页面里报错: {}", clip(what, 300)));
        }
        Ok(r.get("result")
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }
}

impl CdpBrowser {
    /// 懒连接：第一次要动作的时候才连 —— 自己起 Chrome 的话，**也是这时候才拉起
    /// 它**（启动核心不该等一个浏览器）。
    /// 连接断了之后：重新连上，把原来的标签页重新挂上（CDP 的 session 跟着连接
    /// 走，旧的全作废；而标签页在 Chrome 里还在，`targetId` 不变）。自己起的
    /// Chrome 要是已经退出了，就重新起一个 —— 那时标签页确实没了。
    fn reconnect(&self, g: &mut Inner) -> Result<(), String> {
        g.conn = None;
        if let Some(c) = g.chrome.as_mut() {
            if !matches!(c.child.try_wait(), Ok(None)) {
                tracing::info!("自己起的 Chrome 已经退出了，重新起");
                g.chrome = None;
                g.tabs.clear();
            }
        }
        let profile = g.profile;
        let old = std::mem::take(&mut g.tabs);
        let current = old.get(g.current).map(|t| t.id);
        let conn = self.ensure_conn(g)?;
        let mut kept = Vec::new();
        for mut t in old {
            let attached = conn
                .call(
                    None,
                    "Target.attachToTarget",
                    json!({ "targetId": t.target, "flatten": true }),
                )
                .ok()
                .and_then(|r| {
                    r.get("sessionId")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                });
            let Some(session) = attached else {
                continue; // 那只标签已经没了
            };
            if conn.call(Some(&session), "Page.enable", json!({})).is_err()
                || apply_profile(conn, &session, profile).is_err()
            {
                continue;
            }
            t.session = session;
            kept.push(t);
        }
        g.current = kept
            .iter()
            .position(|t| Some(t.id) == current)
            .unwrap_or(kept.len().saturating_sub(1));
        g.tabs = kept;
        Ok(())
    }

    fn ensure_conn<'a>(&self, g: &'a mut Inner) -> Result<&'a mut Conn, String> {
        if g.conn.is_none() {
            if self.managed && g.chrome.is_none() {
                let (chrome, endpoint) = launch_chrome()?;
                tracing::info!(
                    "自己起了无头 Chrome：{}（profile {}，日志 {}）",
                    endpoint,
                    chrome.profile.display(),
                    chrome.log.display()
                );
                g.endpoint = endpoint;
                g.chrome = Some(chrome);
            }
            let ep = if self.managed {
                g.endpoint.clone()
            } else {
                self.endpoint.clone()
            };
            g.conn = Some(Conn::connect(&ep)?);
        }
        Ok(g.conn.as_mut().expect("刚设过"))
    }
}

// ---------------------------------------------------------------------------
// 自己起 Chrome
// ---------------------------------------------------------------------------

/// 找得到的 Chrome 可执行文件（`AGENT_CHROME` 优先）。
const CHROME_NAMES: &[&str] = &[
    "google-chrome-stable",
    "google-chrome",
    "chromium",
    "chromium-browser",
    "chrome",
];

/// 不在 PATH 里的常见安装位置。
#[cfg(not(target_os = "macos"))]
const CHROME_PATHS: &[&str] = &[
    "/opt/google/chrome/chrome",
    "/usr/lib/chromium/chromium",
    "/snap/bin/chromium",
];

/// macOS：`.app` 包里的可执行文件。按「和安卓 WebView 同为 Chromium、越接近
/// 正式版越好」排；Edge / Brave 也是 Chromium，CDP 一样能用。
#[cfg(target_os = "macos")]
const CHROME_PATHS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
];

/// 起一个无头 Chrome，等它把调试端口听上，返回 `(进程, 地址)`。
///
/// **为什么在核心里做**：用的人不该先另开一个终端去起浏览器 —— 那是实现细节。
/// 要「用我自己那个浏览器」的人有 `AGENT_CDP` 那条路（见 `from_env`）。
///
/// 端口是**当场挑一个空闲的**（不是写死 9222）：同一台机器上跑两个核心
/// （比如 `just dev` + `just dev-host`）不会互相抢。profile 是持久的
/// （`$XDG_DATA_HOME/android-agent/chrome`）—— 登录态能留到下次；万一那个
/// profile 被另一个 Chrome 占着（SingletonLock），退一步用临时的。
fn launch_chrome() -> Result<(Chrome, String), String> {
    sweep_orphan_chromes();
    let bin = chrome_bin()?;
    let mut last: Option<String> = None;
    for (i, profile) in chrome_profiles().into_iter().enumerate() {
        match try_launch(&bin, &profile) {
            Ok((child, port, log)) => {
                return Ok((
                    Chrome {
                        child,
                        profile,
                        log,
                    },
                    format!("http://127.0.0.1:{port}"),
                ))
            }
            Err(e) => {
                if i == 0 {
                    tracing::debug!("用持久 profile 起 Chrome 失败（{e}），换个临时的再试");
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| "起 Chrome 失败".to_string()))
}

/// 上一次核心被 SIGKILL（或 panic 崩掉）时留下的无头 Chrome：收掉它们。
///
/// 为什么要这一步：`Drop` 只在**正常退出**时跑（Ctrl-C / SIGTERM），
/// 被 `kill -9` / OOM 干掉时那个 Chrome 会变成孤儿，一直占着内存和一个
/// profile（几十 MB）。判据卡两道，避免误杀别人的浏览器：
///
/// - 命令行里有 `--user-data-dir=<我们的目录>`（持久那个，或者
///   `<临时目录>/android-agent-chrome-*`）
/// - **父进程已经是 init**（ppid == 1）—— 还活着的兄弟核心起的那个不算孤儿，
///   它有自己的爹
///
/// 进程表从哪来：Linux 读 `/proc`；macOS 没有 `/proc`，问 `ps`。
fn sweep_orphan_chromes() {
    let persistent = format!("--user-data-dir={}", chrome_profile_dir().display());
    // 临时 profile 跟着 `temp_dir()` 走（`chrome_profiles`）：Linux 一般是 /tmp，
    // macOS 是 /var/folders/… —— 不能写死 /tmp
    let temp = format!(
        "--user-data-dir={}",
        std::env::temp_dir().join("android-agent-chrome-").display()
    );
    let ours = |cmd: &str| {
        cmd.contains("--remote-debugging-port=")
            && (cmd.contains(&persistent) || cmd.contains(&temp))
            // 只认主进程：`--type=zygote` / `--type=renderer` 那些是它的孩子，
            // 跟着它一起死，不用单独杀
            && !cmd.contains("--type=")
    };
    let mut killed = 0;
    for (pid, ppid, cmd) in process_table() {
        if pid <= 1 || ppid != 1 || !ours(&cmd) {
            continue;
        }
        // SAFETY: kill(2) 只读参数
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        killed += 1;
    }
    if killed > 0 {
        tracing::info!("收掉了 {killed} 个上次留下的无头 Chrome");
    }
}

/// 这台机器上的进程：`(pid, ppid, 命令行)`。拿不到就是空的（不收就是了）。
#[cfg(target_os = "linux")]
fn process_table() -> Vec<(i32, i32, String)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let raw = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        if raw.is_empty() {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // `comm` 里可能有空格和括号，所以从最后一个 `)` 之后切：`state ppid …`
        let ppid = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().nth(1))
            .and_then(|p| p.parse::<i32>().ok())
            .unwrap_or(0);
        out.push((pid, ppid, String::from_utf8_lossy(&raw).replace('\0', " ")));
    }
    out
}

/// macOS（以及别的没有 `/proc` 的 unix）：`ps -axww -o pid=,ppid=,command=`。
/// `ww` 不截断命令行 —— `--user-data-dir=` 往往在很后面。
#[cfg(not(target_os = "linux"))]
fn process_table() -> Vec<(i32, i32, String)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,command="])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    parse_ps(&String::from_utf8_lossy(&out.stdout))
}

/// `ps -o pid=,ppid=,command=` 的输出：前两列是数字，其余整段是命令行。
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps(text: &str) -> Vec<(i32, i32, String)> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, rest) = l.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, cmd) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Some((
                pid.parse().ok()?,
                ppid.parse().ok()?,
                cmd.trim().to_string(),
            ))
        })
        .collect()
}

/// profile 目录：先是持久的那个，失败就退回一次性的（并发的第二个核心）
fn chrome_profiles() -> Vec<PathBuf> {
    vec![
        chrome_profile_dir(),
        std::env::temp_dir().join(format!("android-agent-chrome-{}", std::process::id())),
    ]
}

/// 持久的那个 profile（登录态能留到下次）
fn chrome_profile_dir() -> PathBuf {
    crate::desktop::data_root().join("chrome")
}

fn chrome_bin() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("AGENT_CHROME") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("AGENT_CHROME={} 不是一个文件", p.display()));
    }
    for name in CHROME_NAMES {
        if let Some(path) = which(name) {
            return Ok(path);
        }
    }
    // 常见安装位置（有些发行版不在 PATH 里；macOS 上 Chrome 是个 .app，
    // 从来不在 PATH 里）
    for p in CHROME_PATHS {
        if Path::new(p).is_file() {
            return Ok(PathBuf::from(p));
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        // 没有管理员权限的人会把 .app 拖进 ~/Applications
        for p in CHROME_PATHS.iter().filter_map(|p| p.strip_prefix('/')) {
            let p = Path::new(&home).join(p);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(format!(
        "找不到 Chrome / Chromium（试过 {}）—— 装一个，或者设 AGENT_CHROME 指到它的可执行文件",
        CHROME_NAMES.join(" / ")
    ))
}

/// PATH 里找可执行文件（不引 `which` crate）
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// 真的 spawn 一次，并等端口应答。失败返回原因（含 Chrome 自己的日志尾巴）。
fn try_launch(bin: &Path, profile: &Path) -> Result<(std::process::Child, u16, PathBuf), String> {
    std::fs::create_dir_all(profile).map_err(|e| format!("建 profile 目录失败: {e}"))?;
    let log = profile.join("chrome.log");
    let port = free_port()?;
    let out = std::fs::File::create(&log).map_err(|e| format!("建日志失败: {e}"))?;
    let err = out
        .try_clone()
        .map_err(|e| format!("克隆日志句柄失败: {e}"))?;

    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--headless=new")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        .arg("--hide-scrollbars")
        // 容器 / 内存小的机器上 `/dev/shm` 常常只有 64MB，不给这个 flag 会崩
        .arg("--disable-dev-shm-usage");
    // macOS：新 profile 第一次存 cookie 时 Chrome 会去钥匙串要「Chrome Safe
    // Storage」—— 弹一个系统授权框，无头那边就一直等着。用假钥匙串绕开。
    #[cfg(target_os = "macos")]
    cmd.arg("--use-mock-keychain");
    let child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .map_err(|e| format!("起不了 {}: {e}", bin.display()))?;

    let endpoint = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + CHROME_BOOT_TIMEOUT;
    let mut child = child;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("Chrome 起来就退了（{status}）：{}", tail_of(&log)));
        }
        if http_get_json(&endpoint, "/json/version").is_ok() {
            return Ok((child, port, log));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "Chrome {}s 还没把调试端口听上：{}",
                CHROME_BOOT_TIMEOUT.as_secs(),
                tail_of(&log)
            ));
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// 挑一个空闲端口（bind 0 拿一个，然后放掉）
fn free_port() -> Result<u16, String> {
    let l =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("挑空闲端口失败: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("读端口失败: {e}"))
}

/// 日志尾巴（出错时给模型 / 人看的线索）
fn tail_of(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let tail: String = text
        .chars()
        .rev()
        .take(400)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    if tail.trim().is_empty() {
        "（Chrome 没写日志）".to_string()
    } else {
        tail.trim().to_string()
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 取一段要注进页面的 JS。**只有一份**（`crate::browser_js`）——
/// 手机上的 WebView 也从那儿取，两边不许各写一份。
fn page_js(kind: &str, params: Value) -> Result<String, String> {
    crate::browser_js::build(kind, &params.to_string())
}

/// 把 `target=_blank` 掰成 `_self`（见 `browser_js` 里那段 JS 的说明）。
///
/// 手机那边是在 `onPageStarted` 里注的，这边在加载完成后注 —— 效果一样，
/// 而且那段 JS 自带幂等哨兵（`window.__agentTargetFix`），重复注不会叠加监听器。
/// 注失败不算错（顶多是点 `_blank` 链接时开出一只我们管不住的标签）。
fn install_target_fix(conn: &mut Conn, session: &str) {
    if let Ok(js) = page_js("target_fix", json!({})) {
        let _ = conn.eval(session, &js, JS_TIMEOUT);
    }
}

/// `http://127.0.0.1:9222` → `("127.0.0.1", 9222)`
fn split_endpoint(endpoint: &str) -> Result<(String, u16), String> {
    let rest = endpoint
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("端口不对: {p}"))?),
        None => (rest, 9222),
    };
    if host.is_empty() {
        return Err(format!("地址里没有主机名: {endpoint}"));
    }
    Ok((host.to_string(), port))
}

/// 最小的 HTTP GET（只用来问 `/json/version`，就在本机）。
///
/// 不用 reqwest：那是给 LLM 用的异步客户端，而这里跑在 `spawn_blocking` 上、
/// 只有一行响应体要读。
fn http_get_json(endpoint: &str, path: &str) -> Result<Value, String> {
    let (host, port) = split_endpoint(endpoint)?;
    let mut s = TcpStream::connect((host.as_str(), port))
        .map_err(|e| format!("连不上 {host}:{port}: {e}"))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("设置读超时失败: {e}"))?;
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("发请求失败: {e}"))?;

    // 自己按 `Content-Length` 收工，**不用 `read_to_end`**：Chrome 有时不关
    // 连接（keep-alive），而 `read_to_end` 要等 EOF —— 等到读超时的那一刻
    // 它连**已经读到的**字节一起丢掉（实测就是这么报「Resource temporarily
    // unavailable」的）。
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let mut want: Option<usize> = None;
    loop {
        let done = match (&want, find_header_end(&buf)) {
            (Some(n), Some(end)) => buf.len() >= end + n,
            _ => false,
        };
        if done {
            break;
        }
        match s.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if want.is_none() {
                    if let Some(end) = find_header_end(&buf) {
                        want = content_length(&buf[..end]);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(e) => return Err(format!("读响应失败: {e}")),
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "响应没有 body".to_string())?;
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
        return Err(format!("HTTP {}", head.lines().next().unwrap_or("?")));
    }
    serde_json::from_str(body).map_err(|e| format!("响应不是 JSON: {e}"))
}

/// 响应头结束的位置（`\r\n\r\n` 之后）
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// 响应头里的 `Content-Length`（没有就 None）
fn content_length(head: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(head);
    text.lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .and_then(|v| v.trim().parse().ok())
}

/// 只认 http/https；没写 scheme 就补 `https://`。
///
/// 比手机上那份**严一点**：`javascript:` / `data:` / `file:` / `tel:` 这些
/// 没有 `://` 的 scheme 也挡掉（那边是「没有 `://` 就当主机名」，
/// 于是 `javascript:alert(1)` 会被拼成 `https://javascript:alert(1)`）。
/// `host:8080/…` 这种**带端口的**照样放行 —— 靠「冒号后面是不是数字」区分。
fn normalize_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.is_empty() {
        return None;
    }
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
            .then(|| url.to_string());
    }
    if let Some((head, rest)) = url.split_once(':') {
        let scheme_like = head.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && head
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        let port_like = rest.chars().next().is_some_and(|c| c.is_ascii_digit());
        if scheme_like && !port_like {
            return None;
        }
    }
    Some(format!("https://{url}"))
}

/// 按**字符**截断（多字节边界上 `truncate` 会 panic）
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn ok(text: &str, url: Option<String>, title: Option<String>) -> Value {
    let mut v = json!({ "ok": true, "text": text });
    if let Some(u) = url.filter(|u| !u.is_empty()) {
        v["url"] = json!(u);
    }
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        v["title"] = json!(t);
    }
    v
}

fn err_of(v: &Value) -> Option<String> {
    v.get("error")
        .and_then(|e| e.as_str())
        .filter(|e| !e.is_empty())
        .map(|e| e.to_string())
}

/// 页面信息（url / title / 视口 / 元素数）。动作回包里带上，模型好记账。
///
/// 那段 JS 是 `JSON.stringify(...)`，所以拿到的是**字符串** —— 得再解析一次
/// （漏了这一步的表现很隐蔽：url/title 静默为空，看着像「页面没标题」）。
/// 解析不了不算错，返回 Null（页面信息是锦上添花，不该让动作失败）。
fn page_info(conn: &mut Conn, session: &str) -> Result<Value, String> {
    let js = page_js("page_info", json!({}))?;
    let v = conn.eval(session, &js, JS_TIMEOUT)?;
    Ok(match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::Null),
        other => other,
    })
}

fn with_page_info(mut v: Value, info: &Value) -> Value {
    for k in ["url", "title"] {
        if let Some(s) = info.get(k).and_then(|x| x.as_str()) {
            if !s.is_empty() && v.get(k).is_none() {
                v[k] = json!(s);
            }
        }
    }
    v
}

/// 跑一段「会 JSON.stringify 结果」的 JS，并把那个字符串解析成 Value。
fn eval_json(conn: &mut Conn, session: &str, js: &str, timeout: Duration) -> Result<Value, String> {
    let v = conn.eval(session, js, timeout)?;
    match v {
        Value::String(s) => serde_json::from_str(&s)
            .map_err(|e| format!("页面返回的不是 JSON: {e}（{}）", clip(&s, 120))),
        other => Ok(other),
    }
}

fn eval_str(conn: &mut Conn, session: &str, js: &str) -> Result<String, String> {
    let v = conn.eval(session, js, JS_TIMEOUT)?;
    Ok(match v {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// UA + 视口一起设（两个必须同时换，见 `DESKTOP_UA` 的注释）
fn apply_profile(conn: &mut Conn, session: &str, p: Profile) -> Result<(), String> {
    let (w, h, scale, mobile) = p.viewport();
    conn.call(
        Some(session),
        "Emulation.setDeviceMetricsOverride",
        json!({
            "width": w, "height": h, "deviceScaleFactor": scale, "mobile": mobile,
        }),
    )?;
    conn.call(
        Some(session),
        "Emulation.setUserAgentOverride",
        json!({ "userAgent": p.ua() }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps_output_keeps_the_whole_command_line() {
        // macOS 的孤儿回收靠它：命令行里有空格（.app 路径），参数在很后面
        let text = "    1     0 /sbin/launchd\n\
                    \x20 4242     1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --headless=new --remote-debugging-port=5555 --user-data-dir=/x\n\
                    garbage line\n";
        let rows = parse_ps(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].0, 4242);
        assert_eq!(rows[1].1, 1);
        assert!(rows[1].2.starts_with("/Applications/Google Chrome.app/"));
        assert!(rows[1].2.ends_with("--user-data-dir=/x"));
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            split_endpoint("http://127.0.0.1:9222").unwrap(),
            ("127.0.0.1".to_string(), 9222)
        );
        assert_eq!(
            split_endpoint("127.0.0.1:9223/").unwrap(),
            ("127.0.0.1".to_string(), 9223)
        );
        assert_eq!(
            split_endpoint("http://localhost").unwrap(),
            ("localhost".to_string(), 9222)
        );
        assert!(split_endpoint("http://:9222").is_err());
    }

    /// 只认 http/https；没写 scheme 补 https（和手机上那份一致）
    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_url("example.com"),
            Some("https://example.com".into())
        );
        assert_eq!(
            normalize_url(" http://example.com/a?b=1 "),
            Some("http://example.com/a?b=1".into())
        );
        assert_eq!(
            normalize_url("HTTPS://example.com"),
            Some("HTTPS://example.com".into())
        );
        // 别的 scheme 一律挡掉（页面不该能拉起别的 App，更不该读本地文件）
        assert_eq!(normalize_url("file:///etc/passwd"), None);
        assert_eq!(normalize_url("javascript:alert(1)"), None);
        assert_eq!(normalize_url("intent://x"), None);
        assert_eq!(normalize_url("   "), None);
    }

    /// 每个动作名都得能进 dispatch（不然 CLI 发得出去、后端回「未知动作」）
    #[test]
    fn every_action_is_reachable() {
        // 连一个**连不上**的地址：这些动作在碰网络之前就该报「参数」类的错
        let b = CdpBrowser::connect_to("http://127.0.0.1:1".into(), shots_for_test());
        let mut g = Inner {
            conn: None,
            chrome: None,
            endpoint: "http://127.0.0.1:1".into(),
            tabs: Vec::new(),
            current: 0,
            next_id: 1,
            profile: Profile::Desktop,
        };
        // 不连 Chrome：这些动作在碰网络之前就该报「参数」类的错，不能是「未知动作」
        for a in ACTIONS {
            let r = b.dispatch(&mut g, a, &json!({}));
            if let Err(e) = &r {
                assert!(!e.contains("未知动作"), "{a} 在 dispatch 里没有分支：{e}");
            }
        }
        // 反过来：不认识的动作要说清楚支持哪些（模型据此纠正）
        let e = b.dispatch(&mut g, "nope", &json!({})).unwrap_err();
        assert!(e.contains("未知动作") && e.contains("navigate"), "{e}");
    }

    /// 连不上 Chrome 时的错要**能照着做**（这是模型唯一能看到的线索）
    #[test]
    fn the_offline_error_points_at_the_two_ways_to_get_a_browser() {
        let e = match Conn::connect("http://127.0.0.1:1") {
            Ok(_) => panic!("127.0.0.1:1 上不该有东西在听"),
            Err(e) => e,
        };
        assert!(e.contains("AGENT_CDP"), "{e}");
        assert!(e.contains("AGENT_CHROME"), "{e}");
    }

    /// **真 Chrome 的端到端**：自己起一个无头 Chrome，起一个本地的假页面，
    /// 把动作过一遍。
    ///
    /// 默认跳过（CI / 别人机器上不一定装了 Chrome）：
    ///     AGENT_CDP_TEST=1 cargo test cdp::
    #[test]
    fn end_to_end_against_a_real_chrome() {
        if std::env::var("AGENT_CDP_TEST").as_deref() != Ok("1") {
            eprintln!("跳过（设 AGENT_CDP_TEST=1 才跑；会自己起 Chrome）");
            return;
        }
        let (base, _srv) = serve_test_page();
        // **自己起** Chrome（和核心走同一条路）：不用先另开一个终端起浏览器
        let b = CdpBrowser::managed(shots_for_test());
        let act = |json: Value| -> Value {
            let out = b.execute(&json.to_string()).expect("execute 不该 Err");
            serde_json::from_str(&out).expect("回包要是 JSON")
        };
        let text_of = |v: &Value| {
            v.get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string()
        };

        // navigate：标题和 url 都要读回来
        let v = act(json!({"action": "navigate", "url": base}));
        assert_eq!(v["ok"], true, "{v}");
        assert!(text_of(&v).contains("标题：CDP 测试页"), "{v}");

        // 骨架：能看见标题，且给了能点的 selector
        let v = act(json!({"action": "get_backbone", "max_depth": 4}));
        let t = text_of(&v);
        assert!(t.contains("页面结构"), "{t}");
        assert!(t.contains("hello"), "{t}");
        assert!(t.contains("#go"), "按钮的 selector 要在里面：{t}");

        // 正文（整页文本 / 抽出来的正文）
        let v = act(json!({"action": "get_text"}));
        assert!(text_of(&v).contains("hello 骨架"), "{v}");
        assert!(text_of(&v).contains("一个链接"), "{v}");
        let v = act(json!({"action": "get_readable"}));
        let t = text_of(&v);
        assert!(t.contains("这是一段正文"), "{t}");
        assert!(
            t.contains("body(fallback)") || t.contains("best-block"),
            "{t}"
        );

        // 输入 + 点击 + JS 读回来
        let v = act(json!({"action": "type", "selector": "#q", "text": "abc"}));
        assert_eq!(v["ok"], true, "{v}");
        let v =
            act(json!({"action": "js", "script": "return document.querySelector('#q').value;"}));
        assert!(text_of(&v).contains("abc"), "{v}");
        let v = act(json!({"action": "click", "selector": "#go"}));
        assert_eq!(v["ok"], true, "{v}");
        let v = act(json!({"action": "js", "script": "return document.title;"}));
        assert!(text_of(&v).contains("clicked"), "点了按钮标题该变：{v}");

        // 找不到的元素要报清楚，不是静默成功
        let v = act(json!({"action": "click", "selector": "#nope"}));
        assert_eq!(v["ok"], false, "{v}");
        assert!(text_of(&v).contains("没找到元素"), "{v}");

        // find_elements / scroll / press / wait_for_dom_stable
        let v = act(json!({"action": "find_elements", "selector": "a"}));
        assert!(text_of(&v).contains("匹配到"), "{v}");
        assert_eq!(
            act(json!({"action": "scroll", "direction": "down"}))["ok"],
            true
        );
        assert_eq!(act(json!({"action": "press", "key": "Tab"}))["ok"], true);
        assert_eq!(
            act(json!({"action": "wait_for_dom_stable", "timeout": 2}))["ok"],
            true
        );

        // 截图：文件真的落在 shots 目录里
        let v = act(json!({"action": "screenshot"}));
        assert_eq!(v["ok"], true, "{v}");
        let img = v["image"].as_str().expect("要有 image 字段");
        let name = img.rsplit('/').next().unwrap();
        let path = shots_for_test().host.join(name);
        assert!(path.is_file(), "截图没落盘：{}", path.display());
        assert!(
            std::fs::metadata(&path).unwrap().len() > 1000,
            "截图太小，像是空的"
        );

        // UA / 视口：两个一起换，页面重载后能看到
        let v = act(json!({"action": "set_user_agent", "user_agent": "mobile"}));
        assert_eq!(v["ok"], true, "{v}");
        let v = act(json!({"action": "js", "script": "return navigator.userAgent;"}));
        assert!(text_of(&v).contains("Android"), "UA 没换成移动版：{v}");
        let v = act(json!({"action": "js", "script": "return String(window.innerWidth);"}));
        assert!(text_of(&v).contains("412"), "视口没跟着换：{v}");
        // 像素比是 float32 出来的，别按字面比 "2.6"
        let v = act(json!({"action": "js", "script": "return String(window.devicePixelRatio);"}));
        assert!(text_of(&v).contains("2.599"), "像素比没跟着换：{v}");
        assert_eq!(
            act(json!({"action": "set_user_agent", "user_agent": "desktop"}))["ok"],
            true
        );

        // 多标签
        let v = act(json!({"action": "new_tab", "url": base}));
        assert_eq!(v["ok"], true, "{v}");
        let v = act(json!({"action": "list_tabs"}));
        let t = text_of(&v);
        assert!(t.contains("当前"), "{t}");
        let v = act(json!({"action": "switch_tab", "tab": 1}));
        assert_eq!(v["ok"], true, "{v}");
        let v = act(json!({"action": "close_tab"}));
        assert_eq!(v["ok"], true, "{v}");
        // 关不存在的标签要说清楚有哪些
        let v = act(json!({"action": "switch_tab", "tab": 99}));
        assert_eq!(v["ok"], false, "{v}");
        assert!(text_of(&v).contains("没有编号"), "{v}");

        // 后退（第二只标签已经被关了，当前页是第一次导航的那个）
        assert_eq!(act(json!({"action": "back"}))["ok"], true);

        // 未知动作：要把支持哪些列出来
        let v = act(json!({"action": "nope"}));
        assert!(
            text_of(&v).contains("未知动作") && text_of(&v).contains("navigate"),
            "{v}"
        );
    }

    /// 一个只回同一张页面的本地 HTTP 服务（测试用，进程退出就没了）
    fn serve_test_page() -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                // `viewport` 那行是给「移动版视口」那条断言用的：没有它的话
                // 移动模拟会用 980px 的默认布局宽度（Chrome 的老规矩），
                // 于是 innerWidth 不是 412
                let body = r#"<!doctype html><html><head><title>CDP 测试页</title>
<meta name="viewport" content="width=device-width, initial-scale=1"></head><body>
<h1 id="h">hello 骨架</h1>
<p>这是一段正文，长到足够让 readable 认出它来，不然会被当成噪音丢掉。</p>
<a href="/x">一个链接</a>
<input id="q" placeholder="搜索">
<button id="go" onclick="document.title='clicked'">点我</button>
</body></html>"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK
Content-Type: text/html; charset=utf-8
Content-Length: {}
Connection: close

{}",
                    body.len(),
                    body
                );
            }
        });
        (format!("http://127.0.0.1:{port}/"), h)
    }

    fn shots_for_test() -> ShotDir {
        ShotDir {
            host: std::env::temp_dir().join("agent-cdp-test-shots"),
            guest: "/tmp/agent-cdp-test-shots".into(),
        }
    }
}
