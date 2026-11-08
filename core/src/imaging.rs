//! `read` 工具读图片时的那条路：把图片缩到模型能内联接受的大小。
//!
//! 移植自 OpenMinis 的 `ImageReader.kt`（Android 上用 `BitmapFactory`，
//! 这边用 `image` crate）。策略完全一致：
//!
//! 1. **原图已经够小**（≤2000×2000 且 base64 后 ≤4.5MB）→ 原字节直接发，
//!    不重编码。截图、小图都走这条，零质量损失、还快。
//! 2. 否则解码 → 按 EXIF 转正 → 缩到 2000×2000 以内 → 依次试
//!    PNG / JPEG(q=80→40)，每个尺寸档还按 0.75 逐次缩小，直到塞进上限。
//!
//! 上限按 **base64 字符数** 算（4_718_592 ≈ 4.5 MiB），因为发出去的是
//! `data:` URL；按原始字节能塞下、编码成 base64 后却超了是最容易踩的坑。

use anyhow::Context;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

/// 最长边上限（高清截图够用，再大对模型也没意义）
pub const MAX_DIM: u32 = 2000;
/// 内联 base64 的字符数上限（OpenMinis 同款 4.5 MiB）
pub const MAX_INLINE_BYTES: usize = 4_718_592;
/// JPEG 质量档位（PNG 塞不下就依次往下试）
///
/// 必须从高到低：循环命中即返回，先试高质量才能“同等尺寸下尽量保真”。
const JPEG_QUALITIES: [u8; 5] = [85, 80, 70, 55, 40];
/// 源文件字节上限：再大就不读进内存了（正常手机图片不会到这）
const MAX_SOURCE_BYTES: u64 = 128 * 1024 * 1024;
/// 像素上限。解码是一整张放进内存（RGBA 4 字节/像素），一亿像素就是 400MB，
/// 手机上会直接被 OOM kill。超过就让模型先在沙箱里缩（imagemagick / Pillow）。
const MAX_PIXELS: u64 = 50_000_000;
/// 解码时给 image crate 的硬上限（兜底，防解压炸弹）
const DECODE_MAX_ALLOC: u64 = 256 * 1024 * 1024;

/// 已经处理好的图片：可以直接 base64 内联。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub bytes: Vec<u8>,
    pub mime: String,
    /// 发出去的实际尺寸（缩放后的）
    pub width: u32,
    pub height: u32,
    /// 原图尺寸（按 EXIF 转正后的显示尺寸）
    pub original_width: u32,
    pub original_height: u32,
    /// 原文件有多大（解码前的字节数）。缩放/重编码后依然能告诉模型
    /// 「这份图原本多大」，让它判断要不要先在沙箱里缩一下再读。
    pub source_bytes: usize,
}

/// 看魔数判断是不是认识的图片格式。不是图片返回 `None`（当文本读）。
pub fn mime_of(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && &bytes[..2] == b"BM" {
        return Some("image/bmp");
    }
    None
}

/// mime → 落盘用的扩展名。**只有这里一份**。
///
/// 以前 `lib.rs` 里两处、`mcp.rs` 里一处各抄一遍，而 `image` 依赖在
/// 2026-10-13 已经把 gif / bmp 的解码器去掉了（`.text` 里省 ~90KB）——
/// 那几张表还留着 `gif` / `bmp`，于是「按 mime 定后缀」和「能不能解码」是
/// 两份各自演化的真相。收在这里，新增格式只改一处。
///
/// 认识不了的给 `bin`（不猜，猜错了会让 `GET /api/attachment/<id>` 回错
/// Content-Type）。
pub fn ext_of(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// 扩展名 → mime。`ext_of` 的反向，取回附件时用（`GET /api/attachment/<id>`）。
pub fn mime_of_ext(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// 只读前 12 字节嗅魔数，判断是不是认识的图片。
///
/// **不要为了判断类型把整个文件读进来** —— 一个 200MB 的文本日志会因此
/// 直接 OOM。先嗅头，确定是图片再整份读（图片另有大小上限）。
pub fn is_image_file(path: &Path) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 12];
    let n = file.read(&mut head)?;
    Ok(mime_of(&head[..n]).is_some())
}

