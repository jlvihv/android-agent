//! 桌面（Linux）上的浏览器：**无头 Chrome + CDP**。
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
//! ## 谁起 Chrome
//!
//! 我们**不**替用户起进程（端口、profile、退出清理都是麻烦，而且用户可能想让
//! agent 用他自己那个浏览器）。连的是 `AGENT_CDP`（默认
//! `http://127.0.0.1:9222`）—— `just chrome` 会起一个无头 Chrome 并开这个端口；
//! 用户自己的 Chrome 加 `--remote-debugging-port=9222` 也一样能用（还能看着它动）。
//! 连不上时报的错里会写清楚怎么办，模型照着做就行。
//!
//! ## 和 Kotlin 那份的关系
//!
//! 动作表、返回形状（`{ok, text, url, title, image}`）、以及注入的 JS 都是从
//! `BrowserController.kt` 移植过来的，语义要一致。**两份 JS 现在确实是两份**
//! （Kotlin 那份只能在真机上验，这轮没动它），所以改 JS 的时候两边都得改 ——
//! 这是已知的债，见 docs/08 的「两份实现」。
//! 动作**名字**这一层是能自动对齐的：`ACTIONS` 和沙箱里 `agent browser` 的
//! case 表有测试盯着（`agent_cli_and_cdp_agree_on_the_action_list`）。

use std::io::{Read, Write};
use std::net::TcpStream;
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

/// navigate 等 `Page.loadEventFired` 的上限。等不到**不算失败**（SPA 常常不触发）
const NAV_TIMEOUT: Duration = Duration::from_secs(25);

/// 页面安定判定：指纹多久没变就算稳了；总预算多少。
const SETTLE_MS: Duration = Duration::from_millis(300);
const SETTLE_BUDGET: Duration = Duration::from_secs(4);

/// 骨架树给模型的文本上限（它会进历史，按钱算）
const BACKBONE_MAX_CHARS: usize = 12_000;
const BACKBONE_MAX_NODES: usize = 220;

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
pub struct CdpBrowser {
    endpoint: String,
    /// 截图落哪（宿主目录 + 历史里存的前缀）
    shots: ShotDir,
    /// 用户按了停止：正在跑的动作在下一个检查点退出
    cancelled: AtomicBool,
    inner: Mutex<Inner>,
}

