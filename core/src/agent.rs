//! Agent 主循环。
//!
//! 决策 D5 选了「自研薄 loop」而不是 `rig`，原因就在这个文件里：
//! 每一个事件都要精确地转成 AI SDK UI Message Stream 的 chunk 推给前端。
//! 框架会把 loop 包起来，反而挡路。
//!
//! 工具调用**一律直接执行**，没有审批环节 —— 服务端因此没有「挂起等回复」
//! 的状态，回合结束就是结束，手机 App 随时被杀都没关系。

use std::collections::HashMap;
use std::sync::OnceLock;

use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestMessageContentPartImageArgs,
    ChatCompletionRequestMessageContentPartTextArgs, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
    ChatCompletionRequestUserMessageContentPart, ChatCompletionStreamOptions, ChatCompletionTool,
    ChatCompletionTools, CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
    FunctionCall, FunctionObjectArgs, ImageUrlArgs,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::llm::Provider;
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

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

/// 工具表只建一次。
///
/// 每个 step 都要把整张表序列化进请求（含那段很长的 `bash` 描述），
/// 工具表是**可缓存前缀的一部分**，只建一次也让「字节确定」更保险。
static TOOLS: OnceLock<Vec<ToolSpec>> = OnceLock::new();

pub fn tools() -> &'static [ToolSpec] {
    TOOLS.get_or_init(build_tools)
}

fn build_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read",
            description: "读取文件。输出截断到 2000 行或 50KB，被截断时末尾会写明用哪个 offset 续读。图片（png/jpg/gif/webp/bmp）会缩放后直接发给你（offset/limit 对图片无效），看截图/照片/图表用它。改文件前先用它确认原文。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 /workspace）" },
                        "offset": { "type": "integer", "description": "起始行号，从 1 开始（可选，默认 1）" },
                        "limit": { "type": "integer", "description": "最多读取的行数（可选，默认读到截断上限）" }
                    },
                    "required": ["path"]
                })
            },
        },
        ToolSpec {
            name: "bash",
            description: "在 Alpine Linux 沙箱里跑 shell 命令（cwd=/workspace；每次新进程，文件系统持久、shell 状态不持久）。输出取**尾部**（最后 2000 行或 50KB），截断时完整输出存临时文件并返回路径。已预装 bash/curl/git/jq/rg/fd/tree/unzip/python3。要点：ping 被封用 curl；Python 包优先 `apk add py3-*`；matplotlib 先 `use('Agg')`；后台服务要重定向 stdout/stderr；搜索用 `rg -uuu`。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout": { "type": "number", "description": "超时秒数（**必填**）。先估算这条命令要跑多久；下载/编译/装包可能几千秒。宁可估大：估小了会被中途杀掉，多估不会真等。上限 86400（24h）。" }
                    },
                    "required": ["command", "timeout"]
                })
            },
        },
        ToolSpec {
            name: "edit",
            description: "用一个或多个精确替换编辑单个文件。相邻改动合并成一条；不要为了够到远处的改动而在 oldText 里塞大段没变的内容。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 /workspace）" },
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
    ]
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
        /// 这次工具调用返回的图片（`read` 读图片时才有）。
        ///
        /// 不放进 `content` 里：OpenAI 的 tool 消息只能是字符串，图片必须
        /// 作为紧随其后的一条 `user` 消息发（多个 tool 调用要等全部发完）。
        /// 见 `build_messages`。`#[serde(default)]` 让老会话不受影响。
        #[serde(default)]
        images: Vec<ImagePart>,
    },
}

/// 一张已经缩好、等着内联进请求的图片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    /// 处理后字节的落盘路径（绝对）。组装请求时从盘上读 —— 字节一致才能
    /// 命中 prompt cache，也不让 SQLite 里堆 base64。
    pub path: String,
    pub mime: String,
    /// 沙箱里原图的路径，给模型看（裁剪/看元数据用）和界面展示用。
    pub source: String,
    /// 哪个工具返回的（目前只有 read）
    pub tool: String,
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
                ..
            } => ChatCompletionRequestToolMessageArgs::default()
                .tool_call_id(tool_call_id.as_str())
                .content(content.as_str())
                .build()?
                .into(),
        })
    }
}

/// 当前会话。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    /// 标题：取第一条用户输入的前若干字（Claude 的侧边栏就是这样）
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub messages: Vec<ChatMessage>,
    /// 给 provider 做 prompt cache 路由用的稳定会话 ID
    pub provider_session: String,
}

/// 新的 provider session id（作为 `x-opencode-session` 发出去）。
fn new_provider_session() -> String {
    format!("agent-core-{}", uuid::Uuid::now_v7())
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
            provider_session: new_provider_session(),
        }
    }

    /// 换一个新的 provider session id。
    ///
    /// 什么时候要换：历史被清空、provider / model 变了、工具表变了 ——
    /// 也就是「缓存前缀的语义变了」。**不能每轮都换**，那等于每次冷启动
    /// （provider 靠这个 id 做缓存路由）。
    pub fn rotate_provider_session(&mut self) {
        self.provider_session = new_provider_session();
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
        self.title.clear();
        self.updated_at = now_ms();
        // 历史清空了，前缀完全变了：继续用旧 id 就是「同 session 不同前缀」，
        // 只会让 provider 白跑一次冷启动。
        self.rotate_provider_session();
    }

    /// 有内容吗？空会话（建了、没说过话）不入列表、不落盘。
    ///
    /// 「新对话」点了不发消息就不该留痕 —— 否则连点几次攒一堆空会话。
    pub fn has_content(&self) -> bool {
        !self.messages.is_empty()
    }
}

/// 一段文本里找 `needle`（忽略大小写），命中就返回带省略号的上下文。
///
/// 按**字符**切而不是字节：中文（和多字节 UTF-8）里按字节切会把字切碎。
fn hit_snippet(text: &str, needle: &str) -> Option<String> {
    const BEFORE: usize = 20;
    const AFTER: usize = 60;

    let hay: Vec<char> = text.to_lowercase().chars().collect();
    let pat: Vec<char> = needle.to_lowercase().chars().collect();
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    let at = hay.windows(pat.len()).position(|w| w == pat.as_slice())?;

    let raw: Vec<char> = text.chars().collect();
    let start = at.saturating_sub(BEFORE);
    let end = (at + pat.len() + AFTER).min(raw.len());

    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    // 换行拍平成空格，否则摘要里会带出多行
    s.extend(raw[start..end].iter().map(|c| if *c == '\n' { ' ' } else { *c }));
    if end < raw.len() {
        s.push('…');
    }
    Some(s)
}

