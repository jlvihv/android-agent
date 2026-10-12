//! 联网搜索：Exa。
//!
//! 两条路，思路和 pi-web-access 的 `exa.ts` 一样：
//!
//! - **没有 key**（默认）：Exa 的**公开 MCP 端点** `https://mcp.exa.ai/mcp`。
//!   它不要任何凭据 —— 服务端拿 **Exa 自己的 key** 去打 Exa 的 API，然后按
//!   **IP** 限流（`RATE_LIMIT_QPS=2`、`RATE_LIMIT_DAILY=50`，只算 `tools/call`）。
//!   为什么免费：这是 Exa 的获客漏斗，零配置就能用才有人把它写进默认配置；
//!   免费档只给基础搜索，advanced / agent 那些工具要 key。
//! - **有 key**：直连 `https://api.exa.ai/search`，无限流。
//!   key 从 `EXA_API_KEY`（桌面）或会话库的 `search` 那一行拿（手机）。
//!
//! 手机上两个必须知道的边界：
//!
//! 1. 限流按 **IP**。运营商 CGNAT 下，那个 50/天 是同一个出口 IP 上所有人
//!    共用的 —— 随时可能撞到 429，**不是我们的 bug**，所以错误文案要写清楚
//!    怎么加 key。
//! 2. 没有 SLA。端点和工具名随时可能变，所以 429 / 解析失败都要给出"换个做法"
//!    的出路（用 browser 打开搜索引擎、或者用 key 直连）。
//!
//! 拿到的只是标题 + 链接 + 片段。**读全文是另外的事**：静态页面用 bash 的 curl，
//! 要登录态 / JS 渲染 / 点按钮就用 `browser`。

use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::state::AppState;

/// 免 key 的公开 MCP 端点。`?tools=` 决定服务端注册哪些工具
/// （注册多了会增加它给模型看的工具表，我们只要一个）。
const EXA_MCP: &str = "https://mcp.exa.ai/mcp?tools=web_search_exa";
/// 带 key 时直连的 REST 端点。
const EXA_API: &str = "https://api.exa.ai/search";

const TIMEOUT: Duration = Duration::from_secs(40);

/// 一条搜索结果。
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub title: String,
    pub url: String,
    /// 片段（可能是多条 highlight 拼起来的）。**不是全文**。
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub hits: Vec<Hit>,
    /// 走的是哪条路（`exa-mcp` / `exa-api`），给日志和工具输出用
    pub provider: &'static str,
}

