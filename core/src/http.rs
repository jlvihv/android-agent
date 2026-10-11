//! HTTP / SSE / WebSocket 服务。
//!
//! 前端与 API **同源**（决策 D3），所以不需要 CORS，且 `http://127.0.0.1`
//! 是 secure context —— `crypto.subtle`、Service Worker 都能用。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

use crate::llm::Provider;
use crate::sse;
use crate::state::{AppState, Command, Event, UiSnapshot, VERSION};
use crate::Core;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/snapshot", get(get_snapshot))
        .route("/api/command", post(post_command))
        .route("/api/linux", post(bash_run))
        .route("/api/edit", post(edit_file))
        .route("/api/llm", post(set_llm))
        .route("/api/chat", post(chat))
        .route("/api/session", get(get_session))
        .route("/api/sessions", get(list_sessions).post(new_session))
        .route("/api/sessions/activate", post(activate_session))
        .route("/api/sessions/rename", post(rename_session))
        .route("/api/sessions/delete", post(delete_session))
        .route("/api/reset", post(reset))
        .route("/api/device/snapshot", post(device_snapshot))
        .route("/api/device/log", post(device_log))
        .route("/api/ws", get(ws_upgrade))
        .route("/sandbox", get(sandbox_root))
        .route("/sandbox/", get(sandbox_root))
        .route("/sandbox/{*path}", get(sandbox_file))
        .fallback(crate::assets::serve)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// REST
// ---------------------------------------------------------------------------

async fn health(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let dev = st.device();
    // 优先运行时配置，回退环境变量（桌面）
    let provider = st.llm().or_else(Provider::from_env);
    Json(json!({
        "ok": true,
        "version": VERSION,
        "port": st.port.load(std::sync::atomic::Ordering::Relaxed),
        "platform": std::env::consts::OS,
        "mock": st.mock.load(std::sync::atomic::Ordering::Relaxed),
        "device": dev.map(|d| d.name()),
        "hasSnapshot": st.latest_snapshot().is_some(),
        "linux": st.linux().map(|s| json!({
            "rootfs": s.rootfs().display().to_string(),
        })),
        // 只报“配没配”，不泄 key
        "llm": provider.map(|p| json!({
            "name": p.name,
            "model": p.model,
            "baseUrl": p.base_url,
        })),
    }))
}

