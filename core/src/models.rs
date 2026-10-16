//! 模型清单 + 视觉能力自动判定。
//!
//! 数据来自两处：
//!
//! 1. **provider 自己的 `GET {base_url}/models`** —— 有哪些模型。这是实时、
//!    权威的清单，配置界面/CLI 就靠它。
//! 2. **models.dev 目录**（`https://models.dev/api.json`）—— 每个模型的输入
//!    模态。`/v1/models` 的 OpenAI 标准响应里**没有**模态字段，想知道一个模型
//!    能不能吃 `image_url`，只能查这个目录。
//!
//! 判定方式：先用 base_url 认出是目录里的哪个 provider（精确 api → host），
//! 再按模型 id 查 `modalities.input` 里有没有 `image`。
//!
//! 目录缓存进库里的 `http_cache`（抓取时刻 + **只留用得上的字段**），48 小时过期；
//! 网络挂了就用旧缓存。
//!
//! 为什么不原样存：目录是全网的（230 家、8800 多个条目 —— 同一个模型在几十家网关下各登记
//! 一次），原样 5.5MB，而这里只读每个模型的名字、输入模态、上下文 / 输出上限。存之前按
//! `CatalogProvider` / `CatalogModel` 重新序列化一遍（反序列化时本来就只认这几项），
//! 剩 1MB 出头 —— 手机上每 48 小时少往闪存写 4MB 多。其它服务商的条目**不能删**：
//! 配的是中转站 / 自建网关时，靠「跨服务商按 id 查」认模型（见 `find_model`）。
//! 判定不出来时返回 `None` —— 调用方保留原来的值，**不要**因为一次网络抖动
//! 就把「能看图」刷成「不能」。

use std::collections::BTreeMap;

use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::llm::Provider;

pub const CATALOG_URL: &str = "https://models.dev/api.json";
const TTL: Duration = Duration::from_secs(48 * 3600);
/// 目录/清单接口的超时。
///
/// **必须有**：`visibility_for` 是在 `/api/llm` 里 await 的，`shared_http`
/// 又没有总超时（那是为了 LLM 的长流式）—— 没这个的话，models.dev 卡住
/// 就把配置接口一起卡住。拿不到就沿用旧值，不影响正常使用。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// 给前端 / CLI 的模型条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// 能不能吃 `image_url`（`read` 读图时要不要发给模型）
    pub vision: bool,
    /// 能不能吃 PDF / 文档输入（界面上「文件」那个入口要不要显示）
    pub pdf: bool,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
}

// ---------------------------------------------------------------------------
// models.dev 目录
// ---------------------------------------------------------------------------

type Catalog = BTreeMap<String, CatalogProvider>;

// 这几个结构也是**缓存的格式**：只写有值的字段（见模块说明）

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct CatalogProvider {
    /// 这个 provider 的 API base，用来跟配置里的 base_url 对上
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct CatalogModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modalities: Option<Modalities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limit: Option<Limit>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct Modalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
struct Limit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<u64>,
}

/// 缓存的那份：只留用得上的字段；名字和 id 一样的不存（查不到名字本来就用 id）。
fn slim(mut cat: Catalog) -> String {
    for p in cat.values_mut() {
        for (id, m) in p.models.iter_mut() {
            if m.name.as_deref() == Some(id.as_str()) {
                m.name = None;
            }
        }
    }
    serde_json::to_string(&cat).unwrap_or_default()
}

