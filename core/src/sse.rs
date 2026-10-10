//! AI SDK UI Message Stream 编码器（决策 D7）。
//!
//! 不自己发明流式协议 —— 按 Vercel AI SDK 的公开规范输出 SSE：
//! <https://ai-sdk.dev/docs/ai-sdk-ui/stream-protocol>
//!
//! 好处：
//! - 前端换任何 AI SDK 兼容客户端都能直接吃。我们选了 W2（自己写解析器，
//!   见 `web/src/lib/sse.ts`），但协议是标准的，以后想换回 `@ai-sdk/svelte`
//!   Rust 侧一行都不用改。
//! - `tool-approval-request` / `tool-approval-response` 正好是我们要的人工审批流
//! - reasoning / step / data-* 语义已经想好了，不用重新设计
//!
//! 响应必须带 header `x-vercel-ai-ui-message-stream: v1`，流以 `[DONE]` 结尾。
//!
//! 这里只产出 **chunk 本身**（`serde_json::Value`），不负责传输格式：
//! - HTTP 走 `axum::response::sse`，用 `SseEvent::default().data(v.to_string())`
//! - WebSocket 直接把这几个 Value 塞进 `Event::Agent { chunk }`
//! - 需要裸 SSE 文本时用 `frame()`

use serde_json::{json, Value};

pub const STREAM_HEADER: &str = "x-vercel-ai-ui-message-stream";
pub const STREAM_HEADER_VALUE: &str = "v1";

/// 流终止标记
pub const DONE: &str = "[DONE]";

/// 把 chunk 包成一帧裸 SSE 文本（调试 / 测试 / 非 axum 传输用）
pub fn frame(v: &Value) -> String {
    format!("data: {v}\n\n")
}

/// 终止帧
pub fn done_frame() -> &'static str {
    "data: [DONE]\n\n"
}

// --- 消息 / 步骤生命周期 ---------------------------------------------------

pub fn start(message_id: &str) -> Value {
    json!({ "type": "start", "messageId": message_id })
}

pub fn start_step() -> Value {
    json!({ "type": "start-step" })
}

pub fn finish_step() -> Value {
    json!({ "type": "finish-step" })
}

pub fn finish() -> Value {
    json!({ "type": "finish" })
}

pub fn abort(reason: &str) -> Value {
    json!({ "type": "abort", "reason": reason })
}

pub fn error(text: &str) -> Value {
    json!({ "type": "error", "errorText": text })
}

// --- 正文 -----------------------------------------------------------------

pub fn text_start(id: &str) -> Value {
    json!({ "type": "text-start", "id": id })
}

pub fn text_delta(id: &str, delta: &str) -> Value {
    json!({ "type": "text-delta", "id": id, "delta": delta })
}

pub fn text_end(id: &str) -> Value {
    json!({ "type": "text-end", "id": id })
}

// --- 思考过程 -------------------------------------------------------------

pub fn reasoning_start(id: &str) -> Value {
    json!({ "type": "reasoning-start", "id": id })
}

pub fn reasoning_delta(id: &str, delta: &str) -> Value {
    json!({ "type": "reasoning-delta", "id": id, "delta": delta })
}

pub fn reasoning_end(id: &str) -> Value {
    json!({ "type": "reasoning-end", "id": id })
}

// --- 工具调用 -------------------------------------------------------------

pub fn tool_input_start(tool_call_id: &str, tool_name: &str) -> Value {
    json!({
        "type": "tool-input-start",
        "toolCallId": tool_call_id,
        "toolName": tool_name,
    })
}

pub fn tool_input_delta(tool_call_id: &str, input_text_delta: &str) -> Value {
    json!({
        "type": "tool-input-delta",
        "toolCallId": tool_call_id,
        "inputTextDelta": input_text_delta,
    })
}

pub fn tool_input_available(tool_call_id: &str, tool_name: &str, input: Value) -> Value {
    json!({
        "type": "tool-input-available",
        "toolCallId": tool_call_id,
        "toolName": tool_name,
        "input": input,
    })
}

pub fn tool_output_available(tool_call_id: &str, output: Value) -> Value {
    json!({
        "type": "tool-output-available",
        "toolCallId": tool_call_id,
        "output": output,
    })
}

pub fn tool_output_denied(tool_call_id: &str) -> Value {
    json!({ "type": "tool-output-denied", "toolCallId": tool_call_id })
}

/// 请求人工审批。**省略 `isAutomatic`** 即表示需要显式批准。
pub fn tool_approval_request(tool_call_id: &str, approval_id: &str, reason: &str) -> Value {
    json!({
        "type": "tool-approval-request",
        "toolCallId": tool_call_id,
        "approvalId": approval_id,
        "reason": reason,
    })
}

pub fn tool_approval_response(approval_id: &str, approved: bool, reason: &str) -> Value {
    json!({
        "type": "tool-approval-response",
        "approvalId": approval_id,
        "approved": approved,
        "reason": reason,
    })
}

// --- 自定义数据 -----------------------------------------------------------

/// `data-*` 通道：推设备快照、日志等非 LLM 内容，前端 reducer 同样处理。
pub fn data_part(name: &str, data: Value) -> Value {
    json!({ "type": format!("data-{name}"), "data": data })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_valid_sse() {
        let f = frame(&text_delta("t1", "Hello"));
        assert!(f.starts_with("data: "));
        assert!(f.ends_with("\n\n"));

        let body = f.trim_start_matches("data: ").trim();
        let v: Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["type"], "text-delta");
        assert_eq!(v["id"], "t1");
        assert_eq!(v["delta"], "Hello");
    }

    #[test]
    fn done_marker_matches_spec() {
        assert_eq!(done_frame(), "data: [DONE]\n\n");
        assert_eq!(DONE, "[DONE]");
    }

    #[test]
    fn approval_request_omits_is_automatic() {
        let v = tool_approval_request("c1", "a1", "needs review");
        assert_eq!(v["type"], "tool-approval-request");
        assert_eq!(v["toolCallId"], "c1");
        assert_eq!(v["approvalId"], "a1");
        assert!(
            v.get("isAutomatic").is_none(),
            "省略 isAutomatic 才是需要显式批准"
        );
    }

    #[test]
    fn data_parts_use_the_data_prefix() {
        let v = data_part("snapshot", json!({ "nodes": 3 }));
        assert_eq!(v["type"], "data-snapshot");
        assert_eq!(v["data"]["nodes"], 3);
    }
}