/// 是图片就处理成可内联的形态；不是图片返回 `None`。
pub fn load_if_image(path: &Path) -> anyhow::Result<Option<Encoded>> {
    let meta = std::fs::metadata(path).with_context(|| format!("读取失败 {}", path.display()))?;
    anyhow::ensure!(
        meta.len() <= MAX_SOURCE_BYTES,
        "图片有 {}MB，超过 {}MB 上限",
        meta.len() / 1024 / 1024,
        MAX_SOURCE_BYTES / 1024 / 1024
    );
    let source = std::fs::read(path).with_context(|| format!("读取失败 {}", path.display()))?;
    load_if_image_bytes(&source)
}

/// 同 `load_if_image`，但直接吃字节（界面上的上传没有落盘的原图可读）。
pub fn load_if_image_bytes(source: &[u8]) -> anyhow::Result<Option<Encoded>> {
    anyhow::ensure!(
        source.len() as u64 <= MAX_SOURCE_BYTES,
        "图片有 {}MB，超过 {}MB 上限",
        source.len() / 1024 / 1024,
        MAX_SOURCE_BYTES / 1024 / 1024
    );
    let Some(source_mime) = mime_of(source) else {
        return Ok(None);
    };

    // **故意不兜 panic**（release 是 `panic = "abort"`，见 Cargo.toml）：这些
    // 解码器的公开 API 本来就是 `Result`，畸形输入走 `Err`，会 panic 就是它们的
    // bug（上游靠 fuzz 一个个抓）。对 bug 的正确反应是让它暴露、然后修掉，不是
    // 给它盖毯子 —— 盖上之后这类 bug 永远只在用户那里变成「read 报了个看不懂的
    // 错」，我们这边一点信号都没有。
    //
    // 真遇到解码器 panic：logcat 里会有 `RUST PANIC` + 那个 crate 的源码路径
    //（版本号就在路径里，例如 `zune-jpeg-0.5.15/src/…`），顺着找过去**修 / 换 /
    // 自己写**那个解码器，而不是把 abort 改回去。
    //
    // 上面那两道防线保留：`header()` 预检 + `Limits`（宽高/分配上限）把畸形输入
    // 变成正常的 `Err`，那是防患于未然，和「兜 panic」不是一回事。
    encode(source, source_mime).map(Some)
}

/// 原始字节的 base64 长度（不真的编码，只算长度）
pub fn base64_len(raw: usize) -> usize {
    raw.div_ceil(3) * 4
}

/// `data:` URL，直接塞进 OpenAI 的 `image_url`
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// data URL 进程内缓存的上限（按 base64 后的字节算）。
///
/// **要按 LRU 挤**：整块清空的话，一个 43MB 的 PDF（32MB 源）永远放不进去，
/// 于是每一步都重新读盘、重新 base64 一遍；图多的会话还会在「装满 → 清空 →
/// 重新装」之间来回抖。
const URL_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// LRU：path → (data URL, 它最近被用的序号)。
///
/// 原来是 `HashMap` + `VecDeque<String>` 手工维护顺序，`get` 里
/// `order.iter().position(|p| p == path)` 再 `remove(i)` —— 线性扫 + memmove，
/// 而且 path 在两张表里各存一份 `String`。
///
/// 现在用 `tick` 单调递增的序号表示「什么时候用的」，淘汰队列是一张
/// `BTreeMap<序号, path>`：最久没用的就是 `first_key_value()`，O(log n)。
/// 命中时只往 map 里插一个新序号，**不动任何已有条目**；旧序号留在树里，
/// 淘汰时发现「map 里记的序号和它不一样」就说明已被取代，丢掉即可。
///
/// 为什么值得改：`cached_data_url` 在**组请求**里对每张图各调一次（一个回合
/// 5–8 步、每步都把整段历史重新组装），缓存里通常有几十个大块。
#[derive(Default)]
struct UrlCache {
    /// path → (data URL, 最近使用的序号)
    map: HashMap<String, (Arc<str>, u64)>,
    /// 序号 → path，最久没用的在最前（`pop_first` 就是淘汰对象）
    queue: std::collections::BTreeMap<u64, String>,
    /// 下一个要发的序号
    tick: u64,
    bytes: usize,
}