/// 连接 + 标签页。整个 `execute` 期间被 Mutex 串住：两个动作交叉执行会让
/// `navigate A → click` 点到别的页面上（和 Kotlin 那个 `lock` 同一个理由）。
struct Inner {
    conn: Option<Conn>,
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
    pub fn new(endpoint: String, shots: ShotDir) -> Self {
        Self {
            endpoint,
            shots,
            cancelled: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                conn: None,
                tabs: Vec::new(),
                current: 0,
                next_id: 1,
                profile: Profile::Desktop,
            }),
        }
    }

    /// 连的是哪个调试端口（启动日志 / 体检用）
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 从环境变量构造（桌面用）。`AGENT_CDP` 指 Chrome 的调试端口；
    /// 设成 `off` / `0` 就是不要浏览器。
    pub fn from_env(shots: ShotDir) -> Option<Self> {
        let ep = std::env::var("AGENT_CDP")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:9222".to_string());
        if matches!(ep.as_str(), "off" | "0" | "no") {
            return None;
        }
        Some(Self::new(ep, shots))
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
                let conn = g.conn(&self.endpoint)?;
                conn.drain_events();
                conn.call(Some(&session), "Page.navigate", json!({ "url": target }))?;
                let loaded = conn.wait_event(&session, "Page.loadEventFired", NAV_TIMEOUT)?;
                self.await_dom_stable(conn, &session, SETTLE_BUDGET)?;
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
                let js = backbone_js(depth as usize);
                let session = self.session(g)?;
                let conn = g.conn(&self.endpoint)?;
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
                    click_sel_js(sel)
                } else if let (Some(x), Some(y)) = (
                    a.get("x").and_then(|v| v.as_i64()),
                    a.get("y").and_then(|v| v.as_i64()),
                ) {
                    click_xy_js(x, y)
                } else {
                    return Err("click 需要 selector，或者 x + y".into());
                };
                let session = self.session(g)?;
                let conn = g.conn(&self.endpoint)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, &type_js(sel, text), JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, &get_text_js(sel), JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, READABLE_JS, JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
                let before = eval_str(conn, &session, DOM_SIG_JS)?;
                self.await_dom_stable(conn, &session, Duration::from_secs(secs as u64))?;
                let after = eval_str(conn, &session, DOM_SIG_JS)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, &scroll_js(dir, amount, sel), JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
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
                let conn = g.conn(&self.endpoint)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, &press_js(key), JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
                let out = eval_json(conn, &session, &find_elements_js(sel), JS_TIMEOUT)?;
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
                let conn = g.conn(&self.endpoint)?;
                for s in &sessions {
                    apply_profile(conn, s, want)?;
                }
                conn.drain_events();
                if let Some(s) = sessions.get(current) {
                    // 换视口/UA 必须重载才生效（页面已经按旧值渲染过了）
                    conn.call(Some(s), "Page.reload", json!({}))?;
                    conn.wait_event(s, "Page.loadEventFired", NAV_TIMEOUT)?;
                    self.await_dom_stable(conn, s, SETTLE_BUDGET)?;
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
                let conn = g.conn(&self.endpoint)?;
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
                let conn = g.conn(&self.endpoint)?;
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
        let conn = g.conn(&self.endpoint)?;
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
        let mut last = String::new();
        let mut stable_since = Instant::now();
        loop {
            let sig = eval_str(conn, session, DOM_SIG_JS)?;
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
        format!("CDP {}", self.endpoint)
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
        let out = match self.dispatch(&mut g, name, &a) {
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
}

impl Conn {
    fn connect(endpoint: &str) -> Result<Self, String> {
        let version = http_get_json(endpoint, "/json/version").map_err(|e| {
            format!(
                "连不上 Chrome（{endpoint}）：{e}。\
                 先跑 `just chrome` 起一个无头 Chrome，或者自己起一个带 \
                 `--remote-debugging-port=9222` 的（AGENT_CDP 可以换地址）"
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
        })
    }

    fn send(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<i64, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .map_err(|e| format!("往 CDP 发 {method} 失败: {e}"))?;
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
            Ok(Message::Close(_)) => Err("CDP 连接被关掉了（Chrome 退出了？）".into()),
            Ok(_) => Ok(None), // Ping / Pong / Frame
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(format!("读 CDP 失败: {e}")),
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

impl Inner {
    /// 懒连接：第一次要动作的时候才连（Chrome 可能还没起来）
    fn conn(&mut self, endpoint: &str) -> Result<&mut Conn, String> {
        if self.conn.is_none() {
            self.conn = Some(Conn::connect(endpoint)?);
        }
        Ok(self.conn.as_mut().expect("刚设过"))
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

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

/// 把一段字符串安全地塞进 JS 的单引号字面量里。
fn js_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out
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
    let v = conn.eval(session, PAGE_INFO_JS, JS_TIMEOUT)?;
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

// ---------------------------------------------------------------------------
// 注入的 JS（从 BrowserController.kt 移植）
// ---------------------------------------------------------------------------

/// DOM 指纹：元素个数 + 可见文字长度 + 滚动高度。变了就说明还没稳。
const DOM_SIG_JS: &str = "(function(){var b=document.body;if(!b)return 'null';\
     return document.querySelectorAll('*').length+':'+(b.innerText||'').length+':'+\
     (document.documentElement.scrollHeight||0);})()";

const PAGE_INFO_JS: &str = r##"
(function() {
  return JSON.stringify({
    url: location.href,
    title: document.title,
    ready: document.readyState,
    scrollY: Math.round(window.scrollY),
    scrollHeight: document.documentElement.scrollHeight,
    innerWidth: window.innerWidth,
    innerHeight: window.innerHeight,
    elements: document.querySelectorAll('*').length
  });
})()
"##;

/// 点一个元素的**完整鼠标事件序列**。
///
/// 只调 `el.click()` 不够：很多站点在 `mousedown`/`mouseup` 上做状态
/// （下拉菜单、拖拽、自定义控件），合成 click 到不了它们。
const CLICK_EVENTS: &str = r##"
  var mk = function(type, bubbles) {
    return new MouseEvent(type, {bubbles: bubbles, cancelable: true, view: window});
  };
  el.dispatchEvent(mk('mouseover', true));
  el.dispatchEvent(mk('mouseenter', false));
  el.dispatchEvent(mk('mousemove', true));
  el.dispatchEvent(mk('mousedown', true));
  el.dispatchEvent(mk('mouseup', true));
  el.click();
  el.dispatchEvent(mk('mouseleave', false));
  el.dispatchEvent(mk('mouseout', true));
"##;

fn click_sel_js(selector: &str) -> String {
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var list = document.querySelectorAll('{q}');
  if (!list.length) return JSON.stringify({{error: '没找到元素: {q}'}});
  var el = list[0];
  if (el.disabled) return JSON.stringify({{error: '元素是 disabled 的'}});
  el.scrollIntoView({{block: 'center'}});
  {CLICK_EVENTS}
  return JSON.stringify({{clicked: true, matched: list.length, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)}});
}})()
"##
    )
}

fn click_xy_js(x: i64, y: i64) -> String {
    format!(
        r##"
(function() {{
  var el = document.elementFromPoint({x}, {y});
  if (!el) return JSON.stringify({{error: '坐标 ({x}, {y}) 上没有元素'}});
  {CLICK_EVENTS}
  return JSON.stringify({{clicked: true, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)}});
}})()
"##
    )
}

/// 输入文本。三件事必须都做，少一件就有站点点不动：
/// 1. **native setter** —— React/Vue 在 `input.value` 上装了自己的 setter，
///    直接赋值它看不见（框里有字、state 是空的）；
/// 2. **逐字符 keydown/input/keyup** —— 靠监听按键的搜索框只认事件；
/// 3. **框架同步** —— Angular 认 `$apply`，Vue 认 `$forceUpdate`。
fn type_js(selector: &str, text: &str) -> String {
    let q = js_quote(selector);
    let val = js_quote(text);
    format!(
        r##"
(function() {{
  var list = document.querySelectorAll('{q}');
  if (!list.length) return JSON.stringify({{error: '没找到元素: {q}'}});
  var el = list[0];
  var val = '{val}';
  el.focus();
  if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
    var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(proto, 'value');
    if (setter && setter.set) setter.set.call(el, val); else el.value = val;
  }} else if (el.isContentEditable) {{
    el.innerText = val;
  }}
  for (var i = 0; i < val.length; i++) {{
    var c = val[i];
    el.dispatchEvent(new KeyboardEvent('keydown', {{key: c, bubbles: true}}));
    el.dispatchEvent(new KeyboardEvent('keypress', {{key: c, bubbles: true}}));
    el.dispatchEvent(new InputEvent('input', {{data: c, inputType: 'insertText', bubbles: true}}));
    el.dispatchEvent(new KeyboardEvent('keyup', {{key: c, bubbles: true}}));
  }}
  el.dispatchEvent(new Event('input', {{bubbles: true}}));
  el.dispatchEvent(new Event('change', {{bubbles: true}}));
  try {{
    if (window.angular) {{
      var ng = window.angular.element(el);
      var sc = (ng.scope && ng.scope()) || (ng.injector && ng.injector().get('$rootScope'));
      if (sc && !sc.$$phase) sc.$apply();
    }}
  }} catch (e) {{}}
  try {{ if (el.__vue__) el.__vue__.$forceUpdate(); }} catch (e) {{}}
  el.dispatchEvent(new FocusEvent('blur', {{bubbles: true, relatedTarget: null}}));
  el.blur();
  return JSON.stringify({{typed: true, matched: list.length, length: val.length}});
}})()
"##
    )
}

