//! OpenAI **Responses API**（`POST {base}/responses`）。
//!
//! 这是我们**优先**使用的协议；不支持它的端点会自动退回 Chat Completions
//! （见 `llm.rs` 的 `ApiStyle`）。
//!
//! # 和 Chat Completions 的差别（我们关心到的部分）
//!
//! - 请求体是 `input: [...]` 一串**条目**，不是 `messages`。历史回放时：
//!   - user / system → `{"type":"message","role":…,"content":…}`
//!   - assistant 正文 → `{"type":"message","role":"assistant",
//!     "content":[{"type":"output_text","text":…}]}`
//!   - 工具调用 → `{"type":"function_call","call_id":…,"name":…,"arguments":…}`
//!   - 工具结果 → `{"type":"function_call_output","call_id":…,"output":…}`
//! - 工具表是**扁平**的：`{"type":"function","name":…,"parameters":…}`（不再
//!   像 Chat 那样套一层 `function`）。
//! - 图片是 `input_image`（`image_url` + `detail`），PDF 是 `input_file`
//!   （`file_data` + `filename`）。
//! - 流式是**一堆具名事件**（`response.output_text.delta` …），不是
//!   `choices[].delta`。
//!
//! 我们把这些事件**映射回 `openai::StreamChunk`** —— 这样 `agent.rs` 的
//! 流处理（思考/正文分块、工具参数按 index 累积、用量统计）一行都不用改，
//! 两条协议的差异全被关在这个文件里。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::openai::{
    CompletionTokensDetails, Delta, FunctionChunk, PromptTokensDetails, StreamChoice, StreamChunk,
    ToolCallChunk, Usage,
};

// ---------------------------------------------------------------------------
// 请求
// ---------------------------------------------------------------------------

/// `POST {base}/responses` 的请求体。
#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<InputItem>,
    /// 共享的那一份，同 `openai::ChatRequest::tools`
    pub tools: std::sync::Arc<[Tool]>,
    pub stream: bool,
    /// 不落服务端状态：我们靠完整回放历史（也才可控 prompt 前缀）。
    pub store: bool,
    /// 思考强度。Responses 里它是嵌套对象（Chat Completions 是顶层字符串）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
}

/// `reasoning: { effort: "…" }`。目前只用 `effort`；`summary` 之类等真需要再说。
#[derive(Debug, Clone, Serialize)]
pub struct Reasoning {
    pub effort: String,
}

/// `input` 里的一条。用 `#[serde(untagged)]`：几种形状靠各自字段区分，
/// 我们只序列化（构造哪种发哪种），不做反序列化。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum InputItem {
    Message(MessageItem),
    AssistantMessage(AssistantItem),
    FunctionCall(FunctionCallItem),
    FunctionCallOutput(FunctionCallOutputItem),
}

/// user / system / developer 消息。
#[derive(Debug, Clone, Serialize)]
pub struct MessageItem {
    #[serde(rename = "type")]
    pub kind: &'static str, // "message"
    pub role: String,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<InputPart>),
}

