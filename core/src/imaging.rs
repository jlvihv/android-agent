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

use std::collections::{HashMap, VecDeque};
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
/// （曾经写成 `[80, 85, …]`，`85` 永远轮不到，是条死条目。）
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
    let meta =
        std::fs::metadata(path).map_err(|e| anyhow::anyhow!("读取失败 {}: {e}", path.display()))?;
    anyhow::ensure!(
        meta.len() <= MAX_SOURCE_BYTES,
        "图片有 {}MB，超过 {}MB 上限",
        meta.len() / 1024 / 1024,
        MAX_SOURCE_BYTES / 1024 / 1024
    );
    let source =
        std::fs::read(path).map_err(|e| anyhow::anyhow!("读取失败 {}: {e}", path.display()))?;
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

    // **故意不兜 panic**（release 是 `panic = "abort"`，见 Cargo.toml）。
    //
    // 这里曾经套过一层 `catch_unwind`，理由是「`image` / `zune_jpeg` / `png`
    // 的解码器碰到畸形文件时可能 panic 而不是返回 Err，一张坏图不该带走
    // 整个 App」。那个理由站不住：
    //
    // - 这些解码器的**公开 API 本来就是 `Result`**，畸形输入走 `Err`；
    //   会 panic 就是它们的 bug（上游靠 fuzz 一个个抓）。
    // - 对一个 bug 的正确反应是让它暴露、然后修掉，不是给它盖毯子。盖上
    //   之后这类 bug 永远只在用户那里变成「read 报了个看不懂的错」，我们
    //   这边一点信号都没有 —— 正好是这套策略要防的事。
    //
    // 所以真遇到解码器 panic 时：logcat 里会有 `RUST PANIC` + 那个 crate 的
    // 源码路径（版本号就在路径里，例如 `zune-jpeg-0.5.15/src/…`），顺着找过去，
    // 然后**修 / 换 / 自己写**那个解码器，而不是把 abort 改回去。
    //
    // 上面那两道防线保留：`header()` 预检 + `Limits`（宽高/分配上限）把
    // 畸形输入变成正常的 `Err`，那是防患于未然，和「兜 panic」不是一回事。
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
/// 原来是 32MB、超了整个清空：一个 32MB 的 PDF 编出来 43MB，**永远放不
/// 进去**，于是每一步都重新读盘、重新 base64 一遍；图多的会话也会在
/// 「装满 → 清空 → 重新装」之间来回抖。现在是 LRU：只挤掉最久没用的。
const URL_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct UrlCache {
    map: HashMap<String, Arc<str>>,
    /// 最近用过的在后面
    order: VecDeque<String>,
    bytes: usize,
}

impl UrlCache {
    fn get(&mut self, path: &str) -> Option<Arc<str>> {
        let url = self.map.get(path)?.clone();
        if let Some(i) = self.order.iter().position(|p| p == path) {
            let k = self.order.remove(i).expect("刚找到的下标");
            self.order.push_back(k);
        }
        Some(url)
    }

    fn put(&mut self, path: &str, url: Arc<str>) {
        if url.len() > URL_CACHE_MAX_BYTES || self.map.contains_key(path) {
            return; // 单个就超上限的不缓存（每次现读），别为它把别的全挤掉
        }
        while self.bytes + url.len() > URL_CACHE_MAX_BYTES {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(u) = self.map.remove(&old) {
                self.bytes -= u.len();
            }
        }
        self.bytes += url.len();
        self.order.push_back(path.to_string());
        self.map.insert(path.to_string(), url);
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

    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("读取图片失败 {path}: {e}"))?;
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