fn get_text_js(selector: &str) -> String {
    if selector.is_empty() {
        return r##"
(function() {
  var t = (document.body.innerText || '').trim();
  return JSON.stringify({text: t.substring(0, 40000), length: t.length, url: location.href, title: document.title});
})()
"##
        .to_string();
    }
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var el = document.querySelector('{q}');
  if (!el) return JSON.stringify({{error: '没找到元素: {q}'}});
  var t = (el.innerText || el.textContent || '').trim();
  return JSON.stringify({{text: t.substring(0, 40000), length: t.length}});
}})()
"##
    )
}

/// 滚动。先试 window；页面没动说明滚动在某个内层容器里（现在满大街都是），
/// 就找「overflow 是 auto/scroll 且内容比容器高」里面**面积最大**的那个 ——
/// 面积最大基本就是主内容区，不是侧边的小列表。
fn scroll_js(direction: &str, amount: i64, selector: &str) -> String {
    let px = if direction == "up" { -amount } else { amount };
    let sel = if selector.is_empty() {
        "null".to_string()
    } else {
        format!("'{}'", js_quote(selector))
    };
    format!(
        r##"
(function() {{
  var sel = {sel};
  var px = {px};
  if (sel) {{
    var el = document.querySelector(sel);
    if (!el) return JSON.stringify({{error: '没找到元素: ' + sel}});
    el.scrollBy(0, px);
    return JSON.stringify({{scrolled: true, element: sel, scrollY: Math.round(el.scrollTop), scrollHeight: el.scrollHeight}});
  }}
  var before = window.scrollY;
  window.scrollBy(0, px);
  if (window.scrollY !== before || document.documentElement.scrollHeight <= window.innerHeight + 2) {{
    return JSON.stringify({{scrolled: true, element: 'window', scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight}});
  }}
  var best = null, bestArea = 0;
  var all = document.body.querySelectorAll('*');
  for (var i = 0; i < all.length; i++) {{
    var e = all[i];
    var st = getComputedStyle(e);
    if ((st.overflowY === 'auto' || st.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 5) {{
      var area = e.clientWidth * e.clientHeight;
      if (area > bestArea) {{ best = e; bestArea = area; }}
    }}
  }}
  if (best) {{
    best.scrollBy(0, px);
    return JSON.stringify({{scrolled: true, element: best.tagName.toLowerCase(), scrollY: Math.round(best.scrollTop), scrollHeight: best.scrollHeight}});
  }}
  return JSON.stringify({{scrolled: false, scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight}});
}})()
"##
    )
}

/// 按键。合成事件不会引发「隐式提交」和「Tab 移焦」，这两件事自己补。
fn press_js(key: &str) -> String {
    let name = js_quote(key);
    format!(
        r##"
(function() {{
  var name = '{name}';
  var CODES = {{
    Enter: 13, Tab: 9, Escape: 27, Backspace: 8, Delete: 46, ' ': 32,
    ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39,
    PageUp: 33, PageDown: 34, Home: 36, End: 35
  }};
  var code = CODES[name];
  if (code === undefined) return JSON.stringify({{error: '不支持的按键: ' + name + '（支持 Enter Tab Escape Backspace Delete Space Arrow* PageUp PageDown Home End）'}});
  var el = document.activeElement || document.body;
  function mk(type) {{
    var e = new KeyboardEvent(type, {{
      key: name, code: name, keyCode: code, which: code,
      bubbles: true, cancelable: true, view: window
    }});
    try {{
      Object.defineProperty(e, 'keyCode', {{get: function() {{ return code; }}}});
      Object.defineProperty(e, 'which', {{get: function() {{ return code; }}}});
    }} catch (err) {{}}
    return e;
  }}
  var target = (el === document.body) ? document : el;
  target.dispatchEvent(mk('keydown'));
  target.dispatchEvent(mk('keypress'));
  target.dispatchEvent(mk('keyup'));

  var acted = '';
  if (name === 'Enter') {{
    var form = el.form || (el.closest && el.closest('form'));
    if (form) {{
      try {{
        if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(); acted = '已提交表单'; }}
        else {{
          var b = form.querySelector('button[type=submit], input[type=submit]');
          if (b) {{ b.click(); acted = '已点提交键'; }}
        }}
      }} catch (err) {{ acted = '提交表单失败: ' + err.message; }}
    }}
  }}
  if (name === 'Tab') {{
    var all = document.querySelectorAll('a[href], button, input, select, textarea, [tabindex]');
    var list = [];
    for (var i = 0; i < all.length; i++) {{
      var e2 = all[i];
      if (e2.disabled || e2.tabIndex < 0) continue;
      var st = getComputedStyle(e2);
      if (st.display === 'none' || st.visibility === 'hidden') continue;
      list.push(e2);
    }}
    var at = list.indexOf(el);
    if (list.length) {{
      var next = list[(at + 1 + list.length) % list.length];
      try {{ next.focus(); acted = '焦点移到 <' + next.tagName + '>'; }} catch (err) {{}}
    }}
  }}
  if (name === 'Escape' && document.activeElement && document.activeElement.blur) {{
    try {{ document.activeElement.blur(); acted = '已取消焦点'; }} catch (err) {{}}
  }}
  return JSON.stringify({{pressed: name, tag: el.tagName, acted: acted}});
}})()
"##
    )
}