async fn get_snapshot(State(st): State<Arc<AppState>>) -> Response {
    match st.latest_snapshot() {
        Some(s) => Json(s).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

#[derive(Deserialize)]
struct CommandBody {
    cmd: Command,
}

/// 前端 → Rust → 设备。这是"人按按钮"的通路。
async fn post_command(
    State(st): State<Arc<AppState>>,
    Json(body): Json<CommandBody>,
) -> impl IntoResponse {
    let ack = dispatch(&st, body.cmd).await;
    Json(ack)
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
        .unwrap_or(crate::linux::DEFAULT_TIMEOUT);

    match core.run_linux(&body.command, timeout).await {
        Ok(r) => Json(json!({
            "ok": r.code == 0,
            "output": r.to_text(),
            "exit_code": r.code,
            "truncated": r.truncated,
            "full_output_path": r.full_output_path,
            "timed_out": r.timed_out,
            "wall_time_seconds": (r.duration_ms as f64) / 1000.0,
        }))
        .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")).into_response(),
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

/// 设备（Kotlin）→ Rust：推送新快照。
async fn device_snapshot(
    State(st): State<Arc<AppState>>,
    body: String,
) -> Response {
    match store_snapshot(&st, &body) {
        Ok(s) => Json(json!({ "ok": true, "nodes": s.nodes.len() })).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
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

async fn device_log(State(st): State<Arc<AppState>>, Json(body): Json<LogBody>) -> impl IntoResponse {
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
}

/// `POST /api/chat` → AI SDK UI Message Stream。
///
/// 配了 provider 就跑真实 agent loop（`agent.rs`），否则回退到脚本演示流。
/// 两种情况下**输出格式完全一致**，所以前端不需要知道区别。
async fn chat(State(st): State<Arc<AppState>>, Json(body): Json<ChatBody>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let core = Core {
        state: st.clone(),
    };

    // 运行时配置优先（安卓），回退环境变量（桌面）
    let provider = if body.demo {
        None
    } else {
        st.llm().or_else(Provider::from_env)
    };

    match provider {
        Some(p) => {
            st.log(
                "info",
                format!("chat → {} / {}", p.name, p.model),
            );
            tokio::spawn(async move {
                let txc = tx.clone();
                if let Err(e) = crate::agent::run_turn(&core, &p, &body.text, txc).await {
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
}

fn default_name() -> String {
    "custom".into()
}

/// `POST /api/llm` → 运行时配置 provider。
///
/// 安卓上没法用环境变量，用户也没别的地方填 key，所以需要这个接口。
/// 会把配置写到 `<filesDir>/llm.json`。
async fn set_llm(State(st): State<Arc<AppState>>, Json(body): Json<LlmBody>) -> Response {
    if body.api_key.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "apiKey 不能为空").into_response();
    }
    if body.base_url.trim().is_empty() || body.model.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "baseUrl 和 model 都不能为空").into_response();
    }

    let p = Provider::compatible(
        body.name,
        body.base_url.trim_end_matches('/'),
        body.model,
        body.api_key,
    );
    let dir = st.data_dir.read().ok().and_then(|g| g.clone());
    st.set_llm(p, dir.as_deref());
    // 换了 provider / model，缓存前缀的语义就变了：给所有会话换一个
    // provider session id，否则 provider 侧会把新前缀路由到旧模型的缓存上。
    st.rotate_provider_sessions().await;

    Json(json!({ "ok": true })).into_response()
}

/// `GET /api/session` → 服务端的会话历史，转成前端 `Message[]` 的形状。
///
/// 前端刷新/重建后用它恢复对话（否则消息只活在 JS 内存里）。
async fn get_session(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let store = st.chat.read().await;
    let msgs = store
        .active_ref()
        .map(crate::agent::session_view)
        .unwrap_or_default();
    drop(store);
    // busy 让前端知道「服务端还在跑」—— App 切到后台时 agent 不会停，
    // 切回来要能看出还在进行中，而不是以为卡住了。
    Json(json!({
        "messages": msgs,
        "busy": st.turn_active.load(std::sync::atomic::Ordering::Relaxed),
    }))
}

/// `GET /api/sessions` → 侧边栏的「最近对话」列表
async fn list_sessions(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let store = st.chat.read().await;
    Json(json!({ "sessions": store.list() }))
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
async fn rename_session(
    State(st): State<Arc<AppState>>,
    Json(body): Json<RenameBody>,
) -> Response {
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
    let mut store = st.chat.write().await;
    store.delete(&body.id);
    drop(store);
    st.mark_sessions_dirty();
    Json(json!({ "ok": true })).into_response()
}

/// `POST /api/reset` → 清空会话历史。
async fn reset(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut store = st.chat.write().await;
    store.active_mut().clear();
    drop(store);
    st.mark_sessions_dirty();
    st.log("info", "会话已清空");
    StatusCode::NO_CONTENT
}

/// 把 chunk 通道包成带正确 header 的 SSE 响应，并在结束时补 `[DONE]`。
///
/// 上游是无界通道（见 agent.rs 的 `send`）：前端读得慢只会让 chunk 在这里
/// 排队，不会反压到 agent 循环。
fn sse_response(mut rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>) -> Response {
    let (out_tx, out_rx) =
        tokio::sync::mpsc::channel::<Result<SseEvent, std::convert::Infallible>>(64);

    tokio::spawn(async move {
        while let Some(v) = rx.recv().await {
            if out_tx
                .send(Ok(SseEvent::default().data(v.to_string())))
                .await
                .is_err()
            {
                return; // 客户端断开
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
    // Vite 开发服务器）和不带 Origin 的非浏览器客户端。沙箱里的 HTML 是
    // opaque origin（Origin: null），也挡在外面。
    if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
        if !is_loopback_origin(origin.to_str().unwrap_or("")) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }
    ws.on_upgrade(move |socket| ws_loop(socket, st))
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
// 沙箱文件：`sandbox:` 协议的落点
// ---------------------------------------------------------------------------
//
// AI 在回复里写 `[报告](sandbox:/root/report.html)` 或
// `![图](sandbox:/root/chart.png)`，前端把它改写成 `/sandbox/root/…`
// 打到这里。按 URL 路径直接映射，所以 HTML 里的相对路径（css/js/图片）
// 天然能用。
//
// 安全：这些文件是 AI 写的（内容可能来自网上），却和 App 同源。所以
// 一律加 `Content-Security-Policy: sandbox` —— 页面变成 opaque origin，
// 读不到 /api/*、连不上 WebSocket（见 ws_upgrade 的 Origin 检查），
// 只能看。路径越界和符号链接逃逸由 LinuxSandbox::resolve_path 挡。

async fn sandbox_root(State(st): State<Arc<AppState>>) -> Response {
    serve_sandbox(&st, "/workspace/", true, None).await
}

async fn sandbox_file(
    State(st): State<Arc<AppState>>,
    UrlPath(path): UrlPath<String>,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> Response {
    // 目录要带结尾斜杠，否则页面里的相对链接会解析到上一级
    let trailing = uri.path().ends_with('/');
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    serve_sandbox(&st, &format!("/{path}"), trailing, range).await
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
    h.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
}

async fn serve_sandbox(
    st: &Arc<AppState>,
    guest: &str,
    trailing: bool,
    range: Option<String>,
) -> Response {
    let Some(sb) = st.linux() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Linux 沙箱未初始化").into_response();
    };
    let host = match sb.resolve_path(guest) {
        Ok(p) => p,
        Err(e) => return (StatusCode::FORBIDDEN, format!("{e:#}")).into_response(),
    };

    let meta = match tokio::fs::metadata(&host).await {
        Ok(m) => m,
        Err(_) => return (StatusCode::NOT_FOUND, format!("沙箱里没有 {guest}")).into_response(),
    };

    if meta.is_dir() {
        if !trailing {
            let to = format!("/sandbox{}/", guest.trim_end_matches('/'));
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

    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let mime = if mime.type_() == "text" || mime.essence_str() == "application/javascript" {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };

    let span = match range.as_deref().map(|r| parse_range(r, len)) {
        Some(Some(s)) => Some(s),
        Some(None) => {
            let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                res.headers_mut().insert(axum::http::header::CONTENT_RANGE, v);
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
    h.insert(axum::http::header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
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
         <title>{t}</title><style>body{{font:15px system-ui;background:#151515;color:#f9f9f7;\
         margin:0;padding:12px 16px}}a{{color:inherit;text-decoration:none;display:block;\
         padding:10px 0;border-bottom:1px solid #262624}}.d{{color:#97958d}}</style>\
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

    // 补发当前快照，避免前端刚连上时空白
    if let Some(snap) = st.latest_snapshot() {
        let _ = send_event(&mut socket, &Event::Snapshot { data: snap }).await;
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
    /// 让设备重新读一次 UI 树
    Refresh,
    /// 转发一条指令给设备
    Command { cmd: Command },
    /// 前端发起的日志（调试用）
    Log { level: String, msg: String },
}

async fn handle_client_msg(st: &Arc<AppState>, raw: &str) -> anyhow::Result<()> {
    let msg: ClientMsg = serde_json::from_str(raw)?;
    match msg {
        ClientMsg::Refresh => {
            dispatch(st, Command::Snapshot).await;
        }
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

/// 存快照并广播。`body` 是 `UiSnapshot` 的 JSON。
pub fn store_snapshot(st: &Arc<AppState>, body: &str) -> anyhow::Result<UiSnapshot> {
    let snap: UiSnapshot = serde_json::from_str(body)?;
    if let Ok(mut g) = st.snapshot.write() {
        *g = Some(snap.clone());
    }
    st.emit(Event::Snapshot { data: snap.clone() });
    Ok(snap)
}

/// 把指令发给设备执行。设备实现可能阻塞，所以走 `spawn_blocking`。
pub async fn dispatch(st: &Arc<AppState>, cmd: Command) -> serde_json::Value {
    let id = uuid::Uuid::now_v7().to_string();
    let Some(dev) = st.device() else {
        return json!({ "id": id, "ok": false, "detail": "没有设备实现（mock 未启用或无障碍服务未连接）" });
    };

    let cmd_json = match serde_json::to_string(&cmd) {
        Ok(s) => s,
        Err(e) => return json!({ "id": id, "ok": false, "detail": format!("序列化失败: {e}") }),
    };
    let kind = cmd_name(&cmd);

    let res = tokio::task::spawn_blocking(move || dev.execute(&cmd_json)).await;

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

fn cmd_name(c: &Command) -> &'static str {
    match c {
        Command::Snapshot => "snapshot",
        Command::TapNode { .. } => "tap_node",
        Command::TapXy { .. } => "tap_xy",
        Command::Swipe { .. } => "swipe",
        Command::SetText { .. } => "set_text",
        Command::Key { .. } => "key",
        Command::Launch { .. } => "launch",
    }
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_origin, parse_range};

    #[test]
    fn range_header_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=990-5000", 1000), Some((990, 999)), "结尾越界要截到文件末尾");
        assert_eq!(parse_range("bytes=1000-", 1000), None, "起点越界不满足");
        assert_eq!(parse_range("bytes=0-1,5-9", 1000), None, "多段不支持");
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    #[test]
    fn only_loopback_origins_may_open_the_websocket() {
        for ok in ["http://127.0.0.1:41209", "http://localhost:5173", "http://[::1]:8787", "http://127.0.0.1"] {
            assert!(is_loopback_origin(ok), "{ok}");
        }
        for bad in ["null", "https://evil.example", "http://127.0.0.1.evil.example", "http://192.168.0.2:41209", ""] {
            assert!(!is_loopback_origin(bad), "{bad}");
        }
    }
}