    loop {
        if let Some(bytes) = try_png(&cur) {
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
        tw = ((tw as f64 * 0.75) as u32).max(1);
        th = ((th as f64 * 0.75) as u32).max(1);
        cur = cur.resize_exact(tw, th, FilterType::Lanczos3);
    }
}

fn pixels_ok(w: u32, h: u32) -> bool {
    (w as u64) * (h as u64) <= MAX_PIXELS
}

/// 等比缩进 `MAX_DIM` 以内，**不放大**
fn fit(w: u32, h: u32) -> (u32, u32) {
    if w <= MAX_DIM && h <= MAX_DIM {
        return (w.max(1), h.max(1));
    }
    let scale = f64::min(MAX_DIM as f64 / w as f64, MAX_DIM as f64 / h as f64);
    (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
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

    #[test]
    fn the_url_cache_evicts_the_least_recently_used_and_skips_giants() {
        let mut c = UrlCache::default();
        let third = URL_CACHE_MAX_BYTES / 3;
        let blob = |n: usize| -> Arc<str> { "x".repeat(n).into() };
        c.put("a", blob(third));
        c.put("b", blob(third));
        c.put("c", blob(third));
        assert!(c.get("a").is_some()); // a 刚用过，b 变成最久没用的
        c.put("d", blob(third));
        assert!(c.get("b").is_none(), "该挤掉最久没用的 b");
        assert!(c.get("a").is_some() && c.get("c").is_some() && c.get("d").is_some());

        // 一个就超上限的：不缓存，也不能为它把别的全挤掉
        c.put("huge", blob(URL_CACHE_MAX_BYTES + 1));
        assert!(c.get("huge").is_none());
        assert!(c.get("a").is_some());
        assert!(c.bytes <= URL_CACHE_MAX_BYTES);
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
    fn magic_bytes_identify_the_formats_read_accepts() {
        assert_eq!(mime_of(&png_bytes(2, 2)).unwrap(), "image/png");
        assert_eq!(mime_of(&[0xff, 0xd8, 0xff, 0xe0]).unwrap(), "image/jpeg");
        assert_eq!(mime_of(b"GIF89a......").unwrap(), "image/gif");
        assert_eq!(mime_of(b"RIFF????WEBPVP8 ").unwrap(), "image/webp");
        assert_eq!(mime_of(b"BM....").unwrap(), "image/bmp");
        assert_eq!(mime_of(b"hello world"), None);
    }

    #[test]
    fn data_url_is_cached_after_the_first_read() {
        let p = write_temp("urlcache", &png_bytes(4, 4));
        let path = p.to_str().unwrap().to_string();
        let first = cached_data_url(&path, "image/png").unwrap();
        assert!(first.starts_with("data:image/png;base64,"));

        // 把文件删了：第二次还能拿到 → 证明走的是缓存，没再读盘
        std::fs::remove_file(&p).unwrap();
        let second = cached_data_url(&path, "image/png").unwrap();
        assert_eq!(first, second);

        // 不存在的路径：报错而不是 panic
        assert!(cached_data_url("/no/such/image.png", "image/png").is_err());
    }

    #[test]
    fn is_image_file_sniffs_only_the_header() {
        let p = write_temp("hdr", &png_bytes(2, 2));
        assert!(is_image_file(&p).unwrap());

        let t = std::env::temp_dir().join(format!("agent-image-txt-{}", std::process::id()));
        std::fs::write(&t, "not an image at all").unwrap();
        assert!(!is_image_file(&t).unwrap());
        let _ = std::fs::remove_file(&t);
    }

    #[test]
    fn plain_text_is_not_an_image() {
        let p = std::env::temp_dir().join(format!("agent-image-text-{}", std::process::id()));
        std::fs::write(&p, "just text").unwrap();
        assert!(load_if_image(&p).unwrap().is_none());
    }

    /// 手机竖拍的小图：尺寸没超、字节也不大，但带 EXIF 旋转标记 ——
    /// 不能原样发（模型看到的是躺着的），要转正后重编码。
    #[test]
    fn small_images_with_an_exif_rotation_are_turned_upright() {
        let img = DynamicImage::new_rgb8(40, 20);
        let mut jpeg = Vec::new();
        img.write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        // APP1/Exif：大端 TIFF，一个 IFD 条目 Orientation(0x0112) = 6（顺时针 90°）
        let mut app1: Vec<u8> = vec![0xFF, 0xE1, 0x00, 0x22];
        app1.extend_from_slice(b"Exif\0\0MM\0\x2a\0\0\0\x08");
        app1.extend_from_slice(&[
            0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let mut rotated = jpeg[..2].to_vec();
        rotated.extend_from_slice(&app1);
        rotated.extend_from_slice(&jpeg[2..]);

        let e = load_if_image_bytes(&rotated).unwrap().unwrap();
        assert_ne!(e.bytes, rotated, "带旋转标记的不能原样发");
        assert_eq!((e.width, e.height), (20, 40), "转正之后宽高互换");
    }

    #[test]
    fn small_images_pass_through_untouched() {
        let bytes = png_bytes(64, 48);
        let p = write_temp("small", &bytes);
        let e = load_if_image(&p).unwrap().unwrap();
        assert_eq!(e.bytes, bytes, "小图必须原样发，不重编码");
        assert_eq!(e.mime, "image/png");
        assert_eq!((e.width, e.height), (64, 48));
        assert_eq!((e.original_width, e.original_height), (64, 48));
        assert_eq!(e.source_bytes, bytes.len());
        assert!(fits_inline(e.bytes.len()));
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
    fn jpeg_qualities_are_tried_from_high_to_low() {
        // 循环命中即返回：顺序写反了低质量会盖掉高质量（`[80, 85, …]` 里的 85
        // 永远轮不到），所以顺序本身就是要守住的行为。
        assert!(
            JPEG_QUALITIES.windows(2).all(|w| w[0] > w[1]),
            "{JPEG_QUALITIES:?}"
        );
    }

    #[test]
    fn absurd_pixel_counts_are_rejected_before_decoding() {
        assert!(pixels_ok(4000, 3000)); // 12MP 手机照
        assert!(pixels_ok(2000, 2000));
        assert!(!pixels_ok(12000, 9000)); // 108MP，解码就是 400MB
    }

    #[test]
    fn base64_budget_is_measured_on_the_encoded_form() {
        // 3 字节 → 4 个字符
        assert_eq!(base64_len(0), 0);
        assert_eq!(base64_len(1), 4);
        assert_eq!(base64_len(3), 4);
        assert_eq!(base64_len(4), 8);
        // 边界：正好等于上限不算过，少 3 字节才行
        let at_limit = MAX_INLINE_BYTES / 4 * 3;
        assert_eq!(base64_len(at_limit), MAX_INLINE_BYTES);
        assert!(!fits_inline(at_limit));
        assert!(fits_inline(at_limit - 3));
    }
}
