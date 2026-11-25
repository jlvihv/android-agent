//! 联网搜索：DeepSeek 的 Search API。
//!
//! 一个端点：`POST https://api.deepseek.com/v1/tools/search`，Bearer 鉴权（错误格式
//! 和对话那套 OpenAI 兼容的一样）。key **只认「模型提供商 → DeepSeek」那一行，
//! 而且必须是 platform.deepseek.com 的官方 key** —— OpenCode Go / Zen、中转站的
//! key 打这个端点是 401（见 `read_key`）。没配就说清楚去哪配。
//!
//! 为什么不留「免 key 也能搜」那条路：以前打的是 Exa 的公开 MCP 端点，靠对方
//! 自己的 key 按 **IP** 限流（2 次/秒、50 次/天）。在运营商 CGNAT 下那 50 次是
//! 同一个出口 IP 上所有人共用的，随时 429，而且不是我们的 bug —— 一个「默认能
//! 用、但随时不可用且原因在别人家」的能力，不如明确要 key（见 docs/09）。
//!
//! 几条要用到的边界：
//!
//! - 计费**按成功的请求次数**（无结果 / 失败不计费），`count` 只影响条数不影响价格 ——
//!   多要几条不额外花钱，但**进了上下文的仍然被截**（见 `report`：每条 400 字、
//!   整段 4000 字），所以别指望「要 50 条」能拿回更多信息。**正文不要**
//!   （`need_content: false`）—— 十篇全文进了历史就是按 token 烧钱。
//! - 限流按**账号** QPS 100，超了 429；402 是余额不足（不是 key 错），文案要分开。
//! - 拿到的只是标题 + 链接 + 摘要。**读全文是另外的事**：静态页面用 bash 的 curl，
//!   要登录态 / JS 渲染 / 点按钮就用 `browser`。

use std::time::Duration;

use serde::Deserialize;

use crate::state::AppState;

/// Search 端点。和 chat 同一个 host：`api.deepseek.com`。
const SEARCH_URL: &str = "https://api.deepseek.com/v1/tools/search";

/// key 从这一家服务商的凭据里拿（`providers.rs` 的预设 id）。
const PROVIDER: &str = "deepseek";

/// 没指定条数时取几条。也在这里定义上限 —— 条数的规矩只放一处，
/// 接口层（`/api/websearch`）不再各写一个默认值。
const DEFAULT_COUNT: usize = 5;
const MAX_COUNT: usize = 50;
/// DeepSeek 的 `count` 上限就是 50：写错了在编译期就发现，不用等线上 400。
const _: () = assert!(MAX_COUNT <= 50);

const TIMEOUT: Duration = Duration::from_secs(40);

/// 一条搜索结果。
#[derive(Debug)]
pub struct Hit {
    pub title: String,
    pub url: String,
    /// 摘要，**原样**（多半是 `summary`，空了退回 `snippet`）。是 Markdown 也好、
    /// 带换行也好，都不动 —— 模型自己认识，我们改只会改坏（见 docs/09）。
    pub snippet: String,
}

/// 这个能力挂在哪家身上（给模型看的抬头、设置页都用它）。
pub fn provider_name() -> &'static str {
    crate::providers::display_name(PROVIDER)
}

