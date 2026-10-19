//! HTTP / SSE / WebSocket 服务。
//!
//! 前端与 API **同源**（决策 D3），所以不需要 CORS，且 `http://127.0.0.1`
//! 是 secure context —— `crypto.subtle`、Service Worker 都能用。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::llm::Provider;
use crate::sse;
use crate::state::{same_secret, AppState, Command, Event, VERSION};
use crate::Core;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/command", post(post_command))
        .route("/api/linux", post(bash_run))
        .route("/api/procs", get(list_procs))
        .route("/api/pair", post(pair))
        .route("/api/pairing", get(pairing_info))
        .route("/api/alpine", get(alpine_status))
        .route("/api/alpine/upgrade", post(alpine_upgrade))
        .route("/api/browser", post(browser_action))
        .route("/api/weather", post(weather_run))
        // HTML → 正文（`agent readable`）。请求体就是 HTML 本身，不包 JSON
        .route("/api/readable", post(readable_run))
        .route("/api/mcp", post(mcp_run))
        .route("/api/mcp/servers", get(mcp_servers).post(save_mcp_server))
        .route("/api/mcp/servers/delete", post(delete_mcp_server))
        .route("/api/mcp/servers/enabled", post(set_mcp_enabled))
        .route("/api/skills", get(list_skills))
        .route("/api/usage", get(usage_summary))
        .route("/api/skills/enabled", post(set_skill_enabled))
        // 看一眼工作目录（`agent files`）—— 只读，不动任何文件
        .route("/api/files", post(list_files))
        // 手机系统能力（闹钟 / 短信 / 日历 / 联系人 / 定位 / 剪贴板…）。
        // 沙箱里的 `agent <设备命令>` 走它（见 `phone` 模块）。
        .route("/api/device", post(device_command))
        .route("/api/websearch", post(web_search))
        .route(
            "/api/websearch/config",
            get(search_config).post(set_search_config),
        )
        .route("/api/edit", post(edit_file))
        .route("/api/llm", post(set_llm))
        .route("/api/models", get(get_models))
        .route("/api/providers", get(list_providers))
        .route(
            "/api/providers/{id}",
            post(save_provider).delete(delete_provider),
        )
        .route("/api/providers/{id}/api-style", post(set_api_style))
        .route("/api/model", post(set_model))
        .route(
            "/api/attachment",
            // 附件是真文件（PDF 可能几十 MB），默认 2MB 的 body 限制
            // 在它进 handler 之前就 413 了
            post(upload_attachment).layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/attachment/{id}", get(get_attachment))
        .route("/api/stop", post(stop_turn))
        .route("/api/chat", post(chat))
        .route("/api/session", get(get_session))
        .route("/api/debug/state", get(debug_state))
        .route("/api/debug/request", get(debug_request))
        .route("/api/debug/tools", get(debug_tools))
        .route("/api/debug/system-prompt", post(debug_set_system_prompt))
        .route("/api/sessions", get(list_sessions).post(new_session))
        .route("/api/search", get(search_sessions))
        .route("/api/sessions/activate", post(activate_session))
        .route("/api/sessions/rename", post(rename_session))
        .route("/api/sessions/delete", post(delete_session))
        .route("/api/sessions/unqueue", post(unqueue))
        .route("/api/remote", get(remote))
        .route("/api/remote/relay", post(set_relay))
        // 「选择机器」：这份 API **永远是本机核心**答的 —— 经隧道时隧道也把
        // `/api/machines*` 转回本机（见 `tunnel.rs`）。问的是「我这台机器能连哪些电脑」，
        // 答案在本机的库里，不在正连的那台上。
        .route("/api/machines", get(machines_list))
        .route("/api/machines/connect", post(machines_connect))
        .route("/api/machines/pair", post(machines_pair))
        .route("/api/machines/local", post(machines_local))
        .route("/api/machines/forget", post(machines_forget))
        .route("/api/reset", post(reset))
        .route("/api/device/log", post(device_log))
        .route("/api/ws", get(ws_upgrade))
        // 回合状态的 SSE 流。手机壳订阅它来刷通知栏 / 起停前台服务；它和 `/api/ws`
        // 是同一份东西（`Event` 的 JSON），只是换一个壳好读的传输（见 `turns_stream`）。
        .route("/api/turns/stream", get(turns_stream))
        .route("/auth", get(auth_login))
        .route("/sandbox/{key}", get(sandbox_root))
        .route("/sandbox/{key}/", get(sandbox_root))
        .route("/sandbox/{key}/{*path}", get(sandbox_file))
        .fallback(crate::assets::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .layer(axum::middleware::from_fn(split_origins))
        .with_state(state)
}

/// 沙箱网页用的主机名（独立的源）。见下面「沙箱文件」那段的说明。
///
/// `*.localhost` 由 Chromium 直接解析到 loopback（不走 DNS），而它和
/// `127.0.0.1` 是**不同的站点**：App 的凭据 cookie 种在 `127.0.0.1` 上，
/// 这个源上的页面拿不到、也发不出带它的请求（SameSite=Strict）。
pub const SANDBOX_HOST: &str = "sandbox.localhost";

/// 沙箱网页的源（浏览器里打开沙箱文件用）。端口跟 App 同一个。
pub fn sandbox_origin(port: u16) -> String {
    format!("http://{SANDBOX_HOST}:{port}")
}

/// 这个请求是不是打到沙箱那个源上的（看 `Host` 头）。
fn is_sandbox_host(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    name.eq_ignore_ascii_case(SANDBOX_HOST)
}

/// 两个源分开：`sandbox.localhost` 上**只有沙箱文件**。
///
/// - 别的路径（`/api`、`/auth`、App 自己的页面）一律 404 —— 沙箱网页和
///   App 不同源，靠的就是这一条；哪天有人在这个源上种了凭据也开不了 `/api`。
/// - 沙箱文件在这里**不加** CSP sandbox（见 `sandbox_headers`）：页面有自己
///   正常的源，localStorage / IndexedDB / cookie 都能用。打到 `127.0.0.1`
///   上的沙箱文件（和 App 同源）照旧是 opaque origin。
async fn split_origins(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if !is_sandbox_host(req.headers()) {
        return next.run(req).await;
    }
    if !req.uri().path().starts_with("/sandbox/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut res = next.run(req).await;
    res.headers_mut().remove("content-security-policy");
    res
}

// ---------------------------------------------------------------------------
// 鉴权
// ---------------------------------------------------------------------------
//
// 威胁：手机上**任何**有网络权限的 App 都能连 127.0.0.1，扫一遍端口就能
// 找到核心，然后 `/api/linux` 在沙箱里跑命令 —— 沙箱挂着整个内部存储。
// 所以 `/api/*` 一律要凭据（见 `state::Auth`）：
//
// - App 的 WebView：Kotlin 打开 `/auth?t=<token>`，这里种一个 HttpOnly
//   cookie 再跳回 `/`。之后 fetch / WebSocket / <img> 都自动带上，前端
//   一行代码不用改；页面里的 JS（包括沙箱 iframe）也读不到它。
// - 沙箱里的 `agent` 命令、电脑上的调试脚本：`x-agent-token` 头。
//
// 前端的静态资源（`/`、js、css）不设防：那是公开的代码，不含任何秘密。

const AUTH_COOKIE: &str = "agent_token";
const AUTH_HEADER: &str = "x-agent-token";

/// 请求里带的凭据：先看头，再看 cookie。
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get(AUTH_HEADER).and_then(|v| v.to_str().ok()) {
        return Some(v);
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == AUTH_COOKIE)
        .map(|(_, v)| v)
}

/// 这条连接是怎么进来的（`serve_conn` 给每个请求挂上，`require_auth` 看它）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// 明文 HTTP：手机自己的核心（127.0.0.1）、电脑上自己开的浏览器、局域网里的浏览器
    Plain,
    /// 局域网里的 TLS：手机 App 连电脑（只认配对时记下的证书，见 `tunnel.rs`）
    LanTls,
    /// 经中继进来的 TLS：出门在外的手机（见 `remote.rs`）
    Relay,
}

/// 服务一条连接（HTTP/1.1，带 WebSocket 升级）。**三种连接都走这里**（见 `Via`）：
/// 明文、局域网 TLS、中继 —— 界面、API、鉴权都是同一份路由，区别只在请求上挂的
/// 对端地址和 `Via`。
pub async fn serve_conn<IO>(io: IO, router: Router, peer: SocketAddr, via: Via)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = tower::service_fn(move |mut req: http::Request<hyper::body::Incoming>| {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        req.extensions_mut().insert(via);
        let router = router.clone();
        async move { tower::ServiceExt::oneshot(router, req).await }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(
            hyper_util::rt::TokioIo::new(io),
            hyper_util::service::TowerToHyperService::new(svc),
        )
        .with_upgrades()
        .await;
}

/// 一条刚接进来的 TCP 连接：第一个字节是 TLS 握手（0x16）就走 TLS，否则当明文 HTTP。
///
/// 一个端口两种用法：电脑上自己开浏览器还是 `http://127.0.0.1:8787`，手机 App 走 TLS。
/// 没有 TLS 身份（手机自己的核心、只听本机的电脑）就只有明文。
pub async fn accept_conn(
    s: tokio::net::TcpStream,
    peer: SocketAddr,
    router: Router,
    tls: Option<tokio_rustls::TlsAcceptor>,
) {
    let _ = s.set_nodelay(true);
    if let Some(acceptor) = tls {
        let mut first = [0u8; 1];
        let is_tls = matches!(
            tokio::time::timeout(Duration::from_secs(10), s.peek(&mut first)).await,
            Ok(Ok(1))
        ) && first[0] == 0x16;
        if is_tls {
            match acceptor.accept(s).await {
                Ok(t) => serve_conn(t, router, peer, Via::LanTls).await,
                // 多半是对面不认这张证书（不是配对过的手机）或者半路断了
                Err(e) => tracing::debug!("{peer}: TLS 握手失败：{e}"),
            }
            return;
        }
    }
    serve_conn(s, router, peer, Via::Plain).await;
}

async fn require_auth(
    State(st): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // 沙箱文件在 handler 里核对路径上的钥匙；静态资源和 /auth 本身不设防
    // `/api/pair` 是**换**凭据的那一步（拿配对码换 token），它自己核对配对码
    if req.uri().path() == "/api/pair" && req.extensions().get::<Via>() == Some(&Via::Relay) {
        return (StatusCode::FORBIDDEN, "配对只能在局域网里做").into_response();
    }
    if !req.uri().path().starts_with("/api/") || req.uri().path() == "/api/pair" {
        return next.run(req).await;
    }
    let Some(token) = st.auth().token else {
        return next.run(req).await; // 桌面开发没设 AGENT_TOKEN
    };
    // 桌面：这台电脑自己（loopback）来的不要凭据 —— 宿主模式默认对局域网开放，
    // 凭据是给局域网里的手机的；电脑上自己开浏览器还是 http://127.0.0.1:8787 直接用。
    // **手机上绝不能开**：那里别的 App 也能连 127.0.0.1（见上面「威胁」）。
    if st.trust_loopback.load(std::sync::atomic::Ordering::Relaxed)
        && req
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .is_some_and(|c| c.0.ip().is_loopback())
    {
        return next.run(req).await;
    }
    match presented_token(req.headers()) {
        Some(t) if same_secret(t, &token) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "需要凭据").into_response(),
    }
}

#[derive(Deserialize)]
struct AuthQuery {
    #[serde(default)]
    t: String,
}

/// `GET /auth?t=<token>` → 种 cookie，跳回首页。App 启动时 Kotlin 打开的就是它。
///
/// 跳转是为了让地址栏里不留 token（WebView 的历史、页面的 `location` 都看不到）。
async fn auth_login(State(st): State<Arc<AppState>>, Query(q): Query<AuthQuery>) -> Response {
    let ok = match st.auth().token {
        Some(token) => same_secret(&q.t, &token),
        None => true,
    };
    if !ok {
        return (StatusCode::FORBIDDEN, "凭据不对").into_response();
    }
    let mut res = axum::response::Redirect::to("/").into_response();
    if let Some(token) = st.auth().token {
        // SameSite=Strict：别的网站发来的请求（手机浏览器里的随便一个网页）
        // 不带它。没有 Max-Age：会话 cookie，App 每次启动都会重新种。
        let c = format!("{AUTH_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict");
        if let Ok(v) = HeaderValue::from_str(&c) {
            res.headers_mut().insert(header::SET_COOKIE, v);
        }
    }
    res
}

// ---------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------

async fn health(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dev = st.device();
    // 优先运行时配置，回退环境变量（桌面）
    let provider = st.llm().or_else(Provider::from_env);
    // 能力判定只看缓存（health 不能联网等 models.dev）
    let capabilities = provider.as_ref().map(|p| {
        json!({
            "vision": st.cached_supports_images(p),
            "pdf": st.cached_supports_files(p),
            "apiStyle": p.api_style,
        })
    });
    Json(json!({
        "ok": true,
        "version": VERSION,
        "git": crate::state::GIT,
        "port": st.port.load(std::sync::atomic::Ordering::Relaxed),
        "platform": std::env::consts::OS,
        "mock": st.mock.load(std::sync::atomic::Ordering::Relaxed),
        "device": dev.map(|d| d.name()),
        // 这个核心跑在哪台机器上：界面（侧栏底栏那个小标签）靠它显示
        // 「现在连的是哪台电脑」。手机上给 `null`（本机就是这台手机，见函数注释）。
        "computer": computer_name(),
        // 这个核心的实例 id：页面据此知道「我是哪台给的」（「选择机器」里勾哪一行，
        // 见前端 `lib/machines.svelte.ts`）。经隧道时这个接口是那台电脑答的。
        "id": st.instance_id(),
        "linux": st.linux().map(|s| json!({
            "rootfs": s.rootfs().display().to_string(),
            "host": s.is_host(),
            // 宿主模式下 rootfs 是 `/`，有信息量的是命令的 cwd
            "workspace": s.workspace().map(|w| w.display().to_string()),
        })),
        // 浏览器后端是谁（手机上=壳里的 WebView，桌面上=CDP + 调试端口）
        "browser": st.browser().map(|b| b.describe()),
        // 设备命令后端（只有手机上才是 Some —— 见 `phone` 模块）
        "phone": st.phone().map(|p| p.describe()),
        // 只报“配没配”，不泄 key
        "llm": provider.map(|p| json!({
            "name": p.name,
            "model": p.model,
            "baseUrl": p.base_url,
            "reasoningEffort": p.reasoning_effort,
        })),
        // 界面靠它决定要不要显示「文件」那个附件入口
        "capabilities": capabilities,
        // 沙箱文件的 URL 前缀要它（`/sandbox/<key>/…`）。这个接口本身要凭据，
        // 所以只有 App 自己的页面拿得到。
        "sandboxKey": st.auth().sandbox_key,
    }))
}

/// 这台机器叫什么（macOS 是「电脑名称」，Linux 是主机名）。
///
/// **手机上刻意是 `None`**：安卓进程里的 `gethostname()` 是 `localhost`，
/// 一点信息量都没有 —— 那里「本机」的意思很明确（就是你手里这台手机），
/// 界面自己说得出来。电脑上这个名字才有用：手机可能连的是客厅那台，
/// 也可能是书房的。名字的取法和 mDNS 广播、配对码那一栏**同一个**
/// （`desktop::computer_name`），三处显示的是同一台机器。
fn computer_name() -> Option<String> {
    (std::env::consts::OS != "android").then(crate::desktop::computer_name)
}

#[derive(Deserialize)]
struct CommandBody {
    cmd: Command,
}

