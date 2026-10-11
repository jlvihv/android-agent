//! Agent 主循环。
//!
//! 决策 D5 选了「自研薄 loop」而不是 `rig`，原因就在这个文件里：
//! 每一个事件都要精确地转成 AI SDK UI Message Stream 的 chunk 推给前端，
//! 而且工具调用要能**暂停等人审批**。框架会把 loop 包起来，反而挡路。
//!
//! 工具分两类：
//! - `ReadOnly`：自动执行，结果直接回灌给模型，循环继续
//! - `NeedsApproval`：发 `tool-approval-request` 后**整个回合结束**，
//!   等前端调 `/api/approve` 再从这个点继续
//!
//! 这样设计是为了让服务端**无长驻状态**：手机 App 随时可能被杀，
//! 审批可能发生在几分钟之后。

use std::collections::HashMap;
use std::path::PathBuf;

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessageArgs, ChatCompletionTool, ChatCompletionTools,
    CreateChatCompletionRequestArgs, FunctionCall, FunctionObjectArgs,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::llm::Provider;
use crate::state::{Command, UiSnapshot};
use crate::{linux, sse, Core};

// ---------------------------------------------------------------------------
// BYOT：自己的流式 chunk 类型
// ---------------------------------------------------------------------------
//
// 为什么不用 async-openai 自带的 `CreateChatCompletionStreamResponse`：
// 它的 `delta` 只有 content / tool_calls / role / refusal，**没有
// `reasoning_content`，也没有 `#[serde(flatten)] extra`** —— DeepSeek 系
// 模型的思考过程会被静默丢掉。
//
// `create_stream_byot` 是官方给的逃生口（T0: Serialize, R: DeserializeOwned），
// 需要开 `byot` feature。

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    /// 开了 `stream_options.include_usage` 后，倒数第二个 chunk 带完整用量
    #[serde(default)]
    usage: Option<Usage>,
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
    /// 命中的缓存 token 数（两套字段任取其一）
    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
    }

    pub fn reasoning_tokens(&self) -> Option<u32> {
        self.completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
    }

    /// 缓存命中率。provider 不报缓存字段时返回 None。
    pub fn cache_hit_ratio(&self) -> Option<f64> {
        let hit = self.cached_tokens()? as f64;
        if self.prompt_tokens == 0 {
            return None;
        }
        Some(hit / self.prompt_tokens as f64)
    }
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    /// DeepSeek 系非标准字段
    #[serde(default)]
    reasoning_content: Option<String>,
    /// 另一些模型（如 GLM）用这个
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallChunk>>,
}

impl Delta {
    fn reasoning_text(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionChunk>,
}

#[derive(Debug, Deserialize)]
struct FunctionChunk {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ---------------------------------------------------------------------------
// 工具注册表
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// 只读
    ReadOnly,
    /// 有副作用。
    ///
    /// 默认**自动执行**（`AppState::auto_approve` 为 true）。
    /// 把 auto_approve 关掉就恢复成逐个人工审批。
    SideEffecting,
}

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub kind: ToolKind,
    pub schema: fn() -> Value,
}

pub fn tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "device_snapshot",
            description: "读取当前手机界面的 UI 树。返回节点列表，每个节点有索引 i、类名 c、文本 t、bounds b 和可交互标志 a。要点击任何东西之前必须先调用它。",
            kind: ToolKind::ReadOnly,
            schema: || json!({ "type": "object", "properties": {}, "required": [] }),
        },
        ToolSpec {
            name: "device_tap",
            description: "点击 UI 树中索引为 i 的节点。需要用户批准。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "i": { "type": "integer", "description": "device_snapshot 返回的节点索引" } },
                    "required": ["i"]
                })
            },
        },
        ToolSpec {
            name: "device_type",
            description: "向索引为 i 的输入框写入文本。需要用户批准。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "i": { "type": "integer" },
                        "text": { "type": "string" }
                    },
                    "required": ["i", "text"]
                })
            },
        },
        ToolSpec {
            name: "device_key",
            description: "按系统按键。name 取值：back / home / recents。需要用户批准。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "name": { "type": "string", "enum": ["back", "home", "recents"] } },
                    "required": ["name"]
                })
            },
        },
        ToolSpec {
            name: "bash",
            description: "在设备上的 Alpine Linux 沙箱里执行 shell 命令。返回 stdout 和 stderr。输出截断到**最后** 2000 行或 50KB（先到者为准）；截断时完整输出会存到临时文件并返回路径。可选 timeout（秒，默认 60）。每次调用都是全新的进程：文件系统持久，shell 状态（变量、cwd、后台任务）不持久；cwd 是 /root。要点：ICMP/ping 被封，用 curl/wget；装 Python 包优先 `apk add py3-*` 而不是 pip（musllinux_aarch64 的 wheel 很少）；matplotlib 画图前先 `matplotlib.use('Agg')`；后台服务必须重定向 stdout/stderr，否则调用不会返回；搜索用 rg（`apk add ripgrep`），`rg -uuu` 才包含隐藏和忽略的文件。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout": { "type": "number", "description": "超时秒数（可选，默认 60）" }
                    },
                    "required": ["command"]
                })
            },
        },
        ToolSpec {
            name: "edit",
            description: "用一个或多个精确文本替换来编辑单个文件。每个 edits[].oldText 必须匹配**原始文件**中唯一且不重叠的区域：改同一处或相邻行时合并成一条，不要发出重叠或嵌套的 edits，也不要为了够到远处的改动而在 oldText 里塞一大段没变的内容。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 /root）" },
                        "edits": {
                            "type": "array",
                            "description": "一条或多条替换",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string", "description": "要替换的原文；必须在原文件中唯一，且不能与其他 edits[].oldText 重叠" },
                                    "newText": { "type": "string", "description": "替换成的内容" }
                                },
                                "required": ["oldText", "newText"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                })
            },
        },
        ToolSpec {
            name: "device_launch",
            description: "启动一个应用。需要用户批准。",
            kind: ToolKind::SideEffecting,
            schema: || {
                json!({
                    "type": "object",
                    "properties": { "package": { "type": "string", "description": "包名，如 com.android.settings" } },
                    "required": ["package"]
                })
            },
        },
    ]
}