/// 多个会话。侧边栏的「最近对话」/ 搜索结果都从它来。
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

    /// 给侧边栏用的列表，最近更新的在前。
    ///
    /// **空会话不进列表**：前端点「新对话」不同，那是本地清空视图、服务端
    /// 到第一句话才建会话（见 `POST /api/chat` 的 `fresh`）。这里是兜底 ——
    /// 调试端/CLI 直接 `POST /api/sessions` 建的、`/api/reset` 清空后的、
    /// 以及启动时 `active_mut()` 顺手建的，都是没内容的，不该在抽屉里占位。
    pub fn list(&self) -> Vec<Value> {
        let mut v: Vec<&ChatSession> = self.sessions.iter().filter(|s| s.has_content()).collect();
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

    /// 全文搜：跨会话找用户／助手消息里的子串（忽略大小写），
    /// 每个会话最多回 `per_session` 条，总量封顶 `limit`。
    ///
    /// 直接在内存里线性扫 —— 整个 store 本来就在内存里，会话量级是几十，
    /// 用不上 SQL 的 FTS（少一张表、少一处 schema 迁移）。
    pub fn search(&self, q: &str, per_session: usize, limit: usize) -> Vec<Value> {
        let needle = q.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<&ChatSession> = self.sessions.iter().collect();
        sorted.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

        let mut out = Vec::new();
        for s in sorted {
            let mut in_this = 0;
            for m in &s.messages {
                let (role, text) = match m {
                    ChatMessage::User(t) => ("user", t.as_str()),
                    ChatMessage::Assistant { content, .. } => ("assistant", content.as_str()),
                    // 系统提示词和工具结果不参与搜索：前者不是对话，后者是噪音
                    _ => continue,
                };
                let Some(snippet) = hit_snippet(text, needle) else {
                    continue;
                };
                out.push(json!({
                    "id": s.id,
                    "title": if s.title.is_empty() { "新对话" } else { &s.title },
                    "active": s.id == self.active,
                    "role": role,
                    "snippet": snippet,
                    "updatedAt": s.updated_at,
                }));
                in_this += 1;
                if in_this >= per_session || out.len() >= limit {
                    break;
                }
            }
            if out.len() >= limit {
                break;
            }
        }
        out
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
你是运行在安卓手机上的操作助手：在 Alpine Linux 沙箱里跑命令、改文件、做出成品。\n\
\n\
- cwd 固定 /workspace，就是手机内部存储的 Agent 文件夹（用户能在文件管理器看到，\n\
  卸载不丢）。成品**必须**放这里，按任务建子目录；虚拟环境/依赖/编译产物放 /root\n\
  （重装会清空）。整个内部存储挂在 /sdcard，可读写用户文件 —— 动它之前先说明，\n\
  删除或覆盖先确认；不确定就先 `ls /sdcard`（可能没挂上）。\n\
- 给用户看成品：写 `sandbox:` 链接，如 `[打开](sandbox:/workspace/报告/index.html)`；\n\
  图片/音频/视频都用图片语法 `![说明](sandbox:…)`。做完主动给链接，别只写路径。\n\
- 沙箱里的 `agent` 命令可查看/操作自己：`agent state|request|tools`、\n\
  `agent command`（按键/点击/滑动/启动应用），`agent help` 看全部。\n\
\n\
回答要言简意赅。";

/// 构造发给 provider 的消息数组 = **会话历史原样**。
///
/// 唯一的“拼”是图片 carrier：`read` 读到图时，tool 消息后面会多一条 `user`
/// 消息（图片只能挂在那里）。它由历史推导出来、字节确定，所以不影响
/// “后一轮以前一轮为前缀”这条不变式 —— 系统提示词仍然是历史的第一条
/// （见 `ensure_system_prompt`），请求体仍然是历史的单调扩展。
fn build_messages(
    sess: &ChatSession,
    supports_images: bool,
) -> anyhow::Result<Vec<ChatCompletionRequestMessage>> {
    let msgs = &sess.messages;
    let mut out: Vec<ChatCompletionRequestMessage> = Vec::with_capacity(msgs.len());
    let mut i = 0;
    while i < msgs.len() {
        if !matches!(msgs[i], ChatMessage::Tool { .. }) {
            out.push(msgs[i].to_openai()?);
            i += 1;
            continue;
        }

        // 先把这一串连续的 tool 消息一次发完，再发图片。
        //
        // OpenAI 要求 assistant 的每个 tool_call 后面**紧跟着**对应的 tool
        // 消息，中间不能插 user —— 一个回合调了 3 个工具、只有第 1 个带图时，
        // 图绝不能插在第 1、2 个 tool 之间。
        let mut carriers: Vec<(String, ImagePart)> = Vec::new();
        while i < msgs.len() {
            match &msgs[i] {
                ChatMessage::Tool {
                    tool_call_id,
                    content,
                    images,
                } => {
                    out.push(
                        ChatCompletionRequestToolMessageArgs::default()
                            .tool_call_id(tool_call_id.as_str())
                            .content(content.as_str())
                            .build()?
                            .into(),
                    );
                    for img in images {
                        carriers.push((tool_call_id.clone(), img.clone()));
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        for (id, img) in carriers {
            out.push(image_carrier(&id, &img, supports_images)?);
        }
    }
    Ok(out)
}

/// 把一张工具返回的图片包成一条 `user` 消息。
///
/// 为什么是 user 消息：OpenAI Chat Completions 的 tool 消息内容只能是字符串，
/// 工具结果里的图片没有别的合法位置可挂。OpenMinis 的 OpenAI 编码器也是这么
/// 绕的（`ChatMessagesEncoder` 里那批 "image carriers"）。
fn image_carrier(
    tool_call_id: &str,
    img: &ImagePart,
    supports_images: bool,
) -> anyhow::Result<ChatCompletionRequestMessage> {
    let label = format!(
        "[{} 返回的图片（tool_call_id: {tool_call_id}）；原文件：{}]",
        img.tool, img.source
    );

    let mut parts: Vec<ChatCompletionRequestUserMessageContentPart> = vec![text_part(label)?];
    if supports_images {
        // 走缓存：每一步都要重新组装请求，不能每步都把同一张图重新 base64。
        // 字节仍然逐字一致（文件不变），prompt cache 照命中。
        match crate::imaging::cached_data_url(&img.path, &img.mime) {
            Ok(url) => parts.push(image_part(&url)?),
            Err(e) => {
                tracing::warn!("图片读不到了 {}: {e}", img.path);
                parts.push(text_part("[图片文件已丢失，本轮没能附带]".to_string())?);
            }
        }
    } else {
        parts.push(text_part(
            "[当前模型不支持图片输入，这张图没有附带；它在沙箱里，可以用 bash 处理或让用户在界面上看]"
                .to_string(),
        )?);
    }

    Ok(ChatCompletionRequestUserMessageArgs::default()
        .content(parts)
        .build()?
        .into())
}

fn text_part(text: String) -> anyhow::Result<ChatCompletionRequestUserMessageContentPart> {
    Ok(ChatCompletionRequestMessageContentPartTextArgs::default()
        .text(text)
        .build()?
        .into())
}

fn image_part(url: &str) -> anyhow::Result<ChatCompletionRequestUserMessageContentPart> {
    let image_url = ImageUrlArgs::default().url(url.to_string()).build()?;
    Ok(ChatCompletionRequestMessageContentPartImageArgs::default()
        .image_url(image_url)
        .build()?
        .into())
}

/// 工具表的线上形态（`{"type":"function","function":{…}}` 的数组）。
///
/// 抽出来是给 `/api/debug/tools` 用的：模型（或人）想知道「我有哪些工具」，
/// 不用去翻整份请求体。
pub fn tool_definitions() -> anyhow::Result<Vec<ChatCompletionTools>> {
    tools()
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
        .collect()
}

/// 保证历史里有当前版本的系统提示词。
///
/// 系统提示词**存在历史里**，不是发请求时才拼上去的 —— 这是为了“改了提示词
/// 也不动老会话的前缀”：
///
/// - 老会话的前缀是 `[system: 旧提示词, U1, A1, …]`，原样保留 → 缓存照命中；
/// - 当前提示词没在历史里出现过，就作为一条 `system` 消息**追加在末尾**
///   （新用户输入之前）。它一旦落进历史就固化了，下一轮开始就在缓存里。
/// - 同一条提示词不会重复追加。
///
/// 推论：`provider_session`**不能**因为提示词变了就轮换 —— 那正是要保住的缓存路由。
///
/// `pub` 是给 `/api/debug/request` 用的：它要预测下一次真正会发什么。
pub fn ensure_system_prompt(sess: &mut ChatSession, prompt: &str) {
    let present = sess
        .messages
        .iter()
        .any(|m| matches!(m, ChatMessage::System(s) if s == prompt));
    if !present {
        sess.messages.push(ChatMessage::System(prompt.to_string()));
    }
}

/// 把服务端会话转成前端 `Message[]` 的形状。
///
/// **为什么需要**：前端的消息是纯客户端状态，Activity 重建 / 页面刷新 /
/// 从后台回来都可能把它清掉 —— 实测切出去再回来对话就空了。
/// 而服务端一直留着完整历史（`ChatSession`），所以刷新后能从这里恢复。
///
/// 形状必须和 `web/src/lib/sse.ts` 的 `Message` 一致。
pub fn session_view(sess: &ChatSession) -> Vec<Value> {
    session_view_from(sess, 0)
}

/// `messages[..upto]` 会产生几条前端消息（合并规则与 `session_view_from` 一致）。
///
/// 增量同步时客户端靠它知道「从哪里开始替换」。只数不建 Value，很便宜。
/// 有测试盯着它和 `session_view_from` 不跑偏。
pub fn session_view_count_before(sess: &ChatSession, upto: usize) -> usize {
    let upto = upto.min(sess.messages.len());
    let mut n = 0usize;
    let mut last_assistant = false;
    for m in &sess.messages[..upto] {
        match m {
            ChatMessage::System(_) => {}
            ChatMessage::User(_) => {
                n += 1;
                last_assistant = false;
            }
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
            } => {
                if content.is_empty() && reasoning.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                if !last_assistant {
                    n += 1;
                    last_assistant = true;
                }
            }
            ChatMessage::Tool { .. } => {}
        }
    }
    n
}

/// 同 `session_view`，但只产出 `messages[from..]` 对应的那部分。
///
/// **`from` 必须是一个「回合边界」**（上一条是 User，或 0）—— 否则开头那个
/// assistant 会被当成新的一条，而完整视图会把它合并进前一个 assistant。
/// 调用方（`/api/session`）就是这么取 from 的。
///
/// 存在的理由：会话是只追加的，除当前正在长的那个回合，前面的输出都是最终的，
/// 客户端不用每轮重收整段历史（含每个 50KB 的 bash 输出）。
pub fn session_view_from(sess: &ChatSession, from: usize) -> Vec<Value> {
    let from = from.min(sess.messages.len());
    // 工具结果按 id 索引：只需要切片内的（一个回合的 tool 结果都在它自己后面）
    let mut results: HashMap<&str, &str> = HashMap::new();
    for m in &sess.messages[from..] {
        if let ChatMessage::Tool {
            tool_call_id,
            content,
            ..
        } = m
        {
            results.insert(tool_call_id.as_str(), content.as_str());
        }
    }

    let parse = |s: &str| -> Value {
        serde_json::from_str(s).unwrap_or_else(|_| json!(s))
    };

    let mut out: Vec<Value> = Vec::new();
    for (i, m) in sess.messages.iter().enumerate().skip(from) {
        match m {
            // 系统提示词（第一条，以及后续的“提示词升级”）是给模型看的，不给用户看
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
                        // 没有输出就是还在跑（服务端此刻正在执行）
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

/// 跑一个用户回合，把 chunk 推到 `tx`。
///
/// `session`：指定写进哪个会话（调试端用 —— 见 `scripts/agentctl.py` 的
/// `--session`）。`None` = 写当前活动会话，也就是前端的行为。
pub async fn run_turn(
    core: &Core,
    provider: &Provider,
    user_text: &str,
    session: Option<&str>,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    core.state
        .turn_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // 上一回合的「停止」不能把这一回合也停了
    core.state
        .abort_turn
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let _guard = TurnGuard(core.clone());
    // 系统提示词可能被 /api/debug/system-prompt 覆盖过，所以不能直接用常量。
    // 在拿会话写锁之前先取出来，避免锁套锁。
    let system_prompt = core.state.system_prompt();
    let sid = {
        let mut store = core.state.chat.write().await;
        // 指定了会话就写它：调试端从 CLI 发消息时不该动用户正在看的那条会话。
        let sess = match session {
            Some(id) => match store.get_mut(id) {
                Some(s) => s,
                None => anyhow::bail!("没有这个会话: {id}"),
            },
            None => store.active_mut(),
        };
        // 历史里只追加两类东西：待补的系统提示词（见 `ensure_system_prompt`）
        // 和这条用户消息。不插任何“每轮重发”的上下文块。
        ensure_system_prompt(sess, &system_prompt);
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

    // 不设轮数上限：任务要多少轮就跑多少轮，"干到一半停下" 不是可接受的
    // 行为。唯一的出口是模型自己不再调工具，或者用户按停止
    //（`POST /api/stop` → `abort_requested`）。
    loop {
        if core.state.abort_requested() {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }
        send(tx, sse::start_step()).await;

        let step_out = match stream_one(core, provider, sid, tx).await {
            Ok(c) => c,
            Err(e) => {
                send(tx, sse::error(&format!("{e:#}"))).await;
                send(tx, sse::finish()).await;
                return Err(e);
            }
        };
        send(tx, sse::finish_step()).await;
        let step = step_out;

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

        for call in &calls {
            // 用户按了停止：这一批剩下的工具不再执行
            if core.state.abort_requested() {
                send(tx, sse::abort("已停止")).await;
                send(tx, sse::finish()).await;
                return Ok(());
            }
            // 工具一律直接执行，没有审批环节。不存在的工具由 execute_tool
            // 返回错误，模型看到「未知工具 X」自己会改。
            let outcome =
                execute_tool(core, &call.name, &call.arguments, Some((tx, &call.id))).await;
            send(
                tx,
                sse::tool_output_available(&call.id, outcome.result.clone()),
            )
            .await;
            append(
                core,
                sid,
                ChatMessage::Tool {
                    tool_call_id: call.id.clone(),
                    content: outcome.result.to_string(),
                    images: outcome.images,
                },
            )
            .await;
        }
        // 工具结果回灌，继续下一轮
    }
}

/// 构造一次完整的请求体（含工具表）。
///
/// 抽成纯函数有两个理由：
/// 1. 让「工具表也是可缓存前缀的一部分」这条不变式**可测** —— 只比
///    `messages` 不够，前一轮的完整请求体必须原样出现在后一轮里。
/// 2. 它直接产出**强类型**请求，交给 `create_stream_byot` 一次序列化。
///
/// `pub` 是给 `/api/debug/request` 用的（把模型将看到的东西原样吐出来）。
pub fn build_request(
    sess: &ChatSession,
    model: &str,
    supports_images: bool,
) -> anyhow::Result<CreateChatCompletionRequest> {
    let messages = build_messages(sess, supports_images)?;
    let tools = tool_definitions()?;

    let req = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(messages)
        .tools(tools)
        .stream(true)
        // 要 provider 在流末尾回报 token 用量。
        //
        // 用类型里的字段，**不要**先 `to_value` 再补键：那会把整份请求
        // （含整段历史，可能几 MB）先物化成 Value 树、再序列化一次 ——
        // 每个 step 白拷一遍最贵的东西（一个回合最多 8 个 step）。
        .stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        })
        .build()?;
    Ok(req)
}

/// 调一次 LLM，把 delta 转成 chunk 推出去，返回本轮产出。
async fn stream_one(
    core: &Core,
    provider: &Provider,
    sid: &str,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<StepOutput> {
    // 这个模型能不能看图：自动判定（models.dev，带缓存 + 落盘）。
    // 在拿会话锁之前算 —— 首次判定要联网，不能拖着锁。
    let supports_images = core.state.supports_images_for(provider).await;

    // SYSTEM_PROMPT 是静态常量，**这里不做任何插值** —— 见它上面的说明。
    // 动态的界面/环境信息在 run_turn 里作为独立消息进历史，保证前缀可缓存。
    let (req, session_id) = {
        let store = core.state.chat.read().await;
        let sess = store
            .get(sid)
            .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
        (
            build_request(sess, &provider.model, supports_images)?,
            sess.provider_session.clone(),
        )
    };

    let client = provider.client(&session_id)?;
    // 强类型请求 → `create_stream_byot` 直接序列化，不做多余的 Value 中转。
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


    let mut calls: Vec<RecordedToolCall> = acc
        .into_iter()
        .filter(|(id, name, _)| !id.is_empty() && !name.is_empty())
        .map(|(id, name, arguments)| RecordedToolCall {
            id,
            name,
            arguments,
        })
        .collect();

    // 被输出长度上限截断时要告诉用户，否则看起来就像模型话说到一半。
    //
    // 而且这一轮的**工具调用全部作废**：截断意味着参数 JSON 很可能是半个
    // 对象，而 `execute_tool` 遇到坏 JSON 会退化成 `{}` 再报「缺少参数」——
    // 拿着坏参数去点手机，比直接说清楚要危险得多。
    if finish.as_deref() == Some("length") {
        let dropped = calls.len();
        let message = if dropped > 0 {
            format!("输出达到长度上限被截断，{dropped} 个不完整的工具调用已作废，请重试")
        } else {
            "输出达到长度上限被截断（provider 的 max_tokens）".to_string()
        };
        send(tx, sse::data_part("warning", json!({ "message": message }))).await;
        for c in &calls {
            send(
                tx,
                sse::tool_output_available(
                    &c.id,
                    json!({ "error": "输出被截断，本次工具调用已作废，请重试" }),
                ),
            )
            .await;
        }
        calls.clear();
    }

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

/// 把 bash 的实时输出攒一攒推给前端。
///
/// 不逐块推：一条 `apk add` 每秒几十块，前端每块都重排一次面板。攒
/// LIVE_EVERY 推一次，每次推**到目前为止的尾部**（前端直接替换，不用拼）。
/// 只留尾部 LIVE_TAIL 字节 —— 输出几 MB 的命令也不会每次推几 MB。
async fn pump_live_output(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    tx: mpsc::UnboundedSender<Value>,
    id: String,
) {
    const LIVE_EVERY: std::time::Duration = std::time::Duration::from_millis(120);
    const LIVE_TAIL: usize = 32 * 1024;

    let started = std::time::Instant::now();
    let mut text = String::new();
    // UTF-8 可能被切在块中间：不完整的尾巴留到下一块
    let mut pending: Vec<u8> = Vec::new();
    let mut dirty = false;
    let mut tick = tokio::time::interval(LIVE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let flush = |text: &str| {
        let _ = tx.send(sse::tool_output_preliminary(
            &id,
            json!({
                "output": text,
                "running": true,
                "wall_time_seconds": (started.elapsed().as_millis() as f64 / 100.0).round() / 10.0,
            }),
        ));
    };

    loop {
        tokio::select! {
            chunk = rx.recv() => match chunk {
                Some(b) => {
                    pending.extend_from_slice(&b);
                    match std::str::from_utf8(&pending) {
                        Ok(s) => {
                            text.push_str(s);
                            pending.clear();
                        }
                        Err(e) if e.error_len().is_none() => {
                            let ok = e.valid_up_to();
                            text.push_str(std::str::from_utf8(&pending[..ok]).unwrap_or(""));
                            pending.drain(..ok);
                        }
                        Err(_) => {
                            text.push_str(&String::from_utf8_lossy(&pending));
                            pending.clear();
                        }
                    }
                    if text.len() > LIVE_TAIL * 2 {
                        let mut cut = text.len() - LIVE_TAIL;
                        while !text.is_char_boundary(cut) {
                            cut += 1;
                        }
                        text.drain(..cut);
                    }
                    dirty = true;
                }
                None => break,
            },
            _ = tick.tick() => {
                if dirty {
                    dirty = false;
                    flush(&text);
                }
            }
        }
    }
    if dirty {
        flush(&text);
    }
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

/// `read` 的 offset：从 1 开始；缺失/非法/<=0 都退回第 1 行。
fn parse_offset(args: &Value) -> usize {
    args.get("offset")
        .and_then(|v| v.as_i64())
        .map(|n| n.max(1) as usize)
        .unwrap_or(1)
}

/// `bash` 的 timeout（秒）：**必填**。缺失、非数、非正数都返回 `None`，由调用方
/// 回一条「先估算一下」的错误给模型。超过 `MAX_TIMEOUT` 就夹到上限。
fn parse_timeout(args: &Value) -> Option<std::time::Duration> {
    let secs = args.get("timeout")?.as_f64()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    let capped = secs.min(linux::MAX_TIMEOUT.as_secs_f64());
    Some(std::time::Duration::from_secs_f64(capped))
}

/// `read` 的 limit：缺失或 null = None，负数/非整数报错。
fn parse_limit(args: &Value) -> anyhow::Result<Option<usize>> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| anyhow::anyhow!("limit 必须是整数"))?;
            anyhow::ensure!(n >= 0, "limit 不能是负数");
            Ok(Some(n as usize))
        }
    }
}

// ---------------------------------------------------------------------------
// 工具执行
// ---------------------------------------------------------------------------

/// 一次工具执行的结果：回给模型/前端的 JSON，外加要内联给模型的图片。
struct ToolOutcome {
    result: Value,
    images: Vec<ImagePart>,
}

impl ToolOutcome {
    /// 没有图片的普通结果（绝大多数工具）。
    fn json(result: Value) -> Self {
        Self {
            result,
            images: Vec::new(),
        }
    }
}

/// `live`：(推给前端的通道, 工具调用 id)。给了就把 bash 的输出边跑边推过去。
async fn execute_tool(
    core: &Core,
    name: &str,
    arguments: &str,
    live: Option<(&mpsc::UnboundedSender<Value>, &str)>,
) -> ToolOutcome {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

    // read / bash / edit 都不走设备（`DeviceApi`）：read 和 edit 直接
    // 读写 rootfs 里的文件，bash 起 proot 子进程。
    if name == "read" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 path" }));
        }
        let offset = parse_offset(&args);
        let limit = match parse_limit(&args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };

        return match core.read_any(path, offset, limit).await {
            Ok(linux::ReadResult::Text(view)) => ToolOutcome::json(json!({
                "output": view.content,
                "truncated": view.truncated,
                "totalLines": view.total_lines,
            })),
            Ok(linux::ReadResult::Image(img)) => read_image_outcome(core, path, img).await,
            Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
    }

    if name == "bash" {
        let script = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if script.trim().is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 command" }));
        }
        // timeout **必填**：不让模型靠一个偏短的默认值蒙混，逼它先想「这条
        // 命令要跑多久」。估小了的代价是命令被 SIGKILL、前面的活儿白干，
        // 所以宁可估大。
        let Some(timeout) = parse_timeout(&args) else {
            return ToolOutcome::json(json!({
                "error": "缺少参数 timeout（或不是正数）。调用 bash 之前必须先估算这条命令\
                          要跑多久，把秒数显式传进来：下载 / `apk add` / 编译 / `git clone`\
                          可能要几十分钟到几小时。**宁可估大** —— 估小了命令会被中途杀掉，\
                          之前的进度全白费；多估不会真的等（命令一结束就返回）。上限 24 小时。"
            }));
        };

        let res = match live {
            Some((tx, id)) => {
                let (btx, brx) = mpsc::unbounded_channel();
                let pump = tokio::spawn(pump_live_output(brx, tx.clone(), id.to_string()));
                let r = core.run_linux_live(script, timeout, btx).await;
                // 等最后一批推完，再发最终结果，保证顺序
                let _ = pump.await;
                r
            }
            None => core.run_linux(script, timeout).await,
        };
        return match res {
            Ok(r) => ToolOutcome::json(json!({
                "output": r.to_text(),
                "exit_code": r.code,
                "truncated": r.truncated,
                "full_output_path": r.full_output_path,
                "wall_time_seconds": (r.duration_ms as f64) / 1000.0,
            })),
            Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
    }

    if name == "edit" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 path" }));
        }
        let edits = match parse_edits(&args) {
            Ok(e) => e,
            Err(e) => return ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };

        return match core.edit_file(path, &edits).await {
            Ok(out) => ToolOutcome::json(json!({
                "ok": true,
                "message": format!("已替换 {} 处", edits.len()),
                "firstChangedLine": out.first_changed_line,
                "diff": out.patch,
            })),
            Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
    }

    // 设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）现在**不作为工具**
    // 暴露给模型，以后收进沙箱里的一个命令、由 bash 调用。
    ToolOutcome::json(json!({ "error": format!("未知工具 {name}") }))
}

/// 图片结果：缩好的字节落盘（历史回放时还要重发），回给模型一段能读的说明
/// 外加原图路径；图片本体作为 `images` 走 `build_messages` 里那条 carrier。
async fn read_image_outcome(
    core: &Core,
    guest_path: &str,
    img: crate::imaging::Encoded,
) -> ToolOutcome {
    // 模型不支持图片时不落盘、不附带：只给一段说明。
    // 用缓存里的自动判定值（本回合开始时 stream_one 已经算过）。
    let supports_images = core
        .state
        .llm()
        .map(|p| core.state.cached_supports_images(&p))
        .unwrap_or(true);

    let mime = img.mime.clone();
    let original = format!("{}×{}", img.original_width, img.original_height);
    let mut output = format!("读取图片 {mime}（{original}");
    if (img.width, img.height) != (img.original_width, img.original_height) {
        output.push_str(&format!("，已缩到 {}×{}", img.width, img.height));
    }
    output.push_str(&format!(
        "）。原文件：{guest_path} —— 要裁剪或看元数据就用这个路径。"
    ));

    if !supports_images {
        output.push_str("\n[当前模型不支持图片输入，这张图没有发给模型]");
        return ToolOutcome::json(json!({
            "output": output,
            "imagePath": guest_path,
            "imageMime": mime,
            "truncated": false,
        }));
    }

    match core.save_image(img.bytes, mime.clone()).await {
        Ok(stored) => ToolOutcome {
            result: json!({
                "output": output,
                "imagePath": guest_path,
                "imageMime": mime.clone(),
                "imageWidth": img.width,
                "imageHeight": img.height,
                "truncated": false,
            }),
            images: vec![ImagePart {
                path: stored.display().to_string(),
                mime,
                source: guest_path.to_string(),
                tool: "read".to_string(),
            }],
        },
        Err(e) => {
            tracing::warn!("图片落盘失败: {e:#}");
            output.push_str(&format!("\n[图片没能保存，本轮没发给模型：{e:#}]"));
            ToolOutcome::json(json!({
                "output": output,
                "imagePath": guest_path,
                "imageMime": mime,
                "truncated": false,
            }))
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(sess: &ChatSession) -> Vec<serde_json::Value> {
        build_messages(sess, true)
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

        // 第 1 轮：run_turn 干的事
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("第一问".into()));
        let turn1 = wire(&sess);

        // 第 2 轮：追加 assistant / tool / 新用户输入
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
            images: vec![],
        });
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("第二问".into()));
        let turn2 = wire(&sess);

        assert!(turn2.len() > turn1.len());
        assert_eq!(
            &turn2[..turn1.len()],
            &turn1[..],
            "第 2 轮不是以第 1 轮为前缀 → prompt cache 全部失效"
        );
    }

    /// 改了系统提示词之后，**老会话的前缀一个字都不能动**。
    ///
    /// 系统提示词存在历史里（第一条），新提示词是**追加上去的**一条 system
    /// 消息 —— 老前缀原样保留（缓存照命中），模型也能看到新要求。
    /// 绝不能把第一条替掉。
    #[test]
    fn a_prompt_change_is_appended_and_keeps_the_prefix() {
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, "旧提示词");
        sess.messages.push(ChatMessage::User("第一问".into()));
        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: String::new(),
            tool_calls: vec![],
        });
        let before = wire(&sess);

        // 版本升级了，下一轮开工前
        ensure_system_prompt(&mut sess, "新提示词");
        let after = wire(&sess);
        assert_eq!(&after[..before.len()], &before[..], "老前缀被动 → 缓存全废");
        assert_eq!(after[0]["content"], "旧提示词", "第一条必须是旧的，不能替换");
        assert_eq!(after.last().unwrap()["content"], "新提示词");

        // 同一条提示词不能反复追加
        let n = after.len();
        ensure_system_prompt(&mut sess, "新提示词");
        assert_eq!(wire(&sess).len(), n, "重复追加了同一条提示词");

        // 再发一轮，前缀继续单调增长
        sess.messages.push(ChatMessage::User("第二问".into()));
        let next = wire(&sess);
        assert_eq!(&next[..n], &after[..]);
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
    fn the_request_is_the_history_verbatim() {
        // `build_messages` 就是「历史原样」：不拼、不前置、不插上下文块。
        // 系统提示词是历史里的一条（`ensure_system_prompt` 注的），
        // 所以 请求体长度 == 历史长度，一条不多一条不少。
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("问".into()));
        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: String::new(),
            tool_calls: vec![],
        });
        sess.messages.push(ChatMessage::User("再问".into()));

        let w = wire(&sess);
        assert_eq!(w.len(), sess.messages.len(), "请求体里多出了别的东西");
        assert_eq!(w[0]["role"], "system");
        assert_eq!(w[0]["content"], SYSTEM_PROMPT);
        assert_eq!(
            w.iter().filter(|m| m["role"] == "system").count(),
            1,
            "除了第一条，不该再有任何 system 消息"
        );
    }

    /// 只比 `messages` 不够：工具表也是可缓存前缀的一部分。
    /// 完整请求体（含 tools / stream_options）跨轮必须是前缀关系。
    #[test]
    fn full_request_body_is_prefix_stable_across_turns() {
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("第一问".into()));
        // 转成 Value 是为了断言请求体本身；线上路径不经过这一步
        let req1 =
            serde_json::to_value(build_request(&sess, "deepseek-v4.1-flash", true).unwrap())
                .unwrap();

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
            images: vec![],
        });
        sess.messages.push(ChatMessage::User("第二问".into()));
        let req2 =
            serde_json::to_value(build_request(&sess, "deepseek-v4.1-flash", true).unwrap())
                .unwrap();

        assert_eq!(req1["tools"], req2["tools"], "工具表变了 → 整段前缀缓存失效");
        assert_eq!(req1["model"], req2["model"]);
        assert_eq!(req1["stream_options"], req2["stream_options"]);
        // 用量上报是缓存命中率的唯一来源，不能因为换成强类型字段就丢了
        assert_eq!(req1["stream_options"], json!({ "include_usage": true }));
        let a = req1["messages"].as_array().unwrap();
        let b = req2["messages"].as_array().unwrap();
        assert_eq!(&b[..a.len()], &a[..], "完整请求体不是前缀关系");
    }

    /// 工具返回的图片：必须作为一条 `user` 消息发在**所有** tool 消息之后。
    ///
    /// 顺序错了 OpenAI 直接 400（tool 消息之间不能插 user），而且这种错
    /// 只有真机、只有「一个回合调了好几个工具、其中一个带图」时才会出现。
    #[test]
    fn tool_images_are_carried_after_all_tool_replies() {
        let img = std::env::temp_dir().join(format!("agent-carrier-{}.png", std::process::id()));
        std::fs::write(&img, b"\x89PNG\r\n\x1a\nfake").unwrap();
        let call = |id: &str| RecordedToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        };

        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::User("看图".into()),
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![call("c1"), call("c2")],
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "{}".into(),
                images: vec![ImagePart {
                    path: img.display().to_string(),
                    mime: "image/png".into(),
                    source: "/workspace/a.png".into(),
                    tool: "read".into(),
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "c2".into(),
                content: "{}".into(),
                images: vec![],
            },
        ];

        let v: Vec<Value> = build_messages(&sess, true)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let roles: Vec<&str> = v.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "tool", "user"]);
        let parts = v[4]["content"].as_array().expect("carrier 是内容数组");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // 同一份历史必须序列化出**完全一样**的请求：图片从盘上重读，
        // 字节必须一致，否则 prompt cache 每轮都失效。
        let again: Vec<Value> = build_messages(&sess, true)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v, again, "带图的历史不是逐字节可复现的");

        // 模型不支持图片时：仍然有这条 carrier，但只有文字说明
        let v: Vec<Value> = build_messages(&sess, false)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v[4]["role"], "user");
        let parts = v[4]["content"].as_array().unwrap();
        assert!(parts.iter().all(|p| p["type"] == "text"));

        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn tool_schema_order_and_names_are_fixed() {
        // 工具表也是可缓存前缀的一部分。顺序必须稳定，
        // 所以这里把顺序钉死：谁想改都得先改这个测试。
        //
        // 设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）暂时**不作为工具**
        // 暴露给模型，以后收进沙箱里的一个命令、由 bash 调用。
        let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["read", "bash", "edit"]);
        // 重复调用必须完全一致
        assert_eq!(names, tools().iter().map(|t| t.name).collect::<Vec<_>>());
    }

    /// 删掉的设备工具不能在提示词里留名字。
    ///
    /// 留了模型就会去调一个不存在的工具，换回「错误：没有名为 device_snapshot
    /// 的工具」，白烧一轮，而且这种提示词与实际工具表不一致的 bug 很难在真机上看出来。
    #[test]
    fn removed_device_tools_are_gone_from_prompt_and_registry() {
        let gone = [
            "device_snapshot",
            "device_tap",
            "device_type",
            "device_key",
            "device_launch",
        ];
        for g in gone {
            assert!(!SYSTEM_PROMPT.contains(g), "提示词还在提 {g}");
            assert!(!tools().iter().any(|t| t.name == g), "工具表里还有 {g}");
        }
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
    fn parse_offset_and_limit_match_the_schema() {
        // 缺省 → 第 1 行、读到上限
        assert_eq!(parse_offset(&json!({})), 1);
        assert_eq!(parse_limit(&json!({})).unwrap(), None);
        assert_eq!(parse_limit(&json!({ "limit": null })).unwrap(), None);

        assert_eq!(parse_offset(&json!({ "offset": 42 })), 42);
        // 非法/越界的 offset 退回第 1 行，不报错（模型偶发写 0）
        assert_eq!(parse_offset(&json!({ "offset": 0 })), 1);
        assert_eq!(parse_offset(&json!({ "offset": -3 })), 1);
        assert_eq!(parse_offset(&json!({ "offset": "x" })), 1);

        assert_eq!(parse_limit(&json!({ "limit": 5 })).unwrap(), Some(5));
        assert_eq!(parse_limit(&json!({ "limit": 0 })).unwrap(), Some(0));
        assert!(parse_limit(&json!({ "limit": -1 })).is_err());
        assert!(parse_limit(&json!({ "limit": "5" })).is_err());
    }

    #[test]
    fn bash_timeout_is_required_and_capped() {
        // 缺失 / 非法都不能偷偷用一个短默认值
        assert!(parse_timeout(&json!({})).is_none());
        assert!(parse_timeout(&json!({ "timeout": null })).is_none());
        assert!(parse_timeout(&json!({ "timeout": 0 })).is_none());
        assert!(parse_timeout(&json!({ "timeout": -5 })).is_none());
        assert!(parse_timeout(&json!({ "timeout": "600" })).is_none());

        // 普通值原样，长时间任务（比如 3 小时下载）完全允许
        assert_eq!(parse_timeout(&json!({ "timeout": 600 })).unwrap().as_secs(), 600);
        assert_eq!(
            parse_timeout(&json!({ "timeout": 10800 })).unwrap().as_secs(),
            10800
        );
        // 超过上限就夹到 24h，而不是报错或无限大
        assert_eq!(parse_timeout(&json!({ "timeout": 999_999_999 })).unwrap(), linux::MAX_TIMEOUT);
        assert!(linux::MAX_TIMEOUT >= std::time::Duration::from_secs(6 * 3600));
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
            ChatMessage::Tool { tool_call_id: "c1".into(), content: "{}".into(), images: vec![] },
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

    /// 增量同步的核心不变式：从任意「回合边界」切开，
    /// `count_before(from) + view_from(from)` 必须逐字节等于完整视图。
    #[test]
    fn incremental_session_slice_matches_the_full_view() {
        let call = |id: &str| RecordedToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User("U1".into()),
            ChatMessage::Assistant {
                content: "先看看".into(),
                reasoning: "想".into(),
                tool_calls: vec![call("c1")],
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "{}".into(),
                images: vec![],
            },
            ChatMessage::Assistant {
                content: "答1".into(),
                reasoning: String::new(),
                tool_calls: vec![],
            },
            ChatMessage::User("U2".into()),
            ChatMessage::Assistant {
                content: "答2".into(),
                reasoning: String::new(),
                tool_calls: vec![],
            },
        ];
        let full = session_view(&sess);

        // 只在回合边界切（上一条是 User，或 from == 0）
        for from in 0..=sess.messages.len() {
            if from > 0 && !matches!(&sess.messages[from - 1], ChatMessage::User(_)) {
                continue;
            }
            let head = session_view_count_before(&sess, from);
            let tail = session_view_from(&sess, from);
            let joined: Vec<Value> = full[..head].iter().cloned().chain(tail).collect();
            assert_eq!(joined, full, "from={from} 时增量结果与完整视图不一致");
        }
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

        // 两套字段同时出现、值不一致时取**大**的那个。之前用 `.or()`，
        // OpenAI 系填了 `cached_tokens: 0`（而不是省略）就会把真值吞掉，
        // 日志误报成 0% 命中 —— 而这是判断缓存有没有生效的唯一硬证据。
        let both: Usage = serde_json::from_value(json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": { "cached_tokens": 0 },
            "prompt_cache_hit_tokens": 768
        }))
        .unwrap();
        assert_eq!(both.cached_tokens(), Some(768));
        assert_eq!(both.cache_hit_ratio(), Some(0.768));

        // 命中率夹在 0..1（有的实现里 prompt_tokens 是 miss-only）
        let weird: Usage = serde_json::from_value(json!({
            "prompt_tokens": 100,
            "prompt_cache_hit_tokens": 250
        }))
        .unwrap();
        assert_eq!(weird.cache_hit_ratio(), Some(1.0));

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

    // —— 侧边栏搜索 ——

    fn store_with_two_sessions() -> SessionStore {
        let mut a = ChatSession::new();
        a.title = "Alpine 版本".into();
        a.updated_at = 200;
        a.messages = vec![
            ChatMessage::System("系统提示词里有 apk 这个词，不该被搜到".into()),
            ChatMessage::User("看一下 apk 版本".into()),
            ChatMessage::Assistant {
                content: "Alpine 3.21\n第二行".into(),
                reasoning: String::new(),
                tool_calls: vec![],
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "apk 的输出".into(),
                images: vec![],
            },
        ];

        let mut b = ChatSession::new();
        b.title = "FizzBuzz".into();
        b.updated_at = 100;
        b.messages = vec![ChatMessage::User("写个 fizzbuzz".into())];

        SessionStore { sessions: vec![a, b], active: String::new() }
    }

    #[test]
    fn search_hits_message_text_case_insensitively() {
        let store = store_with_two_sessions();

        let hits = store.search("APK", 3, 60);
        assert_eq!(hits.len(), 1, "只搜用户/助手正文，系统提示词和工具结果不算");
        assert_eq!(hits[0]["title"], "Alpine 版本");
        assert_eq!(hits[0]["role"], "user");
        assert_eq!(hits[0]["snippet"], "看一下 apk 版本");

        let hits = store.search("alpine", 3, 60);
        assert_eq!(hits[0]["snippet"], "Alpine 3.21 第二行", "换行要拍成空格");
    }

    #[test]
    fn search_is_ordered_by_recency_and_capped() {
        let store = store_with_two_sessions();
        // "a" 两个会话都命中；更新的那个（a，updated_at=200）排前面
        let hits = store.search("a", 3, 60);
        assert!(hits.len() >= 2);
        assert_eq!(hits[0]["title"], "Alpine 版本");

        // 每个会话最多 3 条
        let mut many = ChatSession::new();
        many.messages = (0..10).map(|i| ChatMessage::User(format!("命中 {i}"))).collect();
        let store = SessionStore { sessions: vec![many], active: String::new() };
        assert_eq!(store.search("命中", 3, 60).len(), 3);
        assert_eq!(store.search("命中", 3, 1).len(), 1, "总量封顶");
    }

    #[test]
    fn search_handles_empty_and_multibyte() {
        let store = store_with_two_sessions();
        assert!(store.search("", 3, 60).is_empty());
        assert!(store.search("   ", 3, 60).is_empty());
        assert!(store.search("不存在的词", 3, 60).is_empty());

        // 中文按字符切，不能切出半截 UTF-8
        let mut s = ChatSession::new();
        s.messages = vec![ChatMessage::User(format!(
            "{}测试{}",
            "开头".repeat(100),
            "尾巴".repeat(100)
        ))];
        let store = SessionStore { sessions: vec![s], active: String::new() };
        let hits = store.search("测试", 3, 60);
        assert_eq!(hits.len(), 1);
        let snip = hits[0]["snippet"].as_str().unwrap();
        assert!(snip.starts_with('…') && snip.ends_with('…'));
        assert!(snip.chars().count() < 90);
    }

    // —— 空会话 ——

    /// 建了会话但一句话没说 → 不进侧边栏列表。
    ///
    /// 前端点「新对话」已经不再让服务端建会话了，这里是兜底：
    /// 调试端/CLI 直接 create、`/api/reset` 清空后的会话都走这条。
    #[test]
    fn empty_sessions_stay_out_of_the_list() {
        let mut store = SessionStore::default();
        let id = store.create();
        assert!(store.list().is_empty(), "新建但没说话：不该出现在列表里");

        store
            .get_mut(&id)
            .unwrap()
            .messages
            .push(ChatMessage::User("问了".into()));
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], id.as_str());
        assert!(listed[0]["active"].as_bool().unwrap());

        // 清空过（/api/reset）又回到「不算数」
        store.get_mut(&id).unwrap().clear();
        assert!(store.list().is_empty());
    }
}