/// 前端 → Rust → 设备。这是"人按按钮"的通路。
/// 回 `{id, ok, detail}`；带 `?format=text` 时回纯文本（`agent key / tap / swipe /
/// launch / open` 用）：成功就是 `detail`，失败写成 `错误：…`。
async fn post_command(
    State(st): State<Arc<AppState>>,
    Query(q): Query<FormatQuery>,
    Json(body): Json<CommandBody>,
) -> Response {
    let ack = dispatch(&st, body.cmd).await;
    if q.format.as_deref() != Some("text") {
        return Json(ack).into_response();
    }
    let detail = ack
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .trim();
    let text = if ack.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        if detail.is_empty() {
            "完成".to_string()
        } else {
            detail.to_string()
        }
    } else {
        format!(
            "错误：{}",
            if detail.is_empty() {
                "失败了"
            } else {
                detail
            }
        )
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

#[derive(Deserialize)]
struct FormatQuery {
    #[serde(default)]
    format: Option<String>,
}

#[derive(Deserialize)]
struct BashBody {
    /// 要执行的命令（对应 `bash` 工具的 `command` 参数）
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct EditBody {
    path: String,
    edits: Vec<EditItem>,
}

#[derive(Deserialize)]
struct EditItem {
    #[serde(rename = "oldText")]
    old_text: String,
    #[serde(rename = "newText")]
    new_text: String,
}

/// `POST /api/linux` → 直接在 Linux 沙箱里跑一段命令。
///
/// 绕开 LLM，方便单独测沙箱（也是前端"终端"页的接口）。
async fn bash_run(State(st): State<Arc<AppState>>, Json(body): Json<BashBody>) -> Response {
    let core = Core { state: st.clone() };
    let timeout = body
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(crate::linux::DEFAULT_TIMEOUT)
        .min(crate::linux::MAX_TIMEOUT);

    match core.run_linux(&body.command, timeout, None).await {
        Ok(r) => Json(json!({
            "ok": r.code == 0,
            "output": r.to_text(),
            "exit_code": r.code,
            "truncated": r.truncated,
            "full_output_path": r.full_output_path,
            "timed_out": r.timed_out,
            "signal": r.signal,
            "wall_time_seconds": (r.duration_ms as f64) / 1000.0,
        }))
        .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/procs` → 沙箱里现在还有什么在跑（`agent ps` 的后端）。
///
/// 为什么看进程这件事得在核心侧做：只有核心记得每次运行起的是哪个 proot，
/// 而 guest 进程的 `TracerPid` 就是它 —— 凭这个才能认出**命令结束后仍活着**的
/// 常驻进程（`setsid` / `nohup` 刻意脱离的那些，父进程早就是 init 了，
/// 靠 `ps` 自己认不出来），也才能在 /proc 被 hidepid 限制的机器上诚实地说
/// 「看不见」。见 `linux::LinuxSandbox::procs`。
///
#[derive(Deserialize)]
struct PairBody {
    code: String,
}

/// `POST /api/pair` `{code}` → `{token, id, name}`：手机第一次连这台电脑时拿配对码
/// 换访问凭据（见 `desktop::Pairing`）。只有桌面宿主模式对局域网开放时才开着（别的时候 404）。
async fn pair(State(st): State<Arc<AppState>>, Json(b): Json<PairBody>) -> Response {
    let Some(p) = st.pairing.read().ok().and_then(|g| g.clone()) else {
        return (
            StatusCode::NOT_FOUND,
            "这台电脑没开配对（宿主模式对局域网开放时才有）",
        )
            .into_response();
    };
    if let Err(e) = p.check(&b.code) {
        st.log("warn", format!("配对失败：{e}"));
        return (StatusCode::FORBIDDEN, e).into_response();
    }
    let Some(token) = st.auth().token else {
        return (StatusCode::CONFLICT, "核心没设凭据").into_response();
    };
    st.log("info", "一台设备配对成功");
    Json(json!({
        "token": token,
        "id": st.instance_id(),
        "name": crate::desktop::computer_name(),
        // 出门在外怎么连（证书指纹 + 中继），见 `remote.rs`
        "remote": remote_info(&st),
    }))
    .into_response()
}

fn remote_info(st: &AppState) -> Option<crate::state::RemoteInfo> {
    st.remote.read().ok().and_then(|g| g.clone())
}

/// `POST /api/remote/relay {addr, key}` → 让这台电脑挂到这个中继上（要凭据）。
///
/// 手机连过一台配了中继的电脑，就知道中继在哪、密钥是什么；在家连上另一台**还没配**的
/// 电脑时，把它交过来 —— 那台电脑立刻挂上去、记下来，人不用去复制密钥（见 `remote.rs`）。
/// 能调这个接口的本来就拿着这台电脑的凭据（= 能在上面跑命令），没有多给出什么。
async fn set_relay(
    State(st): State<Arc<AppState>>,
    Json(b): Json<crate::state::RelayAddr>,
) -> Response {
    let Some(host) = st.relay_host.read().ok().and_then(|g| g.clone()) else {
        return (
            StatusCode::NOT_FOUND,
            "这个核心不挂中继（只有电脑上、对局域网开放时才有）",
        )
            .into_response();
    };
    match host.attach(b) {
        Ok(()) => Json(json!({ "remote": remote_info(&st) })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// `GET /api/remote` → 出门在外怎么连这台电脑（要凭据）。
///
/// 配对过的手机在局域网里连上时取一次：中继是后来才配的、或者换了服务器，都不用
/// 重新配对。没开（手机自己的核心、只听本机）就是 `null`。
async fn remote(State(st): State<Arc<AppState>>) -> Response {
    Json(json!({
        "id": st.instance_id(),
        "name": crate::desktop::computer_name(),
        "remote": remote_info(&st),
    }))
    .into_response()
}

/// `GET /api/pairing` → 手机上「连接电脑」要输的那个 6 位码（设置页展示，见前端
/// `PairingSection.svelte`）。
///
/// 这个码要只在核心启动时往终端打一行，后台跑的核心（`setsid` / 日志重定向到
/// 文件）就等于没地方看 —— 而后台跑恰恰是常态。
///
/// **只回给这台电脑自己（loopback）**：配对码的全部意义是「证明你人在电脑前」，
/// 从局域网里也能读到的话这一步就白做了。手机上看到的界面正好在局域网那头，
/// 拿到的就是 `available: false`，那一栏自然不显示。（局域网来的请求本来就要
/// 凭据，凭据比码更大 —— 但没必要把码也散出去。）
///
/// `available: false` 的三种情况：不是 loopback、不是宿主模式对局域网开放
/// （手机上、`AGENT_BIND=127.0.0.1`）、以及非桌面的核心 —— 设置页据此整栏不显示。
async fn pairing_info(
    State(st): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let pairing = if addr.ip().is_loopback() {
        st.pairing.read().ok().and_then(|g| g.clone())
    } else {
        None
    };
    let Some(p) = pairing else {
        return Json(json!({ "available": false })).into_response();
    };
    Json(json!({
        "available": true,
        "code": p.code(),
        // 0 = 输错太多次，停用了（重启电脑上的核心换新码）
        "failsLeft": p.remaining(),
        "name": crate::desktop::computer_name(),
    }))
    .into_response()
}

/// `GET /api/machines` → 「选择机器」面板要的那份（见 docs/05 的 4.5）。
///
/// 面板在**哪个页面**上打开都一样：这个接口永远是本机核心答的（经隧道时隧道把
/// `/api/machines*` 转回本机，见 `tunnel.rs`），所以你会看到「自己这台机器」的
/// 配对和发现结果，而不是正连的那台的。
///
/// 返回里的 `local` 是「回到本机」要的那个地址；`platform` 让页面把第一行写成
/// 「这台手机」还是「这台电脑」（和界面其它地方同一个依据）。
async fn machines_list(State(st): State<Arc<AppState>>) -> Response {
    let Some(m) = st.machines() else {
        return no_store();
    };
    // 经中继的那几台在不在线：后台去问（有缓存，十秒内不重问），这次先用手上的。
    // 不在这里等：面板第一帧要的就是这份列表，等中继（最多 1.5 秒）就是让它晚出来
    // 那么久 —— 面板 1.5 秒轮询一次，下一次就带上了
    tokio::spawn({
        let m = m.clone();
        async move { m.refresh_presence().await }
    });
    let list: Vec<Value> = m
        .panel()
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "host": p.host,
                "port": p.port,
                "paired": p.paired,
                "relay": p.relay,
                "current": p.current,
                "online": p.online,
            })
        })
        .collect();
    Json(json!({
        "local": {
            "url": format!("http://127.0.0.1:{}", st.port.load(std::sync::atomic::Ordering::Relaxed)),
            "platform": std::env::consts::OS,
            "name": computer_name(),
            "id": st.instance_id(),
        },
        "current": m.current_id(),
        "list": list,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct MachineBody {
    /// 要连/要配对的那台（面板上那个实例 id）
    #[serde(default)]
    id: String,
    /// 配对码（`/api/machines/pair`）
    #[serde(default)]
    code: String,
    /// 直接给地址（只给电脑用，见 `machines_pair`）
    #[serde(default)]
    host: String,
    #[serde(default)]
    port: u16,
}

/// `POST /api/machines/connect {id}` → `{url}`：起隧道，把**页面**要导航到的地址给它。
///
/// 地址里只有一把隧道钥匙，**没有那台电脑的凭据** —— 页面上任何一个 JS（包括查看器里
/// AI 写的网页）拿着它也只能用界面，拿不到一台电脑的 shell（见 `tunnel.rs`）。
async fn machines_connect(State(st): State<Arc<AppState>>, Json(b): Json<MachineBody>) -> Response {
    let Some(m) = st.machines() else {
        return no_store();
    };
    if b.id.trim().is_empty() {
        return bad_request("没给要连哪台（id）");
    }
    match m.clone().connect(b.id.trim()).await {
        Ok(c) => Json(json!({ "url": c.page_url(), "id": c.id, "name": c.name })).into_response(),
        Err(e) => bad_request(&format!("{e:#}")),
    }
}

/// `POST /api/machines/pair {id, code}` → 和找到的那台配对（输它显示的 6 位码）。
///
/// **手机上只认 id**：页面只能点「本机在局域网里真的找到过」的那几台。`AgentHost` 每个
/// frame 都看得见（包括查看器里 AI 写的网页），能传任意地址的话，一句 JS 就能把这个
/// 界面带到别人的服务器上（见 docs/11）。电脑上没这层黍（页面就是本机核心自己发的），
/// 所以多一条「直接给 host/port」的路 —— avahi 没跑、广播工具不在时还能用。
async fn machines_pair(State(st): State<Arc<AppState>>, Json(b): Json<MachineBody>) -> Response {
    let Some(m) = st.machines() else {
        return no_store();
    };
    let code = b.code.trim();
    if code.is_empty() {
        return bad_request("配对码是空的");
    }
    let id = b.id.trim();
    let (host, port) = if !id.is_empty() {
        // 地址从**本机自己看到过的**那份里取 —— 页面传进来的字符串不直接用
        match m
            .panel()
            .into_iter()
            .find(|p| p.id == id && !p.host.is_empty())
        {
            Some(p) => (p.host, p.port),
            None => {
                return bad_request("局域网里现在看不到这台（等它出现在列表里再配对）");
            }
        }
    } else if cfg!(target_os = "android") {
        return bad_request("手机上只能连广播找得到的电脑（没有手动输入地址）");
    } else if !b.host.trim().is_empty() && b.port != 0 {
        (b.host.trim().to_string(), b.port)
    } else {
        return bad_request("没给要配对哪台（id，或者 host + port）");
    };
    match m.pair(&host, port, code).await {
        Ok(m) => {
            st.log("info", format!("配对成功：{}", m.name));
            Json(json!({ "id": m.id, "name": m.name })).into_response()
        }
        Err(e) => bad_request(&format!("配对失败：{e:#}")),
    }
}

/// `POST /api/machines/local` → `{url}`：回到本机自己的核心。
///
/// 手机上壳会自己处理这一步（`AgentHost.useLocal`，它还要重新种 cookie）；
/// 这个接口是给浏览器那条路的 —— 电脑上从一开始就在 `127.0.0.1:<端口>`，
/// 回去就是导航回那个地址。
async fn machines_local(State(st): State<Arc<AppState>>) -> Response {
    if let Some(m) = st.machines() {
        m.disconnect();
    }
    Json(json!({
        "url": format!("http://127.0.0.1:{}", st.port.load(std::sync::atomic::Ordering::Relaxed)),
    }))
    .into_response()
}

/// `POST /api/machines/forget {id}` → 忘掉一台（它不认我们的凭据了）。
async fn machines_forget(State(st): State<Arc<AppState>>, Json(b): Json<MachineBody>) -> Response {
    let Some(m) = st.machines() else {
        return no_store();
    };
    if b.id.trim().is_empty() {
        return bad_request("没给要忘掉哪台（id）");
    }
    m.forget(b.id.trim());
    st.log("info", "忘掉了一台电脑（下次要重新配对）");
    Json(json!({ "ok": true })).into_response()
}

/// 这台机器没有会话库：机器记账（配对、证书指纹）没地方放。
fn no_store() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "这台机器没有会话库，机器记账没地方放（桌面：AGENT_DATA_DIR / 宿主模式的数据目录）",
    )
        .into_response()
}

fn bad_request(why: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": why }))).into_response()
}

#[derive(Deserialize)]
struct AlpineQuery {
    /// 顺手问一次官方最新版（要联网；不给就只回缓存的）
    #[serde(default)]
    check: Option<u8>,
}

/// `GET /api/alpine` → 沙箱的 Alpine 版本、最新版、升级任务的进度（设置页用）。
///
/// `available: false` = 没有可升级的 Alpine（宿主模式 / 沙箱还没起来）。
async fn alpine_status(State(st): State<Arc<AppState>>, Query(q): Query<AlpineQuery>) -> Response {
    let job = st.alpine.job();
    let Some(rootfs) = st
        .linux()
        .and_then(|sb| sb.proot_config().map(|c| c.rootfs.clone()))
    else {
        return Json(json!({ "available": false, "job": job })).into_response();
    };
    let current = crate::alpine::installed_version(&rootfs);
    let arch = crate::alpine::arch(&rootfs);
    let mut check_error = None;
    let latest = if q.check == Some(1) {
        match crate::alpine::fetch_latest(&st.alpine, &arch).await {
            Ok(v) => Some(v),
            Err(e) => {
                check_error = Some(format!("{e:#}"));
                st.alpine.cached_latest()
            }
        }
    } else {
        st.alpine.cached_latest()
    };
    let upgradable = match (&latest, &current) {
        (Some(l), Some(c)) => crate::alpine::is_newer(l, c),
        _ => false,
    };
    Json(json!({
        "available": true,
        "current": current,
        "arch": arch,
        "latest": latest,
        "upgradable": upgradable,
        "checkError": check_error,
        "job": job,
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
struct AlpineUpgradeBody {
    /// 目标版本（如 `3.24.2`）；不给就升到官方最新的稳定版
    #[serde(default)]
    to: Option<String>,
}

/// `POST /api/alpine/upgrade` → 在后台开始升级（立刻返回；进度看 `GET /api/alpine`）。
async fn alpine_upgrade(
    State(st): State<Arc<AppState>>,
    body: Option<Json<AlpineUpgradeBody>>,
) -> Response {
    let Some(sb) = st.linux() else {
        return (StatusCode::CONFLICT, "沙箱还没起来").into_response();
    };
    let (rootfs, from) = match crate::alpine::precheck(&st, &sb) {
        Ok(v) => v,
        Err(e) => return (StatusCode::CONFLICT, e).into_response(),
    };
    let to = match body.and_then(|b| b.0.to) {
        Some(v) => v,
        None => {
            match crate::alpine::fetch_latest(&st.alpine, &crate::alpine::arch(&rootfs)).await {
                Ok(v) => v,
                Err(e) => {
                    return (StatusCode::BAD_GATEWAY, format!("查不到最新版本：{e:#}"))
                        .into_response()
                }
            }
        }
    };
    if !crate::alpine::is_newer(&to, &from) {
        return (StatusCode::CONFLICT, format!("已经是 {from}，不比 {to} 旧")).into_response();
    }
    if !st.alpine.try_begin(&from, &to) {
        return (StatusCode::CONFLICT, "已经在升级了").into_response();
    }
    tokio::spawn(crate::alpine::run(
        st.clone(),
        sb,
        rootfs,
        from.clone(),
        to.clone(),
    ));
    Json(json!({ "ok": true, "from": from, "to": to })).into_response()
}

/// `?format=text` 直接回排好版的文字（沙箱里的 `agent ps` 就走这条，
/// 跟 `/api/websearch` 一个路数），否则回 JSON。
#[derive(Deserialize)]
struct ProcsQuery {
    #[serde(default)]
    format: Option<String>,
    /// 调用方自己所在的进程组（`agent ps` 从 `/proc/self/stat` 取）——
    /// 用来把它自己那条命令从「正在跑」里去掉。
    #[serde(default, rename = "self")]
    self_pgid: Option<i32>,
}

async fn list_procs(State(st): State<Arc<AppState>>, Query(q): Query<ProcsQuery>) -> Response {
    let Some(sb) = st.linux() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Linux 沙箱未初始化").into_response();
    };
    let report = sb.procs(q.self_pgid);
    if q.format.as_deref() == Some("text") {
        // CLI 直接 `cat` 这段：没有结尾换行的话，下一条命令的输出会接在同一行上
        let mut text = report.to_text();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        return (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            text,
        )
            .into_response();
    }
    Json(json!({
        "ok": true,
        "visible": report.visible,
        "output": report.to_text(),
        "leftovers": report.leftovers,
        "running": report.running,
    }))
    .into_response()
}

/// `POST /api/browser` → 跑一个浏览器动作（`agent browser` 走它）。
///
/// body 是动作（`{action, url, selector, …}`）。回 `text/plain`：就是给模型看的那段
/// 文字（见 `browser_text`），出错时以 `错误：` 开头（CLI 据此以非 0 退出）——
/// 和工具结果、`agent search`、`agent mcp` 同一个写法。两个后端（手机的 WebView、
/// 电脑的 CDP）回的都是 `{ok, text, url?, title?, …}`，`text` 本来就是给人看的，
/// 原样透出 JSON 的话模型看到的是一串 `\n` 转义。
///
/// 为什么要有这个入口：
///
/// 1. 沙箱里的 `agent browser` 走它 —— 于是 bash 也能脚本化地驱动浏览器
///    （批量抓、循环翻页），截图落在 /root/.browser 里能直接用别的工具处理。
/// 2. 走到一次工具调用要花一整轮 LLM（钱 + 时间），调试浏览器时不现实。
///    有个能直接打的入口，改一行 JS 就能立刻验。
async fn browser_action(State(st): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let core = Core { state: st.clone() };
    let text = match core.browser(&body.to_string()).await {
        Ok(v) => browser_text(&v),
        Err(e) => format!("错误：{e:#}"),
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

/// 浏览器回包 → 纯文本。
///
/// 正文就是 `text`；`ok: false` 写成 `错误：…`。页面信息（`url` / `title`）只在正文
/// 里还没提到时补一行 —— 模型靠它记「现在在哪页」，但 `navigate` 那种正文里已经写了。
/// 多标签时（手机后端每个回包都带 `tab` / `tabs`）再补一行在哪只标签上。
fn browser_text(v: &Value) -> String {
    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_end();
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        return format!(
            "错误：{}",
            if text.is_empty() {
                "浏览器动作失败"
            } else {
                text
            }
        );
    }
    let mut out = if text.is_empty() {
        "(无输出)".to_string()
    } else {
        text.to_string()
    };
    let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("");
    if !url.is_empty() && !out.contains(url) {
        let title = v.get("title").and_then(|t| t.as_str()).unwrap_or("");
        if title.is_empty() {
            out.push_str(&format!("\n[页面：{url}]"));
        } else {
            out.push_str(&format!("\n[页面：{title} — {url}]"));
        }
    }
    let tabs = v.get("tabs").and_then(|t| t.as_u64()).unwrap_or(0);
    if tabs > 1 {
        let tab = v.get("tab").and_then(|t| t.as_u64()).unwrap_or(0);
        out.push_str(&format!("\n[标签 #{tab}，共 {tabs} 只]"));
    }
    out
}

/// `POST /api/device` → 跑一条**手机系统能力**命令（`{cmd, argv}`）。
///
/// 沙箱里的 `agent <设备命令>` 走这里（`agent clipboard get`、`agent sms list`…）。
/// 为什么单独开一条而不是挤进 `/api/command`：那条是**无障碍指令**（点按/滑动/按键，
/// 需要服务连着），而这些只需要普通权限；两者的可用条件不同（见 `phone` 模块）。
///
/// 回 `text/plain`：壳回的 `{ok, data}` 里的 `data` 用 `plain::render` 排成文字；
/// 失败写成 `错误：…（错误码）`，CLI 据此以非 0 退出。错误码是给模型分辨用的：
/// `permission_denied`（用户拒了，别重试）、`bad_args`（参数错了）……
///
/// **恒用 200 回**：沙箱里的 `wget` 对 5xx 会**重试**，于是一次“没有这台设备”会被
/// 放成三次请求；成不成看正文开头。
async fn device_command(State(st): State<Arc<AppState>>, Json(body): Json<DeviceBody>) -> Response {
    let core = Core { state: st.clone() };
    let text = match core.phone(&body.cmd, &body.argv).await {
        Ok(v) => device_text(&v),
        Err(e) => {
            let code = if !crate::phone::is_command(&body.cmd) {
                "unknown_command"
            } else if st.phone().is_none() {
                "unavailable"
            } else {
                "failed"
            };
            format!("错误：{e:#}（{code}）")
        }
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

/// 壳的回包（`phone::normalize_reply` 收敛过的 `{ok, data}` / `{ok:false, error}`）→ 文字。
fn device_text(v: &Value) -> String {
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        let e = &v["error"];
        let msg = e
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| e.as_str())
            .unwrap_or("失败了（壳没说原因）");
        return match e.get("code").and_then(|c| c.as_str()) {
            Some(code) => format!("错误：{msg}（{code}）"),
            None => format!("错误：{msg}"),
        };
    }
    match v.get("data") {
        None | Some(Value::Null) => "完成".to_string(),
        Some(d) => crate::plain::render(d),
    }
}

#[derive(Deserialize)]
struct DeviceBody {
    cmd: String,
    #[serde(default)]
    argv: Vec<String>,
}

/// `POST /api/files` → 看一眼工作目录（`{"path":"/workspace"}`，可省）。
///
/// 回的是**排好版的纯文本**（不是 JSON）：它就是给模型和人看的一屏，像 `agent ps`
/// 那样。格式在 `workspace::render` 里只有一份 —— CLI 那边不再拼一遍。
///
/// 为什么要有它：模型只看得见自己敲过的路径，看不见「这个目录整体乱了」。
async fn list_files(State(st): State<Arc<AppState>>, Json(body): Json<FilesBody>) -> Response {
    let Some(sb) = st.linux() else {
        return "错误：沙箱还没起来（首次启动要解压 rootfs，看设置页的进度）".into_response();
    };
    let guest = body
        .path
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "/workspace".to_string());
    match sb.resolve_path(&guest) {
        Ok(host) => match crate::workspace::render(&guest, &host) {
            Ok(text) => text.into_response(),
            Err(e) => format!("错误：{e:#}").into_response(),
        },
        Err(e) => format!("错误：{e:#}").into_response(),
    }
}

#[derive(Deserialize)]
struct FilesBody {
    #[serde(default)]
    path: Option<String>,
}

/// `POST /api/weather` → 天气（`{argv:["current","--lat","31.2","--lon","121.4"]}`）。
///
/// 沙箱里的 `agent weather` 走它。解析在 Rust 侧（`weather` 模块）：
/// 与设备命令不同，这个只是查网络，**手机和电脑上都能用**。
async fn weather_run(Json(body): Json<WeatherBody>) -> Response {
    // 回纯文本（`plain::render`），出错以「错误：」开头 —— 和别的 `agent` 命令一样
    let text = match crate::weather::run(&body.argv).await {
        Ok(v) => crate::plain::render(&v["data"]),
        Err(e) => format!("错误：{e:#}"),
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

/// `agent readable` 一次最多收多大的 HTML。正常文章页几百 KB，带内联脚本的大站
/// 也就几 MB；再大多半是喂错了东西（视频、压缩包），别全读进手机内存。
const READABLE_MAX: usize = 16 * 1024 * 1024;

/// `POST /api/readable` → 正文（请求体是 HTML 原文，回纯文本，出错以「错误：」开头）。
///
/// 用 `Body` 自己读而不是 `Bytes` 提取器：后者超过 axum 默认的 2MB 会回一句英文的
/// 413，CLI 认不出那是错误。
async fn readable_run(body: axum::body::Body) -> Response {
    let text = match axum::body::to_bytes(body, READABLE_MAX).await {
        Err(_) => format!(
            "错误：HTML 超过 {}，不像一个网页",
            crate::reader::fmt_size(READABLE_MAX)
        ),
        // 解析 + 打分是纯 CPU 活，大页面要几十毫秒：别占着 async 线程
        Ok(b) => {
            match tokio::task::spawn_blocking(move || crate::readable::extract_bytes(&b)).await {
                Ok(Ok(a)) => crate::readable::render(&a),
                Ok(Err(e)) => format!("错误：{e:#}"),
                Err(e) => format!("错误：{e}"),
            }
        }
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

#[derive(Deserialize)]
struct WeatherBody {
    #[serde(default)]
    argv: Vec<String>,
}

/// `POST /api/mcp` → `agent mcp …`（`{argv, workdir?}`）。
///
/// 回 `text/plain`：就是给模型看的那段文字，出错时以 `错误：` 开头（CLI 据此以非 0
/// 退出）。`workdir` 是命令的 cwd（`$AGENT_WORKDIR`）：工具回的图片落在它下面的
/// `mcp/`，一个会话的东西都在那个会话的目录里。宿主模式没有会话目录（cwd 是用户的
/// 项目，不往里写），落在临时目录。
async fn mcp_run(State(st): State<Arc<AppState>>, Json(body): Json<McpBody>) -> Response {
    let r = match body.argv.first().map(String::as_str) {
        // 接 / 删服务要动配置（库 + 内存），不在 `mcp::run` 里做
        Some("add") => mcp_add(&st, &body.argv[1..]).await,
        Some("remove") => mcp_remove(&st, &body.argv[1..]).await,
        _ => {
            let out = mcp_out_dir(&st, body.workdir.as_deref());
            crate::mcp::run(&st.mcp, &body.argv, out).await
        }
    };
    let text = match r {
        Ok(t) => t,
        Err(e) => format!("错误：{e:#}"),
    };
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
}

/// `agent mcp add`：**先试连，连得上才存** —— 模型接错了地址 / token 的话当场知道，
/// 不会留下一个永远连不上的配置（设置页那条路不一样：人填的照样存，见 `save_mcp_server`）。
async fn mcp_add(st: &AppState, args: &[String]) -> anyhow::Result<String> {
    let srv = crate::mcp::parse_add(args)?;
    let existed = st.mcp.servers().iter().any(|s| s.name == srv.name);
    // 用户停用的：agent 不能靠「重新接一次」把它打开（开关是用户的）
    anyhow::ensure!(
        !existed || st.mcp.is_enabled(&srv.name),
        "MCP 服务 {} 已经接过、被用户停用了（设置 → MCP 服务）。要用得让用户打开，别重新接",
        srv.name
    );
    // 同名的旧会话不能拿来试新配置
    st.mcp.forget_session(&srv.name).await;
    let tools = st
        .mcp
        .tools(&srv)
        .await
        .map_err(|e| anyhow::anyhow!("{e:#}（没有保存）"))?;
    let name = srv.name.clone();
    st.put_mcp_server(srv).await?;
    Ok(format!(
        "已{} {name}（{} 个工具）。agent mcp list {name} 看有哪些。",
        if existed { "更新" } else { "接上" },
        tools.len()
    ))
}

async fn mcp_remove(st: &AppState, args: &[String]) -> anyhow::Result<String> {
    let [name] = args else {
        anyhow::bail!("用法：agent mcp remove <名字>");
    };
    anyhow::ensure!(
        st.remove_mcp_server(name).await?,
        "没有叫 {name} 的 MCP 服务"
    );
    Ok(format!("已删掉 {name}。"))
}

fn mcp_out_dir(st: &AppState, workdir: Option<&str>) -> Option<crate::mcp::OutDir> {
    let sb = st.linux()?;
    if sb.is_host() {
        let (host, guest) = sb.spill_dir();
        return Some(crate::mcp::OutDir {
            host,
            guest,
            prefix: "agent-out-mcp-",
        });
    }
    let wd = workdir
        .filter(|w| !w.trim().is_empty())
        .unwrap_or(crate::linux::WORKSPACE);
    let guest = format!("{}/mcp", wd.trim_end_matches('/'));
    let host = sb.resolve_new_path(&guest).ok()?;
    Some(crate::mcp::OutDir {
        host,
        guest,
        prefix: "",
    })
}

#[derive(Deserialize)]
struct McpBody {
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
}

/// `GET /api/mcp/servers` → 已接的服务（设置页用）。请求头**只给名字不给值**：
/// 值多半是 token，不往界面上回。
async fn mcp_servers(State(st): State<Arc<AppState>>) -> Json<Value> {
    let list: Vec<Value> = st
        .mcp
        .servers()
        .into_iter()
        .map(|s| {
            json!({
                "name": s.name,
                "url": s.url,
                "headers": s.headers.keys().collect::<Vec<_>>(),
                "enabled": st.mcp.is_enabled(&s.name),
            })
        })
        .collect();
    Json(json!({ "servers": list }))
}

#[derive(Deserialize)]
struct SaveMcpBody {
    name: String,
    url: String,
    /// `None` = 沿用这个服务原来的请求头（改 URL 时不用再填一遍 token）
    #[serde(default)]
    headers: Option<std::collections::BTreeMap<String, String>>,
}

/// `POST /api/mcp/servers` → 加 / 改一个服务，存完试着列一下工具（连得上就回工具数，
/// 连不上也照样存着，回原因 —— 可能只是这会儿没网）。
async fn save_mcp_server(
    State(st): State<Arc<AppState>>,
    Json(body): Json<SaveMcpBody>,
) -> Response {
    let old = st.mcp.servers().into_iter().find(|s| s.name == body.name);
    let srv = crate::mcp::Server {
        name: body.name.trim().to_string(),
        url: body.url.trim().to_string(),
        headers: body
            .headers
            .or_else(|| old.map(|o| o.headers))
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| !k.trim().is_empty())
            .collect(),
    };
    if let Err(e) = srv.validate() {
        return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response();
    }
    if let Err(e) = st.put_mcp_server(srv.clone()).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("存不进去：{e:#}"),
        )
            .into_response();
    }
    match st.mcp.tools(&srv).await {
        Ok(tools) => Json(json!({ "ok": true, "tools": tools.len() })).into_response(),
        Err(e) => Json(json!({ "ok": false, "error": format!("{e:#}") })).into_response(),
    }
}

#[derive(Deserialize)]
struct DeleteMcpBody {
    name: String,
}

async fn delete_mcp_server(
    State(st): State<Arc<AppState>>,
    Json(body): Json<DeleteMcpBody>,
) -> Response {
    match st.remove_mcp_server(&body.name).await {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("删不掉：{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct EnabledBody {
    /// MCP：服务名；skill：目录名
    name: String,
    enabled: bool,
}

/// `POST /api/mcp/servers/enabled` → 启用 / 停用一个 MCP 服务（`{name, enabled}`）。
async fn set_mcp_enabled(State(st): State<Arc<AppState>>, Json(b): Json<EnabledBody>) -> Response {
    match st.set_mcp_enabled(&b.name, b.enabled).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// `POST /api/skills/enabled` → 启用 / 停用一个 skill（`{name: 目录名, enabled}`）。
async fn set_skill_enabled(
    State(st): State<Arc<AppState>>,
    Json(b): Json<EnabledBody>,
) -> Response {
    match st.set_skill_enabled(&b.name, b.enabled) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/usage` → 用量汇总（设置页「用量」）：最近 14 天按天、全部历史按模型。
///
/// 只报 token 和缓存命中，**不估钱**：models.dev 有目录价，但包月套餐（OpenCode Go 这类）
/// 按目录价算出来的数字是错的，不如不给。
async fn usage_summary(State(st): State<Arc<AppState>>) -> Response {
    const DAYS: u32 = 14;
    let Some(db) = st.db() else {
        return Json(json!({ "byDay": [], "byModel": [] })).into_response();
    };
    let r = tokio::task::spawn_blocking(move || db.usage_summary(DAYS)).await;
    let s = match r {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let rows = |v: Vec<(String, i64, i64, i64, i64)>| -> Vec<Value> {
        v.into_iter()
            .map(|(key, calls, input, cache_read, output)| {
                json!({
                    "key": key,
                    "calls": calls,
                    "input": input,
                    "cacheRead": cache_read,
                    "output": output,
                    "hitRatio": if input > 0 { Some(cache_read as f64 / input as f64) } else { None },
                })
            })
            .collect()
    };
    Json(json!({ "byDay": rows(s.by_day), "byModel": rows(s.by_model) })).into_response()
}

/// `GET /api/skills` → 装了哪些 skill（带启用状态），以及放在哪（设置页用）。
async fn list_skills(State(st): State<Arc<AppState>>) -> Json<Value> {
    let dir = st.linux().map(|sb| sb.skills_dir().1);
    Json(json!({ "dir": dir, "skills": st.skills() }))
}

/// `POST /api/websearch` → 联网搜索（`{query, num_results?, format?}`）。
///
/// **这是联网搜索的唯一入口**：沙箱里的 `agent search` 走它（能在一行 shell 里
/// 说清的能力不必再开一个工具）。
///
/// 两种回法：
///
/// - 默认 JSON：`{ok, provider, count, output}`。`output` 是给模型看的那段文字
///   （见 `search::format_hits`）—— 格式化发生在这里，不在 CLI 里，因为两处各写
///   一份就会漂。
/// - `format: "text"`：直接回 `text/plain`，就是那段文字 + 一行抬头
///   （`3 条 · exa-mcp`）。**`agent search` 用这个** —— 打 JSON 的话模型和界面上
///   看到的是一串带 `\n` 转义的字符串（“网址后面怎么都有个 \n”），
///   而且 CLI 还得依赖 jq 才能把它拆开。
///
/// 和 `/api/browser` 一样的理由存在：调试、以及让沙箱里的 `agent search` 能直接用。
async fn web_search(State(st): State<Arc<AppState>>, Json(body): Json<Value>) -> Response {
    let core = Core { state: st.clone() };
    let query = body.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let as_text = body.get("format").and_then(|v| v.as_str()) == Some("text");
    let text = |s: String| {
        (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            s,
        )
            .into_response()
    };
    if query.trim().is_empty() {
        return if as_text {
            text("缺少 query（用法：agent search <检索意图> [条数]）".into())
        } else {
            (StatusCode::BAD_REQUEST, "缺少 query").into_response()
        };
    }
    let num = body
        .get("num_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(5) as usize;
    match core.search_web(query, num).await {
        Ok(found) => {
            if as_text {
                return text(format!(
                    "{} 条 · {}\n\n{}",
                    found.hits.len(),
                    found.provider,
                    crate::search::format_hits(&found)
                ));
            }
            Json(json!({
                "ok": true,
                "provider": found.provider,
                "count": found.hits.len(),
                "output": crate::search::format_hits(&found),
            }))
            .into_response()
        }
        // 429 之类的“能自己恢复”的失败：200 + error 字段，调用方（模型/CLI）
        // 看得到原因和解法，而不是一句 500
        Err(e) => {
            if as_text {
                return text(format!("搜索失败：{:#}", e));
            }
            Json(json!({ "ok": false, "error": format!("{e:#}") })).into_response()
        }
    }
}

#[derive(Deserialize)]
struct SearchConfigBody {
    #[serde(default)]
    exa_api_key: String,
}

/// `GET /api/websearch/config` → 配没配 Exa key（设置页用）。key 本身不回，只回末尾几位；
/// `fromEnv` = 环境变量 `EXA_API_KEY` 给的（桌面开发），设置页里改不了它。
async fn search_config(State(st): State<Arc<AppState>>) -> Json<Value> {
    let env = std::env::var("EXA_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let key = crate::search::read_key(&st);
    Json(json!({
        "configured": key.is_some(),
        "keyHint": key.as_deref().map(crate::providers::key_hint),
        "fromEnv": env.is_some(),
    }))
}

/// `POST /api/websearch/config` → 存 Exa 的 key（库里 `meta` 的 `exa_api_key`）。
///
/// 空字符串就是清掉。不配也能搜（走 Exa 免 key 的公开端点，按 IP 限额）；免费额度
/// 用完了才需要它（见 docs/09）。设置页「联网搜索」那一栏就是填它的。
async fn set_search_config(
    State(st): State<Arc<AppState>>,
    Json(body): Json<SearchConfigBody>,
) -> Response {
    let Some(db) = st.db.read().ok().and_then(|g| g.clone()) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "会话库没打开").into_response();
    };
    let key = body.exa_api_key.trim();
    let res = if key.is_empty() {
        db.delete_meta("exa_api_key")
    } else {
        db.set_meta("exa_api_key", key)
    };
    match res {
        Ok(()) => Json(json!({
            "ok": true,
            "configured": !key.is_empty(),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// `POST /api/edit` → 直接在沙箱里改文件（对应 `edit` 工具）。
async fn edit_file(State(st): State<Arc<AppState>>, Json(body): Json<EditBody>) -> Response {
    let core = Core { state: st.clone() };
    let edits: Vec<crate::editor::Edit> = body
        .edits
        .iter()
        .map(|e| crate::editor::Edit::new(e.old_text.clone(), e.new_text.clone()))
        .collect();

    match core.edit_file(&body.path, &edits).await {
        Ok(out) => Json(json!({
            "ok": true,
            "replaced": edits.len(),
            "firstChangedLine": out.first_changed_line,
            "diff": out.patch,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct LogBody {
    #[serde(default = "default_level")]
    level: String,
    msg: String,
}

fn default_level() -> String {
    "info".into()
}

async fn device_log(
    State(st): State<Arc<AppState>>,
    Json(body): Json<LogBody>,
) -> impl IntoResponse {
    st.log(&body.level, body.msg);
    StatusCode::NO_CONTENT
}

// ---------------------------------------------------------------------------
// SSE：agent 流
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ChatBody {
    #[serde(default)]
    text: String,
    /// 强制跑脚本化演示流（不花 token，用来验证协议链路）
    #[serde(default)]
    demo: bool,
    /// 写进哪个会话。不给 = 当前活动会话（前端的行为）；
    /// 调试端会指定一条专用会话，免得把测试消息混进用户正在聊的那条。
    #[serde(default)]
    session: Option<String>,
    /// 已经在 `/api/attachment` 上传好、随这条消息发出去的附件 id。
    /// 服务端从 `pending_attachments` 取出完整描述后跟着用户消息进历史。
    #[serde(default)]
    attachments: Vec<String>,
    /// 这条消息在界面上的 id。
    ///
    /// 排队时用得上：界面拿它把「排队中」那个气泡和 `data-queued` 对上，
    /// `/api/sessions/unqueue` 也拿它撤。不给就现生成一个（脚本 / agentctl）。
    #[serde(default)]
    id: Option<String>,
}

/// `POST /api/chat` → AI SDK UI Message Stream。
///
/// 配了 provider 就跑真实 agent loop（`agent.rs`），否则回退到脚本演示流。
/// 两种情况下**输出格式完全一致**，所以前端不需要知道区别。
async fn chat(State(st): State<Arc<AppState>>, Json(body): Json<ChatBody>) -> Response {
    // 运行时配置优先（安卓），回退环境变量（桌面）
    let provider = if body.demo {
        None
    } else {
        st.llm().or_else(Provider::from_env)
    };

    // 写进哪个会话：指定的 > 当前活动的（脚本 / agentctl 不指定）。
    let sid = {
        let mut store = st.chat.write().await;
        match &body.session {
            Some(id) if store.get(id).is_some() => id.clone(),
            Some(id) => {
                return (StatusCode::NOT_FOUND, format!("没有这个会话: {id}")).into_response()
            }
            None => store.active_mut().id.clone(),
        }
    };

    // 取出这条消息带的附件。刚从上传接口存进来的，按 id 取走（取走就从
    // pending 里消失，同一份附件不会被两条消息重复引用）。
    //
    // **排在回合检查之前**：排队的消息也要把附件拿在手里，不然「排队期间
    // 上传的那张图」到交给模型时已经不在 pending 里了。
    let mut attachments: Vec<crate::agent::Attachment> = Vec::new();
    if !body.attachments.is_empty() {
        let mut pending = st
            .pending_attachments
            .lock()
            .expect("pending_attachments poisoned");
        for id in &body.attachments {
            match pending.remove(id) {
                Some((_, a)) => attachments.push(a),
                None => {
                    drop(pending);
                    return (
                        StatusCode::BAD_REQUEST,
                        format!("附件已失效（{id}），请重新添加"),
                    )
                        .into_response();
                }
            }
        }
    }

    // 一个会话同时只跑一个回合（两个回合往同一段历史里交错追加就乱了）；
    // 不同会话可以各跑各的。
    //
    // **正跑着的时候不再回 409**：这条消息排进**服务端的**队列，正在跑的那个回合
    // 会在步边界把它取走交给模型（`agent::deliver_queued`）—— 模型一空下来就拿到，
    // 不用等整个回合结束。
    //
    // 队列不能放在前端：那个队列活在 WebView 里，App 切到后台、WebView 被冻住
    // 或回收，那条消息就永远发不出去了 —— 而「等一会儿再发同一个请求」本来就是
    // 核心该保证的事。
    //
    // 队列里**已经有**等着的（上个回合出错后留下的）也排进去：不然这条会
    // 插到它们前面先跑。排进去之后由这里开回合（`continue_with_queued`），按原来的
    // 顺序走。
    let turn = match &provider {
        Some(p) => match if st.has_queued(&sid) {
            None
        } else {
            st.begin_turn(&sid)
        } {
            Some(t) => Some(t),
            None => {
                let id = body.id.clone().unwrap_or_else(new_id);
                st.enqueue(
                    &sid,
                    crate::state::QueuedMsg {
                        id: id.clone(),
                        text: body.text,
                        attachments,
                    },
                );
                st.log("info", format!("这个会话在跑，消息先排队（{}）", sid));
                // 回合可能就在上面 `begin_turn` 失败之后、`enqueue` 之前收了尾 ——
                // 它收尾时看队列还是空的，这条就没人取了。所以排完再试着开一次：
                // 回合还在跑的话这是空操作（`begin_turn` 拿不到），由它在步边界取。
                crate::agent::continue_with_queued(&Core { state: st.clone() }, p, &sid);
                return queued_response(&id);
            }
        },
        None => None,
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let core = Core { state: st.clone() };

    match provider.zip(turn) {
        Some((p, turn)) => {
            st.log("info", format!("chat → {} / {}", p.name, p.model));
            tokio::spawn(async move {
                let txc = tx.clone();
                // `run_turn` 的 TurnGuard 负责 end_turn
                let r = crate::agent::run_turn(&core, &p, &body.text, attachments, &sid, turn, txc)
                    .await;
                if let Err(e) = r {
                    let _ = tx.send(sse::error(&format!("{e:#}")));
                }
                // tx 在这里 drop → 转发任务会补上 [DONE]
            });
        }
        None => {
            st.log("debug", format!("chat (demo): {}", body.text));
            tokio::spawn(async move {
                for chunk in crate::demo::scripted(&body.text) {
                    if tx.send(chunk).is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(35)).await;
                }
            });
        }
    }

    sse_response(rx)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmBody {
    #[serde(default = "default_name")]
    name: String,
    base_url: String,
    model: String,
    api_key: String,
    /// `responses` / `chat` / `auto`（不传 = auto：优先 Responses）
    #[serde(default)]
    api_style: Option<crate::llm::ApiStyle>,
    /// 思考强度。不传 = 不动；空串 = 清掉（回到默认档）。
    #[serde(default)]
    reasoning_effort: Option<String>,
}

#[derive(serde::Deserialize)]
struct ModelsQuery {
    /// `?refresh=1` 强制重拉 models.dev 目录
    #[serde(default)]
    refresh: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelBody {
    model: String,
    /// 哪家的模型（服务商 id）。不给 = 当前这家。给了另一家就切过去
    /// （用设置页存下的那家的 Key）。
    #[serde(default)]
    provider: Option<String>,
    /// 思考强度。不传 = 不动；空串 = 清掉（回到默认档）。
    #[serde(default)]
    reasoning_effort: Option<String>,
}

fn default_name() -> String {
    "custom".into()
}

/// 解 `%XX`（前端用 `encodeURIComponent` 编的文件名）。
///
/// 不引 `percent-encoding` 只为这一个地方：HTTP 头值只能是 ISO-8859-1，
/// 中文文件名必须编码，而 `+`/空格在这里不是查询串、不特殊处理。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 文件链接的前缀（模型在回复里给用户的链接就是这么写的）。
const SCHEME_PREFIX: &str = "preview:";

/// agent 的浏览器要打开的地址：沙箱里的文件 → `sandbox.localhost` 上的 URL。
///
/// 认这几种写法（模型最可能写的）：`preview:/workspace/a.html`（和回复里给
/// 用户的链接同一个写法）、`preview:a.html`（相对 /workspace）、
/// `file:///workspace/a.html`、直接写绝对路径 `/workspace/a.html`。
/// 别的（网址）返回 `None`，原样交给浏览器。
pub fn sandbox_page_url(raw: &str, port: u16, key: &str) -> Option<String> {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    let rest = if lower.starts_with(SCHEME_PREFIX) {
        let after = &raw[SCHEME_PREFIX.len()..];
        let r = after.trim_start_matches('/');
        if after.starts_with('/') {
            format!("/{r}")
        } else {
            format!("/workspace/{r}")
        }
    } else if lower.starts_with("file://") {
        format!("/{}", raw["file://".len()..].trim_start_matches('/'))
    } else if raw.starts_with('/') && !raw.starts_with("//") {
        raw.to_string()
    } else {
        return None;
    };
    let (path, tail) = match rest.find(['?', '#']) {
        Some(i) => rest.split_at(i),
        None => (rest.as_str(), ""),
    };
    // 路径里可能有中文 / 空格：逐字节编码（已经编码过的 % 不再编一次）
    let mut enc = String::with_capacity(path.len());
    for b in percent_decode(path).bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            enc.push(b as char);
        } else {
            enc.push_str(&format!("%{b:02X}"));
        }
    }
    Some(format!("{}/sandbox/{key}{enc}{tail}", sandbox_origin(port)))
}

/// 反过来：结果里出现的 `http://sandbox.localhost:<端口>/sandbox/<钥匙>/…`
/// 改写回 `preview:/…`（解码过的路径）。
///
/// 端口每次启动可能不同、钥匙也不该进历史 —— 模型记住的应该是 `preview:`
/// 这个稳定的写法，下次照样能打开，也能原样写进回复给用户点。
pub fn sandbox_url_to_guest(text: &str, port: u16, key: &str) -> String {
    let prefix = format!("{}/sandbox/{key}", sandbox_origin(port));
    if !text.contains(&prefix) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(&prefix) {
        out.push_str(&rest[..i]);
        let after = &rest[i + prefix.len()..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')'))
            .unwrap_or(after.len());
        let path = &after[..end];
        let path = if path.is_empty() || path == "/" {
            "/workspace/".to_string()
        } else {
            percent_decode(path)
        };
        out.push_str("preview:");
        out.push_str(&path);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `POST /api/llm` → 运行时配置 provider。
///
/// 安卓上没法用环境变量，用户也没别的地方填 key，所以需要这个接口。
/// 会把配置按列写进库里（`llm` / `providers` 表）。
async fn set_llm(State(st): State<Arc<AppState>>, Json(body): Json<LlmBody>) -> Response {
    if body.api_key.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "apiKey 不能为空").into_response();
    }
    if body.base_url.trim().is_empty() || body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "baseUrl 和 model 都不能为空").into_response();
    }

    let mut p = Provider::compatible(
        body.name,
        body.base_url.trim_end_matches('/'),
        body.model,
        body.api_key,
    );
    if let Some(style) = body.api_style {
        p.api_style = style;
    }
    if let Some(e) = body.reasoning_effort {
        p.reasoning_effort = crate::llm::normalize_effort(&e);
    }
    st.set_llm(p.clone());
    // 设置页的「模型提供商」里也要看得到这一家
    st.adopt_provider(&p);
    // 自动判定这个模型能不能吃图片 / PDF（查 models.dev），结果落盘。
    // 不劳烦用户配 —— 这是模型自己的属性。
    let vision = st.supports_images_for(&p).await;
    let pdf = st.supports_files_for(&p).await;
    // 换了 provider / model，缓存前缀的语义就变了：给所有会话换一个
    // provider session id，否则 provider 侧会把新前缀路由到旧模型的缓存上。
    st.rotate_provider_sessions().await;

    Json(json!({ "ok": true, "vision": vision, "pdf": pdf })).into_response()
}

/// `POST /api/stop` → 停当前回合。
///
/// agent loop 没有轮数上限，前端那个「停止」按钮必须真的把服务端循环停下来，
/// 而不只是断开 SSE（循环看不到断连，会继续跑、继续烧 token）。
async fn stop_turn(State(st): State<Arc<AppState>>, body: Option<Json<StopBody>>) -> Response {
    // 不指定会话 = 当前活动会话（调试脚本 / agentctl）
    let sid = match body.and_then(|Json(b)| b.session) {
        Some(id) => id,
        None => match st.chat.read().await.active_ref() {
            Some(s) => s.id.clone(),
            None => return Json(json!({ "ok": true, "running": false })).into_response(),
        },
    };
    let running = st.request_abort(&sid);
    if running {
        st.log("info", "用户请求停止回合");
    }
    Json(json!({ "ok": true, "running": running })).into_response()
}

#[derive(Deserialize)]
struct StopBody {
    #[serde(default)]
    session: Option<String>,
}

/// `GET /api/models` → 所有配好了的服务商的模型清单，按家分组。
///
/// 「选择模型」面板就靠它：几家的 `/models` 并行拉（各自有缓存，见
/// `AppState::models_of`），拉不到的那家带着错误回来，不拖累别家。
///
/// 注意：面板前端**自己还有一层 10 分钟的缓存**（`web/src/lib/models.svelte.ts`），
/// 正常打开面板根本不会发这个请求 —— 后端这层省的是「打扰上游」，
/// 前端那层省的是「面板第一帧没数据 → 先渲染载入中再长高」的布局跳动。
/// `models` 还是当前这家的清单（agentctl 用）。
async fn get_models(State(st): State<Arc<AppState>>, Query(q): Query<ModelsQuery>) -> Response {
    let current = st.llm().or_else(Provider::from_env);
    let current_id = current.as_ref().map(crate::providers::identify);
    let store = st.providers.read().map(|g| g.clone()).unwrap_or_default();

    let mut jobs = tokio::task::JoinSet::new();
    for (id, creds) in store {
        let Some(p) = st.provider_for(&id, &creds, "") else {
            continue;
        };
        let st = st.clone();
        let refresh = q.refresh;
        jobs.spawn(async move {
            let r = st.models_of(&id, &p, refresh).await;
            (id, r)
        });
    }
    let mut got: Vec<(String, anyhow::Result<Vec<crate::models::ModelInfo>>)> =
        jobs.join_all().await;
    // 按设置页里的顺序排：内置的几家在前，「其他」最后
    let order = |id: &str| {
        crate::providers::PRESETS
            .iter()
            .position(|p| p.id == id)
            .unwrap_or(usize::MAX)
    };
    got.sort_by_key(|(id, _)| order(id));

    let mut current_models = serde_json::Value::Array(vec![]);
    let groups: Vec<serde_json::Value> = got
        .into_iter()
        .map(|(id, r)| {
            let name = crate::providers::display_name(&id);
            match r {
                Ok(models) => {
                    let v = json!(models);
                    if current_id == Some(id.as_str()) {
                        current_models = v.clone();
                    }
                    json!({ "id": id, "name": name, "models": v })
                }
                Err(e) => {
                    json!({ "id": id, "name": name, "models": [], "error": format!("{e:#}") })
                }
            }
        })
        .collect();

    Json(json!({
        "current": current.as_ref().map(|p| p.model.clone()),
        "provider": current_id,
        "name": current.as_ref().map(|p| p.name.clone()),
        "baseUrl": current.as_ref().map(|p| p.base_url.clone()),
        "vision": current.as_ref().map(|p| st.cached_supports_images(p)),
        "pdf": current.as_ref().map(|p| st.cached_supports_files(p)),
        "reasoningEffort": current.as_ref().and_then(|p| p.reasoning_effort.clone()),
        "groups": groups,
        "models": current_models,
    }))
    .into_response()
}

/// 换成这个 provider：存下来、判定能力、给会话换 provider session id。
async fn switch_provider(st: &Arc<AppState>, p: &Provider) -> (bool, bool) {
    st.set_llm(p.clone());
    // 自动判定这个模型能不能吃图片 / PDF（查 models.dev），结果落盘。
    let vision = st.supports_images_for(p).await;
    let pdf = st.supports_files_for(p).await;
    // 换了 provider / model，缓存前缀的语义就变了：给所有会话换一个
    // provider session id，否则 provider 侧会把新前缀路由到旧模型的缓存上。
    st.rotate_provider_sessions().await;
    (vision, pdf)
}

/// `GET /api/providers` → 设置页「模型提供商」那一栏：内置的几家 + 其他。
///
/// 只回「配没配」和 Key 的最后 4 位，不回 Key 本身。
async fn list_providers(State(st): State<Arc<AppState>>) -> Response {
    let cur = st.llm();
    let current = cur.as_ref().map(crate::providers::identify);
    let store = st.providers.read().map(|g| g.clone()).unwrap_or_default();
    let row = |id: &str, name: &str, base_url: String, key_url: Option<&str>| {
        let creds = store.get(id);
        let is_current = current == Some(id);
        // 协议：选的（没选就是预设的），以及「自动」这会儿实际落在哪套上 ——
        // 只有正在用的那家知道模型，「这个端点 + 模型不支持 Responses」是按模型记的
        let style = st
            .api_style_override(id)
            .unwrap_or_else(|| crate::providers::default_api_style(id));
        let transport = cur
            .as_ref()
            .filter(|_| is_current)
            .map(|p| match p.transport() {
                crate::llm::Transport::Responses => "responses",
                crate::llm::Transport::Chat => "chat",
            });
        json!({
            "id": id,
            "name": name,
            "baseUrl": base_url,
            "keyUrl": key_url,
            "configured": creds.is_some(),
            "keyHint": creds.map(|c| crate::providers::key_hint(&c.api_key)),
            "current": is_current,
            "custom": id == crate::providers::CUSTOM,
            "apiStyle": style.as_str(),
            "apiStyleDefault": crate::providers::default_api_style(id).as_str(),
            "transport": transport,
        })
    };
    let mut rows: Vec<serde_json::Value> = crate::providers::PRESETS
        .iter()
        .map(|p| row(p.id, p.name, p.base_url.to_string(), Some(p.key_url)))
        .collect();
    let custom_url = store
        .get(crate::providers::CUSTOM)
        .map(|c| c.base_url.clone())
        .unwrap_or_default();
    rows.push(row(
        crate::providers::CUSTOM,
        crate::providers::CUSTOM_NAME,
        custom_url,
        None,
    ));
    Json(json!({ "providers": rows })).into_response()
}

#[derive(Deserialize)]
struct ApiStyleBody {
    /// auto | responses | chat；和预设的一样就当「回到默认」（不单独存）
    style: String,
}

/// `POST /api/providers/{id}/api-style` → 选这家用哪套协议。
async fn set_api_style(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ApiStyleBody>,
) -> Response {
    if !crate::providers::is_known(&id) {
        return (StatusCode::NOT_FOUND, "没有这个服务商").into_response();
    }
    let style = match body.style.as_str() {
        s @ ("auto" | "responses" | "chat") => crate::llm::ApiStyle::parse(s),
        other => {
            return (StatusCode::BAD_REQUEST, format!("不认识的协议：{other}")).into_response()
        }
    };
    let keep = (style != crate::providers::default_api_style(&id)).then_some(style);
    match st.set_api_style(&id, keep) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderBody {
    /// 空 = 不改（已经配过时，比如只改「其他」的 Base URL）
    #[serde(default)]
    api_key: String,
    /// 只有「其他」用
    #[serde(default)]
    base_url: String,
}

/// `POST /api/providers/{id}` → 存一家的 Key。**先验证**（拉一次 `/models`），
/// 不对就不存。
///
/// 还没有在用的模型（第一次配）、或者改的就是当前这家：顺手切过去 ——
/// 填完 Key 就能直接聊，不用再去选一次模型。
async fn save_provider(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
    Json(body): Json<ProviderBody>,
) -> Response {
    if !crate::providers::is_known(&id) {
        return (StatusCode::NOT_FOUND, "没有这个服务商").into_response();
    }
    let old = st.provider_creds(&id);
    let api_key = match body.api_key.trim() {
        "" => match &old {
            Some(c) => c.api_key.clone(),
            None => return (StatusCode::BAD_REQUEST, "API Key 不能为空").into_response(),
        },
        k => k.to_string(),
    };
    let creds = crate::providers::Creds {
        api_key,
        base_url: body.base_url.trim().trim_end_matches('/').to_string(),
    };
    let Some(probe) = st.provider_for(&id, &creds, "") else {
        return (StatusCode::BAD_REQUEST, "Base URL 不能为空").into_response();
    };
    if let Err(e) = crate::models::verify(&probe).await {
        return (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response();
    }

    st.set_provider_creds(&id, Some(creds.clone()));
    let models = st.models_of(&id, &probe, true).await.unwrap_or_default();
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();

    // 第一次配 / 改的就是当前这家 → 切过去（当前这家保留原来选的模型）
    let current = st.llm();
    let current_id = current.as_ref().map(crate::providers::identify);
    let mut switched = false;
    if current.is_none() || current_id == Some(id.as_str()) {
        let model = match &current {
            Some(c) if current_id == Some(id.as_str()) => Some(c.model.clone()),
            _ => crate::providers::pick_default(&id, &ids),
        };
        if let Some(model) = model {
            if let Some(mut p) = st.provider_for(&id, &creds, &model) {
                p.reasoning_effort = current.and_then(|c| c.reasoning_effort);
                switch_provider(&st, &p).await;
                switched = true;
            }
        }
    }
    st.log(
        "info",
        format!(
            "已配置 {}（{} 个模型）",
            crate::providers::display_name(&id),
            ids.len()
        ),
    );
    Json(json!({ "ok": true, "models": ids.len(), "current": switched })).into_response()
}

/// `DELETE /api/providers/{id}` → 删掉一家的 Key。
///
/// 删的是当前在用的那家：切到另一家配好了的（它的首选模型）；一家都不剩
/// 就清空当前 provider。
async fn delete_provider(
    State(st): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    st.set_provider_creds(&id, None);
    let was_current = st
        .llm()
        .is_some_and(|p| crate::providers::identify(&p) == id);
    if was_current {
        let effort = st.llm().and_then(|p| p.reasoning_effort);
        let next = st
            .providers
            .read()
            .ok()
            .and_then(|g| g.iter().next().map(|(k, v)| (k.clone(), v.clone())));
        match next.and_then(|(nid, c)| Some((nid.clone(), st.provider_for(&nid, &c, "")?))) {
            Some((nid, probe)) => {
                let ids: Vec<String> = st
                    .models_of(&nid, &probe, false)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.id)
                    .collect();
                let mut p = probe;
                p.model = crate::providers::pick_default(&nid, &ids).unwrap_or_default();
                p.reasoning_effort = effort;
                if p.model.is_empty() {
                    st.clear_llm();
                } else {
                    switch_provider(&st, &p).await;
                }
            }
            None => st.clear_llm(),
        }
    }
    Json(json!({ "ok": true })).into_response()
}

/// `POST /api/attachment` → 上传一个附件（图片 / PDF）。
///
/// body 就是文件原始字节（不是 multipart）：文件名走 `x-filename`
/// （percent-encoded，避免非ASCII 头非法），类型服务端按魔数自己认，
/// 不信 `content-type`。处理完落盘并记进 `pending_attachments`，
/// 随后的 `/api/chat` 带着返回的 `id` 把它挂到那条用户消息上。
async fn upload_attachment(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "空文件").into_response();
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let name = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "file".into());

    let core = Core { state: st.clone() };
    match core.store_attachment(name, mime, body.to_vec()).await {
        Ok(a) => {
            st.add_pending_attachment(a.clone());
            Json(json!({
                "id": a.id,
                "name": a.name,
                "mime": a.mime,
                "kind": a.kind,
                "url": format!("/api/attachment/{}", a.id),
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/attachment/{id}` → 取回附件本体给用户消息气泡展示。
///
/// `id` 必须是裸文件名（`attachment_path` 把这一点钉死在路径拼接之前）。
async fn get_attachment(State(st): State<Arc<AppState>>, UrlPath(id): UrlPath<String>) -> Response {
    let core = Core { state: st };
    let Some(path) = core.attachment_path(&id) else {
        return (StatusCode::NOT_FOUND, "没有这个附件").into_response();
    };
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, mime)], bytes).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读附件失败: {e}"),
        )
            .into_response(),
    }
}

/// `POST /api/model` → 在当前 provider 内换模型（不用再传 key）。
///
/// 换完自动重判视觉能力（并落盘）—— 模型能不能看图是它自己的属性。
async fn set_model(State(st): State<Arc<AppState>>, Json(body): Json<ModelBody>) -> Response {
    if body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "model 不能为空").into_response();
    }
    let current = st.llm().or_else(Provider::from_env);
    let current_id = current.as_ref().map(crate::providers::identify);
    let mut p = match (&body.provider, current) {
        // 换到另一家：用设置页存下的那家的 Key，思考强度沿用
        (Some(id), cur) if current_id != Some(id.as_str()) => {
            let Some(creds) = st.provider_creds(id) else {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("{} 还没有配置 API Key", crate::providers::display_name(id)),
                )
                    .into_response();
            };
            let Some(mut p) = st.provider_for(id, &creds, "") else {
                return (StatusCode::BAD_REQUEST, "这家服务商的配置不完整").into_response();
            };
            p.reasoning_effort = cur.and_then(|c| c.reasoning_effort);
            p
        }
        (_, Some(cur)) => cur,
        (_, None) => return (StatusCode::BAD_REQUEST, "还没有配置 LLM").into_response(),
    };
    p.model = body.model.trim().to_string();
    if let Some(e) = body.reasoning_effort {
        p.reasoning_effort = crate::llm::normalize_effort(&e);
    }
    let (vision, pdf) = switch_provider(&st, &p).await;
    Json(json!({
        "ok": true,
        "model": p.model,
        "provider": crate::providers::identify(&p),
        "vision": vision,
        "pdf": pdf,
        "reasoningEffort": p.reasoning_effort,
    }))
    .into_response()
}

/// `GET /api/session?rev=…` → 服务端的会话历史，转成前端 `Message[]` 的形状。
///
/// 前端刷新/重建后用它恢复对话（否则消息只活在 JS 内存里）；agent 在服务端
/// 跑着时前端也会轮询它。
///
/// 轮询带上次拿到的 `rev`：一致就只回一句 `unchanged`。不然每次轮询都要把
/// 整段历史（可能几百 KB）重新拼成 JSON 再传一遍 —— 一段长任务（比如一条
/// 跑几分钟的 `apk add`）期间内容根本没变，而那要传几十次。
///
/// 另外两个参数让它能「只给要显示的那些」：
///
/// - `tail=N`：**只给最后 N 条**（视图消息的条数）。前端一屏只看得到几条，
///   没必要把整段历史（含每个 50KB 的 bash 输出）都传过去 —— 长会话首次打开
///   就是几 MB。不传 / `0` = 整段（脚本、调试那条路照旧）。
/// - `before=<from>`：往前翻一页（`from` 就是上一次响应里的那个字段，原样带回来）。
///   历史是只追加的，往前翻永远是在读**已经定了的**东西，所以这一路不带 rev、
///   不参与增量那一套。
///
/// 响应里的 `from` / `hasMore` 是给客户端的窗口指针：`messages` 是完整视图里
/// `[from, from + messages.len())` 那一段，`hasMore` = 上面还有没有更早的。
async fn get_session(
    State(st): State<Arc<AppState>>,
    Query(q): Query<SessionQuery>,
) -> impl IntoResponse {
    // 不指定 = 当前活动会话（界面）；调试脚本指定自己那条调试会话
    let id = {
        let store = st.chat.read().await;
        match q.id.as_deref() {
            Some(id) => store.get(id).map(|s| s.id.clone()),
            None => store.active_ref().map(|s| s.id.clone()),
        }
    };
    let Some(id) = id else {
        return Json(json!({
            "messages": [], "queued": [], "rev": "", "busy": false, "from": 0, "hasMore": false
        }));
    };

    // 往前翻一页（用户往上滚）：只要那一窗消息
    if let Some(before) = q.before {
        return Json(session_page_body(&st, &id, before, q.window()).await);
    }

    let mut v = session_body(&st, &id, &q.rev, q.tail.unwrap_or(0), q.from).await;
    // 哪个会话：前端按它过滤排队那几条
    v["session"] = json!(id);
    // 上一个回合是怎么失败的（没在跑时才报）。排队消息接着开的回合没有人订阅它的流，
    // 错误只有这一条路能让界面看见；前台回合的错误刷新之后也还在
    if !st.is_busy(&id) {
        if let Some(e) = st.turn_error(&id) {
            v["turnError"] = json!(e);
        }
    }
    Json(v)
}

/// 往前翻一页的正文（`?before=<from>`）：`before` 是客户端手上那窗的起点。
///
/// 只回 `messages` / `from` / `hasMore` —— 排队、用量、rev 那些是**当前**的事，
/// 跟一页历史没关系；混在一起发反而要让客户端去分辨哪些能用。
async fn session_page_body(st: &AppState, id: &str, before: usize, limit: usize) -> Value {
    if let Err(e) = st.ensure_loaded(id).await {
        return json!({ "messages": [], "from": before, "hasMore": true, "error": format!("{e:#}") });
    }
    let store = st.chat.read().await;
    let Some(sess) = store.get(id) else {
        return json!({ "messages": [], "from": 0, "hasMore": false });
    };
    if !sess.resident() {
        // 刚读完又被 gc 放掉了（极少见）：这一页先不发，让客户端下次再要 ——
        // 回 hasMore: true 而不是 false，不然那一截历史就静悄悄地看不到了
        return json!({ "messages": [], "from": before, "hasMore": true });
    }
    let (start, end) = crate::agent::session_page(sess, before, limit);
    let from = crate::agent::session_view_count_before(sess, start);
    json!({
        "messages": crate::agent::session_view_range(sess, start, end),
        "from": from,
        "hasMore": from > 0,
    })
}

/// `get_session` 的正文（不含 `session` / `turnError` 这两个每条路都要带的字段）。
///
/// `tail` = 尾部窗口的条数（`0` = 整段）；`client_from` = 客户端手上那窗的起点
/// （增量补丁的下标要按它翻译，见下）。
async fn session_body(
    st: &AppState,
    id: &str,
    client_rev: &str,
    tail: usize,
    client_from: Option<usize>,
) -> Value {
    // 「进行中」看的是**这个**会话：别的会话在跑不影响这里发消息
    let busy = st.is_busy(id);
    // 不活跃的会话消息不在内存里，先读回来
    if let Err(e) = st.ensure_loaded(id).await {
        return json!({ "messages": [], "queued": [], "rev": "", "busy": busy, "error": format!("{e:#}") });
    }

    // 读锁一直持到拼完 —— rev 和它对应的那份 messages 必须来自同一个瞬间，
    // 不然客户端会存下一个「对不上的 rev」，之后就永远以为自己是最新的。
    // 排队等着交给模型的那些（界面上的「排队中」）。**每次都要带**：它们的变化
    // 不进 `rev`（那是消息的版本号），光看 rev 是看不出「这条交出去了」的。
    let queued = crate::agent::queued_view(&st.queued_list(id));
    // token 用量（输入框那个上下文圆环）。`unchanged` 那条路不带它（它只在有 LLM
    // 调用时才变，那时 rev 也一定变了）。在拿会话读锁**之前**取：缓存没命中时
    // 要去阻塞池读库，别拿着锁等。
    let usage = st.usage_of(id).await;

    let store = st.chat.read().await;
    let Some(sess) = store.get(id) else {
        return json!({
            "messages": [], "queued": queued, "rev": "", "busy": false, "from": 0, "hasMore": false
        });
    };
    if !sess.resident() {
        // 读进来之后、拿锁之前又被放掉了（极少见）：让客户端下次再来
        return json!({ "unchanged": true, "queued": queued, "rev": client_rev, "busy": busy });
    }
    let rev = session_rev(sess);
    if !client_rev.is_empty() && client_rev == rev {
        return json!({ "unchanged": true, "queued": queued, "rev": rev, "busy": busy });
    }

    // 增量：客户端上次的 rev 里有 (会话 id, 已存消息数)。同一个会话、且它手上的
    // 消息数没超过当前（没被重置）→ 只补差的那段，见 `session_patch`。
    //
    // **下标要翻译**：补丁里的 `replaceFrom` 是整段视图的下标，而客户端手上可能只有
    // 尾部那一窗（`tail=N`）—— 它那边的下标比整段的小 `client_from`。落点在它手上
    // 那窗之外（说明它离得太远，比如从冻住里醒来）就退到下面「重发尾部窗口」。
    // 不带 `client_from` 的（脚本、老客户端）按整段算，和以前一模一样。
    if let Some((cid, client_len)) = parse_rev(client_rev) {
        if cid == sess.id && client_len <= sess.messages.len() {
            let p = crate::agent::session_patch(sess, client_len);
            let base = client_from.unwrap_or(0);
            if p.replace_from >= base {
                drop(store);
                return json!({
                    "messages": p.messages,
                    "queued": queued,
                    "rev": rev,
                    "busy": busy,
                    "append": true,
                    "replaceFrom": p.replace_from - base,
                    "keepParts": p.keep_parts,
                    "usage": usage,
                });
            }
        }
    }

    // 尾部窗口（`tail = 0` 就是整段）。起点落在回合边界上，见 `session_tail_start`。
    let start = crate::agent::session_tail_start(sess, tail);
    let from = crate::agent::session_view_count_before(sess, start);
    let msgs = crate::agent::session_view_range(sess, start, sess.messages.len());
    drop(store);

    // busy 让前端知道「服务端还在跑」—— App 切到后台时 agent 不会停，
    // 切回来要能看出还在进行中，而不是以为卡住了。
    json!({
        "messages": msgs,
        "queued": queued,
        "rev": rev,
        "busy": busy,
        "usage": usage,
        "from": from,
        "hasMore": from > 0,
    })
}

/// 从 rev 里解出 (会话 id, 已存消息数)。格式见 `session_rev`。
fn parse_rev(rev: &str) -> Option<(&str, usize)> {
    let mut it = rev.split(':');
    let id = it.next()?;
    let len = it.next()?.parse().ok()?;
    if id.is_empty() {
        return None;
    }
    Some((id, len))
}

#[derive(Deserialize)]
struct SessionQuery {
    /// 客户端上次拿到的版本号。空 = 全量要一份。
    #[serde(default)]
    rev: String,
    /// 要哪个会话。不给 = 当前活动会话。
    #[serde(default)]
    id: Option<String>,
    /// 只给最后 `tail` 条视图消息（`0` / 不给 = 整段）。界面首屏就带它。
    #[serde(default)]
    tail: Option<usize>,
    /// 往前翻一页：要「视图下标 < before」那一窗（`before` = 上一次响应里的 `from`）。
    #[serde(default)]
    before: Option<usize>,
    /// 客户端手上那窗的起点（`from`）。增量补丁的下标按它翻译。
    #[serde(default)]
    from: Option<usize>,
}

impl SessionQuery {
    /// 一窗多大（往前翻一页的默认档；前端每次都带 `tail`，这儿只是它没带时的兑底）。
    ///
    /// 和前端那个 `MSG_WINDOW` 同一个数：两侧对不上的话，要么白传（服务端
    /// 给多了）要么半天翻不出一屏（给少了）。
    fn window(&self) -> usize {
        self.tail.filter(|n| *n > 0).unwrap_or(10)
    }
}

/// 会话版本号。**只看元数据，不碰消息内容**，所以便宜。
///
/// 带 `id`：切换会话时长度和时间戳很容易撞车，撞了客户端就会一直拿着
/// 上一个会话的消息不动。
fn session_rev(sess: &crate::agent::ChatSession) -> String {
    format!("{}:{}:{}", sess.id, sess.message_count(), sess.updated_at)
}

// ---------------------------------------------------------------------------
// 调试接口（`/api/debug/*`）
// ---------------------------------------------------------------------------
//
// 存在的理由只有一个：**从电脑上对着真机调，不用重装 APK、也不用手点手机屏幕**。
// 服务本身只听 127.0.0.1（连手机要 `adb forward`，见 scripts/agentctl.py），
// 所以这些接口不会暴露到网上。

/// `GET /api/debug/state` → 一眼看全运行时状态。
///
/// 原本要敲六个接口（health / sessions / linux / llm …）才能拼出来，
/// 而调试时最想要的就是「现在到底什么情况」。前端不用它。
async fn debug_state(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let provider = st.llm().or_else(Provider::from_env);
    let sessions = {
        let store = st.chat.read().await;
        json!({
            "count": store.sessions.len(),
            "active": store.active_ref().map(|s| json!({
                "id": s.id,
                "title": if s.title.is_empty() { "新对话" } else { &s.title },
                "messages": s.message_count(),
                "resident": s.resident(),
                "rev": session_rev(s),
            })),
        })
    };

    Json(json!({
        "version": VERSION,
        "git": crate::state::GIT,
        "platform": std::env::consts::OS,
        "port": st.port.load(std::sync::atomic::Ordering::Relaxed),
        "mock": st.mock.load(std::sync::atomic::Ordering::Relaxed),
        "device": st.device().map(|d| d.name()),
        "busySessions": st.busy_sessions(),
        "sessions": sessions,
        "linux": st.linux().map(|s| json!({
            "rootfs": s.rootfs().display().to_string(),
            "host": s.is_host(),
            "sharedStorage": s.has_shared_storage(),
        })),
        "browser": st.browser().map(|b| b.describe()),
        // 持久化目录（安卓上是 <filesDir>）。调试时要 adb pull 会话库就用它，不用猜路径。
        "dataDir": st
            .data_dir
            .read()
            .ok()
            .and_then(|g| g.clone())
            .map(|p| p.display().to_string()),
        "llm": provider.map(|p| json!({
            "name": p.name,
            "model": p.model,
            "baseUrl": p.base_url,
            "reasoningEffort": p.reasoning_effort,
        })),
        "systemPrompt": {
            "chars": st.system_prompt().chars().count(),
            "overridden": st.system_prompt_overridden(),
        },
    }))
}

/// `GET /api/debug/request` → **下一次**会发给 provider 的完整请求体。
///
/// 调提示词、查 prompt cache 的时候不用猜：它就是模型将看到的东西
/// （`messages[0]` 是历史里的系统提示词，`tools` 是当前工具表）。
///
/// 说是「预测」是因为真的发的时候，历史里可能先补上一条新的系统提示词
/// （`ensure_system_prompt`）—— 这里也照做，所以和实际发送一致。
async fn debug_request(State(st): State<Arc<AppState>>) -> Response {
    let Some(p) = st.llm().or_else(Provider::from_env) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "未配置 LLM provider").into_response();
    };

    let active = st.chat.read().await.active_ref().map(|s| s.id.clone());
    if let Some(id) = &active {
        if let Err(e) = st.ensure_loaded(id).await {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
        }
    }
    let mut sess = {
        let store = st.chat.read().await;
        match store.active_ref() {
            // 拷一份：不想持着锁做拼装，而这是手动的调试接口，不在乎这点开销
            Some(s) => s.clone(),
            None => return (StatusCode::NOT_FOUND, "没有活跃会话").into_response(),
        }
    };
    crate::agent::ensure_system_prompt(&mut sess, &st.system_prompt());

    // 和真正发出去的一致：按解析出来的协议偏好给请求体
    let images = st.cached_supports_images(&p);
    let files = st.cached_supports_files(&p);
    let body = match p.transport() {
        crate::llm::Transport::Responses => crate::agent::build_responses_request(
            &sess,
            st.platform(),
            &p.model,
            images,
            files,
            p.reasoning_effort.as_deref(),
        )
        .and_then(|r| Ok(serde_json::to_value(&r)?)),
        crate::llm::Transport::Chat => crate::agent::build_request(
            &sess,
            st.platform(),
            &p.model,
            images,
            files,
            p.reasoning_effort.as_deref(),
        )
        .and_then(|r| Ok(serde_json::to_value(&r)?)),
    };
    match body {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

/// `GET /api/debug/tools` → 当前工具表（就是发给模型的 `tools` 字段）。
///
/// 存在的意义：沙箱里的 `agent tools` 靠它回答「我自己有哪些工具」，
/// 不用去翻整份请求体。给的是 Chat Completions 的形态（嵌套 `function`）。
async fn debug_tools(State(st): State<Arc<AppState>>) -> Response {
    match serde_json::to_value(crate::agent::tool_definitions(st.platform())) {
        Ok(v) => Json(json!({ "tools": v })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
struct SystemPromptBody {
    /// **原样**使用（不做 trim）：调提示词时字节必须可控。
    /// 空串 = 恢复成编译进二进制的那一版。
    #[serde(default)]
    text: String,
}

/// `POST /api/debug/system-prompt` → 运行时换一份系统提示词。
///
/// 不重装 APK 就能调提示词，只改内存（重启回到编译进去的那份，`prompt::system_prompt`）。
///
/// 换提示词**不会动老会话的前缀**：`ensure_system_prompt` 会把新的作为一条
/// system 消息追加在历史末尾，老前缀原样保留、缓存照命中
/// （见 docs/03-prompt-cache.md §3.1）。
///
/// ```sh
/// curl -s localhost:8787/api/debug/system-prompt \
///   -H 'content-type: application/json' \
///   -d "$(python3 -c 'import json,sys;print(json.dumps({"text":open("p.txt").read()}))')"
/// ```
async fn debug_set_system_prompt(
    State(st): State<Arc<AppState>>,
    Json(body): Json<SystemPromptBody>,
) -> impl IntoResponse {
    let on = !body.text.is_empty();
    st.set_system_prompt(if on { Some(body.text) } else { None });
    let chars = st.system_prompt().chars().count();
    // 覆盖是内存态、重启就没了，但当时必须是看得见的 —— 否则
    // “为什么它的表现和代码里写的不一样”能查半天。
    st.log(
        "warn",
        if on {
            format!("系统提示词已被运行时覆盖（{chars} 字符），重启后回到编译进去的那版")
        } else {
            "系统提示词已恢复成编译进去的那版".to_string()
        },
    );
    Json(json!({ "ok": true, "overridden": on, "chars": chars }))
}

/// `GET /api/sessions` → 侧边栏的「最近对话」列表
async fn list_sessions(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let busy = st.busy_sessions();
    let store = st.chat.read().await;
    let mut list = store.list();
    drop(store);
    // 抽屉里给正在跑的会话打个点：切走了也知道它还在干活
    for s in &mut list {
        let running = s["id"]
            .as_str()
            .is_some_and(|id| busy.iter().any(|b| b == id));
        s["busy"] = json!(running);
    }
    Json(json!({ "sessions": list }))
}

/// `GET /api/search?q=…` → 跨会话搜消息正文。
///
/// 侧边栏顶上那个搜索框就靠它。命中回 `{ id, title, active, role, snippet }`，
/// 点一下就是普通的切会话（activate）。
#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn search_sessions(
    State(st): State<Arc<AppState>>,
    Query(SearchQuery { q }): Query<SearchQuery>,
) -> impl IntoResponse {
    // 消息不在内存里的会话要临时读库（阻塞 I/O），整个搜索放进阻塞线程跑
    let store = st.chat.clone().read_owned().await;
    let db = st.db();
    let qq = q.clone();
    let hits = tokio::task::spawn_blocking(move || {
        // 每个会话最多 3 条、总共 60 条：侧边栏不是搜索页，再多也看不过来
        store.search(&qq, 3, 60, |id| {
            db.as_ref().and_then(|d| d.load_messages(id).ok())
        })
    })
    .await
    .unwrap_or_default();
    Json(json!({ "q": q, "hits": hits }))
}

/// `POST /api/sessions` → 新建会话
async fn new_session(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut store = st.chat.write().await;
    let id = store.create();
    drop(store);
    st.mark_sessions_dirty();
    Json(json!({ "id": id }))
}

#[derive(Deserialize)]
struct ActivateBody {
    id: String,
}

/// `POST /api/sessions/activate` → 切换会话
async fn activate_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ActivateBody>,
) -> Response {
    let mut store = st.chat.write().await;
    let ok = store.activate(&body.id);
    drop(store);
    st.mark_sessions_dirty();
    if ok {
        Json(json!({ "ok": true })).into_response()
    } else {
        (StatusCode::NOT_FOUND, "没有这个会话").into_response()
    }
}

#[derive(Deserialize)]
struct RenameBody {
    id: String,
    title: String,
}

/// `POST /api/sessions/rename` → 重命名会话
async fn rename_session(State(st): State<Arc<AppState>>, Json(body): Json<RenameBody>) -> Response {
    let mut store = st.chat.write().await;
    match store.sessions.iter_mut().find(|s| s.id == body.id) {
        Some(s) => {
            let t = body.title.trim();
            s.title = t.chars().take(60).collect();
            drop(store);
            st.mark_sessions_dirty();
            Json(json!({ "ok": true })).into_response()
        }
        None => (StatusCode::NOT_FOUND, "没有这个会话").into_response(),
    }
}

/// `POST /api/sessions/delete` → 删除会话
async fn delete_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<ActivateBody>,
) -> Response {
    st.delete_session(&body.id).await;
    Json(json!({ "ok": true })).into_response()
}

#[derive(Deserialize)]
struct UnqueueBody {
    session: String,
    id: String,
}

/// `POST /api/sessions/unqueue` → 撤掉一条**还在排队**的用户消息。
///
/// 界面上「排队中」那条气泡左边那个 ×。已经交给模型的撤不掉（那时它是历史了），
/// 这时回 `removed: false` —— 不是什么错误，界面刷新一下就看到它已经落进正文了。
async fn unqueue(State(st): State<Arc<AppState>>, Json(body): Json<UnqueueBody>) -> Response {
    let removed = st.unqueue(&body.session, &body.id).await;
    Json(json!({ "ok": true, "removed": removed })).into_response()
}

/// `POST /api/reset` → 清空会话历史。
async fn reset(State(st): State<Arc<AppState>>) -> Response {
    match st.reset_active().await {
        Ok(()) => {
            st.log("info", "会话已清空");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(why) => (StatusCode::CONFLICT, why).into_response(),
    }
}

/// 一个现生成的 id（客户端没给 `id` 时用：脚本 / agentctl）。
fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// 「这条消息排上队了」的回应：一条只有一帧（`data-queued`）的流。
///
/// 刻意**和正常回合同一种形状**（SSE + 同一个 header）：前端那一路就多一个
/// 「先看首帧」的判断，不用为一个状态分出一条 HTTP 分支。
fn queued_response(id: &str) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let _ = tx.send(sse::queued(id));
    drop(tx); // 通道关掉 → 转发任务补上 [DONE]
    sse_response(rx)
}

/// 把 chunk 通道包成带正确 header 的 SSE 响应，并在结束时补 `[DONE]`。
///
/// 上游是无界通道（见 agent.rs 的 `send`）：前端读得慢只会让 chunk 在这里
/// 排队，不会反压到 agent 循环。排队的大头是 bash 的实时输出（每 120ms 一份、
/// 每份最多 64KB 的尾部快照），而这种东西只有最新的一份有用 —— 所以每次把
/// 积压的一批取出来，被后来者盖掉的快照直接扔（`coalesce`）。客户端挂着连接
/// 却不读（WebView 被挂起）时，积压就不会无限涨。
fn sse_response(mut rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>) -> Response {
    let (out_tx, out_rx) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(v) = rx.try_recv() {
                batch.push(v);
            }
            for v in coalesce(batch) {
                if out_tx
                    .send(Ok(SseEvent::default().data(v.to_string())))
                    .await
                    .is_err()
                {
                    return; // 客户端断开
                }
            }
        }
        // 上游通道关闭 = 回合结束
        let _ = out_tx.send(Ok(SseEvent::default().data(sse::DONE))).await;
    });

    let mut res = Sse::new(ReceiverStream::new(out_rx))
        .keep_alive(KeepAlive::default())
        .into_response();
    res.headers_mut().insert(
        sse::STREAM_HEADER,
        HeaderValue::from_static(sse::STREAM_HEADER_VALUE),
    );
    res
}

/// 扔掉一批 chunk 里被盖掉的工具阶段性输出：同一个工具调用后面还有输出
/// （阶段性的或最终的）时，前面那份快照没有意义 —— 前端是整份替换，不是拼接。
fn coalesce(batch: Vec<Value>) -> Vec<Value> {
    let output_of = |v: &Value| -> Option<String> {
        (v["type"] == "tool-output-available")
            .then(|| v["toolCallId"].as_str().map(str::to_string))
            .flatten()
    };
    let mut later: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut keep = vec![true; batch.len()];
    for (i, v) in batch.iter().enumerate().rev() {
        let Some(id) = output_of(v) else { continue };
        if v["preliminary"] == true && later.contains(&id) {
            keep[i] = false;
        }
        later.insert(id);
    }
    batch
        .into_iter()
        .zip(keep)
        .filter_map(|(v, k)| k.then_some(v))
        .collect()
}

// ---------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    State(st): State<Arc<AppState>>,
) -> Response {
    // WebSocket 不受 CORS 管：手机浏览器里随便一个网页都能连
    // ws://127.0.0.1:<端口>，然后给设备下指令。只放行本机来源（App 自己、
    // Vite 开发服务器）、**同源**的页面（局域网里用手机浏览器开电脑上的核心时，
    // 来源是 `http://192.168.x.x:8787`，就是我们自己），和不带 Origin 的非浏览器
    // 客户端。沙箱里的 HTML 是 opaque origin（Origin: null），也挡在外面；
    // `sandbox.localhost` 那个源和 Host 对不上，同样挡住。
    if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
        let origin = origin.to_str().unwrap_or("");
        let host = headers
            .get(axum::http::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if !is_loopback_origin(origin) && !is_same_origin(origin, host) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

/// `Origin`（`http://host:port`）和请求的 `Host` 头是不是同一个地址。
fn is_same_origin(origin: &str, host: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    !host.is_empty() && rest.trim_end_matches('/').eq_ignore_ascii_case(host)
}

fn is_loopback_origin(origin: &str) -> bool {
    let Some(rest) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = match host.rfind(':') {
        Some(i) if !host.ends_with(']') => &host[..i],
        _ => host,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

// ---------------------------------------------------------------------------
// 沙箱文件：`preview:` 协议的落点
// ---------------------------------------------------------------------------
//
// AI 在回复里写 `[报告](preview:/root/report.html)` 或
// `![图](preview:/root/chart.png)`，前端把它改写成 `/sandbox/root/…`
// 打到这里。按 URL 路径直接映射，所以 HTML 里的相对路径（css/js/图片）
// 天然能用。
//
// 安全：这些文件是 AI 写的（内容和脚本可能来自网上）。它们要是和 App 同源，
// 页面里的任何脚本都能带着 App 的凭据 cookie 调 `/api/linux` —— 在挂着整个
// 内部存储的沙箱里跑命令。两条路：
//
// - **`sandbox.localhost:<端口>`（网页就该在这里开）**：独立的源（见
//   `SANDBOX_HOST` / `split_origins`），碰不到 App 的凭据，所以不加 CSP
//   sandbox —— localStorage / IndexedDB 这些正常能用。查看器的 iframe 和
//   agent 的浏览器都开这个。
// - `127.0.0.1:<端口>`（和 App 同源）：图片 / 音视频 / 文本这些 App 自己取的
//   还走这里。一律加 `Content-Security-Policy: sandbox` —— 页面是 opaque
//   origin，读不到 /api/*、连不上 WebSocket（见 ws_upgrade 的 Origin 检查）。
//
// 路径越界和符号链接逃逸由 LinuxSandbox::resolve_path 挡。

/// 路径上的沙箱钥匙对不对（见 `state::Auth`）。不对一律 404，不说为什么。
fn sandbox_key_ok(st: &AppState, key: &str) -> bool {
    same_secret(key, &st.auth().sandbox_key)
}

async fn sandbox_root(State(st): State<Arc<AppState>>, UrlPath(key): UrlPath<String>) -> Response {
    if !sandbox_key_ok(&st, &key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_sandbox(&st, &key, "/workspace/", true, None).await
}

async fn sandbox_file(
    State(st): State<Arc<AppState>>,
    UrlPath((key, path)): UrlPath<(String, String)>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    if !sandbox_key_ok(&st, &key) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // 目录要带结尾斜杠，否则页面里的相对链接会解析到上一级
    let trailing = uri.path().ends_with('/');
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    serve_sandbox(&st, &key, &format!("/{path}"), trailing, range).await
}

/// 沙箱文件的 Content-Type。
///
/// ⚠️ **不能直接信 `mime_guess`**：`.sh` → `application/x-sh`、`.py` →
/// `text/x-python`、`.json` → `application/json` —— 这些类型 WebView 不保证
/// 当文本渲染，iframe 里就是一片空白（点开一个 `.sh` 链接是空的）。
///
/// 规则：
/// - 网页**和它要的子资源**（html/css/js/json/xml）按原样 —— 生成的网页靠这个能跑
/// - 图片 / 音视频 / 字体 / pdf 按原样 —— 浏览器认识
/// - 其余一律 `text/plain`（先嗅一下像不像文本；不像就 octet-stream 让它下载）
async fn sandbox_mime(host: &std::path::Path, name: &str) -> String {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let e = mime.essence_str();
    let keep = e == "text/html"
        || e == "application/xhtml+xml"
        || e == "text/css"
        || e == "application/javascript"
        || e == "text/javascript"
        || e == "application/json"
        || e.ends_with("+json")
        || e == "application/xml"
        || e == "text/xml"
        || e.starts_with("image/")
        || e.starts_with("audio/")
        || e.starts_with("video/")
        || e.starts_with("font/")
        || e == "application/pdf"
        || e == "application/wasm";
    if keep {
        return if e.starts_with("text/") || e.ends_with("json") || e.ends_with("xml") {
            format!("{e}; charset=utf-8")
        } else {
            e.to_string()
        };
    }
    if looks_like_text(host).await {
        "text/plain; charset=utf-8".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

/// 开头这 4KB 像不像文本：没有 NUL、也没有 \n \r \t 之外的控制字符。
///
/// 用「像不像文本」而不是扩展名白名单：代码文件的后缀太多太杂
/// （`.sh` `.py` `.rs` `.toml` `.env` `.gitignore` …），而二进制倒是很好认。
async fn looks_like_text(host: &std::path::Path) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut f) = tokio::fs::File::open(host).await else {
        return false;
    };
    let mut buf = vec![0u8; 4096];
    let n = match f.read(&mut buf).await {
        Ok(n) => n,
        Err(_) => return false,
    };
    buf[..n]
        .iter()
        .all(|&b| b == b'\n' || b == b'\r' || b == b'\t' || b >= 0x20)
}

/// 沙箱里的错误页（找不到 / 越界 / 沙箱没起来）。
///
/// 为什么要 HTML 而不是一行 `text/plain`：这东西多数时候是给**查看器里的用户**
/// 看的（iframe 里）。一行小字很容易被当成「网页打开是空白」，也说不清为什么没了。
/// 所以页面上直接写明：哪个路径、可能的原因，以及**东西该放哪**。
fn sandbox_error(status: StatusCode, guest: &str, reason: &str) -> Response {
    let title = match status {
        StatusCode::NOT_FOUND => "沙箱里没有这个文件",
        StatusCode::FORBIDDEN => "不能访问这个路径",
        StatusCode::SERVICE_UNAVAILABLE => "沙箱没起来",
        _ => "读不了这个文件",
    };
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <meta name=color-scheme content='dark light'>\
         <title>{title}</title><style>body{{font:15px/1.7 system-ui;background:#000000;color:#f9f9f7;\
         margin:0;padding:26px 20px}}code{{background:#262624;padding:2px 6px;border-radius:6px;\
         word-break:break-all}}h3{{margin:0 0 12px}}p{{color:#97958d;margin:12px 0}}\
         b{{color:#f9f9f7}}\
         /* 浅色：和前端一套色（见 web/src/app.css）—— 这页开在 App 的 WebView 里，\
            不跟就成了一整页黑压在白界面里 */\
         @media (prefers-color-scheme:light){{body{{background:#fcfcfb;color:#1c1c1b}}\
         code{{background:#eae9e4}}p{{color:#5f5e5a}}b{{color:#1c1c1b}}}}</style>\
         <h3>{title}</h3>\
         <p><code>{path}</code></p>\
         <p>{reason}</p>\
         <p><b>App 重装或清数据会清空沙箱内部</b>（<code>/root</code>、<code>/usr</code> 这些）；\
         放在 <code>/workspace</code>（手机内部存储的 Agent 文件夹）里的东西不会丢。\
         让 agent 重新生成一次就行。</p>",
        title = title,
        path = esc(guest),
        reason = esc(reason),
    );
    let mut res = (status, axum::response::Html(html)).into_response();
    sandbox_headers(&mut res, "text/html; charset=utf-8");
    res
}

/// 沙箱页面统一的响应头
fn sandbox_headers(res: &mut Response, mime: &str) {
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime) {
        h.insert(axum::http::header::CONTENT_TYPE, v);
    }
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "sandbox allow-scripts allow-forms allow-modals allow-popups allow-downloads",
        ),
    );
    // 文件随时会被 AI 改写，别缓存
    h.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
}

async fn serve_sandbox(
    st: &Arc<AppState>,
    key: &str,
    guest: &str,
    trailing: bool,
    range: Option<String>,
) -> Response {
    let Some(sb) = st.linux() else {
        return sandbox_error(
            StatusCode::SERVICE_UNAVAILABLE,
            guest,
            "Linux 沙箱没有初始化。",
        );
    };
    let host = match sb.resolve_path(guest) {
        Ok(p) => p,
        Err(e) => return sandbox_error(StatusCode::FORBIDDEN, guest, &format!("{e:#}")),
    };

    let meta = match tokio::fs::metadata(&host).await {
        Ok(m) => m,
        Err(_) => {
            return sandbox_error(
                StatusCode::NOT_FOUND,
                guest,
                "它可能被删过，或者路径写错了。",
            )
        }
    };

    if meta.is_dir() {
        if !trailing {
            let to = format!("/sandbox/{key}{}/", guest.trim_end_matches('/'));
            return axum::response::Redirect::to(&to).into_response();
        }
        let index = host.join("index.html");
        if index.is_file() {
            return serve_bytes(&index, "index.html", None).await;
        }
        return dir_listing(guest, &host).await;
    }

    serve_bytes(&host, guest, range).await
}

/// 流式返回文件，支持 `Range`。
///
/// 音视频必须支持区间请求：<video> 拖进度条就是发 Range，服务端不认
/// 的话只能从头播、拖不动。顺带大文件也不再整个读进内存。
async fn serve_bytes(host: &std::path::Path, name: &str, range: Option<String>) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut file = match tokio::fs::File::open(host).await {
        Ok(f) => f,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);

    let mime = sandbox_mime(host, name).await;

    let span = match range.as_deref().map(|r| parse_range(r, len)) {
        Some(Some(s)) => Some(s),
        Some(None) => {
            let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                res.headers_mut()
                    .insert(axum::http::header::CONTENT_RANGE, v);
            }
            return res;
        }
        None => None,
    };

    let (status, start, count) = match span {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b - a + 1),
        None => (StatusCode::OK, 0, len),
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let stream = tokio_util::io::ReaderStream::new(file.take(count));
    let mut res = (status, axum::body::Body::from_stream(stream)).into_response();
    sandbox_headers(&mut res, &mime);
    let h = res.headers_mut();
    h.insert(
        axum::http::header::ACCEPT_RANGES,
        HeaderValue::from_static("bytes"),
    );
    if let Ok(v) = HeaderValue::from_str(&count.to_string()) {
        h.insert(axum::http::header::CONTENT_LENGTH, v);
    }
    if let Some((a, b)) = span {
        if let Ok(v) = HeaderValue::from_str(&format!("bytes {a}-{b}/{len}")) {
            h.insert(axum::http::header::CONTENT_RANGE, v);
        }
    }
    res
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` → 闭区间 `[a, b]`。
/// 不满足（越界、多段）返回 None。
fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (a <= b && a < len).then_some((a, b))
}

/// 没有 index.html 的目录：给一个简单的文件列表
async fn dir_listing(guest: &str, host: &std::path::Path) -> Response {
    let mut entries: Vec<(String, bool)> = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(host).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            entries.push((e.file_name().to_string_lossy().into_owned(), dir));
        }
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let mut html = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <meta name=color-scheme content='dark light'>\
         <title>{t}</title><style>body{{font:15px system-ui;background:#000000;color:#f9f9f7;\
         margin:0;padding:12px 16px}}a{{color:inherit;text-decoration:none;display:block;\
         padding:10px 0;border-bottom:1px solid #262624}}.d{{color:#97958d}}\
         /* 浅色：和前端一套色（见 web/src/app.css）*/\
         @media (prefers-color-scheme:light){{body{{background:#fcfcfb;color:#1c1c1b}}\
         a{{border-bottom-color:#e7e6e1}}.d{{color:#5f5e5a}}}}</style>\
         <p class=d>{t}</p>",
        t = esc(guest)
    );
    if !guest.trim_end_matches('/').is_empty() {
        html.push_str("<a href='../' class=d>..</a>");
    }
    for (name, dir) in entries {
        let slash = if dir { "/" } else { "" };
        html.push_str(&format!(
            "<a href=\"{h}{slash}\">{n}{slash}</a>",
            h = esc(&name),
            n = esc(&name)
        ));
    }
    let mut res = html.into_response();
    sandbox_headers(&mut res, "text/html; charset=utf-8");
    res
}

/// 回合状态的 SSE 流（手机壳订阅它：刷通知栏那一行、起停前台服务）。
///
/// 为什么不直接用 `/api/ws`：壳那边（Kotlin）不想为一个「只收不发」的小用途引一个
/// WebSocket 客户端（握手 + 帧解析得自己写）；SSE 就是「HTTP 长连接 + 几行 `data:`」，
/// `HttpURLConnection` 读一行处理一行就完了。两边发的是**同一个 `Event` 的 JSON**。
///
/// 连上先补一帧当前状态（`AppState::turn_snapshot`）：订阅者可能在回合跑了一半才连上
/// （壳刚起、刚从系统冻住里醒来、网络刚回来），而一条 `bash` 可能几分钟不产生变化 ——
/// 不补的话它的通知栏就一直空着。
///
/// 鉴权走和其他 `/api/*` 一样的中间件（token cookie / 配对凭据），所以「手机连电脑上的
/// 核心」那条也能用同一个客户端订阅。
async fn turns_stream(State(st): State<Arc<AppState>>) -> Response {
    let mut rx = st.tx.subscribe();
    let (tx, out_rx) = tokio::sync::mpsc::channel::<Result<SseEvent, Infallible>>(64);

    tokio::spawn(async move {
        for ev in st.turn_snapshot() {
            if send_turn(&tx, &ev).await.is_err() {
                return;
            }
        }
        loop {
            match rx.recv().await {
                // 只转发回合状态。这条流不是事件总线，日志/设备事件不捎带
                Ok(ev @ (Event::Turn { .. } | Event::TurnActive { .. })) => {
                    if send_turn(&tx, &ev).await.is_err() {
                        break; // 客户端断开
                    }
                }
                Ok(_) => {}
                // 慢了：状态本来就是「最新一份覆盖旧的」，丢了就丢了
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    Sse::new(ReceiverStream::new(out_rx))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// 把一条事件塞进 SSE 流。
async fn send_turn(
    tx: &tokio::sync::mpsc::Sender<Result<SseEvent, Infallible>>,
    ev: &Event,
) -> Result<(), ()> {
    let Ok(json) = serde_json::to_string(ev) else {
        return Ok(()); // 序列化不了就丢这一帧，不值得把整条流掐了
    };
    tx.send(Ok(SseEvent::default().data(json)))
        .await
        .map_err(|_| ())
}

async fn ws_loop(mut socket: WebSocket, st: Arc<AppState>) {
    let mut rx = st.tx.subscribe();
    // 握手：先告诉前端自己在哪、有没有设备
    let hello = Event::Hello {
        port: st.port.load(std::sync::atomic::Ordering::Relaxed),
        version: VERSION.into(),
        platform: std::env::consts::OS.into(),
        mock: st.mock.load(std::sync::atomic::Ordering::Relaxed),
    };
    if send_event(&mut socket, &hello).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // Rust → 前端
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if send_event(&mut socket, &ev).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("ws subscriber lagged {n} events");
                }
                Err(_) => break,
            },

            // 前端 → Rust
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Err(e) = handle_client_msg(&st, t.as_str()).await {
                        st.log("warn", format!("bad ws message: {e}"));
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // ping/pong/binary 忽略
                Some(Err(_)) => break,
            },
        }
    }
}

async fn send_event(socket: &mut WebSocket, ev: &Event) -> Result<(), axum::Error> {
    let s = serde_json::to_string(ev).unwrap_or_else(|_| "{}".into());
    socket.send(Message::Text(s.into())).await
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMsg {
    /// 转发一条指令给设备
    Command { cmd: Command },
    /// 前端发起的日志（调试用）
    Log { level: String, msg: String },
}

async fn handle_client_msg(st: &Arc<AppState>, raw: &str) -> anyhow::Result<()> {
    let msg: ClientMsg = serde_json::from_str(raw)?;
    match msg {
        ClientMsg::Command { cmd } => {
            dispatch(st, cmd).await;
        }
        ClientMsg::Log { level, msg } => st.log(&level, format!("[web] {msg}")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 共享逻辑（HTTP 和 WS 都走这里；将来 uniffi 导出也复用）
// ---------------------------------------------------------------------------

/// 把指令发给设备执行。设备实现可能阻塞，所以走 `spawn_blocking`。
pub async fn dispatch(st: &Arc<AppState>, cmd: Command) -> serde_json::Value {
    let id = uuid::Uuid::now_v7().to_string();
    let kind = cmd_name(&cmd);

    let res = match (st.device(), &cmd) {
        (Some(dev), _) => {
            let cmd_json = match serde_json::to_string(&cmd) {
                Ok(s) => s,
                Err(e) => {
                    return json!({ "id": id, "ok": false, "detail": format!("序列化失败: {e}") })
                }
            };
            tokio::task::spawn_blocking(move || dev.execute(&cmd_json)).await
        }
        // 打开 URI 不需要无障碍：没有设备实现时交给浏览器那条（它不依赖无障碍，
        // 启动时就注进来了）。`agent open` / `$BROWSER` / `xdg-open` 都落在这里。
        (None, Command::Open { url }) if st.browser().is_some() => {
            let host = st.browser().expect("刚查过");
            let action = json!({ "action": "__open_uri", "url": url }).to_string();
            tokio::task::spawn_blocking(move || host.execute(&action).and_then(browser_reply)).await
        }
        (None, _) => {
            return json!({ "id": id, "ok": false, "detail": "没有设备实现（mock 未启用或无障碍服务未连接）" })
        }
    };

    let (ok, detail) = match res {
        Ok(Ok(d)) => (true, d),
        Ok(Err(e)) => (false, e),
        Err(e) => (false, format!("任务失败: {e}")),
    };

    st.log(
        if ok { "debug" } else { "warn" },
        format!("cmd {kind} -> {}", if ok { "ok" } else { "err" }),
    );
    st.emit(Event::Ack {
        id: id.clone(),
        ok,
        detail: detail.clone(),
    });

    json!({ "id": id, "ok": ok, "detail": detail })
}

/// 浏览器那边的回执（`{ok, text, error}`）→ 设备指令的 `Result` 形状。
fn browser_reply(raw: String) -> Result<String, String> {
    let v: Value = serde_json::from_str(&raw).map_err(|_| raw.clone())?;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
        Ok(text("text"))
    } else {
        let why = text("text");
        Err(if why.is_empty() { text("error") } else { why })
    }
}

fn cmd_name(c: &Command) -> &'static str {
    match c {
        Command::TapXy { .. } => "tap_xy",
        Command::Swipe { .. } => "swipe",
        Command::Key { .. } => "key",
        Command::Launch { .. } => "launch",
        Command::Open { .. } => "open",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        coalesce, is_loopback_origin, is_same_origin, parse_range, parse_rev, session_rev,
    };
    use super::{sandbox_page_url, sandbox_url_to_guest};

    /// 一个开着凭据、带一个真 rootfs 目录的核心：rootfs/workspace/site/index.html。
    async fn sandbox_core(
        tag: &str,
    ) -> (std::sync::Arc<crate::state::AppState>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("agent-http-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(root.join("workspace/site")).unwrap();
        std::fs::write(root.join("workspace/site/index.html"), "<h1>hi</h1>").unwrap();
        let st = crate::state::AppState::new();
        st.set_token(Some("s3cret".into()));
        st.set_linux(std::sync::Arc::new(crate::linux::LinuxSandbox::new(
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
                workspace: None,
                shared: None,
            },
        )));
        (st, root)
    }

    /// 手机系统能力的回包 → 纯文本：数据排成文字；失败写「错误：消息（错误码）」，
    /// 错误码留着（模型靠它分辨「用户拒了权限」和「参数错了」）。
    #[test]
    fn device_replies_render_as_plain_text() {
        let t = |v: serde_json::Value| crate::http::device_text(&v);
        assert_eq!(
            t(serde_json::json!({ "ok": true, "data": { "level": 82, "charging": true } })),
            "charging: true\nlevel: 82"
        );
        assert_eq!(
            t(
                serde_json::json!({ "ok": false, "error": { "code": "permission_denied", "message": "用户拒绝了短信权限" } })
            ),
            "错误：用户拒绝了短信权限（permission_denied）"
        );
        assert_eq!(t(serde_json::json!({ "ok": true, "data": null })), "完成");
        assert_eq!(
            t(serde_json::json!({ "ok": true, "data": "用法：agent sms list …" })),
            "用法：agent sms list …"
        );
    }

    /// 浏览器回包渲染成纯文本：正文原样，失败写「错误：」，页面 / 标签信息只在需要时补。
    #[test]
    fn browser_replies_render_as_plain_text() {
        let t = |v: serde_json::Value| crate::http::browser_text(&v);
        assert_eq!(
            t(
                serde_json::json!({ "ok": true, "text": "已打开 https://a.com/\n标题：A", "url": "https://a.com/", "title": "A" })
            ),
            "已打开 https://a.com/\n标题：A",
            "正文里已经有网址就不再补"
        );
        assert_eq!(
            t(
                serde_json::json!({ "ok": true, "text": "已点击 <a> 登录", "url": "https://a.com/x", "title": "X" })
            ),
            "已点击 <a> 登录\n[页面：X — https://a.com/x]"
        );
        assert_eq!(
            t(serde_json::json!({ "ok": false, "text": "找不到元素 #q" })),
            "错误：找不到元素 #q"
        );
        assert_eq!(
            t(serde_json::json!({ "ok": true, "text": "ok", "tab": 2, "tabs": 3 })),
            "ok\n[标签 #2，共 3 只]"
        );
        assert_eq!(
            t(serde_json::json!({ "ok": true, "text": "ok", "tab": 1, "tabs": 1 })),
            "ok",
            "单标签不啰嗦"
        );
        assert_eq!(
            t(serde_json::json!({ "text": "没有 ok 字段的原样文本" })),
            "没有 ok 字段的原样文本"
        );
    }

    /// 用户停用的 MCP 服务，agent 不能靠 `agent mcp add` 重新接一次把它打开（开关是用户的）。
    /// 在连网之前就拒，配置和开关都不动。
    #[tokio::test]
    async fn agent_cannot_reenable_a_disabled_mcp_server_by_adding_it_again() {
        let st = crate::state::AppState::new();
        st.mcp.set_servers(vec![crate::mcp::Server {
            name: "gh".into(),
            url: "https://example.invalid/mcp".into(),
            headers: Default::default(),
        }]);
        st.mcp.set_enabled("gh", false);
        let e = crate::http::mcp_add(&st, &["gh".into(), "https://other.invalid/mcp".into()])
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("停用"), "{e:#}");
        assert!(!st.mcp.is_enabled("gh"));
        assert_eq!(
            st.mcp.servers()[0].url,
            "https://example.invalid/mcp",
            "配置不动"
        );
    }

    /// `agent ps` 的后端：`/api/procs` 跟别的 `/api` 一样要凭据，`?format=text`
    /// 回人话（沙箱里的 `agent ps` 直接打它）。
    #[tokio::test]
    async fn procs_endpoint_needs_auth_and_speaks_plain_text() {
        let (st, _root) = sandbox_core("procs").await;
        let (base, c) = serve(st.clone()).await;

        let unauth = c.get(format!("{base}/api/procs")).send().await.unwrap();
        assert_eq!(unauth.status(), 401);

        let r = c
            .get(format!("{base}/api/procs?format=text"))
            .header("x-agent-token", "s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let ctype = r.headers()["content-type"].to_str().unwrap().to_string();
        assert!(ctype.starts_with("text/plain"), "{ctype}");
        let body = r.text().await.unwrap();
        // 新建的空沙箱没登记过任何运行：要么「没有别的东西在跑」，要么 /proc 看不见
        assert!(
            body.contains("没有别的东西在跑") || body.contains("看不见任何进程"),
            "{body}"
        );

        let v: serde_json::Value = c
            .get(format!("{base}/api/procs"))
            .header("x-agent-token", "s3cret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["ok"], true);
        assert!(v["leftovers"].as_array().unwrap().is_empty());
        assert!(v["running"].as_array().unwrap().is_empty());
    }

    /// `agent readable` 从头到尾走一遍：生成的脚本 → curl → `/api/readable` → 正文。
    /// 页面故意做到 2MB 以上（axum 默认的请求体上限），大页面也得收得下；
    /// 不是 UTF-8 的以「错误：」回来，脚本据此非 0 退出。
    #[tokio::test]
    async fn agent_readable_pipes_html_through_the_core() {
        let st = crate::state::AppState::new();
        st.set_token(Some("s3cret".into()));
        let (base, _) = serve(st).await;
        let dir = std::env::temp_dir().join(format!("agent-readable-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("agent");
        std::fs::write(&script, crate::cli::script(crate::cli::Machine::Host)).unwrap();

        let para =
            "这是正文里的一段话，长到足够让 Readability 认出这里才是文章，逗号也够多，打分才高。";
        let filler = format!(
            "<script>var x = '{}';</script>",
            "x".repeat(3 * 1024 * 1024)
        );
        let html = format!(
            "<html><head><title>大页面</title>{filler}</head><body><nav>首页 关于</nav>\
             <article><p>{para}</p><p>{para}第二段。</p><p>{para}第三段。</p></article></body></html>"
        );
        let run = |input: Vec<u8>| {
            let script = script.clone();
            let base = base.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut child = std::process::Command::new("sh")
                        .arg(&script)
                        .arg("readable")
                        .env("AGENT_BASE", &base)
                        .env("AGENT_TOKEN", "s3cret")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .unwrap();
                    child.stdin.take().unwrap().write_all(&input).unwrap();
                    child.wait_with_output().unwrap()
                })
                .await
                .unwrap()
            }
        };

        let out = run(html.into_bytes()).await;
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{text}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.starts_with("大页面\n\n"), "{text}");
        assert!(
            text.contains("第三段。") && !text.contains("首页"),
            "{text}"
        );

        let out = run(b"<meta charset=gbk>\xd6\xd0\xce\xc4".to_vec()).await;
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(1), "{text}");
        assert!(
            text.starts_with("错误：") && text.contains("iconv"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 两个源分开：`sandbox.localhost` 上只有沙箱文件，而且不关进 opaque origin
    /// （存储能用）；同一个文件从 `127.0.0.1` 取照旧带 CSP sandbox；
    /// 带着正确凭据也别想在沙箱那个源上开 `/api`、`/auth`、App 页面。
    #[tokio::test]
    async fn the_sandbox_origin_serves_only_sandbox_files_and_unsandboxed() {
        let (st, root) = sandbox_core("origin").await;
        let key = st.auth().sandbox_key;
        let (base, c) = serve(st.clone()).await;
        let port = base.rsplit(':').next().unwrap().to_string();
        let sandbox_host = format!("sandbox.localhost:{port}");
        let file = format!("{base}/sandbox/{key}/workspace/site/index.html");

        let r = c
            .get(&file)
            .header("host", &sandbox_host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(
            r.headers().get("content-security-policy").is_none(),
            "独立的源上不该再关进 opaque origin"
        );
        assert_eq!(r.text().await.unwrap(), "<h1>hi</h1>");

        let r = c.get(&file).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let csp = r.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            csp.starts_with("sandbox") && !csp.contains("allow-same-origin"),
            "{csp}"
        );

        for path in ["/api/health", "/auth?t=s3cret", "/"] {
            let r = c
                .get(format!("{base}{path}"))
                .header("host", &sandbox_host)
                .header("x-agent-token", "s3cret")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 404, "{path}");
        }
        // App 那边照常
        let r = c
            .get(format!("{base}/api/health"))
            .header("x-agent-token", "s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 浏览器要打开的地址：沙箱文件的几种写法都映射到独立的源上；网址不动。
    #[test]
    fn sandbox_paths_map_to_the_sandbox_origin_and_back() {
        let base = "http://sandbox.localhost:4000/sandbox/k";
        let map = |s: &str| sandbox_page_url(s, 4000, "k");
        assert_eq!(
            map("preview:/workspace/a.html").unwrap(),
            format!("{base}/workspace/a.html")
        );
        assert_eq!(
            map("preview:///workspace/a.html").unwrap(),
            format!("{base}/workspace/a.html")
        );
        assert_eq!(
            map("preview:site/a.html").unwrap(),
            format!("{base}/workspace/site/a.html")
        );
        assert_eq!(
            map("file:///root/x.html").unwrap(),
            format!("{base}/root/x.html")
        );
        assert_eq!(
            map("/workspace/a.html?q=1#top").unwrap(),
            format!("{base}/workspace/a.html?q=1#top")
        );
        assert_eq!(
            map("preview:/workspace/报告 1/index.html").unwrap(),
            format!("{base}/workspace/%E6%8A%A5%E5%91%8A%201/index.html")
        );
        assert_eq!(
            map("preview:/workspace/%E6%8A%A5.html").unwrap(),
            format!("{base}/workspace/%E6%8A%A5.html"),
            "已经编码过的不再编一次"
        );
        for web in [
            "https://example.com",
            "example.com/a",
            "//cdn.x/y.js",
            "http://127.0.0.1:1/",
        ] {
            assert!(map(web).is_none(), "{web}");
        }

        // 回来的结果改写回 `preview:`（端口和钥匙不进历史）
        let back = |s: &str| sandbox_url_to_guest(s, 4000, "k");
        assert_eq!(
            back(&format!(
                "已打开 {base}/workspace/%E6%8A%A5%E5%91%8A%201/index.html\n标题：x"
            )),
            "已打开 preview:/workspace/报告 1/index.html\n标题：x"
        );
        assert_eq!(back(&format!("{base}/")), "preview:/workspace/");
        assert_eq!(back("https://example.com/"), "https://example.com/");
        assert_eq!(
            back(&format!("{base}/workspace/a.html")),
            back(&back(&format!("{base}/workspace/a.html"))),
            "幂等"
        );
    }

    /// 积压的一批里，被后来者盖掉的阶段性输出扔掉；别的 chunk（正文、
    /// 最终输出、别的工具的快照）一个不少、顺序不变。
    #[test]
    fn coalesce_drops_only_superseded_live_output() {
        use crate::sse;
        let p = |id: &str, n: u32| sse::tool_output_preliminary(id, serde_json::json!(n));
        let batch = vec![
            p("a", 1),
            sse::text_delta("t", "x"),
            p("b", 1),
            p("a", 2),
            sse::tool_output_available("b", serde_json::json!("done")),
            p("a", 3),
        ];
        let out = coalesce(batch.clone());
        assert_eq!(
            out,
            vec![batch[1].clone(), batch[4].clone(), batch[5].clone()],
            "a 只留最后一份；b 的快照被它的最终输出盖掉"
        );
    }
    use crate::agent::{ChatMessage, ChatSession};

    /// 起一个真的核心服务，返回 (base, 不跟随跳转的 client)。
    async fn serve(st: std::sync::Arc<crate::state::AppState>) -> (String, reqwest::Client) {
        crate::llm::install_crypto_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, super::router(st)).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        (format!("http://{addr}"), client)
    }

    /// 假的 OpenAI 兼容服务：`/models` 只认 `Bearer good`，返回两个能聊的模型 + 一个 embedding。
    async fn fake_models_server() -> String {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(|h: axum::http::HeaderMap| async move {
                let ok =
                    h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good");
                if !ok {
                    return (axum::http::StatusCode::UNAUTHORIZED, "bad key").into_response();
                }
                axum::Json(serde_json::json!({ "data": [
                    { "id": "m-small" }, { "id": "m-big" }, { "id": "text-embedding-3-small" }
                ] }))
                .into_response()
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    use axum::response::IntoResponse;

    /// 设置页的流程：错的 Key 不存；第一次配好就切过去；清单只留能聊的；
    /// 删掉当前这家，当前 provider 跟着清空。
    #[tokio::test]
    async fn configuring_a_provider_validates_the_key_and_lists_its_models() {
        let st = crate::state::AppState::new();
        let upstream = fake_models_server().await;
        let (base, c) = serve(st.clone()).await;
        let post = |path: &str, body: serde_json::Value| {
            c.post(format!("{base}{path}")).json(&body).send()
        };

        let r = post(
            "/api/providers/custom",
            serde_json::json!({ "apiKey": "bad", "baseUrl": upstream }),
        )
        .await
        .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert!(r.text().await.unwrap().contains("API Key 不对"));
        assert!(
            st.provider_creds("custom").is_none(),
            "验证没过的 Key 不能存"
        );

        let r = post(
            "/api/providers/custom",
            serde_json::json!({ "apiKey": "good", "baseUrl": upstream }),
        )
        .await
        .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let p = st.llm().expect("第一次配好就该切过去，不用再选一次模型");
        assert_eq!((p.model.as_str(), p.api_key.as_str()), ("m-small", "good"));

        let list: serde_json::Value = c
            .get(format!("{base}/api/providers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let custom = list["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "custom")
            .unwrap()
            .clone();
        assert_eq!(custom["configured"], true);
        assert_eq!(custom["current"], true);
        assert_eq!(custom["keyHint"], "…good");
        assert!(
            custom.get("apiKey").is_none() && !list.to_string().contains("\"good\""),
            "不能把 Key 回给前端"
        );

        let m: serde_json::Value = c
            .get(format!("{base}/api/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let ids: Vec<&str> = m["groups"][0]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["m-small", "m-big"], "embedding 这种不能聊的要筛掉");
        assert_eq!(m["provider"], "custom");

        // 没配过的那家：不能切过去
        let r = post(
            "/api/model",
            serde_json::json!({ "model": "gpt-5", "provider": "openai" }),
        )
        .await
        .unwrap();
        assert_eq!(r.status().as_u16(), 400);

        let r = post(
            "/api/model",
            serde_json::json!({ "model": "m-big", "provider": "custom" }),
        )
        .await
        .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(st.llm().unwrap().model, "m-big");

        let r = c
            .delete(format!("{base}/api/providers/custom"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(st.llm().is_none(), "一家都不剩了，当前 provider 也没了");
    }

    /// 手机上别的 App 能连 127.0.0.1：没凭据的请求一律挡在 `/api` 外面。
    #[tokio::test]
    async fn the_api_needs_a_token_and_the_app_gets_it_as_a_cookie() {
        let st = crate::state::AppState::new();
        st.set_token(Some("s3cret".into()));
        let key = st.auth().sandbox_key;
        let (base, c) = serve(st).await;

        let status = |r: reqwest::Response| r.status().as_u16();
        assert_eq!(
            status(c.get(format!("{base}/api/health")).send().await.unwrap()),
            401
        );
        assert_eq!(
            status(
                c.post(format!("{base}/api/linux"))
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
            ),
            401,
            "跑命令的接口尤其不能裸奔"
        );
        assert_eq!(
            status(
                c.get(format!("{base}/api/health"))
                    .header("x-agent-token", "wrong")
                    .send()
                    .await
                    .unwrap()
            ),
            401
        );

        // 沙箱里的 agent 命令 / 调试脚本：带头
        let h: serde_json::Value = c
            .get(format!("{base}/api/health"))
            .header("x-agent-token", "s3cret")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(h["sandboxKey"], key.as_str());
        // 「现在连的是哪台机器」：桌面上就是这台机器的主机名 ——
        // 名字和 mDNS 广播 / 配对码那两处取的是同一个（`desktop::computer_name`），
        // 界面上显示的和手机搜到的那台得对得上
        assert_eq!(h["computer"], crate::desktop::computer_name().as_str());

        // App 的 WebView：/auth 种 cookie，再跳回首页
        assert_eq!(
            status(c.get(format!("{base}/auth?t=nope")).send().await.unwrap()),
            403
        );
        let r = c.get(format!("{base}/auth?t=s3cret")).send().await.unwrap();
        assert!(r.status().is_redirection());
        assert_eq!(r.headers()["location"], "/", "地址栏里不该留着 token");
        let cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
        assert!(cookie.starts_with("agent_token=s3cret;"), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "页面里的 JS 不该读得到它");
        let pair = cookie.split(';').next().unwrap();
        assert_eq!(
            status(
                c.get(format!("{base}/api/sessions"))
                    .header("cookie", pair)
                    .send()
                    .await
                    .unwrap()
            ),
            200
        );

        // 前端的静态资源不设防
        assert_ne!(status(c.get(format!("{base}/")).send().await.unwrap()), 401);

        // 沙箱文件看路径上的钥匙：错的就是没有（这里没起沙箱，对的钥匙会走到「沙箱没起来」）
        assert_eq!(
            status(
                c.get(format!("{base}/sandbox/wrong/workspace/a.txt"))
                    .send()
                    .await
                    .unwrap()
            ),
            404
        );
        assert_eq!(
            status(
                c.get(format!("{base}/sandbox/{key}/workspace/a.txt"))
                    .send()
                    .await
                    .unwrap()
            ),
            503
        );
    }

    /// 不同会话可以各跑一个回合；同一个会话跑着时，新消息进**队列**（不是 409）。
    #[tokio::test]
    async fn a_busy_session_queues_the_second_message_but_others_do_not_care() {
        let st = crate::state::AppState::new();
        *st.llm.write().unwrap() = Some(crate::llm::Provider::compatible(
            "x",
            "http://127.0.0.1:9",
            "m",
            "k",
        ));
        let (a, b) = {
            let mut store = st.chat.write().await;
            (store.create(), store.create())
        };
        let _running = st.begin_turn(&a).unwrap();
        let (base, c) = serve(st.clone()).await;

        // a 正跑着：这条消息排进服务端的队列（回一条只有 data-queued 的流），
        // 不是 409 —— 前端那个队列活在 WebView 里，App 切后台就发不出去了
        let r = c
            .post(format!("{base}/api/chat"))
            .json(&serde_json::json!({ "text": "再来", "session": a, "id": "q1" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body = r.text().await.unwrap();
        assert!(body.contains("data-queued"), "得告诉界面这条排上了：{body}");
        assert!(!body.contains("data-error"), "{body}");
        let q = st.queued_list(&a);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].id, "q1", "客户端给的 id 得原样留着（界面靠它撤）");
        assert_eq!(q[0].text, "再来");

        // 会话里的排队清单（界面画「排队中」那些气泡）：形状就是一条用户消息
        let v: serde_json::Value = c
            .get(format!("{base}/api/session?id={a}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["queued"][0]["id"], "q1");
        assert_eq!(v["queued"][0]["role"], "user");
        assert_eq!(v["queued"][0]["done"], false);
        assert_eq!(v["queued"][0]["parts"][0]["text"], "再来");

        // 撤掉它（气泡左边那个 ×）
        let v: serde_json::Value = c
            .post(format!("{base}/api/sessions/unqueue"))
            .json(&serde_json::json!({ "session": a, "id": "q1" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["removed"], true);
        assert!(st.queued_list(&a).is_empty());
        // 再撤一次就没得撤了（不是错误）
        let v: serde_json::Value = c
            .post(format!("{base}/api/sessions/unqueue"))
            .json(&serde_json::json!({ "session": a, "id": "q1" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["removed"], false);

        // b 不受 a 影响：能开始（provider 是个连不上的地址，回合会以报错结束）
        let r = c
            .post(format!("{base}/api/chat"))
            .json(&serde_json::json!({ "text": "你好", "session": b }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let _ = r.text().await; // 等这个回合结束
        assert!(st.is_busy(&a), "b 的回合结束不能把 a 的「进行中」清掉");
        assert!(!st.is_busy(&b));

        // 停 b（没在跑）不影响 a
        let r: serde_json::Value = c
            .post(format!("{base}/api/stop"))
            .json(&serde_json::json!({ "session": b }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["running"], false);
        assert!(!_running.abort_requested());
    }

    #[test]
    fn range_header_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(
            parse_range("bytes=990-5000", 1000),
            Some((990, 999)),
            "结尾越界要截到文件末尾"
        );
        assert_eq!(parse_range("bytes=1000-", 1000), None, "起点越界不满足");
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), None, "多段不支持");
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    /// 版本号必须能反映「会话内容变了」，而且不同会话不能撞车。
    ///
    /// 它错了的后果很隐蔽：客户端发来的 rev 一旦与服务端算出的相同，
    /// 就会收到 `unchanged`、界面永远停在一份旧消息上。
    #[test]
    fn session_rev_tracks_content_and_identity() {
        let mut a = ChatSession::new();
        let r0 = session_rev(&a);

        // 新消息 → 变
        a.messages.push(ChatMessage::user("问"));
        let r1 = session_rev(&a);
        assert_ne!(r0, r1);

        // 内容不变 → 不变（轮询时靠这个省掉整段历史的传输）
        assert_eq!(r1, session_rev(&a));

        // 换个会话，即使消息数一样也不能相同（否则会卡在别的会话的内容上）
        let mut b = ChatSession::new();
        b.messages.push(ChatMessage::user("问"));
        b.updated_at = a.updated_at;
        assert_ne!(session_rev(&b), session_rev(&a));
    }

    #[test]
    fn parse_rev_reads_the_session_id_and_message_count() {
        assert_eq!(parse_rev("abc:7:123"), Some(("abc", 7)));
        // 空 / 残缺 / 非法 → None（当作全量拉）
        assert_eq!(parse_rev(""), None);
        assert_eq!(parse_rev("abc"), None);
        assert_eq!(parse_rev(":7:123"), None);
        assert_eq!(parse_rev("abc:x:123"), None);
    }

    #[test]
    fn only_loopback_origins_may_open_the_websocket() {
        for ok in [
            "http://127.0.0.1:41209",
            "http://localhost:5173",
            "http://[::1]:8787",
            "http://127.0.0.1",
        ] {
            assert!(is_loopback_origin(ok), "{ok}");
        }
        for bad in [
            "null",
            "https://evil.example",
            "http://127.0.0.1.evil.example",
            "http://192.168.0.2:41209",
            "",
        ] {
            assert!(!is_loopback_origin(bad), "{bad}");
        }
        assert!(
            !is_loopback_origin("http://sandbox.localhost:4000"),
            "沙箱那个源不算本机来源"
        );
        // 局域网里用手机浏览器开电脑上的核心：同源放行，别的源不放
        assert!(is_same_origin(
            "http://192.168.0.5:8787",
            "192.168.0.5:8787"
        ));
        assert!(!is_same_origin(
            "http://evil.example:8787",
            "192.168.0.5:8787"
        ));
        assert!(!is_same_origin("null", "192.168.0.5:8787"));
        assert!(!is_same_origin("http://192.168.0.5:8787", ""));
        assert!(
            !is_same_origin("http://sandbox.localhost:8787", "127.0.0.1:8787"),
            "沙箱网页的源不能连 WebSocket"
        );
    }

    /// 设备通道的 `open` 指令：沙箱里的 `agent-open` / `$BROWSER` / `xdg-open`
    /// 最终都走这里（`agent open` → `POST /api/command`）。
    #[tokio::test]
    async fn device_open_uri_takes_any_scheme() {
        let st = crate::state::AppState::new();
        st.set_device(crate::mock::MockDevice::new());
        let (base, c) = serve(st.clone()).await;

        // **不设 scheme 白名单**：https、App 深链、intent://、tel: 都直接转下去
        for uri in [
            "https://github.com/login/device",
            "androidamap://navi?sourceApplication=agent&lat=39.9&lon=116.3",
            "weixin://",
            "intent://scan/#Intent;scheme=zxing;package=com.google.zxing.client.android;end",
            "tel:10086",
        ] {
            let r = c
                .post(format!("{base}/api/command"))
                .json(&serde_json::json!({ "cmd": { "type": "open", "url": uri } }))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 200, "{uri}");
            let v: serde_json::Value = r.json().await.unwrap();
            assert_eq!(v["ok"], true, "{uri}: {v}");
            assert!(
                v["detail"].as_str().unwrap_or("").contains("opened"),
                "{uri}: {v}"
            );
        }

        // 没有设备通道（桌面、或无障碍没连上）：说实话，而不是假装开了
        let (base2, c2) = serve(crate::state::AppState::new()).await;
        let v: serde_json::Value = c2
            .post(format!("{base2}/api/command"))
            .json(&serde_json::json!({ "cmd": { "type": "open", "url": "https://x.example" } }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["ok"], false);
        assert!(
            v["detail"].as_str().unwrap_or("").contains("没有设备"),
            "{v}"
        );
    }

    /// 记下收到的浏览器动作，按 `__open_uri` 的约定回话。
    struct RecordingBrowser(std::sync::Mutex<Vec<String>>, bool);

    impl crate::state::BrowserApi for RecordingBrowser {
        fn execute(&self, action_json: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(action_json.to_string());
            Ok(if self.1 {
                r#"{"ok":true,"text":"opened x"}"#.into()
            } else {
                r#"{"ok":false,"text":"App 在后台"}"#.into()
            })
        }
    }

    /// 打开 URI 不该依赖无障碍：没有设备通道时交给浏览器那条（`__open_uri`），
    /// 那边的失败原因原样带回来。点击之类真要无障碍的指令照旧报「没有设备」。
    #[tokio::test]
    async fn open_falls_back_to_the_browser_host_without_accessibility() {
        let st = crate::state::AppState::new();
        let b = std::sync::Arc::new(RecordingBrowser(Default::default(), true));
        st.set_browser(b.clone());
        let (base, c) = serve(st.clone()).await;
        let cmd = |cmd: serde_json::Value| {
            c.post(format!("{base}/api/command"))
                .json(&serde_json::json!({ "cmd": cmd }))
                .send()
        };

        let v: serde_json::Value = cmd(serde_json::json!({ "type": "open", "url": "weixin://" }))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["detail"], "opened x");
        let sent: serde_json::Value = serde_json::from_str(&b.0.lock().unwrap()[0]).unwrap();
        assert_eq!(sent["action"], "__open_uri");
        assert_eq!(sent["url"], "weixin://");

        let v: serde_json::Value = cmd(serde_json::json!({ "type": "tap_xy", "x": 1, "y": 2 }))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["detail"].as_str().unwrap().contains("没有设备"), "{v}");
        assert_eq!(b.0.lock().unwrap().len(), 1, "点击不该落到浏览器上");

        let st2 = crate::state::AppState::new();
        st2.set_browser(std::sync::Arc::new(RecordingBrowser(
            Default::default(),
            false,
        )));
        let (base2, c2) = serve(st2).await;
        let v: serde_json::Value = c2
            .post(format!("{base2}/api/command"))
            .json(&serde_json::json!({ "cmd": { "type": "open", "url": "https://x.example" } }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["detail"], "App 在后台");
    }
}