/// 读目录：先用新鲜的缓存（库里的 `http_cache`）；没有就联网拉。联网失败退回旧缓存。
/// 没有库（桌面开发）就每次都联网。
async fn catalog(refresh: bool, db: Option<&Db>) -> Option<Catalog> {
    let cached = || -> Option<(Catalog, u64)> {
        let (body, at) = db?.cached(CATALOG_URL).ok()??;
        Some((serde_json::from_str(&body).ok()?, at))
    };
    if !refresh {
        if let Some((cat, at)) = cached() {
            if now_secs().saturating_sub(at) < TTL.as_secs() {
                return Some(cat);
            }
        }
    }
    match fetch_catalog().await {
        Ok(cat) => {
            if let Some(db) = db {
                if let Err(e) = db.put_cached(CATALOG_URL, &slim(cat.clone()), now_secs()) {
                    tracing::warn!("写 models.dev 缓存失败: {e:#}");
                }
            }
            Some(cat)
        }
        Err(e) => {
            tracing::warn!("拉 models.dev 失败（用旧缓存）: {e:#}");
            cached().map(|(cat, _)| cat)
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn fetch_catalog() -> anyhow::Result<Catalog> {
    let http = crate::llm::shared_http()?;
    let resp = http
        .get(CATALOG_URL)
        .header("user-agent", crate::llm::USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "models.dev 返回 {}",
        resp.status()
    );
    let raw = resp.text().await?;
    let cat: Catalog = serde_json::from_str(&raw)?;
    anyhow::ensure!(!cat.is_empty(), "models.dev 目录是空的");
    Ok(cat)
}

// ---------------------------------------------------------------------------
// 模型清单
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    /// OpenRouter 风格的网关会在这里报模态（`image_input` / `text_output`）
    #[serde(default)]
    architecture: Option<RawArch>,
}

#[derive(Debug, Deserialize, Default)]
struct RawArch {
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<RawModel>,
}

/// provider 的模型清单（带 vision 判定）。
///
/// `/models` 拿不到就退回 models.dev 目录里该 provider 的清单。
pub async fn list(
    provider: &Provider,
    refresh: bool,
    db: Option<&Db>,
) -> anyhow::Result<Vec<ModelInfo>> {
    let cat = catalog(refresh, db).await.unwrap_or_default();
    let entry = provider_entry(&cat, &provider.base_url);

    match fetch_models(provider).await {
        Ok(raw) if !raw.is_empty() => Ok(raw
            .into_iter()
            .map(|r| info_for(&cat, entry.map(|(k, _)| k), r))
            .collect()),
        other => {
            if let Err(e) = &other {
                tracing::warn!("拉 {}/models 失败: {e:#}", provider.base_url);
            }
            let Some((_, p)) = entry else {
                anyhow::bail!("拿不到模型列表（provider /models 和 models.dev 都没数据）");
            };
            let mut out: Vec<ModelInfo> = p
                .models
                .iter()
                .map(|(id, m)| ModelInfo {
                    id: id.clone(),
                    name: m.name.clone().unwrap_or_else(|| id.clone()),
                    vision: model_vision(m).unwrap_or(false),
                    pdf: model_pdf(m).unwrap_or(false),
                    context_window: m.limit.as_ref().and_then(|l| l.context),
                    max_output: m.limit.as_ref().and_then(|l| l.output),
                })
                .collect();
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(out)
        }
    }
}

/// 验证 Key：直接拉一次 `/models`，**不**退回 models.dev 目录。
///
/// `list()` 拉不到会用目录兜底 —— 那是为了「网络抖一下也有清单可选」，但拿来
/// 验证 Key 就会把一个错的 Key 当成对的。返回模型数。
pub async fn verify(provider: &Provider) -> anyhow::Result<usize> {
    match fetch_models(provider).await {
        Ok(v) => Ok(v.len()),
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("401") || msg.contains("403") {
                anyhow::bail!("API Key 不对（服务商拒绝了：{msg}）");
            }
            Err(e.context("连不上服务商"))
        }
    }
}

async fn fetch_models(provider: &Provider) -> anyhow::Result<Vec<RawModel>> {
    let url = format!("{}/models", provider.base_url.trim_end_matches('/'));
    let http = crate::llm::shared_http()?;
    let mut req = http.get(&url).header("user-agent", crate::llm::USER_AGENT);
    if !provider.api_key.is_empty() {
        req = req.bearer_auth(&provider.api_key);
    }
    for (k, v) in &provider.extra_headers {
        req = req.header(k, v);
    }
    let resp = req.timeout(HTTP_TIMEOUT).send().await?;
    anyhow::ensure!(resp.status().is_success(), "{url} 返回 {}", resp.status());
    let body: ModelsResponse = resp.json().await?;
    Ok(body.data)
}

fn info_for(cat: &Catalog, provider_key: Option<&str>, raw: RawModel) -> ModelInfo {
    let dev = find_model(cat, provider_key, &raw.id);
    // 网关自己在 `architecture.input_modalities` 里报了模态就用它，否则查目录
    let modalities = raw
        .architecture
        .as_ref()
        .map(|a| a.input_modalities.clone())
        .unwrap_or_default();
    let vision = vision_from_modalities(&modalities)
        .or_else(|| dev.and_then(model_vision))
        .unwrap_or(false);
    let pdf = pdf_from_modalities(&modalities)
        .or_else(|| dev.and_then(model_pdf))
        .unwrap_or(false);

    ModelInfo {
        name: raw
            .name
            .clone()
            .or_else(|| dev.and_then(|m| m.name.clone()))
            .unwrap_or_else(|| raw.id.clone()),
        context_window: dev.and_then(|m| m.limit.as_ref()?.context),
        max_output: dev.and_then(|m| m.limit.as_ref()?.output),
        id: raw.id,
        vision,
        pdf,
    }
}

/// 给单个模型自动判定能不能看图。查不到返回 `None`（调用方保留旧值）。
pub async fn vision_for(
    provider: &Provider,
    model_id: &str,
    refresh: bool,
    db: Option<&Db>,
) -> Option<bool> {
    let cat = catalog(refresh, db).await?;
    let key = provider_entry(&cat, &provider.base_url).map(|(k, _)| k);
    let m = find_model(&cat, key, model_id)?;
    model_vision(m)
}

/// 给单个模型自动判定能不能吃 PDF。查不到返回 `None`（调用方保留旧值）。
pub async fn pdf_for(
    provider: &Provider,
    model_id: &str,
    refresh: bool,
    db: Option<&Db>,
) -> Option<bool> {
    let cat = catalog(refresh, db).await?;
    let key = provider_entry(&cat, &provider.base_url).map(|(k, _)| k);
    let m = find_model(&cat, key, model_id)?;
    model_pdf(m)
}

// ---------------------------------------------------------------------------
// 匹配
// ---------------------------------------------------------------------------

/// 按 base_url 找目录里的 provider：先精确比 `api`（带/不带 `/v1`），
/// 再退回主机名。
fn provider_entry<'a>(cat: &'a Catalog, base_url: &str) -> Option<(&'a str, &'a CatalogProvider)> {
    let strip = |s: &str| s.trim_end_matches('/').to_string();
    let target = strip(base_url);
    let mut candidates = vec![target.clone()];
    if let Some(rest) = target.strip_suffix("/v1") {
        candidates.push(rest.to_string());
    } else {
        candidates.push(format!("{target}/v1"));
    }

    for (key, p) in cat {
        let Some(api) = p.api.as_deref() else {
            continue;
        };
        let api = strip(api);
        if candidates.contains(&api) {
            return Some((key.as_str(), p));
        }
    }

    let host = host_of(&target)?;
    for (key, p) in cat {
        if let Some(api) = p.api.as_deref() {
            if host_of(api).as_deref() == Some(host.as_str()) {
                return Some((key.as_str(), p));
            }
        }
    }
    None
}