/// assistant 的正文回放。内容块是 `output_text`。
#[derive(Debug, Clone, Serialize)]
pub struct AssistantItem {
    #[serde(rename = "type")]
    pub kind: &'static str, // "message"
    pub role: &'static str, // "assistant"
    pub content: Vec<OutputPart>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum OutputPart {
    #[serde(rename = "output_text")]
    Text { text: String },
}

/// 输入内容块。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum InputPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image {
        /// 和缓存共享同一份（见 `imaging::cached_data_url`）
        image_url: std::sync::Arc<str>,
        detail: String,
    },
    #[serde(rename = "input_file")]
    File {
        file_data: std::sync::Arc<str>,
        filename: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallItem {
    #[serde(rename = "type")]
    pub kind: &'static str, // "function_call"
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCallOutputItem {
    #[serde(rename = "type")]
    pub kind: &'static str, // "function_call_output"
    pub call_id: String,
    pub output: ToolOutput,
}

/// 工具结果。Responses 允许把图片**直接放在工具结果里**
/// （`output` 是内容块数组），比 Chat 那边靠一条额外的 user carrier 干净得多。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ToolOutput {
    Text(String),
    Parts(Vec<InputPart>),
}

/// 工具表：扁平的 `{"type":"function","name":…,"parameters":…}`。
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub kind: &'static str, // "function"
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

// ---------------------------------------------------------------------------
// 流式事件 → openai::StreamChunk
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "response.output_text.delta")]
    TextDelta { delta: String },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta { delta: String },
    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningDelta { delta: String },
    #[serde(rename = "response.output_item.added")]
    ItemAdded { output_index: u32, item: Item },
    #[serde(rename = "response.output_item.done")]
    ItemDone { output_index: u32, item: Item },
    #[serde(rename = "response.function_call_arguments.delta")]
    ArgsDelta { output_index: u32, delta: String },
    #[serde(rename = "response.function_call_arguments.done")]
    ArgsDone {
        output_index: u32,
        arguments: String,
    },
    #[serde(rename = "response.completed")]
    Completed { response: ResponsePayload },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponsePayload },
    #[serde(rename = "response.failed")]
    Failed { response: ResponsePayload },
    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default)]
        code: Option<String>,
    },
    /// 其余事件（`response.created`、`response.in_progress`、各种 done…）忽略
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsePayload {
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
    #[serde(default)]
    output_tokens_details: Option<OutputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct OutputTokensDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

/// 事件 → `StreamChunk`。要跨事件记状态（某个工具调用是不是已经收过增量
/// 参数），所以是个 struct。
#[derive(Default)]
pub struct Mapper {
    /// 已经通过 `.delta` 收过参数的 `output_index` —— 对应的 `.done` 就别
    /// 再整体补一遍，否则参数会翻倍。
    saw_args_delta: HashSet<u32>,
}

impl Mapper {
    /// 一条 SSE 的 `data:` 载荷 →（可选的）`StreamChunk`。
    /// 返回 `Err` 会把错误透传给前端；返回 `Ok(None)` 表示这条事件忽略。
    pub fn map(&mut self, data: &str) -> anyhow::Result<Option<StreamChunk>> {
        let event: Event = match serde_json::from_str(data) {
            Ok(e) => e,
            // 未知/不认识的形状：跳过比整条流崩掉好
            Err(e) => {
                tracing::debug!("responses: 跳过无法解析的事件（{e}）: {}", clip(data));
                return Ok(None);
            }
        };

        let one = |delta: Delta| {
            Some(StreamChunk {
                choices: vec![StreamChoice {
                    delta,
                    finish_reason: None,
                }],
                usage: None,
                error: None,
            })
        };

        Ok(match event {
            Event::TextDelta { delta } => one(Delta {
                content: Some(delta),
                ..Delta::default()
            }),
            Event::ReasoningSummaryDelta { delta } | Event::ReasoningDelta { delta } => {
                one(Delta {
                    reasoning_content: Some(delta),
                    ..Delta::default()
                })
            }
            Event::ItemAdded { output_index, item } if item.kind == "function_call" => one(
                tool_delta(output_index, item.call_id, item.name, item.arguments),
            ),
            Event::ArgsDelta {
                output_index,
                delta,
            } => {
                self.saw_args_delta.insert(output_index);
                one(tool_delta(output_index, None, None, Some(delta)))
            }
            Event::ArgsDone {
                output_index,
                arguments,
            } => {
                // `insert` 返回 false = 之前已经有过参数（delta 或这一次）。
                // 记下来，后面的 `output_item.done` 就不会把整段参数再拼一遍 ——
                // 只发 done、不发 delta 的实现，原来会得到 `{…}{…}` 这种坏 JSON。
                if self.saw_args_delta.insert(output_index) {
                    one(tool_delta(output_index, None, None, Some(arguments)))
                } else {
                    None
                }
            }
            Event::ItemDone { output_index, item } if item.kind == "function_call" => {
                // 只有整段参数随 item 一起给的实现（没有 delta）才补
                if self.saw_args_delta.contains(&output_index) {
                    None
                } else {
                    one(tool_delta(
                        output_index,
                        item.call_id,
                        item.name,
                        item.arguments,
                    ))
                }
            }
            // 给一个 finish_reason：`pump_frames` 靠它认出「流是正常结束的」
            //（Responses 没有 `[DONE]`），没带用量的实现也不会被当成断流。
            Event::Completed { response } => finish_chunk(response, Some("stop".to_string())),
            Event::Incomplete { response } => {
                let reason = response
                    .incomplete_details
                    .as_ref()
                    .and_then(|d| d.reason.as_deref())
                    .unwrap_or("");
                // 输出被长度上限截断 → 复用 Chat 那边的 "length" 语义
                let fr = (reason == "max_output_tokens").then(|| "length".to_string());
                finish_chunk(response, fr)
            }
            Event::Failed { response } => Some(StreamChunk {
                choices: vec![],
                usage: None,
                error: Some(
                    response
                        .error
                        .unwrap_or_else(|| Value::String("response.failed".into())),
                ),
            }),
            Event::Error { message, code } => Some(StreamChunk {
                choices: vec![],
                usage: None,
                error: Some(match code {
                    Some(c) => Value::String(format!("{c}: {message}")),
                    None => Value::String(message),
                }),
            }),
            Event::Other => None,
            // 带 guard 的 ItemAdded/ItemDone 没匹配上（不是 function_call）
            _ => None,
        })
    }
}

fn tool_delta(
    index: u32,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
) -> Delta {
    Delta {
        tool_calls: Some(vec![ToolCallChunk {
            index,
            id,
            function: Some(FunctionChunk { name, arguments }),
        }]),
        ..Delta::default()
    }
}

/// `response.completed` / `response.incomplete`：带用量，可能带终止原因。
fn finish_chunk(response: ResponsePayload, finish_reason: Option<String>) -> Option<StreamChunk> {
    let usage = response.usage.map(convert_usage);
    if usage.is_none() && finish_reason.is_none() {
        return None;
    }
    Some(StreamChunk {
        choices: vec![StreamChoice {
            delta: Delta::default(),
            finish_reason,
        }],
        usage,
        error: None,
    })
}

/// Responses 的用量字段名和 Chat 不同，统一成我们内部的 `Usage`。
fn convert_usage(u: ResponsesUsage) -> Usage {
    Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.total_tokens,
        prompt_tokens_details: Some(PromptTokensDetails {
            cached_tokens: Some(u.input_tokens_details.map(|d| d.cached_tokens).unwrap_or(0)),
        }),
        completion_tokens_details: Some(CompletionTokensDetails {
            reasoning_tokens: Some(
                u.output_tokens_details
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0),
            ),
        }),
        prompt_cache_hit_tokens: None,
        prompt_cache_miss_tokens: None,
    }
}