/// 找 key：环境变量优先（桌面开发），其次会话库的 `search` 行（手机）。
///
/// 会话库那行的形状是 `{"exaApiKey":"exa-…"}`。存库而不是存文件，和 provider
/// 配置同一个理由：写 JSON 文件时进程被杀就是半截文件（见 `state::PersistedProvider`）。
pub fn read_key(state: &AppState) -> Option<String> {
    if let Ok(v) = std::env::var("EXA_API_KEY") {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    let db = state.db.read().ok().and_then(|g| g.clone())?;
    let raw = db.get_meta("search").ok().flatten()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get("exaApiKey")
        .and_then(|k| k.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 搜一次。`key` 有就直连，没有就走公开 MCP。
pub async fn run(query: &str, num: usize, key: Option<&str>) -> anyhow::Result<Found> {
    let query = query.trim();
    anyhow::ensure!(!query.is_empty(), "缺少参数 query");
    let num = num.clamp(1, 20);

    match key {
        Some(k) => search_api(query, num, k).await,
        None => search_mcp(query, num).await,
    }
}

// ---------------------------------------------------------------------------
// 免 key：公开 MCP
// ---------------------------------------------------------------------------

async fn search_mcp(query: &str, num: usize) -> anyhow::Result<Found> {
    let client = crate::llm::shared_http()?;

    // 一条裸的 JSON-RPC。**不做 initialize 握手** —— 这个端点无状态，
    // 一次 tools/call 就够（pi-web-access 也是这么打的）。
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "web_search_exa",
            "arguments": { "query": query, "numResults": num }
        }
    });

    let resp = client
        .post(EXA_MCP)
        .header("content-type", "application/json")
        // 两个都要带上：服务端可能回 SSE，也可能直接回 JSON
        .header("accept", "application/json, text/event-stream")
        // 有它 Exa 那边能看出是哪个客户端（他们用来统计/排查）
        .header("x-exa-source", "android-agent")
        .timeout(TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("连不上 Exa 的搜索端点: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if status.as_u16() == 429 {
        anyhow::bail!(
            "Exa 免费额度用尽（按 IP 限 2 次/秒、50 次/天）。这个额度是**同一个出口 IP \
             共用**的，别人的手机也可能把它用掉。三条出路：① 过一会儿再试；\
             ② 配一个 Exa key（写进会话库 search 行或 EXA_API_KEY 环境变量）；\
             ③ 改用 browser 打开搜索引擎（bing.com / duckduckgo.com）自己找。\
             原始响应：{}",
            text.chars().take(200).collect::<String>()
        );
    }
    if !status.is_success() {
        anyhow::bail!(
            "Exa MCP 返回 {status}：{}",
            text.chars().take(300).collect::<String>()
        );
    }

    let payload = decode_rpc_body(&text)?;
    if let Some(err) = payload.get("error") {
        anyhow::bail!("Exa MCP 报错：{err}");
    }
    let content = payload
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Exa MCP 没给内容：{}", text.chars().take(200).collect::<String>()))?;
    if payload.pointer("/result/isError").and_then(|v| v.as_bool()) == Some(true) {
        anyhow::bail!("Exa MCP 报错：{}", content.chars().take(300).collect::<String>());
    }

    Ok(Found {
        hits: parse_mcp_text(content, num),
        provider: "exa-mcp",
    })
}

/// 把响应体解码成 JSON-RPC 信封。
///
/// 这个端点回的是 **SSE**（`event: message` + `data: {…}`），但按 MCP 规范也
/// 允许直接回 JSON —— 两种都认，先扫 `data:` 行，扫不到就整段当 JSON 解。
fn decode_rpc_body(body: &str) -> anyhow::Result<Value> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(rest) {
            // 一帧可能是通知（没有 result/error），跳过继续找
            if v.get("result").is_some() || v.get("error").is_some() {
                return Ok(v);
            }
        }
    }
    serde_json::from_str::<Value>(body)
        .map_err(|e| anyhow::anyhow!("Exa MCP 的响应既不是 SSE 也不是 JSON: {e}"))
}

/// 解析 MCP 给的文本块。形状是：
///
/// ```text
/// Title: 标题
/// URL: https://…
/// Published: …
/// Author: …
/// Highlights:
/// 片段…
///
/// ---
///
/// Title: …
/// ```
fn parse_mcp_text(text: &str, num: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    for block in text.split("\n\n---") {
        let mut title = String::new();
        let mut url = String::new();
        let mut body = String::new();
        let mut in_body = false;

        for line in block.lines() {
            let line = line.trim_end();
            if !in_body {
                if let Some(v) = line.strip_prefix("Title:") {
                    title = v.trim().to_string();
                    continue;
                }
                if let Some(v) = line.strip_prefix("URL:") {
                    url = v.trim().to_string();
                    continue;
                }
                if line.starts_with("Highlights:") {
                    in_body = true;
                    continue;
                }
                if let Some(v) = line.strip_prefix("Text:") {
                    body.push_str(v.trim());
                    body.push(' ');
                    in_body = true;
                    continue;
                }
                // Published / Author 之类：模型看片段就够了，不占额度
            } else {
                let t = line.trim();
                if !t.is_empty() && t != "---" {
                    body.push_str(t);
                    body.push(' ');
                }
            }
        }

        if url.starts_with("http") {
            // MCP 那边没标题时会写 `Title: N/A` —— 别让模型看到一排 "N/A"，
            // 回退成域名至少能看出是哪个站
            let t = title.trim();
            hits.push(Hit {
                title: if t.is_empty() || t.eq_ignore_ascii_case("n/a") {
                    host_of(&url)
                } else {
                    title
                },
                url,
                snippet: squeeze(&body),
            });
        }
        if hits.len() >= num {
            break;
        }
    }
    hits
}