/// 列出一个 selector 匹配到的元素。坐标是**视口坐标**（`click` 的 x/y 要的
/// 正是它），另给 pageY 让模型知道要滚多远。
fn find_elements_js(selector: &str) -> String {
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var all = document.querySelectorAll('{q}');
  var sx = window.scrollX || 0, sy = window.scrollY || 0;
  var items = [];
  var limit = Math.min(all.length, 20);
  for (var i = 0; i < limit; i++) {{
    var el = all[i];
    var r = el.getBoundingClientRect();
    var st = getComputedStyle(el);
    items.push({{
      i: i,
      tag: el.tagName,
      text: (el.innerText || el.value || '').replace(/\s+/g, ' ').trim().substring(0, 80),
      href: (el.href || '').substring(0, 200),
      x: Math.round(r.x + r.width / 2),
      y: Math.round(r.y + r.height / 2),
      w: Math.round(r.width), h: Math.round(r.height),
      pageY: Math.round(r.top + sy),
      visible: r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < window.innerHeight &&
               st.display !== 'none' && st.visibility !== 'hidden'
    }});
  }}
  return JSON.stringify({{count: all.length, shown: limit, items: items, scrollY: Math.round(sy)}});
}})()
"##
    )
}

/// 页面骨架：不把 HTML 丢给模型（太大且全是噪音），而是抽一棵**可见元素树**。
///
/// 1. 挑候选：可见、够大、且「有内容」（语义元素，或自己带文字且没有可见子元素）；
/// 2. **合并同位置的包装层**（±8px）—— 现代页面上百个无意义 div 靠这步消失；
/// 3. 每个节点算一个**唯一的短 selector**（这是模型能"点"的前提）；
/// 4. 内外层文字一样时只留内层；
/// 5. 按面积建树、剪掉空壳、按 max_depth 截断；
/// 6. 直接拼成缩进文本（模型读的就是它），带上 rect。
fn backbone_js(max_depth: usize) -> String {
    BACKBONE_JS
        .replace("__MAX_DEPTH__", &max_depth.to_string())
        .replace("__LIMIT__", &BACKBONE_MAX_NODES.to_string())
}

