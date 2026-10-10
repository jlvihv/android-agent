//! 前端静态资源。Vite 构建产物在编译期嵌入 .so（决策 D3）。

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
        return (
            [(header::CONTENT_TYPE, mime.as_ref())],
            Body::from(f.data.into_owned()),
        )
            .into_response();
    }

    // SPA fallback：非 API 路径一律回 index.html
    match WebAssets::get("index.html") {
        Some(f) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from(f.data.into_owned()),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "web/dist 是空的 —— 先跑 `just web-build`",
        )
            .into_response(),
    }
}