impl UrlCache {
    fn get(&mut self, path: &str) -> Option<Arc<str>> {
        let (url, old) = self.map.get(path)?;
        let (url, old) = (url.clone(), *old);
        self.tick += 1;
        // 旧序号必须从队列里摘掉：留着的话它排在队头，淘汰时会被当成
        // 「这条最久没用」—— 而它其实刚被用过（实测过，测试抓到的）。
        self.queue.remove(&old);
        self.queue.insert(self.tick, path.to_string());
        if let Some((_, stamp)) = self.map.get_mut(path) {
            *stamp = self.tick;
        }
        Some(url)
    }

    fn put(&mut self, path: &str, url: Arc<str>) {
        if url.len() > URL_CACHE_MAX_BYTES || self.map.contains_key(path) {
            return; // 单个就超上限的不缓存（每次现读），别为它把别的全挤掉
        }
        while self.bytes + url.len() > URL_CACHE_MAX_BYTES {
            if !self.evict_oldest() {
                break;
            }
        }
        self.bytes += url.len();
        self.tick += 1;
        self.queue.insert(self.tick, path.to_string());
        self.map.insert(path.to_string(), (url, self.tick));
    }

    /// 淘汰最久没用的那条。返回有没有真的淘汰掉东西。
    ///
    /// `get` 命中时会把旧序号从队列里摘掉，所以队列和 `map` 是**一一对应**的，
    /// 正常情况下一摘就中。这里的 `is_some_and` 只是防御（也让「谁不在这张表里」
    /// 这种不一致不至于静默留下垃圾条目）。
    fn evict_oldest(&mut self) -> bool {
        while let Some((stamp, path)) = self.queue.pop_first() {
            if self.map.get(&path).is_some_and(|(_, s)| *s == stamp) {
                if let Some((u, _)) = self.map.remove(&path) {
                    self.bytes -= u.len();
                }
                return true;
            }
        }
        false
    }

    /// 只给测试看：缓存里有几条。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

fn url_cache() -> &'static Mutex<UrlCache> {
    static C: OnceLock<Mutex<UrlCache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(UrlCache::default()))
}

/// 读文件 + base64 成 data URL，**带进程内缓存**。
///
/// 为什么需要缓存：Chat Completions 是无状态的，每一步都要把整段历史重新
/// 组装成请求，图片也就被重新编码一次。一条带图的会话跑 50 步，同一张图就
/// 要 base64 50 遍 —— 纯浪费。
///
/// 返回 `Arc<str>`：请求体里直接共享这一份，组装请求不再把几十 MB 的
/// 字符串拷来拷去。
///
/// 缓存是安全的：文件是我们自己写出去的（uuid 文件名，写完不再改）。
pub fn cached_data_url(path: &str, mime: &str) -> anyhow::Result<Arc<str>> {
    if let Some(url) = url_cache().lock().ok().and_then(|mut c| c.get(path)) {
        return Ok(url);
    }

    let bytes = std::fs::read(path).with_context(|| format!("读取图片失败 {path}"))?;
    let url: Arc<str> = data_url(mime, &bytes).into();

    if let Ok(mut cache) = url_cache().lock() {
        cache.put(path, url.clone());
    }
    Ok(url)
}