fn find_tool(name: &str) -> Option<ToolSpec> {
    tools().into_iter().find(|t| t.name == name)
}

// ---------------------------------------------------------------------------
// 会话状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant {
        content: String,
        reasoning: String,
        tool_calls: Vec<RecordedToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    fn to_openai(&self) -> anyhow::Result<ChatCompletionRequestMessage> {
        Ok(match self {
            ChatMessage::System(s) => ChatCompletionRequestSystemMessageArgs::default()
                .content(s.as_str())
                .build()?
                .into(),
            ChatMessage::User(s) => ChatCompletionRequestUserMessageArgs::default()
                .content(s.as_str())
                .build()?
                .into(),
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                let mut b = ChatCompletionRequestAssistantMessageArgs::default();
                if !content.is_empty() {
                    b.content(content.as_str());
                }
                if !tool_calls.is_empty() {
                    b.tool_calls(
                        tool_calls
                            .iter()
                            .map(|tc| {
                                ChatCompletionMessageToolCalls::Function(
                                    ChatCompletionMessageToolCall {
                                        id: tc.id.clone(),
                                        function: FunctionCall {
                                            name: tc.name.clone(),
                                            arguments: tc.arguments.clone(),
                                        },
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    );
                }
                b.build()?.into()
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => ChatCompletionRequestToolMessageArgs::default()
                .tool_call_id(tool_call_id.as_str())
                .content(content.as_str())
                .build()?
                .into(),
        })
    }
}

/// 待审批的工具调用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    /// 标题：取第一条用户输入的前若干字（Claude 的侧边栏就是这样）
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
    /// approval_id -> 待审批调用
    pub pending: HashMap<String, PendingApproval>,
    /// 给 provider 做 prompt cache 路由用的稳定会话 ID
    pub provider_session: String,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl ChatSession {
    pub fn new() -> Self {
        let now = now_ms();
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            title: String::new(),
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            pending: HashMap::new(),
            provider_session: format!("agent-core-{}", uuid::Uuid::now_v7()),
        }
    }

    /// 第一条用户输入决定标题
    fn maybe_title(&mut self, text: &str) {
        if self.title.is_empty() {
            let t: String = text.trim().chars().take(28).collect();
            self.title = if text.trim().chars().count() > 28 {
                format!("{t}…")
            } else {
                t
            };
        }
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.pending.clear();
        self.title.clear();
        self.updated_at = now_ms();
    }
}

/// 多个会话。侧边栏的「最近对话」就是它。
#[derive(Default, Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: Vec<ChatSession>,
    pub active: String,
}

impl SessionStore {
    /// 当前会话，没有就建一个
    pub fn active_mut(&mut self) -> &mut ChatSession {
        if self.sessions.is_empty() {
            let s = ChatSession::new();
            self.active = s.id.clone();
            self.sessions.push(s);
        }
        if !self.sessions.iter().any(|s| s.id == self.active) {
            self.active = self.sessions[0].id.clone();
        }
        let id = self.active.clone();
        self.sessions.iter_mut().find(|s| s.id == id).unwrap()
    }

    pub fn active_ref(&self) -> Option<&ChatSession> {
        self.sessions.iter().find(|s| s.id == self.active)
    }

    pub fn get(&self, id: &str) -> Option<&ChatSession> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ChatSession> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn create(&mut self) -> String {
        let s = ChatSession::new();
        let id = s.id.clone();
        self.sessions.push(s);
        self.active = id.clone();
        id
    }

    pub fn activate(&mut self, id: &str) -> bool {
        if self.sessions.iter().any(|s| s.id == id) {
            self.active = id.to_string();
            true
        } else {
            false
        }
    }

    pub fn delete(&mut self, id: &str) {
        self.sessions.retain(|s| s.id != id);
        if self.active == id {
            self.active = self.sessions.first().map(|s| s.id.clone()).unwrap_or_default();
        }
    }

    /// 给侧边栏用的列表，最近更新的在前
    pub fn list(&self) -> Vec<Value> {
        let mut v: Vec<&ChatSession> = self.sessions.iter().collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        v.into_iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "title": if s.title.is_empty() { "新对话" } else { &s.title },
                    "active": s.id == self.active,
                    "updatedAt": s.updated_at,
                    "count": s.messages.iter().filter(|m| matches!(m, ChatMessage::User(_))).count(),
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 提示词：缓存友好性
// ---------------------------------------------------------------------------
//
// ⚠️ **SYSTEM_PROMPT 必须是常量，任何时候都不能变。**
//
// 原因：它是消息数组的第一个元素，而 prompt cache 是**按前缀**命中的。
// 只要第一个字节变了，整个对话的缓存全部失效。
// 按 OpenCode Go 的定价，DeepSeek V4.1 Flash 的输入 $0.30/M，
// 缓存读取 $0.006/M —— 差 50 倍。把界面快照塞进 system prompt
// 意味着屏幕每次一变就重算整个上下文。
//
// 所以：
//   - 系统提示词：静态常量，零插值
//   - 动态内容（界面快照、设备信息）：作为一个**独立消息**插在
//     用户输入前面，只生成一次然后冻结进历史
//   - 工具表：顺序固定，序列化字节完全确定
//   - 会话 ID：开一次就不变（provider 靠它做缓存路由）
//
// 最终消息形状（前缀单调增长，每轮都能命中上一轮的前缀）：
//
//   [system: SYSTEM_PROMPT]          ← 永远不变
//   [system: 上下文#1]                ← 第一轮生成后冻结
//   [user:   U1]
//   [assistant: ...]
//   [tool: ...]
//   [system: 上下文#2]                ← 第二轮才追加
//   [user:   U2]
//   ...

pub const SYSTEM_PROMPT: &str = "\
你是运行在安卓手机上的操作助手，可以通过工具读取和操作手机界面，\n\
也可以在 Alpine Linux 沙箱里执行命令、编辑文件。\n\
\n\
工具使用原则：\n\
1. 要点击任何东西之前，先调 device_snapshot 看清当前界面。\n\
2. 用节点的索引 i 引用元素，不要猜坐标。\n\
3. 每次设备操作的工具结果里都会带上操作后的最新界面，以它为准，\n\
   不要沿用对话历史里的旧界面描述。\n\
4. 跑命令用 bash，改文件用 edit。沙箱内文件路径基于 /root。\n\
5. 装 Python 包优先 `apk add py3-*`，不要用 pip。\n\
\n\
给用户看沙箱里的文件：\n\
- 用 `sandbox:` 协议写 Markdown 链接或图片，路径就是沙箱内的绝对路径，例如\n\
  `[打开报告](sandbox:/root/report/index.html)`、`![图表](sandbox:/root/chart.png)`。\n\
- 用户点链接会在 App 里直接打开：HTML 正常渲染、脚本能运行，页面里用相对路径\n\
  引用的 css/js/图片也能加载；指向目录会列出文件。图片直接内嵌在回复里显示，\n\
  png/jpg/gif/webp/svg 都可以，不需要转换格式。\n\
- 做了网页、图表、图片等成品时，主动给出这样的链接，不要只把路径写成代码。\n\
\n\
回答要求：用中文，简洁直接，不要寒暄。";

/// 每轮用户输入前插入的上下文块。
///
/// 生成一次就冻结进历史 —— **绝不**每轮重建 system prompt。
pub fn context_block(core: &Core) -> String {
    context_block_from(
        core.state.linux().map(|s| s.rootfs().to_path_buf()),
        core.state.latest_snapshot().as_ref(),
    )
}

/// 拆出来是为了能在不构造 `Core` 的情况下测。
fn context_block_from(linux_rootfs: Option<PathBuf>, screen: Option<&UiSnapshot>) -> String {
    let linux = match linux_rootfs {
        Some(p) => format!(
            "Alpine Linux 沙箱可用，rootfs={}，cwd=/root。",
            p.display()
        ),
        None => "Linux 沙箱不可用（bash / edit 会报错）。".into(),
    };

    let screen = match screen {
        Some(s) => summarize(s)["text"].as_str().unwrap_or("").to_string(),
        None => "（还没有界面快照，需要时调 device_snapshot）".into(),
    };

    format!("<环境>\n{linux}\n\n<当前界面>\n{screen}\n</当前界面>\n</环境>")
}

/// 构造发给 provider 的消息数组。
///
/// **缓存不变式**：对同一个会话，后一轮的结果必须以前一轮的结果为**前缀**。
/// 任何往 `SYSTEM_PROMPT` 里插值、或重排历史的行为都会破坏它 ——
/// 一旦破坏，prompt cache 全部失效，成本直接涨 50 倍。
/// `message_prefix_is_stable_across_turns` 就是守住这条的回归测试。
fn build_messages(
    sess: &ChatSession,
) -> anyhow::Result<Vec<ChatCompletionRequestMessage>> {
    let mut msgs = Vec::with_capacity(sess.messages.len() + 1);
    msgs.push(ChatMessage::System(SYSTEM_PROMPT.to_string()).to_openai()?);
    for m in &sess.messages {
        msgs.push(m.to_openai()?);
    }
    Ok(msgs)
}

/// 把服务端会话转成前端 `Message[]` 的形状。
///
/// **为什么需要**：前端的消息是纯客户端状态，Activity 重建 / 页面刷新 /
/// 从后台回来都可能把它清掉 —— 实测切出去再回来对话就空了。
/// 而服务端一直留着完整历史（`ChatSession`），所以刷新后能从这里恢复。
///
/// 形状必须和 `web/src/lib/sse.ts` 的 `Message` 一致。
pub fn session_view(sess: &ChatSession) -> Vec<Value> {
    // 工具结果先按 id 索引：assistant 消息里的 tool_calls 要配上它
    let mut results: HashMap<&str, &str> = HashMap::new();
    for m in &sess.messages {
        if let ChatMessage::Tool {
            tool_call_id,
            content,
        } = m
        {
            results.insert(tool_call_id.as_str(), content.as_str());
        }
    }

    let parse = |s: &str| -> Value {
        serde_json::from_str(s).unwrap_or_else(|_| json!(s))
    };

    let mut out: Vec<Value> = Vec::new();
    for (i, m) in sess.messages.iter().enumerate() {
        match m {
            // 上下文块（界面/环境快照）是给模型看的，不给用户看
            ChatMessage::System(_) => {}

            ChatMessage::User(text) => out.push(json!({
                "id": format!("u{i}"),
                "role": "user",
                "steps": 0,
                "done": true,
                "parts": [{ "type": "text", "id": "u", "text": text }],
            })),

            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                let mut parts: Vec<Value> = Vec::new();
                if !reasoning.is_empty() {
                    parts.push(json!({ "type": "reasoning", "id": format!("r{i}"), "text": reasoning }));
                }
                if !content.is_empty() {
                    parts.push(json!({ "type": "text", "id": format!("t{i}"), "text": content }));
                }
                for tc in tool_calls {
                    let output = results.get(tc.id.as_str()).map(|c| parse(c));
                    parts.push(json!({
                        "type": "tool",
                        "toolCallId": tc.id,
                        "toolName": tc.name,
                        "inputText": "",
                        "input": parse(&tc.arguments),
                        "state": if output.is_some() { "output" } else { "ready" },
                        "output": output,
                    }));
                }
                if parts.is_empty() {
                    continue;
                }
                // 同一回合的多步（思考 → 工具 → 思考 → 正文 …）合成一条消息，
                // 和流式时前端看到的一样。不合的话刷新前后排版不一致：
                // 流式是一条消息里的几段，恢复出来是好几条消息、间距也不同。
                if let Some(last) = out.last_mut().filter(|m| m["role"] == "assistant") {
                    last["parts"].as_array_mut().expect("parts 是数组").extend(parts);
                    last["steps"] = json!(last["steps"].as_u64().unwrap_or(1) + 1);
                } else {
                    out.push(json!({
                        "id": format!("a{i}"),
                        "role": "assistant",
                        "steps": 1,
                        "done": true,
                        "parts": parts,
                    }));
                }
            }

            // 已经并进上面 assistant 的 part 里了
            ChatMessage::Tool { .. } => {}
        }
    }
    out
}

/// 一个回合最多几轮「模型 → 工具 → 模型」，防止死循环烧钱
const MAX_STEPS: usize = 8;

/// 跑一个用户回合，把 chunk 推到 `tx`。
pub async fn run_turn(
    core: &Core,
    provider: &Provider,
    user_text: &str,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    core.state
        .turn_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _guard = TurnGuard(core.clone());
    let sid = {
        let mut store = core.state.chat.write().await;
        let sess = store.active_mut();
        // 上下文作为独立消息，**放在用户输入之前**，且只生成这一次。
        // 这样历史前缀单调增长，每轮都能命中上一轮的缓存。
        sess.messages.push(ChatMessage::System(context_block(core)));
        sess.messages.push(ChatMessage::User(user_text.into()));
        sess.maybe_title(user_text);
        sess.updated_at = now_ms();
        sess.id.clone()
    };
    core.state.mark_sessions_dirty();
    drive(core, provider, &sid, &tx).await
}

/// 往本回合所属的会话追加一条消息。
///
/// **按 id 找，不用 `active_mut()`**：回合跑在后台，用户随时可能在侧边栏
/// 切到别的会话或新建会话 —— 之前全用 active_mut()，切走之后模型的回复
/// 和工具结果就写进了**另一个**会话，两边都坏掉。
/// 会话中途被删了就丢掉（下一步 stream_one 会报错停下）。
async fn append(core: &Core, sid: &str, m: ChatMessage) {
    let mut store = core.state.chat.write().await;
    if let Some(s) = store.get_mut(sid) {
        s.messages.push(m);
        s.updated_at = now_ms();
    }
    drop(store);
    core.state.mark_sessions_dirty();
}

/// 回合结束时清掉 busy 标志。用 Drop 保证任何路径（包括 panic 和
/// 提前 return）都会清 —— 否则前端会一直显示「运行中」。
struct TurnGuard(Core);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0
            .state
            .turn_active
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.0.state.mark_sessions_dirty();
    }
}

