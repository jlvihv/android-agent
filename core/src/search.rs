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

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::linux::LinuxSandbox;
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

/// 整段输出最多这么多字（**不是**每条摘要的字数上限）。
///
/// 为什么是整段预算而不是「每条截 400 字」：DeepSeek 返回的 `summary` 是
/// **600～800 字、本来就是给模型读的**（实测还有 1900+ 字的），把一条完整摘要
/// 砍在第 400 个字上等于给模型半截话，而它自己还看不出来。整段预算下每条都
/// 完整，超了就在条与条之间断开，并明说「还有几条没显示」。
///
/// 20K 是按钱算的：这段文字进历史 = 进提示词缓存前缀。20K 汉字 ≈ 20K token，
/// 一次搜索的上下文成本大致和一篇长文档相当 —— 换来的是「搜一次就够，不用
/// 反复搜同一个东西」。真要看全文仍然让模型自己 `curl` / `browser` 挑着读。
///
/// 超了的那几条不丢：写进临时文件，路径跟在正文后面（和 `bash` 输出溢出同一个
/// 套路，见 `LinuxSandbox::spill_captured`）。模型要那几条没显示的，`read` /
/// `grep` 那个文件就行，不用再搜一次（再搜一次是要花钱的）。
const BUDGET: usize = 20_000;

/// 结果 → 给模型看的一段文字（**连抬头一起**）。
///
/// 抬头和正文放在一个函数里：`/api/websearch` 的 JSON 和 `agent search` 的纯文本
/// 用的是同一段东西，分两处拼就会漂（以前抬头在 `http.rs`、正文在这儿，改一处
/// 忘一处）。
///
/// 摘要**原样**给（Markdown、代码块、换行都是信息）。超预算时按**条**截停，超出的
/// 那些整份写进 `spill_dir` 的临时文件（和 `bash` 输出溢出同一个套路），路径附在
/// 正文后面 —— 不丢数据，也不用再搜一次（再搜是要花钱的）。
///
/// 全程只往一个 `String` 里写：不逐条 `format!` 出中间串（每轮一次分配 + 一次
/// 拷贝），字符数也不重新遍历整个输出 —— 只数刚追加的那一段。超预算就
/// `truncate` 回退（`String::truncate` 只改长度，O(1)）。
pub fn report(hits: &[Hit], sandbox: Option<&LinuxSandbox>) -> String {
    // 抬头 + 每条的大致字数，一次算好容量：省掉长到 20KB 过程中的反复 realloc。
    // 估算不用准，超了 `push_str` 自己会再涨，少了也不浪费。
    let estimate = hits
        .iter()
        .map(|h| h.title.len() + h.url.len() + h.snippet.len() + 8)
        .sum::<usize>()
        .min(BUDGET + 1024)
        + 64;
    let mut out = String::with_capacity(estimate);
    use std::fmt::Write as _;
    let _ = writeln!(out, "{} 条 · {}\n", hits.len(), provider_name());
    if hits.is_empty() {
        out.push_str(
            "没搜到结果。换个说法，或者用 `agent browser navigate` 打开搜索引擎自己翻。\n",
        );
        return out;
    }

    let mut chars = out.chars().count();
    let mut shown = hits.len();
    for (i, h) in hits.iter().enumerate() {
        let mark = out.len();
        let _ = write!(out, "{}. {}\n   {}\n   ", i + 1, h.title, h.url);
        // 摘要里的换行跟着缩进，多行也不散架。没有换行时直接借原文，不复制。
        let snip = h.snippet.trim();
        if snip.contains('\n') {
            for (k, line) in snip.split('\n').enumerate() {
                if k > 0 {
                    out.push_str("\n   ");
                }
                out.push_str(line);
            }
        } else {
            out.push_str(snip);
        }
        out.push('\n');

        // 只数刚写进去的这一段，不重新遍历整个输出
        let added = out[mark..].chars().count();
        // 第一条无论如何都给：一条都不显示的话，这个回答就没意义了
        if i > 0 && chars + added > BUDGET {
            out.truncate(mark);
            shown = i;
            break;
        }
        chars += added;
    }
    if shown == hits.len() {
        return out;
    }

    // 超了：**只把没显示的那几条**写进临时文件（正文里已经有的不必再存一份），
    // 并在正文里说清还有几条、在哪
    let rest = &hits[shown..];
    let spilled = sandbox.and_then(|sb| spill(rest, sb));
    let _ = match &spilled {
        Some(p) => writeln!(
            out,
            "…（到字数上限了，剩余 {} 条结果在 {p}，要看 `read {p}`）",
            rest.len()
        ),
        // 写不进临时文件（没有沙箱 / 磁盘满）只能老实说，别假装有
        None => writeln!(
            out,
            "…（到字数上限了，剩余 {} 条结果没显示；调小条数或换更准的说法）",
            rest.len()
        ),
    };
    out
}

/// 把**没显示的那几条**写进临时目录（`spill_dir`，`gc` 会按 `agent-out-` 前缀
/// 清扫），返回**模型看到的**那个路径。
///
/// 只存剩下的，不存全文：正文里已经有的那几条，模型已经知道了，再存一份只会让
/// 它 `read` 的时候把已知内容重读一遍（还白占磁盘）。
///
/// 走 `spill_dir` 而不是自己拼 `/tmp`：沙箱里那是 rootfs 的 /tmp（模型能直接
/// `read`），桌面上就是系统临时目录 —— 两种模式同一个写法，和 bash 溢出一样。
///
/// 序号从 1 起重新编（不是接着正文的）：文件是独立的一份，接着编会让模型以为
/// 前面几条也在这文件里。抬头写明「这是接着正文的那几条」。
fn spill(rest: &[Hit], sandbox: &LinuxSandbox) -> Option<String> {
    let (dir, guest_dir) = sandbox.spill_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!("agent-out-search-{}.md", uuid::Uuid::now_v7());
    let body_len: usize = rest
        .iter()
        .map(|h| h.snippet.len() + h.title.len() + 32)
        .sum();
    let mut body = String::with_capacity(body_len + 96);
    use std::fmt::Write as _;
    let _ = writeln!(
        body,
        "# {} 条 · {}（搜索结果里超出字数上限的那部分）\n",
        rest.len(),
        provider_name()
    );
    for (i, h) in rest.iter().enumerate() {
        let _ = writeln!(
            body,
            "{}. {}\n   {}\n\n{}\n",
            i + 1,
            h.title,
            h.url,
            h.snippet.trim()
        );
    }
    std::fs::write(dir.join(&name), body).ok()?;
    Some(Path::new(&guest_dir).join(&name).display().to_string())
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

    /// 超预算时按**条**截停（不是切半截），并说明还有几条；没沙箱就直说没有路径。
    #[test]
    fn 超预算按条截停_不切半截() {
        let mk = |i: usize| Hit {
            title: format!("标题 {i}"),
            url: format!("https://e.com/{i}"),
            // 每条 8000 字：三条就超 20K
            snippet: "字".repeat(8000),
        };
        let hits: Vec<Hit> = (0..3).map(mk).collect();
        let text = report(&hits, None);
        // 第三条整条没要，前两条是完整的（末尾不是半截而被裁掉的痕迹）
        assert!(text.contains("1. 标题 0"), "第一条要给");
        assert!(text.contains("2. 标题 1"), "第二条要给");
        assert!(!text.contains("3. 标题 2"), "第三条超预算，整条不要");
        assert!(text.contains("剩余 1 条结果没显示"), "{text}");
        // 8000 个「字」一个都不能少
        assert!(text.matches('字').count() >= 16_000, "整条摘要都要在");
    }
}