/// 先把这些文件读进缓存（阻塞 I/O，调用方放进 `spawn_blocking`）。
///
/// 组请求时要拿着会话的读锁；第一次用到的图片 / PDF 要读盘 + base64，
/// 几十 MB 的 PDF 能占住锁几百毫秒，期间别的会话都没法追加消息。
/// 所以先在锁外把文件备好，组请求时全是缓存命中。
pub fn prewarm(files: &[(String, String)]) {
    for (path, mime) in files {
        if let Err(e) = cached_data_url(path, mime) {
            tracing::debug!("预读 {path} 失败（组请求时会降级成文字说明）: {e}");
        }
    }
}

fn fits_inline(raw: usize) -> bool {
    base64_len(raw) < MAX_INLINE_BYTES
}

fn encode(source: &[u8], source_mime: &str) -> anyhow::Result<Encoded> {
    // --- 快路：只读头（尺寸 + EXIF 方向），不整个解码 ---
    if let Ok((w, h, orientation)) = header(source) {
        anyhow::ensure!(
            pixels_ok(w, h),
            "图片 {w}×{h} 太大（超过 {} 万像素），先在沙箱里缩一下：\n\n    apk add imagemagick && convert in.jpg -resize 2000x2000 out.jpg\n\n然后再 read out.jpg。",
            MAX_PIXELS / 10_000
        );
        // 带 EXIF 旋转标记的不能原样发：手机竖拍的小图，模型看到的会是躺着的
        // （provider 不保证按 EXIF 转正）。走慢路解码、转正、重编码。
        let upright = orientation == Orientation::NoTransforms;
        if upright && w <= MAX_DIM && h <= MAX_DIM && fits_inline(source.len()) {
            return Ok(Encoded {
                bytes: source.to_vec(),
                mime: source_mime.to_string(),
                width: w,
                height: h,
                original_width: w,
                original_height: h,
                source_bytes: source.len(),
            });
        }
    }

    // --- 慢路：解码 → 转正 → 缩放 → 重编码 ---
    let mut reader = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|e| anyhow::anyhow!("识别图片格式失败: {e}"))?;
    // `Limits` 是 non_exhaustive，只能在 Default 实例上改字段
    let mut limits = Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| anyhow::anyhow!("解码失败: {e}"))?;
    // EXIF 旋转：手机竖拍的照片几乎都靠这个标记，不转正模型看到的就是躺着的
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|e| anyhow::anyhow!("解码失败: {e}"))?;
    img.apply_orientation(orientation);

    let (ow, oh) = (img.width(), img.height());
    let (mut tw, mut th) = fit(ow, oh);

    // 先缩到目标尺寸，**然后把原图放掉**。一张 4800 万像素的照片解码出来是
    // 150–200MB，不能整个编码循环都占着；后续逐次 0.75 缩小时都在小图上做。
    let mut cur = if (tw, th) == (ow, oh) {
        img
    } else {
        let small = img.resize_exact(tw, th, FilterType::Lanczos3);
        drop(img);
        small
    };

    // 先试 PNG 只对**无损**的源图有意义（截图式的界面图、图表、带透明的）。
    // 照片、JPEG 截图拿去无损压缩：2000px 的图手机上要几百毫秒，出来好几 MB，
    // 还常常塞得进上限 —— 于是发出去一张大 PNG。这类直接出 JPEG。
    let lossless = matches!(source_mime, "image/png" | "image/gif" | "image/bmp");
    loop {
        if let Some(bytes) = lossless.then(|| try_png(&cur)).flatten() {
            if fits_inline(bytes.len()) {
                return Ok(Encoded {
                    bytes,
                    mime: "image/png".into(),
                    width: tw,
                    height: th,
                    original_width: ow,
                    original_height: oh,
                    source_bytes: source.len(),
                });
            }
        }
        for q in JPEG_QUALITIES {
            if let Some(bytes) = try_jpeg(&cur, q) {
                if fits_inline(bytes.len()) {
                    return Ok(Encoded {
                        bytes,
                        mime: "image/jpeg".into(),
                        width: tw,
                        height: th,
                        original_width: ow,
                        original_height: oh,
                        source_bytes: source.len(),
                    });
                }
            }
        }

        if tw <= 1 && th <= 1 {
            anyhow::bail!("图片缩到 1×1 仍然超过内联上限，没法发给模型");
        }
        tw = ((f64::from(tw) * 0.75) as u32).max(1);
        th = ((f64::from(th) * 0.75) as u32).max(1);
        cur = cur.resize_exact(tw, th, FilterType::Lanczos3);
    }
}