/// 审批结果回来之后，从这里继续。
pub async fn resolve_approval(
    core: &Core,
    provider: &Provider,
    approval_id: &str,
    approved: bool,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    core.state
        .turn_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _guard = TurnGuard(core.clone());
    // 审批挂在哪个会话上就回到哪个会话，不管当前激活的是谁
    let pending = {
        let mut store = core.state.chat.write().await;
        store
            .sessions
            .iter_mut()
            .find_map(|s| s.pending.remove(approval_id).map(|p| (s.id.clone(), p)))
    };
    let Some((sid, p)) = pending else {
        let _ = tx
            .send(sse::error(&format!("未知的审批 ID: {approval_id}")));
        return Ok(());
    };

    let _ = tx
        .send(sse::tool_approval_response(approval_id, approved, ""));

    if approved {
        let output = execute_tool(core, &p.name, &p.arguments).await;
        let _ = tx
            .send(sse::tool_output_available(&p.tool_call_id, output.clone()));
        append(
            core,
            &sid,
            ChatMessage::Tool {
                tool_call_id: p.tool_call_id.clone(),
                content: output.to_string(),
            },
        )
        .await;
    } else {
        let _ = tx.send(sse::tool_output_denied(&p.tool_call_id));
        append(
            core,
            &sid,
            ChatMessage::Tool {
                tool_call_id: p.tool_call_id.clone(),
                content: "用户拒绝了这个操作。请不要重试，改为向用户说明情况。".into(),
            },
        )
        .await;
    }

    drive(core, provider, &sid, &tx).await
}

