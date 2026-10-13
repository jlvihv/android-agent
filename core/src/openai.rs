//! 自研的 OpenAI 兼容 Chat Completions 协议。
//!
//! # 为什么不用 `async-openai`
//!
//! 原本（决策 D4）是「不手写协议」，用 `async-openai`。后来发现它有两个
//! 我们控制不了的问题：
//!
//! 1. `Client::with_config()` 内部**先** `reqwest::Client::new()` 造一个默认
//!    连接，然后才轮到我们 `with_http_client` 覆盖它。在
//!    `rustls-no-provider` 下这个默认连接没有 crypto provider，直接 panic
//!    —— 而它建的这个连接我们根本拿不到。
//! 2. 我们真正用到的只有两样：一个请求体 + 一条 SSE 流。请求体本来就是我们
//!    自己拼的（图片 carrier、prompt cache 的前缀不变式都在我们这边），
//!    流式 chunk 也必须是我们自己的类型（要接住 DeepSeek 的
//!    `reasoning_content`，它自带的类型会丢）。
//!
//! 手写之后少一个大依赖（安卓 .so 也小一点），而且每个字节都在我们手里 ——
//! 这对「请求体逐字节可复现、从而命中 prompt cache」是好事。
//!
//! # 覆盖范围
//!
//! 任何 OpenAI 兼容端点：OpenAI / DeepSeek / Kimi / DashScope / 智谱 /
//! SiliconFlow / OpenRouter / Groq / xAI / vLLM / LM Studio / Ollama(`/v1`)，
//! 以及绝大多数中转站。Anthropic / Gemini 的原生协议不支持（要走中转）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// `POST {base_url}/chat/completions` 的请求体。
///
/// 字段顺序固定，序列化是确定性的 —— prompt cache 靠这个。
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    /// 共享的那一份（`agent::tool_definitions`）：每一步都要带上整张表，
    /// 不必每步重新拼 schema。
    pub tools: std::sync::Arc<[Tool]>,
    pub stream: bool,
    pub stream_options: StreamOptions,
    /// 思考强度（`minimal` / `low` / `medium` / `high`）。
    ///
    /// `None` 时**整个字段都不发**：有些网关见到不认识的参数会直接 400，
    /// 而默认档（什么都不发）才是所有端点的最大公约数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// 一条对话消息。序列化成 `{"role": "user", …}`。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        /// 只有工具调用、没有正文时省略（`null` 有些端点会 400）
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// 这一步的思考内容，**原样回传**（DeepSeek 系的非标准字段）。
        ///
        /// DeepSeek 的思考模式：请求带了 `tools`，之后每一次请求都要把所有
        /// assistant 消息的 `reasoning_content` 完整传回（没调工具的那步也算），
        /// 不传就 400；而且不传的话模型每一步都看不到自己上一步想了什么。
        /// 不返回思考内容的模型（OpenAI 的 Chat Completions 就不给）历史里本来
        /// 就是空的，这个字段自然不出现。
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// 用户消息的内容：纯文本，或一组内容块（图片 / PDF 只能走这里）。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<Part>),
}

