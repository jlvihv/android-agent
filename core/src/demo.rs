//! 演示用的脚本化 chunk 序列。
//!
//! 目的：在**没有 API key、没有网络**的情况下，把这条链路完整跑通并验证：
//!
//! ```text
//! Rust sse 编码器 → axum SSE → HTTP → TS parseUiStream → applyChunk → UI
//! ```
//!
//! 顺便演示人工审批流（`tool-approval-request`）长什么样。
//! 阶段 2 接入真实 LLM 后，这个模块只在 `--demo` 下使用。

use serde_json::{json, Value};

use crate::sse;

/// 返回一条完整的、符合 AI SDK UI Message Stream 规范的 chunk 序列。
#[allow(clippy::vec_init_then_push)]
pub fn scripted(prompt: &str) -> Vec<Value> {
    let msg_id = uuid::Uuid::now_v7().to_string();
    let mut out = Vec::new();

    out.push(sse::start(&msg_id));
    out.push(sse::start_step());

    // 思考过程
    out.push(sse::reasoning_start("r1"));
    out.push(sse::reasoning_delta(
        "r1",
        "用户想让我操作手机。先读一次当前界面的 UI 树，",
    ));
    out.push(sse::reasoning_delta("r1", "看清有哪些可点的元素再决定下一步。"));
    out.push(sse::reasoning_end("r1"));

    // 正文
    out.push(sse::text_start("t1"));
    let reply = if prompt.trim().is_empty() {
        "这是一条**演示流**，没有调用任何 LLM。".to_string()
    } else {
        format!("收到：`{prompt}`\n\n（演示流，未调用 LLM）")
    };
    for piece in chunk_text(&reply, 6) {
        out.push(sse::text_delta("t1", &piece));
    }
    out.push(sse::text_end("t1"));

    // 工具调用 1：只读，自动执行
    out.push(sse::tool_input_start("call_snap", "device_snapshot"));
    out.push(sse::tool_input_available(
        "call_snap",
        "device_snapshot",
        json!({}),
    ));
    out.push(sse::tool_output_available(
        "call_snap",
        json!({ "package": "com.example.launcher", "nodes": 5 }),
    ));

    // 工具调用 2：有副作用，需要人工审批
    out.push(sse::tool_input_start("call_tap", "device_tap"));
    out.push(sse::tool_input_available(
        "call_tap",
        "device_tap",
        json!({ "i": 2, "label": "打开设置" }),
    ));
    out.push(sse::tool_approval_request(
        "call_tap",
        "appr_1",
        "点击会离开当前应用，需要你确认",
    ));

    out.push(sse::finish_step());
    out.push(sse::finish());
    out
}

/// 按字符切分，尽量不切在 UTF-8 边界外（这里按 char 切，天然安全）
fn chunk_text(s: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    chars
        .chunks(n.max(1))
        .map(|c| c.iter().collect::<String>())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_starts_and_finishes_correctly() {
        let chunks = scripted("测试");
        assert_eq!(chunks[0]["type"], "start");
        assert_eq!(chunks.last().unwrap()["type"], "finish");
    }

    #[test]
    fn script_contains_a_tool_call_requiring_approval() {
        let chunks = scripted("");
        let approvals: Vec<_> = chunks
            .iter()
            .filter(|c| c["type"] == "tool-approval-request")
            .collect();
        assert_eq!(approvals.len(), 1);
        assert!(approvals[0].get("isAutomatic").is_none());
    }

    #[test]
    fn every_chunk_has_a_type_field() {
        for c in scripted("x") {
            assert!(c.get("type").and_then(|t| t.as_str()).is_some(), "{c}");
        }
    }

    #[test]
    fn text_chunks_reassemble_to_original() {
        let s = "你好，世界！这是一段中文测试文本。";
        let joined: String = chunk_text(s, 6).concat();
        assert_eq!(joined, s);
    }
}