/// 一轮 LLM 调用的产出
struct StepOutput {
    content: String,
    reasoning: String,
    calls: Vec<RecordedToolCall>,
}

/// 真正的 loop：模型 → 工具 → 模型 …
async fn drive(
    core: &Core,
    provider: &Provider,
    sid: &str,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let message_id = uuid::Uuid::now_v7().to_string();
    send(tx, sse::start(&message_id)).await;

    for _step in 0..MAX_STEPS {
        send(tx, sse::start_step()).await;

        let step = match stream_one(core, provider, sid, tx).await {
            Ok(c) => c,
            Err(e) => {
                send(tx, sse::error(&format!("{e:#}"))).await;
                send(tx, sse::finish()).await;
                return Err(e);
            }
        };
        send(tx, sse::finish_step()).await;

        // 不管有没有工具调用，这一轮的 assistant 输出都要进历史
        if !step.content.is_empty() || !step.reasoning.is_empty() || !step.calls.is_empty() {
            append(
                core,
                sid,
                ChatMessage::Assistant {
                    content: step.content.clone(),
                    reasoning: step.reasoning.clone(),
                    tool_calls: step.calls.clone(),
                },
            )
            .await;
        }

        let calls = step.calls;
        if calls.is_empty() {
            // 模型不再要求工具 → 本回合结束
            send(tx, sse::finish()).await;
            return Ok(());
        }

        let mut all_auto = true;
        let mut pending_any = false;

        for (idx, call) in calls.iter().enumerate() {
            let kind = find_tool(&call.name).map(|t| t.kind);

            match kind {
                Some(ToolKind::ReadOnly) => {
                    let output = execute_tool(core, &call.name, &call.arguments).await;
                    send(tx, sse::tool_output_available(&call.id, output.clone())).await;
                    append(
                        core,
                        sid,
                        ChatMessage::Tool {
                            tool_call_id: call.id.clone(),
                            content: output.to_string(),
                        },
                    )
                    .await;
                }
                Some(ToolKind::SideEffecting) => {
                    if core.state.auto_approve() {
                        // 默认路径：直接执行，不打断 agent
                        let output = execute_tool(core, &call.name, &call.arguments).await;
                        send(tx, sse::tool_output_available(&call.id, output.clone())).await;
                        append(
                            core,
                            sid,
                            ChatMessage::Tool {
                                tool_call_id: call.id.clone(),
                                content: output.to_string(),
                            },
                        )
                        .await;
                    } else {
                        // 审批模式（auto_approve = false）
                        all_auto = false;
                        pending_any = true;
                        let approval_id = format!("appr_{}", uuid::Uuid::now_v7());
                        let reason = approval_reason(&call.name, &call.arguments);
                        {
                            let mut store = core.state.chat.write().await;
                            let Some(sess) = store.get_mut(sid) else { break };
                            sess.pending.insert(
                                approval_id.clone(),
                                PendingApproval {
                                    tool_call_id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                },
                            );
                        }
                        send(
                            tx,
                            sse::tool_approval_request(&call.id, &approval_id, &reason),
                        )
                        .await;
                    }
                }
                None => {
                    // 模型编了一个不存在的工具，如实告诉它
                    let msg = format!("错误：没有名为 {} 的工具", call.name);
                    send(tx, sse::tool_output_available(&call.id, json!({ "error": msg }))).await;
                    append(
                        core,
                        sid,
                        ChatMessage::Tool {
                            tool_call_id: call.id.clone(),
                            content: msg,
                        },
                    )
                    .await;
                }
            }
            let _ = idx;
        }

        if pending_any {
            // 停下等人。前端批准/拒绝后会打 /api/approve 继续。
            send(tx, sse::finish()).await;
            return Ok(());
        }
        if !all_auto {
            send(tx, sse::finish()).await;
            return Ok(());
        }
        // 全是只读工具 → 继续下一轮
    }

    send(
        tx,
        sse::error(&format!("超过 {MAX_STEPS} 轮工具调用，已停止")),
    )
    .await;
    send(tx, sse::finish()).await;
    Ok(())
}

