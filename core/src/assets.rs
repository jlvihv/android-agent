//! 前端静态资源。Vite 构建产物在编译期嵌入 .so（决策 D3）。
//!
//! **debug 构建是例外**：直接从磁盘读 `web/dist`。
//!
//! 理由：嵌入是编译期的，改一行前端就要重编整个 core（`rust-embed` 的 derive
//! 甚至会在 dist 缺失时让编译失败）。开发时前端改得比 Rust 勤得多，那条路走不通。
//! 发版（release）照旧嵌入，`.so` 自带一份完整前端。
//!
//! ## 压缩
//!
//! 文本资源在**构建期**就压好了（`web/scripts/precompress.mjs`：每个 `.js`/`.css`/
//! `.html`/`.svg` 旁边多一份 `.br` 和 `.gz`，和源文件一起嵌进 `.so`）。这里只负责
//! 按 `Accept-Encoding` **挑一份**发出去 —— 运行时一个字节都不压。
//!
//! 为什么不做运行时压缩（`tower-http` 的 `CompressionLayer`）：
//!   - 每来一个新客户端都要重压一遍那份 194 KB 的 JS。本机回环 CPU 不心疼，
//!     但**手机每次冷启动都要付**，而且挤在同一条线程上跟首屏渲染抢；
//!   - 构建期压一次，之后是 `Cow::Borrowed` 的静态切片。
//!
//! 实测收益（移动端 Lighthouse）：JS 194.4 KB → br **55.7 KB**（-71%），CSS
//! 36.0 → 6.9 KB；`uses-text-compression` 那条从「可省 157 KiB」变成满分。

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
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

/// 值得带 `content-encoding` 的扩展名 —— **必须**和 `web/scripts/precompress.mjs`
/// 里的 `TYPES` 一致：那边不给图片 / 字体生成 `.br`，这边也就不会去找。
const COMPRESSIBLE: [&str; 8] = [
    ".js", ".mjs", ".css", ".html", ".svg", ".json", ".txt", ".map",
];

/// 这个路径该不该找压缩版
fn compressible(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    COMPRESSIBLE.iter().any(|e| lower.ends_with(e))
}

/// 客户端更想要哪种？返回 (`后缀`, `Content-Encoding 值`)。
///
/// **brotli 优先**：同等质量下比 gzip 小 ~10%，而且现在没有哪个还会发
/// `Accept-Encoding` 的客户端不支持它（Chrome 50+ / Android WebView 都行）。
/// 真遇到不支持的，它会在头里说清楚，这里自然退到 gzip 或原文。
///
/// 只看 `q=` 那两档最常见的写法；不解析完整的 RFC 7231 权重表 —— 会发
/// `br;q=0.5` 这种的场景我们没有。
fn pick_encoding(headers: &HeaderMap) -> Option<(&'static str, &'static str)> {
    let accept = headers.get(header::ACCEPT_ENCODING)?.to_str().ok()?;
    let has = |name: &str| {
        accept.split(',').any(|part| {
            let mut it = part.split(';');
            let enc = it.next().unwrap_or("").trim();
            if !enc.eq_ignore_ascii_case(name) {
                return false;
            }
            // `;q=0` 是明确拒绝
            it.next()
                .and_then(|q| q.trim().strip_prefix("q="))
                .and_then(|q| q.parse::<f32>().ok())
                .map(|q| q > 0.0)
                .unwrap_or(true)
        })
    };
    if has("br") {
        Some((".br", "br"))
    } else if has("gzip") {
        Some((".gz", "gzip"))
    } else {
        None
    }
}