fn clip(s: &str) -> &str {
    let end = s.char_indices().nth(200).map(|(i, _)| i).unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req_json(r: &ResponsesRequest) -> Value {
        serde_json::to_value(r).unwrap()
    }

    /// 把一串事件喂给 Mapper，拼出每个工具调用最终的参数文本。
    fn args_after(events: &[Value]) -> String {
        let mut m = Mapper::default();
        let mut args = String::new();
        for e in events {
            if let Some(c) = m.map(&e.to_string()).unwrap() {
                for ch in c.choices {
                    for tc in ch.delta.tool_calls.unwrap_or_default() {
                        if let Some(a) = tc.function.and_then(|f| f.arguments) {
                            args.push_str(&a);
                        }
                    }
                }
            }
        }
        args
    }

    #[test]
    fn arguments_given_only_at_done_are_not_repeated_by_item_done() {
        // 不发 delta、只在 done 里给整段参数的实现：args.done 和 output_item.done
        // 都带着参数。原来两边各拼一遍，得到 `{…}{…}`。
        let item = json!({ "type": "function_call", "call_id": "c1", "name": "bash",
                           "arguments": r#"{"command":"ls"}"# });
        let args = args_after(&[
            json!({ "type": "response.output_item.added", "output_index": 0,
                    "item": { "type": "function_call", "call_id": "c1", "name": "bash", "arguments": "" } }),
            json!({ "type": "response.function_call_arguments.done", "output_index": 0,
                    "arguments": r#"{"command":"ls"}"# }),
            json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
        ]);
        assert_eq!(args, r#"{"command":"ls"}"#);
    }

    #[test]
    fn completed_without_usage_still_marks_the_end_of_the_stream() {
        let mut m = Mapper::default();
        let c = m
            .map(&json!({ "type": "response.completed", "response": {} }).to_string())
            .unwrap()
            .expect("completed 必须产出一帧，pump_frames 靠它认出正常结束");
        assert!(c.choices[0].finish_reason.is_some());
    }

    #[test]
    fn user_message_and_parts_match_the_responses_wire_shape() {
        let r = ResponsesRequest {
            model: "gpt-5".into(),
            input: vec![
                InputItem::Message(MessageItem {
                    kind: "message",
                    role: "system".into(),
                    content: MessageContent::Text("你是助手".into()),
                }),
                InputItem::Message(MessageItem {
                    kind: "message",
                    role: "user".into(),
                    content: MessageContent::Parts(vec![
                        InputPart::Text {
                            text: "看图".into(),
                        },
                        InputPart::Image {
                            image_url: "data:image/png;base64,AAA".into(),
                            detail: "auto".into(),
                        },
                        InputPart::File {
                            file_data: "data:application/pdf;base64,BBB".into(),
                            filename: "a.pdf".into(),
                        },
                    ]),
                }),
            ],
            tools: vec![].into(),
            stream: true,
            store: false,
            reasoning: None,
        };
        let v = req_json(&r);
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["input"][0]["role"], "system");
        assert_eq!(v["input"][0]["type"], "message");
        assert_eq!(v["input"][0]["content"], "你是助手");
        assert_eq!(v["input"][1]["content"][1]["type"], "input_image");
        assert_eq!(
            v["input"][1]["content"][1]["image_url"],
            "data:image/png;base64,AAA"
        );
        assert_eq!(v["input"][1]["content"][2]["type"], "input_file");
        assert_eq!(v["input"][1]["content"][2]["filename"], "a.pdf");
    }

    #[test]
    fn assistant_and_tool_items_match_the_responses_wire_shape() {
        let r = ResponsesRequest {
            model: "gpt-5".into(),
            input: vec![
                InputItem::AssistantMessage(AssistantItem {
                    kind: "message",
                    role: "assistant",
                    content: vec![OutputPart::Text {
                        text: "我来看看".into(),
                    }],
                }),
                InputItem::FunctionCall(FunctionCallItem {
                    kind: "function_call",
                    call_id: "call_1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                }),
                InputItem::FunctionCallOutput(FunctionCallOutputItem {
                    kind: "function_call_output",
                    call_id: "call_1".into(),
                    output: ToolOutput::Text("ok".into()),
                }),
            ],
            tools: vec![Tool {
                kind: "function",
                name: "bash".into(),
                description: "跑命令".into(),
                parameters: json!({ "type": "object" }),
            }]
            .into(),
            stream: true,
            store: false,
            reasoning: None,
        };
        let v = req_json(&r);
        assert_eq!(v["input"][0]["role"], "assistant");
        assert_eq!(v["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(v["input"][0]["content"][0]["text"], "我来看看");
        assert_eq!(v["input"][1]["type"], "function_call");
        assert_eq!(v["input"][1]["call_id"], "call_1");
        assert_eq!(v["input"][2]["type"], "function_call_output");
        // 工具表是扁平的：name 直接在 function 项上，没有嵌套
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["name"], "bash");
        assert!(v["tools"][0].get("function").is_none());
    }

    #[test]
    fn text_and_reasoning_deltas_map_to_content() {
        let mut m = Mapper::default();
        let c = m
            .map(r#"{"type":"response.output_text.delta","delta":"你好"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("你好"));
        let c = m
            .map(r#"{"type":"response.reasoning_summary_text.delta","delta":"想"}"#)
            .unwrap()
            .unwrap();
        assert_eq!(c.choices[0].delta.reasoning_text(), Some("想"));
    }

    #[test]
    fn function_call_deltas_accumulate_by_output_index() {
        let mut m = Mapper::default();
        let added = m
            .map(
                r#"{"type":"response.output_item.added","output_index":1,
                    "item":{"type":"function_call","call_id":"call_9","name":"bash","arguments":""}}"#,
            )
            .unwrap()
            .unwrap();
        let tc = &added.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, 1);
        assert_eq!(tc.id.as_deref(), Some("call_9"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("bash"));

        let d = m
            .map(
                r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"command\""}"#,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            d.choices[0].delta.tool_calls.as_ref().unwrap()[0]
                .function
                .as_ref()
                .unwrap()
                .arguments
                .as_deref(),
            Some("{\"command\"")
        );

        // 已经收过 delta → `.done` 不能再来一遍（否则参数翻倍）
        assert!(m
            .map(r#"{"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"command\":\"ls\"}"}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn completed_event_carries_usage_and_incomplete_means_truncated() {
        let mut m = Mapper::default();
        let c = m
            .map(
                r#"{"type":"response.completed","response":{"usage":{
                    "input_tokens":100,"output_tokens":20,"total_tokens":120,
                    "input_tokens_details":{"cached_tokens":80},
                    "output_tokens_details":{"reasoning_tokens":5}}}}"#,
            )
            .unwrap()
            .unwrap();
        let u = c.usage.unwrap();
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 20);
        assert_eq!(u.cached_tokens(), Some(80));
        assert_eq!(u.reasoning_tokens(), Some(5));
        assert_eq!(u.cache_hit_ratio(), Some(0.8));

        let c = m
            .map(
                r#"{"type":"response.incomplete","response":{
                    "incomplete_details":{"reason":"max_output_tokens"}}}"#,
            )
            .unwrap()
            .unwrap();
        assert_eq!(c.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn error_events_surface_instead_of_silently_ending() {
        let mut m = Mapper::default();
        let c = m
            .map(r#"{"type":"error","code":"rate_limit_exceeded","message":"slow down"}"#)
            .unwrap()
            .unwrap();
        assert!(c.error.is_some());
    }
}