const BACKBONE_JS: &str = r##"
(function() {
  var MAX_DEPTH = __MAX_DEPTH__;
  var MERGE = 8;
  var MIN = 6;
  var LIMIT = __LIMIT__;

  function vis(el) {
    var st = getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
    var r = el.getBoundingClientRect();
    return r.width >= MIN && r.height >= MIN;
  }
  var SEM = {IMG:1, SVG:1, VIDEO:1, CANVAS:1, INPUT:1, TEXTAREA:1, SELECT:1, BUTTON:1, A:1, IFRAME:1, AUDIO:1};
  var CLICK = {A:1, BUTTON:1, INPUT:1, TEXTAREA:1, SELECT:1, SUMMARY:1};
  var SKIP = {SCRIPT:1, STYLE:1, NOSCRIPT:1, BR:1, HR:1, META:1, LINK:1, TEMPLATE:1, HEAD:1};
  function text(el) { return ((el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim()); }
  function isLeaf(el) {
    if (SKIP[el.tagName]) return false;
    if (SEM[el.tagName]) return true;
    var kids = el.children;
    for (var i = 0; i < kids.length; i++) {
      if (!SKIP[kids[i].tagName] && vis(kids[i])) return false;
    }
    return true;
  }
  function near(a, b) {
    return Math.abs(a.left - b.left) <= MERGE && Math.abs(a.top - b.top) <= MERGE &&
           Math.abs(a.right - b.right) <= MERGE && Math.abs(a.bottom - b.bottom) <= MERGE;
  }
  // selector **必须唯一** —— 模型拿它去点，指到十几个元素上就点不动了。
  function uniq(s) { try { return document.querySelectorAll(s).length === 1; } catch (e) { return false; } }
  // 数字开头的 id **不能**写成 `#50051583`（CSS 里非法，querySelector 直接抛）
  function idSel(id) {
    if (/^[A-Za-z_-][A-Za-z0-9_-]*$/.test(id)) return '#' + id;
    return '[id="' + String(id).replace(/"/g, '\\"') + '"]';
  }
  function sel(el) {
    if (el.id) { var byId = idSel(el.id); if (uniq(byId)) return byId; }
    var parts = [], cur = el, guard = 0;
    while (cur && cur !== document.body && cur !== document.documentElement && guard++ < 8) {
      if (cur.id) { parts.unshift(idSel(cur.id)); break; }
      var seg = cur.tagName.toLowerCase();
      var p = cur.parentElement;
      if (p) {
        var same = 0, idx = 0;
        for (var i = 0; i < p.children.length; i++) {
          if (p.children[i].tagName === cur.tagName) { same++; if (p.children[i] === cur) idx = same; }
        }
        if (same > 1) seg += ':nth-of-type(' + idx + ')';
      }
      parts.unshift(seg);
      cur = p;
      if (parts.length >= 2 && uniq(parts.join(' > '))) return parts.join(' > ');
    }
    return parts.join(' > ');
  }

  var all = document.body.querySelectorAll('*');
  var nodes = [];
  for (var i = 0; i < all.length; i++) {
    var el = all[i];
    if (!vis(el) || !isLeaf(el)) continue;
    var r = el.getBoundingClientRect();
    var t = text(el);
    var interesting = t.length > 1 || SEM[el.tagName] || (el.tagName === 'A' && el.href);
    if (!interesting) continue;
    // 合并同位置的包装层：一路提升到最外层的"外壳"。
    // **碰见可点的语义元素就停** —— 继续往上合并的话 selector 会指到一个
    // div/td/span 上，而点 wrapper 是点不出反应的。
    var rep = el, rr = r;
    var up = el.parentElement;
    while (!CLICK[rep.tagName] && up && up !== document.body && up !== document.documentElement &&
           vis(up) && near(rr, up.getBoundingClientRect())) {
      rep = up; rr = up.getBoundingClientRect();
      up = up.parentElement;
    }
    var dup = false;
    for (var k = 0; k < nodes.length; k++) { if (nodes[k].el === rep) { dup = true; break; } }
    if (dup) continue;
    nodes.push({el: rep, rect: rr, area: rr.width * rr.height, text: t.substring(0, 200)});
  }

  // 外层和内层文字一模一样时只留内层（三层包装一份标题 = 三倍噪音，
  // 而且**能点的通常是内层那个 a**）
  var kept = [];
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i], dupText = false;
    if (n.text) {
      for (var j = 0; j < nodes.length; j++) {
        if (j !== i && nodes[j].el !== n.el && n.el.contains(nodes[j].el) && nodes[j].text === n.text) { dupText = true; break; }
      }
    }
    if (!dupText) kept.push(n);
  }
  nodes = kept;

  nodes.sort(function(a, b) { return b.area - a.area; });
  function contains(a, b) { return a.el !== b.el && a.el.contains(b.el); }
  var roots = [];
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i], parent = null, best = Infinity;
    for (var j = 0; j < i; j++) {
      if (contains(nodes[j], n) && nodes[j].area < best) { parent = nodes[j]; best = nodes[j].area; }
    }
    if (parent) { (parent.kids = parent.kids || []).push(n); } else { roots.push(n); }
  }
  function prune(n) {
    (n.kids || []).forEach(prune);
    if ((!n.kids || n.kids.length === 1) && !n.text) {
      var c = n.kids && n.kids[0];
      if (c) { n.el = c.el; n.rect = c.rect; n.text = c.text; n.kids = c.kids; }
    }
    if (n.kids) n.kids = n.kids.filter(function(k) { return (k.kids && k.kids.length) || k.text || SEM[k.el.tagName]; });
  }
  function textOf(n) { return (n.text || '').substring(0, 120); }
  roots.forEach(prune);
  roots = roots.filter(function(r) { return textOf(r) || SEM[r.el.tagName] || (r.kids && r.kids.length); });

  var lines = [], count = 0, depth = 0;
  function emit(n, d, pad) {
    if (count >= LIMIT) return;
    count++; if (d > depth) depth = d;
    var el = n.el, r = n.rect;
    var head = pad + '<' + el.tagName.toLowerCase() + '>';
    if (el.id) head += ' #' + el.id;
    var cls = (typeof el.className === 'string' ? el.className : '').trim().split(/\s+/).filter(Boolean).slice(0, 2);
    if (cls.length) head += ' .' + cls.join('.');
    head += ' | ' + sel(el);
    head += ' | ' + Math.round(r.left) + ',' + Math.round(r.top) + ' ' + Math.round(r.width) + 'x' + Math.round(r.height);
    lines.push(head);
    var t = textOf(n);
    if (t) lines.push(pad + '  "' + t + '"');
    if (el.tagName === 'A' && el.href) {
      try {
        var u = new URL(el.href);
        var host = (u.host === location.host) ? '' : (u.host + ' ');
        lines.push(pad + '  -> ' + host + u.pathname + (u.search || ''));
      }
      catch (e) { lines.push(pad + '  -> ' + el.href.substring(0, 100)); }
    }
    if (el.tagName === 'IMG') lines.push(pad + '  img ' + (el.naturalWidth || 0) + 'x' + (el.naturalHeight || 0) + ' ' + (el.currentSrc || el.src || '').substring(0, 100));
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
      var tp = el.type || el.tagName.toLowerCase();
      var v = el.type === 'password' ? '[redacted]' : (el.value || '');
      lines.push(pad + '  input ' + tp + (v ? ' value="' + String(v).substring(0, 60) + '"' : '') + (el.placeholder ? ' placeholder="' + el.placeholder.substring(0, 60) + '"' : ''));
    }
    if (d >= MAX_DEPTH) return;
    (n.kids || []).forEach(function(k) { emit(k, d + 1, pad + '  '); });
  }
  roots.forEach(function(r) { emit(r, 1, ''); });
  if (!lines.length) lines.push('（这一屏没有可读的可见元素）');
  return JSON.stringify({text: lines.join('\n'), nodes: count, depth: depth});
})()
"##;