/// 去掉 Markdown 标记和多余空白，留一段能读的片段。
fn squeeze(s: &str) -> String {
    let cleaned = s
        .replace("**", "")
        .replace("###", "")
        .replace('#', " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'");
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn host_of(url: &str) -> String {
    url.split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

// ---------------------------------------------------------------------------
// 带 key：直连 REST
// ---------------------------------------------------------------------------

async fn search_api(query: &str, num: usize, key: &str) -> anyhow::Result<Found> {
    let client = crate::llm::shared_http()?;
    let body = json!({
        "query": query,
        "type": "auto",
        "numResults": num,
        "contents": { "highlights": true },
    });

    let resp = client
        .post(EXA_API)
        .header("x-api-key", key)
        .header("x-exa-integration", "android-agent")
        .timeout(TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("连不上 Exa: {e}"))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // 别把 key 回显出去
        let safe = text.replace(key, "***");
        anyhow::bail!(
            "Exa API 返回 {status}：{}",
            safe.chars().take(300).collect::<String>()
        );
    }

    let v: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Exa API 的响应不是 JSON: {e}"))?;
    let mut hits = Vec::new();
    if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
        for item in arr.iter().take(num) {
            let Some(url) = item.get("url").and_then(|u| u.as_str()) else {
                continue;
            };
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .filter(|t| !t.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| host_of(url));
            let snippet = item
                .get("highlights")
                .and_then(|h| h.as_array())
                .map(|hl| {
                    hl.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .filter(|s| !s.trim().is_empty())
                .or_else(|| item.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_default();
            hits.push(Hit {
                title,
                url: url.to_string(),
                snippet: squeeze(&snippet),
            });
        }
    }
    Ok(Found {
        hits,
        provider: "exa-api",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Title: Newsroom\nURL: https://www.anthropic.com/news\n\
        Published: 2026-10-06T00:00:00.000Z\nAuthor: N/A\nHighlights:\n\
        #### Introducing Claude Haiku 5.5\n\nOur fastest model yet. …\n\n---\n\n\
        Title: Another\nURL: https://example.com/a\nHighlights:\nA &amp; B\n";

    #[test]
    fn mcp_text_blocks_become_hits() {
        let hits = parse_mcp_text(SAMPLE, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Newsroom");
        assert_eq!(hits[0].url, "https://www.anthropic.com/news");
        assert!(hits[0].snippet.contains("Introducing Claude Haiku 5.5"));
        assert!(!hits[0].snippet.contains("####"), "Markdown 标记要清掉");
        assert_eq!(hits[1].snippet, "A & B", "HTML 实体要还原");
    }

    #[test]
    fn mcp_text_respects_num() {
        assert_eq!(parse_mcp_text(SAMPLE, 1).len(), 1);
    }

    #[test]
    fn missing_title_falls_back_to_host() {
        let hits = parse_mcp_text("Title: \nURL: https://a.b/c/d\nHighlights:\nx\n", 5);
        assert_eq!(hits[0].title, "a.b");
        // MCP 那边没标题时写的就是这个
        let na = parse_mcp_text("Title: N/A\nURL: https://a.b/c/d\nHighlights:\nx\n", 5);
        assert_eq!(na[0].title, "a.b");
    }

    #[test]
    fn sse_and_plain_json_both_decode() {
        let sse = "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        assert_eq!(decode_rpc_body(sse).unwrap().pointer("/result/content/0/text").unwrap(), "hi");
        let plain = "{\"result\":{\"ok\":true}}";
        assert_eq!(decode_rpc_body(plain).unwrap()["result"]["ok"], true);
        assert!(decode_rpc_body("not json").is_err());
    }
}
