//! HTTP / SSE / WebSocket 服务。
//!
//! 前端与 API **同源**（决策 D3），所以不需要 CORS，且 `http://127.0.0.1`
//! 是 secure context —— `crypto.subtle`、Service Worker 都能用。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
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
        .route("/api/approve", post(approve))
        .route("/api/session", get(get_session))
        .route("/api/sessions", get(list_sessions).post(new_session))
        .route("/api/sessions/activate", post(activate_session))
        .route("/api/sessions/rename", post(rename_session))
        .route("/api/sessions/delete", post(delete_session))
        .route("/api/reset", post(reset))
        .route("/api/device/snapshot", post(device_snapshot))
        .route("/api/device/log", post(device_log))
        .route("/api/ws", get(ws_upgrade))
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
        "autoApprove": st.auto_approve(),
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApproveBody {
    approval_id: String,
    approved: bool,
}

/// `POST /api/chat` → AI SDK UI Message Stream。
///
/// 配了 provider 就跑真实 agent loop（`agent.rs`），否则回退到脚本演示流。
/// 两种情况下**输出格式完全一致**，所以前端不需要知道区别。
async fn chat(State(st): State<Arc<AppState>>, Json(body): Json<ChatBody>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
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
                    let _ = tx.send(sse::error(&format!("{e:#}"))).await;
                }
                // tx 在这里 drop → 转发任务会补上 [DONE]
            });
        }
        None => {
            st.log("debug", format!("chat (demo): {}", body.text));
            tokio::spawn(async move {
                for chunk in crate::demo::scripted(&body.text) {
                    if tx.send(chunk).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(35)).await;
                }
            });
        }
    }

    sse_response(rx)
}

/// `POST /api/approve` → 继续被挂起的回合。
async fn approve(State(st): State<Arc<AppState>>, Json(body): Json<ApproveBody>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
    let core = Core {
        state: st.clone(),
    };

    let Some(p) = st.llm().or_else(Provider::from_env) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "未配置 LLM provider（需要 OPENCODE_API_KEY 或 AGENT_LLM_API_KEY）",
        )
            .into_response();
    };

    let approval_id = body.approval_id.clone();
    let approved = body.approved;
    st.log(
        "info",
        format!("approve {approval_id} → {approved}"),
    );

    tokio::spawn(async move {
        let txc = tx.clone();
        if let Err(e) =
            crate::agent::resolve_approval(&core, &p, &approval_id, approved, txc).await
        {
            let _ = tx.send(sse::error(&format!("{e:#}"))).await;
        }
    });

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
    Json(json!({ "messages": msgs }))
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
    if store.activate(&body.id) {
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
    Json(json!({ "ok": true })).into_response()
}

/// `POST /api/reset` → 清空会话历史。
async fn reset(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    let mut store = st.chat.write().await;
    store.active_mut().clear();
    st.log("info", "会话已清空");
    StatusCode::NO_CONTENT
}

/// 把 chunk 通道包成带正确 header 的 SSE 响应，并在结束时补 `[DONE]`。
fn sse_response(mut rx: tokio::sync::mpsc::Receiver<serde_json::Value>) -> Response {
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

async fn ws_upgrade(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_loop(socket, st))
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