/// 找目录里这个模型的条目：本 provider 精确 → 跨 provider 精确 → 逐段缩短别名。
fn find_model<'a>(
    cat: &'a Catalog,
    provider_key: Option<&str>,
    model_id: &str,
) -> Option<&'a CatalogModel> {
    // 1. 本 provider
    if let Some(key) = provider_key {
        if let Some(m) = cat.get(key).and_then(|p| p.models.get(model_id)) {
            return Some(m);
        }
    }
    // 2. 跨 provider 精确（同一个 id 被很多家发布）
    let exact = |id: &str| -> Option<&CatalogModel> {
        let mut fallback = None;
        for p in cat.values() {
            if let Some(m) = p.models.get(id) {
                if m.modalities.is_some() {
                    return Some(m);
                }
                fallback.get_or_insert(m);
            }
        }
        fallback
    };
    if let Some(m) = exact(model_id) {
        return Some(m);
    }
    // 3. 别名：中转站会给 id 加后缀（`glm-5.3-flash-cpa`）
    for candidate in alias_candidates(model_id) {
        if let Some(m) = exact(&candidate) {
            return Some(m);
        }
    }
    None
}

/// `a-b-c` → `["a-b-c", "a-b"]`（`-` / `:` 分段，至少留 2 段）
fn alias_candidates(model_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = model_id.to_string();
    loop {
        let cut = current.rfind(['-', ':']);
        let Some(cut) = cut else { break };
        if cut == 0 {
            break;
        }
        current.truncate(cut);
        if current.split(['-', ':']).filter(|s| !s.is_empty()).count() < 2 {
            break;
        }
        out.push(current.clone());
    }
    out
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next()?;
    let host = host.rsplit('@').next()?; // 去掉 user:pass@
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn model_vision(m: &CatalogModel) -> Option<bool> {
    vision_from_modalities(&m.modalities.as_ref()?.input)
}

fn model_pdf(m: &CatalogModel) -> Option<bool> {
    pdf_from_modalities(&m.modalities.as_ref()?.input)
}

fn vision_from_modalities(list: &[String]) -> Option<bool> {
    modality_present(list, "image")
}

/// PDF 是 models.dev 目录里的一个输入模态（`pdf`）。网关自己的
/// `architecture.input_modalities` 偶尔会写成 `file_input` —— 一起认。
fn pdf_from_modalities(list: &[String]) -> Option<bool> {
    if list.is_empty() {
        return None;
    }
    Some(list.iter().any(|s| {
        let n = normalize_modality(s);
        n == "pdf" || n == "file"
    }))
}

fn modality_present(list: &[String], want: &str) -> Option<bool> {
    if list.is_empty() {
        return None;
    }
    Some(list.iter().any(|s| normalize_modality(s) == want))
}

/// OpenAI / OpenRouter 写 `image_input`，models.dev 写 `image` —— 统一成裸名
fn normalize_modality(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .trim_end_matches("_input")
        .trim_end_matches("_output")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_fixture() -> Catalog {
        serde_json::from_str(
            r#"{
              "opencode-go": {
                "name": "OpenCode Go",
                "api": "https://opencode.ai/zen/go/v1",
                "models": {
                  "ds-vision": { "name": "DS Vision", "modalities": { "input": ["text","image"] }, "limit": {"context": 1000000, "output": 384000} },
                  "ds-pdf":    { "name": "DS PDF",    "modalities": { "input": ["text","pdf"] } },
                  "ds-text":   { "name": "DS Text",   "modalities": { "input": ["text"] } }
                }
              },
              "other": {
                "name": "Other",
                "api": "https://other.example/v1",
                "models": {
                  "glm-5.3-flash": { "modalities": { "input": ["text","image"] } }
                }
              }
            }"#,
        )
        .unwrap()
    }

    /// 缓存的那份只留用得上的字段，读回来查出来的结果和原样的一样
    #[test]
    fn the_cached_catalog_keeps_only_what_is_used() {
        let raw = r#"{"p":{"name":"P","api":"https://p.example/v1","doc":"https://…","models":{
          "m1":{"id":"m1","name":"m1","description":"很长的一段介绍","cost":{"input":1},"release_date":"2026-01-01",
                "modalities":{"input":["text","image"],"output":["text"]},"limit":{"context":1000,"output":10}},
          "m2":{"id":"m2","name":"Model Two","modalities":{"input":["text"]}}}}}"#;
        let cat: Catalog = serde_json::from_str(raw).unwrap();
        let cached = slim(cat.clone());
        for gone in [
            "description",
            "cost",
            "release_date",
            "doc",
            "\"output\":[",
            "\"name\":\"m1\"",
        ] {
            assert!(!cached.contains(gone), "{gone} 不该进缓存：{cached}");
        }
        let back: Catalog = serde_json::from_str(&cached).unwrap();
        let key = provider_entry(&back, "https://p.example/v1").map(|(k, _)| k);
        let m1 = find_model(&back, key, "m1").unwrap();
        assert_eq!(model_vision(m1), Some(true));
        assert_eq!(m1.limit.as_ref().and_then(|l| l.context), Some(1000));
        assert_eq!(
            find_model(&back, key, "m2").unwrap().name.as_deref(),
            Some("Model Two")
        );
        assert!(
            cached.len() * 2 < raw.len(),
            "{} vs {}",
            cached.len(),
            raw.len()
        );
    }

    #[test]
    fn base_url_matches_the_provider_entry() {
        let cat = catalog_fixture();
        // 带 /v1、不带 /v1、尾斜杠都要能对上
        for url in [
            "https://opencode.ai/zen/go/v1",
            "https://opencode.ai/zen/go/v1/",
            "https://opencode.ai/zen/go",
        ] {
            let (key, _) = provider_entry(&cat, url).unwrap();
            assert_eq!(key, "opencode-go", "{url}");
        }
    }

    #[test]
    fn provider_entry_falls_back_to_hostname() {
        let cat = catalog_fixture();
        let (key, _) = provider_entry(&cat, "https://opencode.ai/some/other/path").unwrap();
        assert_eq!(key, "opencode-go");
    }

    #[test]
    fn vision_comes_from_the_catalog_modalities() {
        let cat = catalog_fixture();
        let key = Some("opencode-go");
        assert_eq!(
            model_vision(find_model(&cat, key, "ds-vision").unwrap()),
            Some(true)
        );
        assert_eq!(
            model_vision(find_model(&cat, key, "ds-text").unwrap()),
            Some(false)
        );
        // 目录里没有 → None，调用方保留旧值
        assert!(find_model(&cat, key, "unknown-model").is_none());
    }

    #[test]
    fn aliased_ids_fall_back_to_the_shortened_prefix() {
        let cat = catalog_fixture();
        // 中转站加后缀：`glm-5.3-flash-cpa` → `glm-5.3-flash`
        let m = find_model(&cat, None, "glm-5.3-flash-cpa").unwrap();
        assert_eq!(model_vision(m), Some(true));
        // 太短的（只剩一段）不许匹配，避免 gpt/claude 这种泛化
        assert!(alias_candidates("glm-5.3-flash-cpa").iter().all(|c| c
            .split(['-', ':'])
            .filter(|s| !s.is_empty())
            .count()
            >= 2));
    }

    #[test]
    fn modality_spellings_are_normalized() {
        assert_eq!(normalize_modality("IMAGE_INPUT"), "image");
        assert_eq!(normalize_modality("text_output"), "text");
        assert_eq!(vision_from_modalities(&["image_input".into()]), Some(true));
        assert_eq!(vision_from_modalities(&["text".into()]), Some(false));
        assert_eq!(vision_from_modalities(&[]), None);
        // PDF 也是一样的写法
        assert_eq!(pdf_from_modalities(&["pdf".into()]), Some(true));
        assert_eq!(pdf_from_modalities(&["file_input".into()]), Some(true));
        assert_eq!(pdf_from_modalities(&["text".into()]), Some(false));
        assert_eq!(pdf_from_modalities(&[]), None);
    }

    #[test]
    fn pdf_comes_from_the_catalog_modalities() {
        let cat = catalog_fixture();
        let key = Some("opencode-go");
        assert_eq!(
            model_pdf(find_model(&cat, key, "ds-pdf").unwrap()),
            Some(true)
        );
        assert_eq!(
            model_pdf(find_model(&cat, key, "ds-vision").unwrap()),
            Some(false)
        );
        // 图片和 PDF 是两件独立的事，不能互相推导
        assert_eq!(
            model_vision(find_model(&cat, key, "ds-pdf").unwrap()),
            Some(false)
        );
        assert_eq!(
            model_pdf(find_model(&cat, key, "ds-text").unwrap()),
            Some(false)
        );
    }
}
