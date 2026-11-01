//! 前端静态资源。Vite 构建产物在编译期嵌入 .so（决策 D3）。
//!
//! **debug 构建是例外**：直接从磁盘读 `web/dist`。
//!
//! 理由：嵌入是编译期的，改一行前端就要重编整个 core（`rust-embed` 的 derive
//! 甚至会在 dist 缺失时让编译失败）。开发时前端改得比 Rust 勤得多，那条路走不通。
//! 发版（release / release-thin）照旧嵌入，`.so` 自带一份完整前端。

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;

#[cfg(not(debug_assertions))]
#[derive(RustEmbed)]
#[folder = "../web/dist"]
pub struct WebAssets;

/// 取一个静态文件：release 从嵌入的表里拿（`Cow::Borrowed`，零拷贝），
/// debug 现读磁盘（`Cow::Owned`）。
#[cfg(not(debug_assertions))]
fn get(path: &str) -> Option<Cow<'static, [u8]>> {
    WebAssets::get(path).map(|f| f.data)
}

#[cfg(debug_assertions)]
fn get(path: &str) -> Option<Cow<'static, [u8]>> {
    // 按 crate 根算，不是进程 cwd —— 直接起 `./target/debug/agent-dev` 时
    // cwd 不一定是仓库根。
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../web/dist")
        .join(path);
    std::fs::read(p).ok().map(Cow::Owned)
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(data) = get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        // Vite 产物文件名带内容哈希，可以永久缓存；index.html 每次都要验证，
        // 否则换了新版 .so 还会加载旧页面。没有这个头 WebView 每次启动都
        // 重新拉一遍 JS/CSS。
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache),
            ],
            body(data),
        )
            .into_response();
    }

    // SPA fallback：非 API 路径一律回 index.html
    match get("index.html") {
        Some(data) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body(data),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "web/dist 是空的 —— 先跑 `just web build`",
        )
            .into_response(),
    }
}

/// release 构建里资源是编进 .so 的静态切片，直接引用，不拷贝。
fn body(data: Cow<'static, [u8]>) -> Body {
    match data {
        Cow::Borrowed(b) => Body::from(b),
        Cow::Owned(v) => Body::from(v),
    }
}
