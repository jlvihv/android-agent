//! 前端静态资源。Vite 构建产物在编译期嵌入 .so（决策 D3）。

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../web/dist"]
pub struct WebAssets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(f) = WebAssets::get(path) {
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
            body(f.data),
        )
            .into_response();
    }

    // SPA fallback：非 API 路径一律回 index.html
    match WebAssets::get("index.html") {
        Some(f) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            body(f.data),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "web/dist 是空的 —— 先跑 `just web-build`",
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