fn pixels_ok(w: u32, h: u32) -> bool {
    u64::from(w) * u64::from(h) <= MAX_PIXELS
}

/// 等比缩进 `MAX_DIM` 以内，**不放大**
fn fit(w: u32, h: u32) -> (u32, u32) {
    if w <= MAX_DIM && h <= MAX_DIM {
        return (w.max(1), h.max(1));
    }
    let scale = f64::min(
        f64::from(MAX_DIM) / f64::from(w),
        f64::from(MAX_DIM) / f64::from(h),
    );
    (
        ((f64::from(w) * scale).round() as u32).max(1),
        ((f64::from(h) * scale).round() as u32).max(1),
    )
}

/// 只读图片头：尺寸和 EXIF 方向（建 decoder 只解析头，不解码像素）。
fn header(bytes: &[u8]) -> anyhow::Result<(u32, u32, Orientation)> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Ok((w, h, orientation))
}

fn try_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
        .ok()?;
    Some(buf)
}

fn try_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut Cursor::new(&mut buf), quality)
        .encode_image(img)
        .ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LRU 的语义：命中要把那条推到「最新」，淘汰永远淘汰最久没用的那条。
    ///
    /// 这块自己实现过两版（`VecDeque` + `position`、条目记下标 + `swap_remove`），
    /// 都容易写错 —— 而错了的表现是「某张图被提前淘汰，每步都要重新读盘 +
    /// base64」，只看代码看不出来。
    #[test]
    fn url_cache_evicts_the_least_recently_used() {
        let mut c = UrlCache::default();
        c.put("a", Arc::from("AA")); // 2 字节
        c.put("b", Arc::from("BB"));
        c.put("c", Arc::from("CC"));
        assert_eq!(c.len(), 3);

        // 用一下 a：它变成最新的，淘汰时该轮到 b
        assert_eq!(c.get("a").as_deref(), Some("AA"));
        assert_eq!(c.len(), 3, "命中不该改变条数");

        // 再加一条 4 字节：6 字节没超上限，谁都不该被淘汰
        c.put("d", Arc::from("DDDD"));
        assert_eq!(
            c.len(),
            4,
            "没超上限不该淘汰：{:?}",
            c.map.keys().collect::<Vec<_>>()
        );

        // 再塞到超上限（> 64MB）太费内存，这里直接验证「淘汰挑最久没用的」：
        // 把上限调成 3 字节，看它先丢 b 还是先丢刚用过的 a。
        let mut tiny = UrlCache::default();
        tiny.put("a", Arc::from("A"));
        tiny.put("b", Arc::from("B"));
        tiny.get("a"); // a 变最新
        tiny.put("c", Arc::from("C")); // 3 字节，没超
        assert_eq!(tiny.len(), 3);
        // 队列顺序（序号从小到大）= 最久没用 → 最新：b(2) → a(3) → c(4)
        assert_eq!(
            tiny.queue.values().cloned().collect::<Vec<_>>(),
            vec!["b", "a", "c"],
            "命中 a 之后，a 必须排到 b 后面"
        );
        // 淘汰最久没用的那条 = b
        assert!(tiny.evict_oldest());
        assert!(!tiny.map.contains_key("b"), "b 最久没用，先被淘汰");
        assert!(tiny.map.contains_key("a") && tiny.map.contains_key("c"));
        assert_eq!(tiny.bytes, 2, "淘汰后字节数要跟着减");
    }

    /// 淘汰只按字节算；单条就超过上限的东西不缓存，也不该把别人挤掉。
    #[test]
    fn url_cache_skips_oversize_entries_without_evicting_others() {
        let mut c = UrlCache::default();
        c.put("small", Arc::from("x"));
        let huge = URL_CACHE_MAX_BYTES + 1;
        let big: Arc<str> = "y".repeat(huge).into();
        c.put("huge", big);
        assert!(!c.map.contains_key("huge"), "单条超上限不缓存");
        assert!(c.map.contains_key("small"), "不该为它把别人挤掉");
    }

    /// 反复命中同一条时，淘汰队列里的陈旧序号要被丢掉，且缓存条数不涨。
    #[test]
    fn url_cache_survives_repeated_hits() {
        let mut c = UrlCache::default();
        c.put("a", Arc::from("A"));
        for _ in 0..1000 {
            assert_eq!(c.get("a").as_deref(), Some("A"));
        }
        assert_eq!(c.len(), 1, "反复命中不该多出条目");
        assert_eq!(c.queue.len(), 1, "命中要把旧序号摘掉，队列里只该剩一条");
        assert!(c.get("missing").is_none());
    }

    /// 两张表必须互为反向：写下去用什么后缀，取回来就得是什么 mime。
    /// （`gif` / `bmp` 现在解不了码，但历史里可能有老文件，后缀映射得留着。）
    #[test]
    fn ext_and_mime_tables_agree() {
        for mime in [
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "image/bmp",
            "application/pdf",
        ] {
            let ext = ext_of(mime);
            assert_ne!(ext, "bin", "{mime} 应该有后缀");
            assert_eq!(mime_of_ext(ext), mime, "{mime} → {ext} → 反查不一致");
        }
        // 轰出来的魔数也走同一套
        for bytes in [
            &b"\x89PNG\r\n\x1a\n0000"[..],
            &b"\xff\xd8\xff000"[..],
            &b"GIF89a"[..],
            &b"RIFF0000WEBP"[..],
            &b"BM00"[..],
        ] {
            let mime = mime_of(bytes).expect("认得的格式");
            assert_ne!(ext_of(mime), "bin", "魔数认出来的格式必须有后缀");
        }
        assert_eq!(ext_of("application/x-weird"), "bin");
        assert_eq!(mime_of_ext("bin"), "application/octet-stream");
    }

    use image::{Rgb, RgbImage};

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        try_png(&DynamicImage::ImageRgb8(img)).unwrap()
    }

    fn write_temp(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("agent-image-{tag}-{}.png", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn oversized_images_are_downscaled_and_fit_inline() {
        // 2500×2500 超过 2000 上限 → 必须走缩放那条路
        let bytes = png_bytes(2500, 2500);
        let p = write_temp("big", &bytes);
        let e = load_if_image(&p).unwrap().unwrap();
        assert!(
            e.width <= MAX_DIM && e.height <= MAX_DIM,
            "{}×{}",
            e.width,
            e.height
        );
        assert_eq!((e.original_width, e.original_height), (2500, 2500));
        // 缩放/重编码后，原始文件大小仍然如实报出来
        assert_eq!(e.source_bytes, bytes.len());
        assert!(fits_inline(e.bytes.len()));
        assert_eq!(mime_of(&e.bytes), Some(e.mime.as_str()));
    }

    #[test]
    fn absurd_pixel_counts_are_rejected_before_decoding() {
        assert!(pixels_ok(4000, 3000)); // 12MP 手机照
        assert!(pixels_ok(2000, 2000));
        assert!(!pixels_ok(12000, 9000)); // 108MP，解码就是 400MB
    }
}