/// 调一次 LLM，把 delta 转成 chunk 推出去，返回本轮产出。
async fn stream_one(
    core: &Core,
    provider: &Provider,
    sid: &str,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<StepOutput> {
    // SYSTEM_PROMPT 是静态常量，**这里不做任何插值** —— 见它上面的说明。
    // 动态的界面/环境信息在 run_turn 里作为独立消息进历史，保证前缀可缓存。
    let (messages, session_id) = {
        let store = core.state.chat.read().await;
        let sess = store
            .get(sid)
            .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
        (build_messages(sess)?, sess.provider_session.clone())
    };

    let tools: Vec<ChatCompletionTools> = tools()
        .iter()
        .map(|t| {
            Ok(ChatCompletionTools::Function(ChatCompletionTool {
                function: FunctionObjectArgs::default()
                    .name(t.name)
                    .description(t.description)
                    .parameters((t.schema)())
                    .build()?,
            }))
        })
        .collect::<anyhow::Result<_>>()?;

    let req = CreateChatCompletionRequestArgs::default()
        .model(provider.model.clone())
        .messages(messages)
        .tools(tools)
        .stream(true)
        .build()?;

    // 要 provider 在流末尾回报 token 用量。
    //
    // `ChatCompletionStreamOptions` 没有 derive Default，逐字段构造很脆，
    // 所以先序列化成 Value 再补一个键 —— `create_stream_byot` 的 T0 只要 Serialize。
    let mut req = serde_json::to_value(&req)?;
    req["stream_options"] = json!({ "include_usage": true });

    let client = provider.client(&session_id)?;
    let mut stream = client
        .chat()
        .create_stream_byot::<_, StreamChunk>(req)
        .await?;

    use tokio_stream::StreamExt;

    // 工具调用是分片到达的：按 index 累积
    let mut acc: Vec<(String, String, String)> = Vec::new(); // (id, name, args)
    // 每个工具调用是否已经告诉前端"开始了"。
    //
    // 之前要等这一步整个流完、才一次性发 tool-input-available —— 模型写
    // 一条长命令（或一大段 edit）的那几秒里，界面上什么都没有。现在 id 和
    // 名字一到就发 start，参数边到边发 delta，前端能边写边显示。
    let mut started: Vec<bool> = Vec::new();
    let mut blocks = BlockStream::default();
    let mut text_acc = String::new();
    let mut reason_acc = String::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Usage> = None;

    while let Some(item) = stream.next().await {
        let chunk = item?;
        if chunk.usage.is_some() {
            usage = chunk.usage.clone();
        }
        for choice in chunk.choices {
            if let Some(r) = choice.delta.reasoning_text() {
                reason_acc.push_str(r);
                for c in blocks.push(BlockKind::Reasoning, r) {
                    send(tx, c).await;
                }
            }
            if let Some(c) = choice.delta.content.as_deref() {
                if !c.is_empty() {
                    text_acc.push_str(c);
                    for c in blocks.push(BlockKind::Text, c) {
                        send(tx, c).await;
                    }
                }
            }
            if let Some(fr) = choice.finish_reason.as_deref() {
                if fr == "length" {
                    finish = Some("length".to_string());
                }
            }
            if let Some(tcs) = choice.delta.tool_calls {
                for tc in tcs {
                    let i = tc.index as usize;
                    while acc.len() <= i {
                        acc.push((String::new(), String::new(), String::new()));
                        started.push(false);
                    }
                    if let Some(id) = tc.id {
                        acc[i].0 = id;
                    }
                    let mut piece = String::new();
                    if let Some(f) = tc.function {
                        if let Some(n) = f.name {
                            acc[i].1.push_str(&n);
                        }
                        if let Some(a) = f.arguments {
                            acc[i].2.push_str(&a);
                            piece = a;
                        }
                    }

                    let (id, name, args) = &acc[i];
                    if !started[i] && !id.is_empty() && !name.is_empty() {
                        started[i] = true;
                        // 先收尾正在流的思考/正文，工具行才排在它们后面
                        if let Some(c) = blocks.close() {
                            send(tx, c).await;
                        }
                        send(tx, sse::tool_input_start(id, name)).await;
                        // id/名字到之前就来了的参数一并补上
                        if !args.is_empty() {
                            send(tx, sse::tool_input_delta(id, args)).await;
                        }
                    } else if started[i] && !piece.is_empty() {
                        send(tx, sse::tool_input_delta(id, &piece)).await;
                    }
                }
            }
        }
    }

    // 收尾最后一个块，之后才是工具调用 —— 前端按收到的顺序排 part
    if let Some(c) = blocks.close() {
        send(tx, c).await;
    }

    // 被 max_tokens 截断时要告诉用户，否则看起来就像模型话说到一半
    if finish.as_deref() == Some("length") {
        send(
            tx,
            sse::data_part("warning", json!({ "message": "输出被 max_tokens 截断" })),
        )
        .await;
    }

    // 把缓存命中情况摆出来 —— 这是验证 prompt cache 的唯一硬证据
    if let Some(u) = &usage {
        match u.cache_hit_ratio() {
            Some(ratio) => tracing::info!(
                "prompt cache 命中 {:.1}%（cached={} prompt={} completion={} reasoning={}）",
                ratio * 100.0,
                u.cached_tokens().unwrap_or(0),
                u.prompt_tokens,
                u.completion_tokens,
                u.reasoning_tokens().unwrap_or(0),
            ),
            None => tracing::info!(
                "prompt={} completion={}（provider 未报缓存字段）",
                u.prompt_tokens,
                u.completion_tokens
            ),
        }
        send(tx, sse::data_part("usage", serde_json::to_value(u)?)).await;
    }


    let calls: Vec<RecordedToolCall> = acc
        .into_iter()
        .filter(|(id, name, _)| !id.is_empty() && !name.is_empty())
        .map(|(id, name, arguments)| RecordedToolCall {
            id,
            name,
            arguments,
        })
        .collect();

    for c in &calls {
        let input: Value = serde_json::from_str(&c.arguments).unwrap_or(json!({}));
        send(tx, sse::tool_input_available(&c.id, &c.name, input)).await;
    }

    Ok(StepOutput {
        content: text_acc,
        reasoning: reason_acc,
        calls,
    })
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum BlockKind {
    Text,
    Reasoning,
}

/// 流式文本/思考块的开合。
///
/// **每个块一个独立 id**。之前写死 "t1" / "r1"：一个回合里有好几步
/// （思考 → 正文 → 工具 → 思考 → 正文 …），前端按 id 找块追加，第二步
/// 的思考就接到了第一步的思考后面，正文也全堆进第一段 —— 顺序全乱了。
/// 同一步里思考和正文交替出现时也各开新块，保持模型输出的原始顺序。
#[derive(Default)]
struct BlockStream {
    open: Option<(BlockKind, String)>,
}

impl BlockStream {
    /// 追加一段内容。返回要发出的 chunk：可能先收尾上一块、再开新块。
    fn push(&mut self, kind: BlockKind, delta: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if self.open.as_ref().map(|(k, _)| *k) != Some(kind) {
            out.extend(self.close());
            let id = match kind {
                BlockKind::Text => format!("t-{}", uuid::Uuid::now_v7()),
                BlockKind::Reasoning => format!("r-{}", uuid::Uuid::now_v7()),
            };
            out.push(match kind {
                BlockKind::Text => sse::text_start(&id),
                BlockKind::Reasoning => sse::reasoning_start(&id),
            });
            self.open = Some((kind, id));
        }
        let (_, id) = self.open.as_ref().expect("刚开过块");
        out.push(match kind {
            BlockKind::Text => sse::text_delta(id, delta),
            BlockKind::Reasoning => sse::reasoning_delta(id, delta),
        });
        out
    }

    fn close(&mut self) -> Option<Value> {
        self.open.take().map(|(k, id)| match k {
            BlockKind::Text => sse::text_end(&id),
            BlockKind::Reasoning => sse::reasoning_end(&id),
        })
    }
}

async fn send(tx: &mpsc::UnboundedSender<Value>, v: Value) {
    let _ = tx.send(v);
}

/// 把工具参数里的 `edits` 数组解成 `editor::Edit`。
fn parse_edits(args: &Value) -> anyhow::Result<Vec<crate::editor::Edit>> {
    let arr = args
        .get("edits")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("edits 必须是数组"))?;
    anyhow::ensure!(!arr.is_empty(), "edits 至少要有一项");

    arr.iter()
        .enumerate()
        .map(|(i, e)| {
            let old = e
                .get("oldText")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("edits[{i}].oldText 必须是字符串"))?;
            let new = e
                .get("newText")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("edits[{i}].newText 必须是字符串"))?;
            Ok(crate::editor::Edit::new(old, new))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 工具执行
// ---------------------------------------------------------------------------

async fn execute_tool(core: &Core, name: &str, arguments: &str) -> Value {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

    // bash / edit 不走设备（DeviceApi）：bash 起 proot 子进程，edit 直接改 rootfs 里的文件
    if name == "bash" {
        let script = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if script.trim().is_empty() {
            return json!({ "error": "缺少参数 command" });
        }
        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_f64())
            .map(|s| std::time::Duration::from_secs_f64(s.clamp(1.0, 600.0)))
            .unwrap_or(linux::DEFAULT_TIMEOUT);

        return match core.run_linux(script, timeout).await {
            Ok(r) => json!({
                "output": r.to_text(),
                "exit_code": r.code,
                "truncated": r.truncated,
                "full_output_path": r.full_output_path,
                "wall_time_seconds": (r.duration_ms as f64) / 1000.0,
            }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        };
    }

    if name == "edit" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return json!({ "error": "缺少参数 path" });
        }
        let edits = match parse_edits(&args) {
            Ok(e) => e,
            Err(e) => return json!({ "error": format!("{e:#}") }),
        };

        return match core.edit_file(path, &edits).await {
            Ok(out) => json!({
                "ok": true,
                "message": format!("已替换 {} 处", edits.len()),
                "firstChangedLine": out.first_changed_line,
                "diff": out.patch,
            }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        };
    }

    let cmd = match name {
        "device_snapshot" => Command::Snapshot,
        "device_tap" => match args.get("i").and_then(|v| v.as_u64()) {
            Some(i) => Command::TapNode { i: i as u32 },
            None => return json!({ "error": "缺少参数 i" }),
        },
        "device_type" => {
            let i = args.get("i").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Command::SetText { i, text }
        }
        "device_key" => Command::Key {
            name: args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("back")
                .to_string(),
        },
        "device_launch" => Command::Launch {
            package: args
                .get("package")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        },
        other => return json!({ "error": format!("未知工具 {other}") }),
    };

    let ack = core.dispatch(cmd).await;

    // 设备操作会改变界面。把**最新快照一并回给模型**，否则它只能拿
    // 对话历史里的旧画面脉猜 —— 实测就会说出"已点击"却复述旧界面。
    // 等一下是给设备侧读树/回推快照留时间。
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    match core.state.latest_snapshot() {
        Some(snap) => {
            let mut out = summarize(&snap);
            out["result"] = ack;
            out
        }
        None => ack,
    }
}

/// 把 UI 树压成给模型看的紧凑文本。
///
/// 决策 D10 的落地：只保留可交互节点 + 有文本的节点，并且用
/// `[i] Class "text" (x,y)` 这种紧凑格式，避免几百个节点撑爆上下文。
pub fn summarize(snap: &UiSnapshot) -> Value {
    let mut lines = vec![format!(
        "包名: {}\nActivity: {}\n屏幕: {}x{}",
        snap.package, snap.activity, snap.screen.w, snap.screen.h
    )];

    for n in &snap.nodes {
        let interactive = n.a.interesting();
        if !interactive && n.t.is_empty() {
            continue;
        }
        let mut line = format!("[{}] {}", n.i, n.c);
        if !n.t.is_empty() {
            line.push_str(&format!(" \"{}\"", n.t.replace('\n', " ")));
        }
        if !n.id.is_empty() {
            line.push_str(&format!(" id={}", n.id));
        }
        let [l, t, r, b] = n.b;
        line.push_str(&format!(" ({},{},{},{})", l, t, r, b));
        let mut caps = Vec::new();
        if n.a.clickable {
            caps.push("clickable");
        }
        if n.a.editable {
            caps.push("editable");
        }
        if n.a.scrollable {
            caps.push("scrollable");
        }
        if n.a.checkable {
            caps.push("checkable");
        }
        if !n.a.enabled {
            caps.push("disabled");
        }
        if !caps.is_empty() {
            line.push_str(&format!(" [{}]", caps.join(",")));
        }
        if !n.s.is_empty() {
            line.push_str(&format!(" {{{}}}", n.s));
        }
        lines.push(line);
    }

    json!({ "text": lines.join("\n"), "nodes": snap.nodes.len() })
}

fn approval_reason(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));
    match name {
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let head: String = cmd.chars().take(120).collect();
            format!("在 Linux 沙箱里执行：{head}")
        }
        "edit" => format!(
            "编辑文件 {}",
            args.get("path").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "device_tap" => format!(
            "点击节点 {}（有副作用，会离开当前界面）",
            args.get("i").map(|v| v.to_string()).unwrap_or_default()
        ),
        "device_type" => "向输入框写入文本".into(),
        "device_key" => format!(
            "按 {} 键",
            args.get("name").and_then(|v| v.as_str()).unwrap_or("back")
        ),
        "device_launch" => format!(
            "启动应用 {}",
            args.get("package").and_then(|v| v.as_str()).unwrap_or("")
        ),
        _ => "该操作需要确认".into(),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(sess: &ChatSession) -> Vec<serde_json::Value> {
        build_messages(sess)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect()
    }

    /// **这是最重要的一条测试。**
    ///
    /// prompt cache 按前缀命中。只要后一轮的消息数组不是以前一轮为前缀，
    /// 缓存就全废。DeepSeek V4.1 Flash 的输入是 $0.30/M、缓存读取 $0.006/M
    /// —— 差 50 倍，所以这不是优化，是正确性。
    #[test]
    fn message_prefix_is_stable_across_turns() {
        let mut sess = ChatSession::default();

        // 第 1 轮
        sess.messages
            .push(ChatMessage::System(context_block_from(None, None)));
        sess.messages.push(ChatMessage::User("第一问".into()));
        let turn1 = wire(&sess);

        // 第 2 轮：追加 assistant / tool / 新上下文 / 新用户输入
        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: "想".into(),
            tool_calls: vec![RecordedToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "ok".into(),
        });
        sess.messages
            .push(ChatMessage::System(context_block_from(None, None)));
        sess.messages.push(ChatMessage::User("第二问".into()));
        let turn2 = wire(&sess);

        assert!(turn2.len() > turn1.len());
        assert_eq!(
            &turn2[..turn1.len()],
            &turn1[..],
            "第 2 轮不是以第 1 轮为前缀 → prompt cache 全部失效"
        );
    }

    #[test]
    fn system_prompt_is_a_frozen_constant_with_no_interpolation() {
        assert!(
            !SYSTEM_PROMPT.contains('{'),
            "system prompt 里出现格式占位符，说明被插值了"
        );
        for dynamic in ["rootfs", "/data/", "Android", "Activity", "1080"] {
            assert!(
                !SYSTEM_PROMPT.contains(dynamic),
                "system prompt 里不该出现运行时才知道的内容: {dynamic}"
            );
        }
    }

    #[test]
    fn changing_the_screen_does_not_change_the_cached_prefix() {
        // 界面变了，但 system prompt 和之前的历史都不能变 ——
        // 变化只能出现在新追加的上下文消息里
        let a = context_block_from(None, None);
        let b = context_block_from(Some(PathBuf::from("/x/rootfs")), None);
        assert_ne!(a, b, "上下文块本身应该反映环境差异");

        let mut sess = ChatSession::default();
        sess.messages.push(ChatMessage::System(a));
        sess.messages.push(ChatMessage::User("q".into()));
        let before = wire(&sess);

        sess.messages.push(ChatMessage::Assistant {
            content: "a".into(),
            reasoning: String::new(),
            tool_calls: vec![],
        });
        sess.messages.push(ChatMessage::System(b));
        let after = wire(&sess);

        assert_eq!(&after[..before.len()], &before[..], "前缀被动了");
    }

    #[test]
    fn tool_schema_order_and_names_are_fixed() {
        // 工具表也是可缓存前缀的一部分。顺序必须稳定，
        // 所以这里把顺序钉死：谁想改都得先改这个测试。
        let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "device_snapshot",
                "device_tap",
                "device_type",
                "device_key",
                "bash",
                "edit",
                "device_launch",
            ]
        );
        // 重复调用必须完全一致
        assert_eq!(names, tools().iter().map(|t| t.name).collect::<Vec<_>>());
    }

    #[test]
    fn tool_schemas_serialize_byte_identically() {
        let once = serde_json::to_string(&tools().iter().map(|t| (t.schema)()).collect::<Vec<_>>())
            .unwrap();
        let twice = serde_json::to_string(&tools().iter().map(|t| (t.schema)()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn side_effecting_tools_are_the_ones_that_can_change_things() {
        let side: Vec<&str> = tools()
            .iter()
            .filter(|t| t.kind == ToolKind::SideEffecting)
            .map(|t| t.name)
            .collect();
        assert!(side.contains(&"bash"));
        assert!(side.contains(&"edit"));
        assert!(!side.contains(&"device_snapshot"), "读界面不该算有副作用");
    }

    #[test]
    fn parse_edits_reads_the_shape_the_schema_declares() {
        let args = json!({
            "path": "/root/a.txt",
            "edits": [
                { "oldText": "a", "newText": "b" },
                { "oldText": "c", "newText": "d" }
            ]
        });
        let edits = parse_edits(&args).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].old_text, "a");
        assert_eq!(edits[1].new_text, "d");
    }

    #[test]
    fn parse_edits_rejects_malformed_input_with_a_locatable_message() {
        let e = parse_edits(&json!({ "edits": "nope" })).unwrap_err();
        assert!(e.to_string().contains("数组"), "{e}");

        let e = parse_edits(&json!({ "edits": [ { "oldText": "a" } ] })).unwrap_err();
        assert!(e.to_string().contains("edits[0].newText"), "{e}");

        let e = parse_edits(&json!({ "edits": [] })).unwrap_err();
        assert!(e.to_string().contains("至少"), "{e}");
    }

    #[test]
    fn session_view_keeps_a_turns_steps_in_one_message_in_order() {
        let call = |id: &str| RecordedToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        };
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("ctx".into()),
            ChatMessage::User("问".into()),
            ChatMessage::Assistant { content: "先看看".into(), reasoning: "想1".into(), tool_calls: vec![call("c1")] },
            ChatMessage::Tool { tool_call_id: "c1".into(), content: "{}".into() },
            ChatMessage::Assistant { content: "答".into(), reasoning: "想2".into(), tool_calls: vec![] },
            ChatMessage::User("再问".into()),
            ChatMessage::Assistant { content: "再答".into(), reasoning: String::new(), tool_calls: vec![] },
        ];
        let v = session_view(&sess);
        let roles: Vec<&str> = v.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"], "一个回合一条助手消息");

        let parts: Vec<String> = v[1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| format!("{}:{}", p["type"].as_str().unwrap(), p["text"].as_str().unwrap_or("")))
            .collect();
        assert_eq!(parts, ["reasoning:想1", "text:先看看", "tool:", "reasoning:想2", "text:答"]);

        let ids: std::collections::HashSet<&str> =
            v[1]["parts"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
        assert_eq!(ids.len(), 4, "合并后的思考/正文 id 不能撞");
    }

    #[test]
    fn each_block_gets_its_own_id_in_output_order() {
        let ids = |chunks: &[Value]| -> Vec<(String, String)> {
            chunks
                .iter()
                .map(|c| {
                    (
                        c["type"].as_str().unwrap().to_string(),
                        c["id"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect()
        };

        // 第一步：思考 → 正文
        let mut a = BlockStream::default();
        let mut step1 = a.push(BlockKind::Reasoning, "想");
        step1.extend(a.push(BlockKind::Reasoning, "一想"));
        step1.extend(a.push(BlockKind::Text, "答"));
        step1.extend(a.close());
        // 第二步（新的一次模型调用）：又是思考 → 正文
        let mut b = BlockStream::default();
        let mut step2 = b.push(BlockKind::Reasoning, "再想");
        step2.extend(b.push(BlockKind::Text, "再答"));
        step2.extend(b.close());

        let s1 = ids(&step1);
        let kinds: Vec<&str> = s1.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            kinds,
            [
                "reasoning-start",
                "reasoning-delta",
                "reasoning-delta",
                "reasoning-end",
                "text-start",
                "text-delta",
                "text-end"
            ],
            "切换类型时先收尾上一块"
        );
        assert_eq!(s1[0].1, s1[3].1, "同一块 id 一致");
        assert_ne!(s1[0].1, s1[4].1, "思考和正文不同块");

        let s2 = ids(&step2);
        assert_ne!(s1[0].1, s2[0].1, "第二步的思考不能和第一步同 id，否则前端会接到一起");
        assert_ne!(s1[4].1, s2[3].1, "第二步的正文同理");
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

        // 冷启动
        let cold: Usage = serde_json::from_value(json!({
            "prompt_tokens": 2138,
            "prompt_tokens_details": { "cached_tokens": 0 }
        }))
        .unwrap();
        assert_eq!(cold.cache_hit_ratio(), Some(0.0));

        // provider 什么都不报时不能瞎猜
        let unknown: Usage = serde_json::from_value(json!({ "prompt_tokens": 50 })).unwrap();
        assert_eq!(unknown.cache_hit_ratio(), None);
    }
}