/// 搜索用的 key：**只认 DeepSeek 官方那一行**（`providers` 表的 `deepseek`）。
///
/// 「写死」到什么程度，以及为什么不放宽：
///
/// - **只按 id 取 `deepseek`**，不遍历 `providers` 找「任何一个能用的 key」。搜索
///   结果的好坏直接决定模型下一步看什么，用一个打不通的 key 换回一句 401、再白
///   跑一轮，比「明确说没配」糟得多。
/// - **不借用 OpenCode Go / Zen 的 key**：它们看着也是 DeepSeek 系模型，但打
///   `api.deepseek.com` 是 401（实测 `oc_sk_…` → `Your api key … is invalid`）。
///   中转站同理 —— 搜索端点不在它们代理的范围内。
/// - **没有环境变量、没有单独的搜索配置**：配置只有一个来源（设置页写进
///   `providers` 表），不然「界面上填的」和「shell 里带的」会互相打架。
///
/// 在内存里的 `providers` 缓存中取，不查库 —— 搜索是每回合都可能调一次的路径，
/// 没必要为一次读 key 去碰 SQLite。
pub fn read_key(state: &AppState) -> Option<String> {
    // 不看 `Creds.base_url`：内置那家的 Base URL 是写死的（`providers::preset`），
    // 这个字段只给「其他（OpenAI 兼容）」用，`providers::provider` 对内置的也忽略它。
    // 拿它当判据等于让「搜索能不能用」取决于一个没人设的字段 —— 真填了非官方
    // endpoint，401 的文案会把原因说清楚，那比这里静默判死好。
    state
        .provider_creds(PROVIDER)
        .map(|c| c.api_key.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// 每个失败文案里都带这一句：搜索不可用时，模型手里还有别的路，
/// 别让它卡死在「搜索失败了」。
const ELSEWHERE: &str =
    "改用 `agent browser navigate` 打开搜索引擎（bing.com / duckduckgo.com）自己找";

/// 搜一次。`num` 是 `None` 就用 [`DEFAULT_COUNT`]（条数的默认值和上限都在这里定）。
///
/// 没配 key 就直接说清楚去哪配，不去猜别的后端。
pub async fn run(state: &AppState, query: &str, num: Option<usize>) -> anyhow::Result<Vec<Hit>> {
    let query = query.trim();
    anyhow::ensure!(!query.is_empty(), "缺少参数 query");
    let num = num.unwrap_or(DEFAULT_COUNT).clamp(1, MAX_COUNT);

    let key = read_key(state).ok_or_else(|| {
        anyhow::anyhow!(
            "联网搜索只认 platform.deepseek.com 的官方 API Key，现在没配。三条出路：\
             ① 设置 → 模型提供商 → DeepSeek 里填一个官方 key（哪怕平时聊天走 \
             OpenCode Go，搜索也要单独填这一行 —— 别家的 key 打这个端点会 401）；\
             ② {ELSEWHERE}；\
             ③ 已知 URL 的静态页面直接 `curl`。"
        )
    })?;
    search_api(query, num, &key).await
}

// ---------------------------------------------------------------------------
// 结果 → 模型看的文本
// ---------------------------------------------------------------------------

/// 结果 → 一段排好版的文字（**连抬头一起**）。
///
/// 抬头和正文放在一个函数里：`/api/websearch` 的 JSON 和 `agent search` 的纯文本
/// 用的是同一段东西，分两处拼就会漂（以前抬头在 `http.rs`、正文在这儿，改一处
/// 忘一处）。
///
/// 这段文字会进历史、进提示词缓存，所以**按钱算**：每条片段截到 400 字、
/// 整段封顶 4000 字。要读全文是另一件事，让模型自己挑一条去 `curl` /
/// `agent browser`。把十篇全文（几十 KB）塞进历史才是真浪费。
pub fn report(hits: &[Hit]) -> String {
    let mut out = format!("{} 条 · {}\n\n", hits.len(), provider_name());
    if hits.is_empty() {
        out.push_str(
            "没搜到结果。换个说法，或者用 `agent browser navigate` 打开搜索引擎自己翻。\n",
        );
        return out;
    }
    let mut chars = 0usize;
    for (i, h) in hits.iter().enumerate() {
        // 摘要原样留着（Markdown、代码块、换行都是信息），只按字数截断 ——
        // 截断是为了省钱，不是为了"好看"。
        let snip: String = h.snippet.chars().take(400).collect();
        let snip = snip.trim().replace('\n', "\n   ");
        let item = format!("{}. {}\n   {}\n   {}\n", i + 1, h.title, h.url, snip);
        chars += item.chars().count();
        out.push_str(&item);
        if chars > 4000 {
            out.push_str("…（结果太长，已截断；调小条数或换更准的说法）\n");
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// DeepSeek Search API
// ---------------------------------------------------------------------------

/// 响应形状（只取用得上的字段；多出来的 `site_icon` / `authority` / `date` 由
/// serde 忽略）。用结构体而不是 `Value` 逐层 `get`：字段名写错在编译期就发现，
/// 也不会给每个字段写一遍 `and_then(as_str)`。
#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
    /// 长摘要（600～800 字），本来就是给模型读的
    #[serde(default)]
    summary: String,
    #[serde(default)]
    site_name: Option<String>,
}

async fn search_api(query: &str, num: usize, key: &str) -> anyhow::Result<Vec<Hit>> {
    let client = crate::llm::shared_http()?;
    // `need_content: false`：只要摘要。正文留给模型自己按需 `curl` / `browser` 读，
    // 一次性拉十篇全文塞进历史是纯粹的浪费。
    let body = serde_json::json!({ "query": query, "count": num, "need_content": false });

    let resp = client
        .post(SEARCH_URL)
        .header("authorization", format!("Bearer {key}"))
        .timeout(TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("连不上 DeepSeek 的搜索端点: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{}", api_error(status, &text, key));
    }

    let resp: Resp = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("DeepSeek Search 的响应不是 JSON: {e}"))?;
    Ok(resp.results.iter().filter_map(hit_of).take(num).collect())
}

/// 失败的文案：每种状态码都给出「接下来干什么」，而不是一句 500。
fn api_error(status: reqwest::StatusCode, text: &str, key: &str) -> String {
    // 别把 key 回显出去（有的网关会把请求头原样带回来）
    let raw: String = text.replace(key, "***").chars().take(300).collect();
    match status.as_u16() {
        401 => format!(
            "DeepSeek 说这个 API Key 无效（401）。搜索只认 platform.deepseek.com 的官方 \
             key —— OpenCode Go / Zen 或中转站的 key 打这个端点是 401，不能用。\
             去 设置 → 模型提供商 → DeepSeek 换一个官方 key，或者{ELSEWHERE}。原始响应：{raw}"
        ),
        402 => format!(
            "DeepSeek 账号余额不足（402），搜索按成功请求计费（5 元 / 1000 次）。\
             充值后再试，或者{ELSEWHERE}。原始响应：{raw}"
        ),
        429 => format!(
            "DeepSeek 的搜索 QPS 用完了（账号级 100 次/秒，429）。\
             等一下再试，或者{ELSEWHERE}。原始响应：{raw}"
        ),
        _ => format!("DeepSeek Search 返回 {status}：{raw}"),
    }
}

/// 一条结果 → `Hit`。`None` = 这条没法用（没链接 / 不是 http，模型拿到也没用）。
fn hit_of(item: &Item) -> Option<Hit> {
    let url = item.url.trim();
    if !url.starts_with("http") {
        return None;
    }
    // 优先 `summary`（长摘要），空了退回 `snippet` —— 服务端缺字段时会给空串，
    // 空串要当成没有，不能拿它盖住另一个。
    let raw = [&item.summary, &item.snippet]
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let title = item.title.trim();
    let title = if title.is_empty() {
        // 没标题时回退成站点名，再退成域名，别让模型看到一排空白
        item.site_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let host = crate::text::host_name(url);
                if host.is_empty() {
                    url.to_string()
                } else {
                    host
                }
            })
    } else {
        title.to_string()
    };
    Some(Hit {
        title,
        url: url.to_string(),
        snippet: raw.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Vec<Hit> {
        let resp: Resp = serde_json::from_str(json).unwrap();
        resp.results.iter().filter_map(hit_of).collect()
    }

    /// 摘要原样保留：`**` 是 Markdown，模型自己认识，我们不碰。
    #[test]
    fn 优先_summary_没标题退回域名() {
        let hits = parse(
            r#"{"results":[
                {"title":"A","url":"https://example.com/a","snippet":"短","summary":"长：**3.21** 发布"},
                {"url":"https://news.example.org/b","summary":"","snippet":"只有短摘要"},
                {"title":"没有链接","summary":"丢"},
                {"title":"不是 http","url":"ftp://x/y"}
            ]}"#,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].snippet, "长：**3.21** 发布");
        assert_eq!(hits[1].title, "news.example.org");
        assert_eq!(hits[1].snippet, "只有短摘要");
    }
}