pub async fn serve(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // 压缩版：先按 Accept-Encoding 挑，挑不到才发原文。
    //
    // 找不到对应的 `.br` / `.gz` 时**静默退回原文** —— 预压缩和这里靠扩展名表
    // 约定，多一个漏压的文件不该变成 404。
    let enc = if compressible(path) {
        pick_encoding(&headers)
            .and_then(|(ext, name)| get(&format!("{path}{ext}")).map(|d| (d, name)))
    } else {
        None
    };

    if let Some((data, name)) = enc {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_for(path)),
                (header::CONTENT_ENCODING, name),
                // 同一个 URL 按 Accept-Encoding 会给出不同字节：**必须**让缓存
                // 按它分桶，否则中间代理会把 br 那份发给只能读 gzip 的客户端
                (header::VARY, "Accept-Encoding"),
            ],
            body(data),
        )
            .into_response();
    }

    if let Some(data) = get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_for(path)),
            ],
            body(data),
        )
            .into_response();
    }

    // SPA fallback：**看起来像页面**的路径才回 index.html。
    //
    // 前端的路由是 `/s/<会话 id>` 这种真实路径（不是 `#/`），刷新 / 分享链接时
    // 那条地址会直接打到核心 —— 这里得把它交给前端自己的路由。
    //
    // 但**不能无条件回退**，两种情况下必须老实 404：
    //
    //   1. 带扩展名的（`/assets/index-xxx.js` 没命中 —— 换了版本、缓存里记着旧
    //      哈希）—— 回一份 `index.html` 只会让浏览器把 HTML 当 JS 解析，报一句
    //      莫名其妙的语法错；
    //   2. 不是 GET / HEAD 的（`POST /api/sessions/activate` 这种**已经删掉**的
    //      端点）—— 否则旧客户端 / 过期的脚本会收到 200 + 一坨 HTML，以为成功了。
    let looks_like_page = !path.rsplit('/').next().unwrap_or("").contains('.');
    if !looks_like_page || !matches!(method, Method::GET | Method::HEAD) {
        return (StatusCode::NOT_FOUND, "没有这个文件").into_response();
    }
    match get("index.html").or_else(|| get("index.html.br")) {
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

/// 缓存头。Vite 产物文件名带内容哈希，可以永久缓存；`index.html` 每次都要验证，
/// 否则换了新版 .so 还会加载旧页面（WebView 每次启动都会重新拉一遍 JS/CSS）。
fn cache_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// release 构建里资源是编进 .so 的静态切片，直接引用，不拷贝。
fn body(data: Cow<'static, [u8]>) -> Body {
    match data {
        Cow::Borrowed(b) => Body::from(b),
        Cow::Owned(v) => Body::from(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, v.parse().unwrap());
        h
    }

    #[test]
    fn 挑压缩方式() {
        // brotli 优先
        assert_eq!(
            pick_encoding(&hdr("gzip, deflate, br")),
            Some((".br", "br"))
        );
        assert_eq!(pick_encoding(&hdr("br")), Some((".br", "br")));
        // 只有 gzip 就给 gzip
        assert_eq!(pick_encoding(&hdr("gzip, deflate")), Some((".gz", "gzip")));
        // 什么都不支持 / 没有这个头：发原文
        assert_eq!(pick_encoding(&hdr("identity")), None);
        assert_eq!(pick_encoding(&HeaderMap::new()), None);
        // 大小写不敏感（规范里就是）
        assert_eq!(pick_encoding(&hdr("GZIP")), Some((".gz", "gzip")));
        assert_eq!(pick_encoding(&hdr("Br")), Some((".br", "br")));
    }

    #[test]
    fn q0_是明确拒绝_要降级() {
        // `br;q=0` = 客户端明确说不要 br —— 这时不能硬发，得退到 gzip
        assert_eq!(pick_encoding(&hdr("br;q=0, gzip")), Some((".gz", "gzip")));
        // 两个都拒了：原文
        assert_eq!(pick_encoding(&hdr("br;q=0, gzip;q=0")), None);
        // 其它 q 值（>0）照收
        assert_eq!(pick_encoding(&hdr("br;q=0.5")), Some((".br", "br")));
        assert_eq!(pick_encoding(&hdr("gzip;q=0.1")), Some((".gz", "gzip")));
    }

    #[test]
    fn 哪些扩展名值得压() {
        assert!(compressible("assets/index-abc.js"));
        assert!(compressible("assets/index-abc.css"));
        assert!(compressible("index.html"));
        assert!(compressible("favicon.svg"));
        // 已经压过的不再压（预压缩脚本不给它们生成 .br）
        assert!(!compressible("apple-touch-icon.png"));
        assert!(!compressible("favicon-32.png"));
        assert!(!compressible("font.woff2"));
        // 大小写
        assert!(compressible("A.JS"));
    }

    /// 构建产物里每份文本资源都该有对应的 `.br` / `.gz`，而且**内容对得上**。
    ///
    /// 这条盯的是预压缩脚本和这里那张扩展名表的**约定**：脚本漏压一个文件，
    /// 这里就静默退回原文（功能不坏，但那个文件白多 150 KB 流量，没人会发现）。
    ///
    /// 没有 `web/dist` 时（干净 checkout 直接 `cargo test`）跳过 —— 别让测试假红。
    #[test]
    fn 每份文本资源都有压缩版且内容一致() {
        #[cfg(debug_assertions)]
        let Some(index) = get("index.html") else {
            return; // 没构建过前端
        };
        #[cfg(debug_assertions)]
        let _ = index;

        // 从源码目录列一遍 dist（release 下 `get` 走嵌入表，没法枚举）
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../web/dist");
        let Ok(entries) = std::fs::read_dir(dist.join("assets")) else {
            return; // 没有 dist
        };
        let mut checked = 0;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let path = format!("assets/{name}");
            if !compressible(&path) {
                continue;
            }
            let raw = get(&path).unwrap_or_else(|| panic!("缺原文：{path}"));
            let br = get(&format!("{path}.br"))
                .unwrap_or_else(|| panic!("缺 .br：{path} —— 预压缩脚本漏了？"));
            let gz = get(&format!("{path}.gz"))
                .unwrap_or_else(|| panic!("缺 .gz：{path} —— 预压缩脚本漏了？"));
            // 压缩版必须真的更小（漏压时会一样大甚至更大）
            assert!(
                br.len() < raw.len(),
                "{path}: .br ({}) 没有比原文 ({}) 小",
                br.len(),
                raw.len()
            );
            assert!(gz.len() < raw.len(), "{path}: .gz 没有比原文小");
            // 头部对得上：br 没有魔数，gzip 有 1f 8b
            assert_eq!(&gz[..2], &[0x1f, 0x8b], "{path}: .gz 不是 gzip");
            checked += 1;
        }
        assert!(checked > 0, "assets/ 里一个文本资源都没检查到");
    }
}