/// 一个内容块。`type` 由 variant 名自动转 snake_case。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}`
    ImageUrl {
        image_url: ImageUrl,
    },
    /// `{"type":"file","file":{"file_data":"data:application/pdf;base64,…","filename":"x.pdf"}}`
    File {
        file: FileData,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageUrl {
    /// 和缓存共享同一份（几 MB 的 base64），见 `imaging::cached_data_url`
    pub url: std::sync::Arc<str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileData {
    pub file_data: std::sync::Arc<str>,
    pub filename: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// 工具表里的一项：`{"type":"function","function":{…}}`。
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

// ---------------------------------------------------------------------------
// 响应（流式）
// ---------------------------------------------------------------------------
//
// BYOT：不用任何 SDK 自带的 chunk 类型 —— 它们只有 content / tool_calls /
// role，没有 `reasoning_content`（DeepSeek 系）也没有 `reasoning`（GLM）的
// 位置，思考过程会被静默丢掉。

#[derive(Debug, Default, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    /// 开了 `stream_options.include_usage` 后，倒数第二个 chunk 带完整用量
    #[serde(default)]
    pub usage: Option<Usage>,
    /// 网关可能在流里直接发 `{"error": {…}}`（比如限流/上下文超限）。
    /// 不接的话会被当成一个空 chunk 忽略，前端看着像「卡住不回复」。
    #[serde(default)]
    pub error: Option<Value>,
    /// Responses API 的一个思考条目（`type: "reasoning"` 的输出项），**原样**。
    ///
    /// 下一次请求要原样放回 input（见 `agent::build_responses_input`）：不放的话
    /// 模型每一步都看不到自己上一步想了什么。Chat Completions 没有这个东西
    /// （那边的思考内容走 `reasoning_content`）。
    #[serde(skip)]
    pub reasoning_item: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek 系非标准字段
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// 另一些模型（如 GLM）用这个
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallChunk>>,
}

impl Delta {
    pub fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Deserialize)]
pub struct ToolCallChunk {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionChunk>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionChunk {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// token 用量。
///
/// 缓存命中字段有**两套写法**，都要接：
/// - OpenAI 系：`prompt_tokens_details.cached_tokens`
/// - DeepSeek 系：`prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`
///
/// （实测 OpenCode Go 的 `deepseek-v4.1-flash` 走的是**前者**。）
///
/// 这两个数字是验证 prompt cache 真的命中的唯一手段 —— 缓存命中价差 50 倍，
/// 不能只靠"我写对了"来相信。
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct CompletionTokensDetails {
    /// 推理 token。DeepSeek 系按 output 计费，所以它直接影响成本。
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl Usage {
    /// 命中的缓存 token 数。
    ///
    /// 两套字段都接。同时出现时取**大**的那个 —— 之前用 `.or()`，
    /// 只要 OpenAI 系先报了 `cached_tokens: 0`（有些网关冷启动会填 0
    /// 而不是省略），DeepSeek 系报的真值就被吞掉，日志变成「命中 0%」，
    /// 而命中率正是判断缓存有没有生效的唯一硬证据。
    pub fn cached_tokens(&self) -> Option<u32> {
        let openai = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens);
        match (openai, self.prompt_cache_hit_tokens) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
    }

    /// 缓存命中率。provider 不报缓存字段时返回 None。
    ///
    /// 夹到 `[0,1]`：`prompt_tokens` 含不含缓存部分各厂商定义不同，
    /// 有的实现里它是 miss-only，直接相除会得到 >100%。
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let hit = self.cached_tokens()? as f64;
        if self.prompt_tokens == 0 {
            return None;
        }
        Some((hit / self.prompt_tokens as f64).clamp(0.0, 1.0))
    }
}

// ---------------------------------------------------------------------------
// SSE 解析
// ---------------------------------------------------------------------------

/// 把一条 SSE 响应体读成一串 `StreamChunk`。
///
/// `map` 把每条事件的 `data:` 载荷变成 chunk（两条协议的差异都在这里面）。
/// 空载荷、注释帧、`[DONE]` 都在这里统一处理 —— 两种协议一样。
///
/// 自己缓冲：网络分片会把一帧切在中间。帧分隔是空行（`\n\n`）。
pub async fn pump_frames<F>(
    resp: reqwest::Response,
    tx: tokio::sync::mpsc::UnboundedSender<anyhow::Result<StreamChunk>>,
    mut map: F,
) -> anyhow::Result<()>
where
    F: FnMut(&str) -> anyhow::Result<Option<StreamChunk>>,
{
    use tokio_stream::StreamExt;

    let mut stream = resp.bytes_stream();
    // **按字节**攒，凑齐一帧再解码。原来是每个网络分片各自
    // `from_utf8_lossy` —— 一个汉字的三个字节被切在两片里，两边都变成 `�`，
    // 回复里时不时冒出乱码。帧边界（空行）是纯 ASCII，切字节是安全的。
    let mut buf: Vec<u8> = Vec::new();
    // 收到过结束标记没有：`[DONE]`（Chat），或者带 finish_reason / 用量的那一帧
    // （Responses 没有 `[DONE]`，`response.completed` 映射成后者）。
    let mut terminal = false;
    let mut on_chunk = |data: &str| -> anyhow::Result<Option<StreamChunk>> {
        let c = map(data)?;
        if let Some(c) = &c {
            terminal |= c.usage.is_some() || c.choices.iter().any(|ch| ch.finish_reason.is_some());
        }
        Ok(c)
    };
    loop {
        let Some(next) = stream.next().await else {
            break;
        };
        buf.extend_from_slice(&next?);

        while let Some((end, sep)) = frame_end(&buf) {
            let frame = String::from_utf8_lossy(&buf[..end]).into_owned();
            buf.drain(..end + sep);
            if handle_frame(&frame, &tx, &mut on_chunk)? {
                return Ok(());
            }
        }
    }
    // 有些服务端在结尾不发空行
    if !buf.iter().all(u8::is_ascii_whitespace) {
        let frame = String::from_utf8_lossy(&buf).into_owned();
        if handle_frame(&frame, &tx, &mut on_chunk)? {
            return Ok(());
        }
    }
    // 连接在结束标记之前断了（切网络、网关超时）。不报错的话，这半截回复
    // 会被当成完整的一步：半截的工具参数照样拿去执行，回合「正常」结束。
    //
    // 原来这里有一行 `drop(on_chunk)`：`on_chunk` 闭包可变借走了 `terminal`，
    // 想手动在读 `terminal` 前把借还了。现在不需要 —— NLL 在闭包最后一次使用
    // （上面那个 `handle_frame`）之后就把借结束了；而 clippy 的 `drop_non_drop`
    // 还会报它（闭包不是 `Drop` 类型，那句只是延长了自己的生命周期）。
    anyhow::ensure!(terminal, "连接在回复结束前断开了");
    Ok(())
}

/// 找第一个帧边界（空行）：返回 (帧长度, 分隔符长度)。`\n\n` 和 `\r\n\r\n` 都认。
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) if b < a => Some((b, 4)),
        (Some(a), _) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

/// 处理一帧。返回 `true` 表示收到 `[DONE]` 或消费端没了。
fn handle_frame<F>(
    frame: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<anyhow::Result<StreamChunk>>,
    map: &mut F,
) -> anyhow::Result<bool>
where
    F: FnMut(&str) -> anyhow::Result<Option<StreamChunk>>,
{
    let mut data = String::new();
    for raw in frame.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with(':') {
            continue; // SSE 注释（keep-alive 之类）
        }
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.strip_prefix(' ').unwrap_or(rest);
        if !data.is_empty() {
            data.push('\n');
        }
        data.push_str(rest);
    }

    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }
    if let Some(chunk) = map(data)? {
        // 消费端没了（前端断连）就停，不用继续读
        return Ok(tx.send(Ok(chunk)).is_err());
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 把几段字节当成网络分片，跑一遍 `pump_frames`，收回解析出的正文和结果。
    async fn pump(pieces: Vec<&'static [u8]>) -> (String, anyhow::Result<()>) {
        let stream = tokio_stream::iter(pieces.into_iter().map(Ok::<_, std::io::Error>));
        let resp = reqwest::Response::from(
            http::Response::builder()
                .body(reqwest::Body::wrap_stream(stream))
                .unwrap(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let r = pump_frames(resp, tx, |d| {
            Ok(Some(serde_json::from_str::<StreamChunk>(d)?))
        })
        .await;
        let mut text = String::new();
        while let Ok(Ok(c)) = rx.try_recv() {
            for ch in c.choices {
                text.push_str(ch.delta.content.as_deref().unwrap_or(""));
            }
        }
        (text, r)
    }

    #[tokio::test]
    async fn a_character_split_across_network_chunks_is_not_garbled() {
        // 「你」= E4 BD A0，切在第二个字节后面
        let (text, r) = pump(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"\xE4\xBD",
            b"\xA0\"}}]}\r\n\r\ndata: [DONE]\n\n",
        ])
        .await;
        r.unwrap();
        assert_eq!(text, "你");
    }

    #[tokio::test]
    async fn a_stream_cut_before_the_end_is_an_error_not_a_short_reply() {
        let (text, r) = pump(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"half\"}}]}\n\n",
        ])
        .await;
        assert_eq!(text, "half");
        assert!(r.is_err(), "没有 [DONE] 也没有 finish_reason，必须当断流");

        let (_, r) = pump(vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n",
        ])
        .await;
        r.unwrap();
    }

    #[test]
    fn user_text_message_is_a_bare_string() {
        let m = Message::User {
            content: UserContent::Text("你好".into()),
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({ "role": "user", "content": "你好" })
        );
    }

    #[test]
    fn image_and_file_parts_match_the_openai_wire_shape() {
        let m = Message::User {
            content: UserContent::Parts(vec![
                Part::Text {
                    text: "看图".into(),
                },
                Part::ImageUrl {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,AAA".into(),
                    },
                },
                Part::File {
                    file: FileData {
                        file_data: "data:application/pdf;base64,BBB".into(),
                        filename: "a.pdf".into(),
                    },
                },
            ]),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "user");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][1]["type"], "image_url");
        assert_eq!(
            v["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAA"
        );
        assert_eq!(v["content"][2]["type"], "file");
        assert_eq!(v["content"][2]["file"]["filename"], "a.pdf");
    }

    #[test]
    fn assistant_with_only_tool_calls_omits_content() {
        let m = Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                },
            }],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "assistant");
        assert!(v.get("content").is_none(), "没有正文时不能带 content: null");
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "bash");
    }

    #[test]
    fn tool_message_carries_tool_call_id() {
        let m = Message::Tool {
            tool_call_id: "c1".into(),
            content: "ok".into(),
        };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "ok" })
        );
    }

    #[test]
    fn sse_frames_are_parsed_and_done_stops() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut map = |data: &str| {
            let c: StreamChunk = serde_json::from_str(data)?;
            Ok(Some(c))
        };
        assert!(!handle_frame(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}",
            &tx,
            &mut map
        )
        .unwrap());
        assert!(handle_frame("data: [DONE]", &tx, &mut map).unwrap());
        // 注释帧 / 空帧不该产出东西
        assert!(!handle_frame(": keep-alive", &tx, &mut map).unwrap());
        assert!(!handle_frame("", &tx, &mut map).unwrap());
        let got = rx.try_recv().unwrap().unwrap();
        assert_eq!(got.choices[0].delta.content.as_deref(), Some("你"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn usage_understands_both_cache_field_styles() {
        // OpenAI 系（实测 OpenCode Go 的 deepseek-v4.1-flash 用这个）
        let openai: Usage = serde_json::from_value(json!({
            "prompt_tokens": 2138,
            "completion_tokens": 139,
            "prompt_tokens_details": { "cached_tokens": 2048 },
            "completion_tokens_details": { "reasoning_tokens": 136 }
        }))
        .unwrap();
        assert_eq!(openai.cached_tokens(), Some(2048));
        assert_eq!(openai.reasoning_tokens(), Some(136));
        let r = openai.cache_hit_ratio().unwrap();
        assert!((r - 0.958).abs() < 0.001, "{r}");

        // DeepSeek 系
        let ds: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        }))
        .unwrap();
        assert_eq!(ds.cached_tokens(), Some(80));
        assert_eq!(ds.cache_hit_ratio(), Some(0.8));

        // 两套字段同时出现、值不一致时取**大**的那个。
        let both: Usage = serde_json::from_value(json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": { "cached_tokens": 0 },
            "prompt_cache_hit_tokens": 768
        }))
        .unwrap();
        assert_eq!(both.cached_tokens(), Some(768));
        assert_eq!(both.cache_hit_ratio(), Some(0.768));

        // 命中率夹在 0..1
        let weird: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "prompt_cache_hit_tokens": 250
        }))
        .unwrap();
        assert_eq!(weird.cache_hit_ratio(), Some(1.0));

        // provider 什么都不报时不能瞎猜
        let unknown: Usage = serde_json::from_value(json!({ "prompt_tokens": 50 })).unwrap();
        assert_eq!(unknown.cache_hit_ratio(), None);
    }
}