/// 抽正文（Readability 的简化版，只留真管用的那几条）。
const READABLE_JS: &str = r##"
(function() {
  var SKIP_TAGS = {NAV:1, FOOTER:1, ASIDE:1, HEADER:1, FORM:1, SCRIPT:1, STYLE:1, NOSCRIPT:1, IFRAME:1, BUTTON:1, SELECT:1};
  var JUNK = /(^|[\s_-])(nav|menu|sidebar|footer|header|comment|share|sharing|social|promo|ads?|advert|breadcrumb|cookie|consent|privacy|legal|terms|gdpr|ccpa|disclaimer|banner|subscribe|newsletter|signup|login|paywall|related|recommend|popup|modal|toolbar|masthead)([\s_-]|$)/i;
  var POS = /(^|[\s_-])(article|body|content|entry|main|page|post|story|text|blog|read)([\s_-]|$)/i;
  var NEG = /(^|[\s_-])(nav|menu|sidebar|footer|comment|promo|related|recommend|popular|banner|cookie|consent|privacy|legal|terms|gdpr|subscribe|newsletter|signup|login|paywall|popup|modal|advert|ads?|social|share|widget|tool|tags)([\s_-]|$)/i;
  function t(el) { return (el.innerText || el.textContent || '').trim(); }
  function names(el) {
    return (typeof el.className === 'string' ? el.className : '') + ' ' +
           (el.id || '') + ' ' + (el.getAttribute ? (el.getAttribute('role') || '') : '');
  }
  function inChrome(el) {
    try { return !!el.closest('nav, footer, aside, header, form, [aria-hidden="true"]'); } catch (e) { return false; }
  }
  // 自己或**上面 6 层**里任何一层像周边，就当周边。只查自己不够 ——
  // 实测 CNBC 的 cookie 声明是一堆正常 <p>，包在一个 `#...CookieNotice` 壳里。
  function junk(el) {
    var cur = el, depth = 0;
    while (cur && cur !== document.body && depth++ < 6) {
      if (JUNK.test(names(cur))) return true;
      cur = cur.parentElement;
    }
    return false;
  }
  function hidden(el) {
    var st = getComputedStyle(el);
    return st.display === 'none' || st.visibility === 'hidden';
  }
  function bad(el) { return SKIP_TAGS[el.tagName] || inChrome(el) || hidden(el) || junk(el); }
  function weight(el) {
    var s = names(el);
    if (NEG.test(s)) return 0.15;
    if (POS.test(s)) return 2;
    return 1;
  }

  var h1 = null;
  var hs = document.querySelectorAll('h1');
  for (var i = 0; i < hs.length; i++) {
    if (!bad(hs[i]) && t(hs[i]).length > 3) { h1 = hs[i]; break; }
  }

  var cands = document.querySelectorAll('article, main, [role=main], div, section, td');
  var best = null, bestScore = 0;
  for (var i = 0; i < cands.length; i++) {
    var c = cands[i];
    if (bad(c)) continue;
    var ps = c.querySelectorAll('p');
    if (!ps.length) continue;
    var s = 0;
    for (var j = 0; j < ps.length; j++) {
      var p = ps[j];
      if (bad(p)) continue;
      var pt = t(p);
      if (pt.length < 25) continue;
      s += 1 + Math.min(pt.length / 100, 3);
    }
    if (s <= 0) continue;
    s *= weight(c);
    if (h1 && c.contains(h1)) s += 20;
    var all = t(c);
    var linkLen = 0;
    var as = c.querySelectorAll('a');
    for (var k = 0; k < as.length; k++) linkLen += t(as[k]).length;
    if (all.length > 0 && linkLen / all.length > 0.5) s *= 0.3;
    s += Math.min(all.length / 1000, 5);
    if (s > bestScore) { bestScore = s; best = c; }
  }

  var host = best, source = 'best-block';
  if (!host || bestScore < 6) { host = document.body; source = 'body(fallback)'; }

  var chunks = [];
  var nodes = host.querySelectorAll('h1, h2, h3, p, li, blockquote, pre, figcaption');
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i];
    if (bad(n)) continue;
    if (n.tagName === 'LI' && n.querySelector('p')) continue;
    var txt = t(n);
    if (!txt || txt.length < 2) continue;
    if (n.tagName === 'PRE') { chunks.push('```\n' + (n.innerText || '') + '\n```'); continue; }
    if (n.tagName === 'H1' || n.tagName === 'H2' || n.tagName === 'H3') { chunks.push('## ' + txt); continue; }
    if (chunks.length > 400) break;
    var links = n.querySelectorAll('a[href]');
    if (links.length === 1 && t(links[0]).length > txt.length * 0.6) {
      try {
        var u = new URL(links[0].href);
        var where = (u.host === location.host) ? (u.pathname + (u.search || '')) : (u.host + u.pathname);
        chunks.push(txt + '  [' + where + ']');
        continue;
      } catch (e) {}
    }
    chunks.push(txt);
  }
  var out = [];
  for (var i = 0; i < chunks.length; i++) {
    if (i > 0 && chunks[i] === chunks[i - 1]) continue;
    out.push(chunks[i]);
  }
  var body = out.join('\n\n');
  return JSON.stringify({text: body.substring(0, 40000), length: body.length, source: source, title: document.title, url: location.href});
})()
"##;

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 选择器 / 文本里的引号和换行不能把注入的 JS 拼坏（引号逃逸）
    #[test]
    fn js_quote_escapes_what_would_break_out() {
        assert_eq!(js_quote("a'b"), "a\\'b");
        assert_eq!(js_quote("a\\b"), "a\\\\b");
        assert_eq!(js_quote("a\nb"), "a\\nb");
        assert_eq!(js_quote("a\u{2028}b"), "a\\u2028b");
        // 拼进 JS 之后，那个字面量里不该出现裸的引号
        let js = click_sel_js("div[data-x='y']");
        assert!(
            js.contains(r#"querySelectorAll('div[data-x=\'y\']')"#),
            "{js}"
        );
    }

    /// 骨架 JS 的两个占位符都得被替换掉（留着的话页面里是语法错误）
    #[test]
    fn backbone_js_has_no_placeholders_left() {
        let js = backbone_js(3);
        assert!(
            !js.contains("__MAX_DEPTH__") && !js.contains("__LIMIT__"),
            "{js}"
        );
        assert!(js.contains("var MAX_DEPTH = 3;"));
        assert!(js.contains(&format!("var LIMIT = {BACKBONE_MAX_NODES};")));
        // 深度要夹在 1..10（调用方保证；这里钉住 clamp 那行没被改掉）
        let clamped = backbone_js(10);
        assert!(clamped.contains("var MAX_DEPTH = 10;"));
    }

    /// 每个动作名都得能进 dispatch（不然 CLI 发得出去、后端回「未知动作」）
    #[test]
    fn every_action_is_reachable() {
        let b = CdpBrowser::new("http://127.0.0.1:1".into(), shots_for_test());
        let mut g = Inner {
            conn: None,
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
    fn the_offline_error_tells_you_how_to_start_chrome() {
        let e = match Conn::connect("http://127.0.0.1:1") {
            Ok(_) => panic!("127.0.0.1:1 上不该有东西在听"),
            Err(e) => e,
        };
        assert!(e.contains("just chrome"), "{e}");
        assert!(e.contains("--remote-debugging-port"), "{e}");
    }

    /// **真 Chrome 的端到端**：起一个本地的假页面，把动作过一遍。
    ///
    /// 默认跳过（CI / 别人机器上不一定有 Chrome，也不一定有调试端口）：
    ///     just chrome                        # 另开一个终端
    ///     AGENT_CDP_TEST=1 cargo test cdp::
    #[test]
    fn end_to_end_against_a_real_chrome() {
        if std::env::var("AGENT_CDP_TEST").as_deref() != Ok("1") {
            eprintln!("跳过（设 AGENT_CDP_TEST=1 且先 `just chrome` 才跑）");
            return;
        }
        let (base, _srv) = serve_test_page();
        let b = CdpBrowser::new("http://127.0.0.1:9222".into(), shots_for_test());
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
