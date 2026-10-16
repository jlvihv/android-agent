//! Agent 主循环。
//!
//! 决策 D5 选了「自研薄 loop」而不是 `rig`，原因就在这个文件里：
//! 每一个事件都要精确地转成 AI SDK UI Message Stream 的 chunk 推给前端。
//! 框架会把 loop 包起来，反而挡路。
//!
//! 工具调用**一律直接执行**，没有审批环节 —— 服务端因此没有「挂起等回复」
//! 的状态，回合结束就是结束，手机 App 随时被杀都没关系。

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::llm::{Provider, Transport};
use crate::openai::{
    ChatRequest, FileData, FunctionCall, FunctionDef, ImageUrl, Message, Part, StreamOptions, Tool,
    ToolCall, Usage, UserContent,
};
use crate::platform::Platform;
use crate::prompt;
use crate::responses::{
    AssistantItem, FunctionCallItem, FunctionCallOutputItem, InputItem, InputPart, MessageContent,
    MessageItem, OutputPart, Reasoning, ResponsesRequest, ToolOutput,
};
use crate::{linux, sse, Core};

// ---------------------------------------------------------------------------
// 工具注册表
// ---------------------------------------------------------------------------

pub struct ToolSpec {
    pub name: &'static str,
    /// 按平台拼出来的（`bash` 那条「在哪跑、有什么」随平台变），拼一次就冻结
    pub description: String,
    pub schema: fn() -> Value,
}

/// 工具表每个平台只建一次。
///
/// 每个 step 都要把整张表序列化进请求（含那段很长的 `bash` 描述），
/// 工具表是**可缓存前缀的一部分**，只建一次也让「字节确定」更保险。
pub fn tools(p: Platform) -> &'static [ToolSpec] {
    static TOOLS: [OnceLock<Vec<ToolSpec>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    TOOLS[p as usize].get_or_init(|| build_tools(p))
}

fn build_tools(p: Platform) -> Vec<ToolSpec> {
    // 文案和 schema 都在 `prompt` 里（改提示词不用翻这个文件）
    vec![
        ToolSpec {
            name: "read",
            description: prompt::read_description(),
            schema: prompt::read_schema,
        },
        ToolSpec {
            name: "bash",
            description: prompt::bash_description(p),
            schema: prompt::bash_schema,
        },
        ToolSpec {
            name: "edit",
            description: prompt::edit_description(),
            schema: prompt::edit_schema,
        },
    ]
}

// ---------------------------------------------------------------------------
// 会话状态
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatMessage {
    System(String),
    /// 用户消息。`attachments` 是在输入卡片里附的图片 / PDF（没附就是空的）。
    User {
        text: String,
        attachments: Vec<Attachment>,
    },
    Assistant {
        content: String,
        reasoning: String,
        tool_calls: Vec<RecordedToolCall>,
        /// Responses API 给的思考条目，**原样**（见 `StreamChunk::reasoning_item`）。
        ///
        /// 下一次请求要原样放回 input —— 不放的话模型每一步都看不到自己上一步
        /// 想了什么（不报错，只是更容易前后不连贯、每步重想一遍）。`reasoning`
        /// 是给人看的文字，这个是给服务端的；Chat Completions 那边没有它。
        reasoning_items: Vec<Value>,
    },
    Tool {
        tool_call_id: String,
        content: String,
        /// 这次工具调用返回的图片（`read` 读图片时才有）。
        ///
        /// 不放进 `content` 里：OpenAI 的 tool 消息只能是字符串，图片必须
        /// 作为紧随其后的一条 `user` 消息发（多个 tool 调用要等全部发完）。
        /// 见 `build_messages`。
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
}

/// 用户从界面上附的一个文件。
///
/// 字节**落盘**（`<filesDir>/attachments/<id>`）而不是进历史：和图片
/// carrier 同一个理由 —— 后面每轮都要重发，SQLite 里不能堆 base64。
/// `id` 同时是磁盘文件名，也是前端取回的 URL 段（`/api/attachment/<id>`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    /// 绝对路径（data_dir 底下），组装请求时从盘上读
    pub path: String,
    pub mime: String,
    /// 用户选文件时的原始文件名，给模型看
    pub name: String,
    pub kind: AttachmentKind,
}

/// 附件类型：图片走 `image_url`，PDF 走 `file`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Pdf,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl ChatMessage {
    /// 一条不带附件的用户消息。
    pub fn user(text: impl Into<String>) -> Self {
        ChatMessage::User {
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    /// 是不是一条用户消息。回合边界、标题、计数都靠它。
    pub fn is_user(&self) -> bool {
        matches!(self, ChatMessage::User { .. })
    }

    /// 用户消息的正文（非用户消息返回 None）。
    pub fn user_text(&self) -> Option<&str> {
        match self {
            ChatMessage::User { text, .. } => Some(text),
            _ => None,
        }
    }

    /// 转成 Chat Completions 的一条消息。
    ///
    /// 带附件的用户消息正常路径由 `build_messages` 处理（它知道模型
    /// 支不支持图片/PDF）；这里只保证单独调用时也不炸，正文里点名有哪些附件。
    fn to_openai(&self) -> Message {
        match self {
            ChatMessage::System(s) => Message::System { content: s.clone() },
            ChatMessage::User { text, attachments } => {
                let mut body = text.clone();
                for a in attachments {
                    body.push_str(&format!("\n[附件：{}（{}）]", a.name, a.mime));
                }
                Message::User {
                    content: UserContent::Text(body),
                }
            }
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => Message::Assistant {
                content: (!content.is_empty()).then(|| content.clone()),
                // 思考内容原样回传（见 `openai::Message::Assistant`）
                reasoning_content: (!reasoning.is_empty()).then(|| reasoning.clone()),
                tool_calls: tool_calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        },
                    })
                    .collect(),
            },
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } => Message::Tool {
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
            },
        }
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
    /// 消息**不在内存里**时的摘要；`None` = 消息都在 `messages` 里。
    ///
    /// 不活跃的会话只留元数据（几十个会话、每个带一堆几十 KB 的 bash 输出，
    /// 全放内存是几十 MB）。要用时 `AppState::ensure_loaded` 从库里读回来，
    /// 落盘之后 `AppState::evict_idle` 再放掉。见 `db.rs`。
    #[serde(skip)]
    pub stub: Option<Stub>,
    /// 消息上次从库里读进来的时刻：刚读进来的别马上又放掉。
    #[serde(skip)]
    pub loaded_at: Option<std::time::Instant>,
    /// 清空过几次（`clear`）。落盘靠它认出「整段换了」—— 只看条数的话，
    /// 清空后两秒内又长过了旧长度，库里的旧消息就留下来了。
    #[serde(skip)]
    pub epoch: u32,
}

/// 消息不在内存里的会话，侧边栏和落盘还要知道的那点东西。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stub {
    /// 消息条数（= 库里的行数，也是落盘的水位线）
    pub len: usize,
    /// 其中用户消息几条（侧边栏的计数）
    pub users: usize,
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
            stub: None,
            loaded_at: None,
            epoch: 0,
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
        self.stub = None;
        self.epoch += 1;
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
        self.message_count() > 0
    }

    /// 消息在不在内存里。不在的话 `messages` 是空的，**别拿它当真**。
    pub fn resident(&self) -> bool {
        self.stub.is_none()
    }

    /// 消息条数（不管在不在内存里）。
    pub fn message_count(&self) -> usize {
        self.stub.map_or(self.messages.len(), |s| s.len)
    }

    /// 用户消息条数（不管在不在内存里）。
    pub fn user_count(&self) -> usize {
        match self.stub {
            Some(s) => s.users,
            None => self.messages.iter().filter(|m| m.is_user()).count(),
        }
    }

    /// 放掉内存里的消息，只留摘要。调用方保证它们**已经落盘**。
    pub fn evict(&mut self) {
        if self.resident() {
            self.stub = Some(Stub {
                len: self.messages.len(),
                users: self.user_count(),
            });
            self.messages = Vec::new();
            self.loaded_at = None;
        }
    }

    /// 把从库里读回来的消息装回去（已经在内存里就不动）。
    pub fn install(&mut self, messages: Vec<ChatMessage>) {
        if !self.resident() {
            self.messages = messages;
            self.stub = None;
            self.loaded_at = Some(std::time::Instant::now());
        }
    }
}

/// 一段文本里找 `needle`（忽略大小写），命中就返回带省略号的上下文。
///
/// 按**字符**切而不是字节：中文（和多字节 UTF-8）里按字节切会把字切碎。
fn hit_snippet(text: &str, needle: &str) -> Option<String> {
    const BEFORE: usize = 20;
    const AFTER: usize = 60;

    let raw: Vec<char> = text.chars().collect();
    // 小写化可能改变字符数（`İ` → `i̇` 是两个字符）：逐字符展开，每个小写字符
    // 记住它来自原文第几个字符，命中位置才能换算回原文。
    let mut hay: Vec<char> = Vec::with_capacity(raw.len());
    let mut origin: Vec<usize> = Vec::with_capacity(raw.len());
    for (i, c) in raw.iter().enumerate() {
        for l in c.to_lowercase() {
            hay.push(l);
            origin.push(i);
        }
    }
    let pat: Vec<char> = needle.to_lowercase().chars().collect();
    if pat.is_empty() || hay.len() < pat.len() {
        return None;
    }
    let at_low = hay.windows(pat.len()).position(|w| w == pat.as_slice())?;
    let at = origin[at_low];
    let last = origin[at_low + pat.len() - 1];

    let start = at.saturating_sub(BEFORE);
    let end = (last + 1 + AFTER).min(raw.len());

    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    // 换行拍平成空格，否则摘要里会带出多行
    s.extend(
        raw[start..end]
            .iter()
            .map(|c| if *c == '\n' { ' ' } else { *c }),
    );
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

    /// 删掉并交还这个会话（调用方要清它引用的文件）。
    pub fn delete(&mut self, id: &str) -> Option<ChatSession> {
        let at = self.sessions.iter().position(|s| s.id == id);
        let gone = at.map(|i| self.sessions.remove(i));
        if self.active == id {
            self.active = self
                .sessions
                .first()
                .map(|s| s.id.clone())
                .unwrap_or_default();
        }
        gone
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
                    "count": s.user_count(),
                })
            })
            .collect()
    }

    /// 全文搜：跨会话找用户／助手消息里的子串（忽略大小写），
    /// 每个会话最多回 `per_session` 条，总量封顶 `limit`。
    ///
    /// 线性扫，会话量级是几十，用不上 SQL 的 FTS（少一张表、少一处 schema
    /// 迁移）。消息不在内存里的会话由 `load` 临时从库里读（读不出来就跳过），
    /// 扫完就扔，不装回 store。`load` 是阻塞 I/O，调用方放进 `spawn_blocking`。
    pub fn search(
        &self,
        q: &str,
        per_session: usize,
        limit: usize,
        mut load: impl FnMut(&str) -> Option<Vec<ChatMessage>>,
    ) -> Vec<Value> {
        let needle = q.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut sorted: Vec<&ChatSession> = self.sessions.iter().collect();
        sorted.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

        let mut out = Vec::new();
        for s in sorted {
            let fetched;
            let messages: &[ChatMessage] = if s.resident() {
                &s.messages
            } else {
                let Some(m) = load(&s.id) else { continue };
                fetched = m;
                &fetched
            };
            let mut in_this = 0;
            for m in messages {
                let (role, text) = match m {
                    ChatMessage::User { text, .. } => ("user", text.as_str()),
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

// 提示词（系统提示词 + 工具描述 + schema）都在 `prompt` 模块里，见那里的模块说明。
// `system_prompt` 从这里再导出一次：`state.rs` / `http.rs` / dump_prompt 一直用的是
// `agent::system_prompt`，没必要为了搬家把它们全改一遍。
pub use crate::prompt::system_prompt;

/// 历史末尾那步如果有没结果的工具调用，就地补上（回合开始前调）。
///
/// 两家协议都要求每个 tool call 后面跟着它的结果，缺一个整个请求就 400 ——
/// 那条会话从此再也发不出消息。缺口只可能出在**末尾**（进程在两步之间被杀），
/// 中间的工具调用都在下一步开始前收齐了结果（停止也会补，见 `stop_with_pending`）。
/// 落进历史而不只是组请求时补，界面上那几个工具也就不再一直转圈。
fn close_dangling_tail(sess: &mut ChatSession) {
    let Some(at) = sess
        .messages
        .iter()
        .rposition(|m| !matches!(m, ChatMessage::Tool { .. }))
    else {
        return;
    };
    let ChatMessage::Assistant { tool_calls, .. } = &sess.messages[at] else {
        return;
    };
    let answered: HashSet<&str> = sess.messages[at + 1..]
        .iter()
        .filter_map(|m| match m {
            ChatMessage::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let missing: Vec<String> = tool_calls
        .iter()
        .filter(|tc| !answered.contains(tc.id.as_str()))
        .map(|tc| tc.id.clone())
        .collect();
    for id in missing {
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: id,
            content: prompt::INTERRUPTED.to_string(),
            images: Vec::new(),
        });
    }
}

/// 构造发给 provider 的消息数组 = **会话历史原样**。
///
/// 唯一的“拼”是图片 carrier：`read` 读到图时，tool 消息后面会多一条 `user`
/// 消息（图片只能挂在那里）。它由历史推导出来、字节确定，所以不影响
/// “后一轮以前一轮为前缀”这条不变式 —— 系统提示词仍然是历史的第一条
/// （见 `ensure_system_prompt`），请求体仍然是历史的单调扩展。
fn build_messages(
    sess: &ChatSession,
    supports_images: bool,
    supports_files: bool,
) -> anyhow::Result<Vec<Message>> {
    let msgs = &sess.messages;
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut i = 0;
    while i < msgs.len() {
        // 带附件的用户消息：正文 + 图片 / PDF 内容块拼在一条消息里。
        if let ChatMessage::User { text, attachments } = &msgs[i] {
            out.push(user_message(
                text,
                attachments,
                supports_images,
                supports_files,
            ));
            i += 1;
            continue;
        }
        if !matches!(msgs[i], ChatMessage::Tool { .. }) {
            out.push(msgs[i].to_openai());
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
                    out.push(Message::Tool {
                        tool_call_id: tool_call_id.clone(),
                        content: content.clone(),
                    });
                    for img in images {
                        carriers.push((tool_call_id.clone(), img.clone()));
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        for (id, img) in carriers {
            out.push(image_carrier(&id, &img, supports_images));
        }
    }
    Ok(out)
}

/// 把一张工具返回的图片包成一条 `user` 消息。
///
/// 为什么是 user 消息：Chat Completions 的 tool 消息内容只能是字符串，
/// 工具结果里的图片没有别的合法位置可挂。Responses API 没这个问题（图片
/// 直接放在 `function_call_output` 里，见 `build_responses_input`）。
fn image_carrier(tool_call_id: &str, img: &ImagePart, supports_images: bool) -> Message {
    let label = format!(
        "[read 返回的图片（tool_call_id: {tool_call_id}）；原文件：{}]",
        img.source
    );

    let mut parts: Vec<Part> = vec![text_part(label)];
    if supports_images {
        // 走缓存：每一步都要重新组装请求，不能每步都把同一张图重新 base64。
        // 字节仍然逐字一致（文件不变），prompt cache 照命中。
        match crate::imaging::cached_data_url(&img.path, &img.mime) {
            Ok(url) => parts.push(image_part(url)),
            Err(e) => {
                tracing::warn!("图片读不到了 {}: {e}", img.path);
                parts.push(text_part("[图片文件已丢失，本轮没能附带]".to_string()));
            }
        }
    } else {
        parts.push(text_part(
            "[当前模型不支持图片输入，这张图没有附带；它在沙箱里，可以用 bash 处理或让用户在界面上看]"
                .to_string(),
        ));
    }

    Message::User {
        content: UserContent::Parts(parts),
    }
}

fn text_part(text: String) -> Part {
    Part::Text { text }
}

fn image_part(url: std::sync::Arc<str>) -> Part {
    Part::ImageUrl {
        image_url: ImageUrl { url },
    }
}

fn file_part(name: &str, data_url: std::sync::Arc<str>) -> Part {
    Part::File {
        file: FileData {
            file_data: data_url,
            filename: name.to_string(),
        },
    }
}

/// 一条带附件的用户消息：正文 + 每个附件一个内容块。
///
/// 图片发 `image_url`，PDF 发 `file`（`file_data` 是 data URL）。
/// 模型不支持对应模态时降级成文字说明 —— 宁可能力判定出错时模型少看一眼，
/// 也不能发一个必然 400 的请求。
fn user_message(
    text: &str,
    attachments: &[Attachment],
    supports_images: bool,
    supports_files: bool,
) -> Message {
    if attachments.is_empty() {
        return Message::User {
            content: UserContent::Text(text.to_string()),
        };
    }

    let mut parts: Vec<Part> = Vec::new();
    // 只有附件、没有文字时不要塞一个空 text 块：有些 provider 会把空内容块
    // 当成非法请求。
    if !text.is_empty() {
        parts.push(text_part(text.to_string()));
    }
    for a in attachments {
        parts.push(text_part(format!("[附件：{}（{}）]", a.name, a.mime)));
        match a.kind {
            AttachmentKind::Image => {
                if !supports_images {
                    parts.push(text_part(format!(
                        "[当前模型不支持图片输入，{}没有附带；可以让用户在界面上看，或存到沙箱用 bash 处理]",
                        a.name
                    )));
                    continue;
                }
                match crate::imaging::cached_data_url(&a.path, &a.mime) {
                    Ok(url) => parts.push(image_part(url)),
                    Err(e) => {
                        tracing::warn!("附件读不到了 {}: {e}", a.path);
                        parts.push(text_part(format!("[附件 {} 已丢失，本轮没能附带]", a.name)));
                    }
                }
            }
            AttachmentKind::Pdf => {
                if !supports_files {
                    parts.push(text_part(format!(
                        "[当前模型不支持 PDF 输入，{}没有附带]",
                        a.name
                    )));
                    continue;
                }
                match crate::imaging::cached_data_url(&a.path, &a.mime) {
                    Ok(url) => parts.push(file_part(&a.name, url)),
                    Err(e) => {
                        tracing::warn!("附件读不到了 {}: {e}", a.path);
                        parts.push(text_part(format!("[附件 {} 已丢失，本轮没能附带]", a.name)));
                    }
                }
            }
        }
    }

    Message::User {
        content: UserContent::Parts(parts),
    }
}

/// 工具表的线上形态（Chat Completions：`{"type":"function","function":{…}}`）。
///
/// 抽出来是给 `/api/debug/tools` 用的：模型（或人）想知道「我有哪些工具」，
/// 不用去翻整份请求体。
///
/// 只建一次、之后共享（`Arc`）：每一步的请求都要带整张表，没必要每步重新
/// 跑一遍 `json!` 拼 schema（也让「工具表字节确定」更没悬念）。
pub fn tool_definitions(p: Platform) -> std::sync::Arc<[Tool]> {
    static DEFS: [OnceLock<std::sync::Arc<[Tool]>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    DEFS[p as usize]
        .get_or_init(|| {
            tools(p)
                .iter()
                .map(|t| Tool {
                    kind: "function".into(),
                    function: FunctionDef {
                        name: t.name.to_string(),
                        description: t.description.clone(),
                        parameters: (t.schema)(),
                    },
                })
                .collect()
        })
        .clone()
}

/// Responses API 的工具表：**扁平**的 `{"type":"function","name":…}`。
fn tool_definitions_responses(p: Platform) -> std::sync::Arc<[crate::responses::Tool]> {
    static DEFS: [OnceLock<std::sync::Arc<[crate::responses::Tool]>>; 3] =
        [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    DEFS[p as usize]
        .get_or_init(|| {
            tools(p)
                .iter()
                .map(|t| crate::responses::Tool {
                    kind: "function",
                    name: t.name.to_string(),
                    description: t.description.clone(),
                    parameters: (t.schema)(),
                })
                .collect()
        })
        .clone()
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

/// 前端用户消息的 parts：正文 + 附件。
///
/// 附件本体不在 JSON 里，只给 id（拼成 `/api/attachment/<id>`）和名字；
/// 图片的缩略图、PDF 的下载都靠那个路由。
fn user_view_parts(text: &str, attachments: &[Attachment]) -> Vec<Value> {
    let mut parts = vec![json!({ "type": "text", "id": "u", "text": text })];
    for (n, a) in attachments.iter().enumerate() {
        parts.push(json!({
            "type": if a.kind == AttachmentKind::Image { "image" } else { "file" },
            "id": format!("a{n}"),
            "url": format!("/api/attachment/{}", a.id),
            "name": a.name,
            "mime": a.mime,
        }));
    }
    parts
}

/// 排队等着交给模型的用户消息 → 前端视图（`/api/session` 的 `queued`）。
///
/// 形状就是一条普通用户消息（`{id, role, parts, done}`，和 `session_view` 里
/// 那些一样），`done: false` 是唯一的区别 —— 界面拿它直接当一条消息渲染，
/// 「排队中」那些虚线气泡就是这么来的（`App.svelte` 的 `pending`）。
///
/// 为什么不在前端自己攒：App 切到后台、WebView 被回收之后，前端那份就没了 ——
/// 而排队的消息还在（服务端排的），回来得看得见、也能撤。
pub fn queued_view(msgs: &[crate::state::QueuedMsg]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            json!({
                "id": m.id,
                "role": "user",
                "steps": 0,
                "done": false,
                "parts": user_view_parts(&m.text, &m.attachments),
            })
        })
        .collect()
}

/// 把服务端会话转成前端 `Message[]` 的形状。
///
/// **为什么需要**：前端的消息是纯客户端状态，Activity 重建 / 页面刷新 /
/// 从后台回来都会把它清掉（切出去再回来对话就空了）；而服务端一直留着完整
/// 历史（`ChatSession`），刷新后能从这里恢复。
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
            ChatMessage::User { .. } => {
                n += 1;
                last_assistant = false;
            }
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
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
    let mut results: HashMap<&str, (&str, &[ImagePart])> = HashMap::new();
    for m in &sess.messages[from..] {
        if let ChatMessage::Tool {
            tool_call_id,
            content,
            images,
        } = m
        {
            results.insert(tool_call_id.as_str(), (content.as_str(), images.as_slice()));
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for (i, m) in sess.messages.iter().enumerate().skip(from) {
        match m {
            // 系统提示词（第一条，以及后续的“提示词升级”）是给模型看的，不给用户看
            ChatMessage::System(_) => {}

            ChatMessage::User { text, attachments } => out.push(json!({
                "id": format!("u{i}"),
                "role": "user",
                "steps": 0,
                "done": true,
                "parts": user_view_parts(text, attachments),
            })),

            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut parts: Vec<Value> = Vec::new();
                if !reasoning.is_empty() {
                    parts.push(
                        json!({ "type": "reasoning", "id": format!("r{i}"), "text": reasoning }),
                    );
                }
                if !content.is_empty() {
                    parts.push(json!({ "type": "text", "id": format!("t{i}"), "text": content }));
                }
                for tc in tool_calls {
                    // 结果就是模型收到的那段文本，原样给
                    let result = results.get(tc.id.as_str());
                    let output = result.map(|(c, _)| *c);
                    let images: Vec<&str> = result
                        .map(|(_, imgs)| imgs.iter().map(|i| i.source.as_str()).collect())
                        .unwrap_or_default();
                    parts.push(json!({
                        "type": "tool",
                        "toolCallId": tc.id,
                        "toolName": tc.name,
                        "inputText": "",
                        "input": serde_json::from_str::<Value>(&tc.arguments)
                            .unwrap_or_else(|_| json!(tc.arguments)),
                        // 没有输出就是还在跑（服务端此刻正在执行）
                        "state": if output.is_some() { "output" } else { "ready" },
                        "output": output,
                        "images": images,
                    }));
                }
                if parts.is_empty() {
                    continue;
                }
                // 同一回合的多步（思考 → 工具 → 思考 → 正文 …）合成一条消息，
                // 和流式时前端看到的一样。不合的话刷新前后排版不一致：
                // 流式是一条消息里的几段，恢复出来是好几条消息、间距也不同。
                if let Some(last) = out.last_mut().filter(|m| m["role"] == "assistant") {
                    last["parts"]
                        .as_array_mut()
                        .expect("parts 是数组")
                        .extend(parts);
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

/// 一条历史消息在前端视图里贡献几个 part（和 `session_view_from` 的规则一致，
/// 有测试盯着两边不跑偏）。
fn view_parts_len(m: &ChatMessage) -> usize {
    match m {
        ChatMessage::Assistant {
            content,
            reasoning,
            tool_calls,
            ..
        } => {
            usize::from(!reasoning.is_empty()) + usize::from(!content.is_empty()) + tool_calls.len()
        }
        _ => 0,
    }
}

/// 会不会在视图里变成（或并进）一条 assistant 消息。
fn is_visible_assistant(m: &ChatMessage) -> bool {
    matches!(m, ChatMessage::Assistant { .. }) && view_parts_len(m) > 0
}

/// 增量同步：客户端手上是 `messages[..client_len]` 的视图，给它补到最新。
///
/// 会话是只追加的，前面的消息都是最终的；唯一会变的是**当前回合**那条
/// assistant 消息 —— 一个回合的多步都并在它里面。
///
/// 只退回到 `client_len` 之前**最后一步**（那一步的工具结果可能是新到的），
/// 并告诉客户端：那条 assistant 消息的前 `keep_parts` 个 part 留着，后面换成
/// 新发的。**不能整段重发**：回合不限步数，100 步、每步一个 50KB 的 bash 输出，
/// 重发一次就是几 MB（服务端拼、客户端 parse、整条消息重新渲染），越跑越慢；
/// 这样退回去多少与回合长度无关。
pub struct SessionPatch {
    /// 客户端的消息数组从这里开始替换
    pub replace_from: usize,
    /// `Some(k)`：`messages[0]` 接在客户端第 `replace_from` 条（assistant）的
    /// 前 k 个 part 后面，而不是整条替换
    pub keep_parts: Option<usize>,
    pub messages: Vec<Value>,
}

pub fn session_patch(sess: &ChatSession, client_len: usize) -> SessionPatch {
    let msgs = &sess.messages;
    let len = msgs.len();
    let client_len = client_len.min(len);

    // 没有新消息，或者新的是一条用户消息（新回合）：从 client_len 往后发
    let continuing = client_len < len && !msgs[client_len].is_user();
    if !continuing {
        return SessionPatch {
            replace_from: session_view_count_before(sess, client_len),
            keep_parts: None,
            messages: session_view_from(sess, client_len),
        };
    }

    // 同一回合的延续。回合起点：上一条是 User（或到 0）
    let mut turn_start = client_len;
    while turn_start > 0 && !msgs[turn_start - 1].is_user() {
        turn_start -= 1;
    }
    let assistants: Vec<usize> = (turn_start..client_len)
        .filter(|&i| is_visible_assistant(&msgs[i]))
        .collect();
    let (Some(&first), Some(&last)) = (assistants.first(), assistants.last()) else {
        // 这个回合在客户端那边还没有 assistant：新开的那条就是它
        return SessionPatch {
            replace_from: session_view_count_before(sess, client_len),
            keep_parts: None,
            messages: session_view_from(sess, client_len),
        };
    };

    let keep: usize = msgs[turn_start..last].iter().map(view_parts_len).sum();
    let prior_steps = assistants.len() - 1;
    let mut out = session_view_from(sess, last);
    if let Some(head) = out.first_mut() {
        // 并进去之后它就是完整视图里那一条：id 和步数都按整条算
        head["id"] = json!(format!("a{first}"));
        let steps = head["steps"].as_u64().unwrap_or(1) as usize + prior_steps;
        head["steps"] = json!(steps);
    }
    SessionPatch {
        replace_from: session_view_count_before(sess, turn_start),
        keep_parts: Some(keep),
        messages: out,
    }
}

/// 跑一个用户回合，把 chunk 推到 `tx`。
///
/// `sid`：写进哪个会话。`turn`：这个回合的停止开关 —— 由调用方用
/// `AppState::begin_turn(sid)` 拿到（拿不到说明这个会话已经有回合在跑），
/// 回合结束时这里负责 `end_turn`。
pub async fn run_turn(
    core: &Core,
    provider: &Provider,
    user_text: &str,
    attachments: Vec<Attachment>,
    sid: &str,
    turn: std::sync::Arc<crate::state::Turn>,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    run_turn_with(
        core,
        provider,
        vec![(user_text.to_string(), attachments)],
        sid,
        turn,
        tx,
    )
    .await
}

/// 和 `run_turn` 一样，但一次带**一批**用户消息进历史。
///
/// 队列里攒了好几条时走这条（`continue_with_queued`）：一次全带上，模型只被调用
/// 一次 —— 一条一条发的话模型会先答一句、剩下的等下一个步边界才拿到，中间白白多
/// 出好几轮输出（用户连着说的话应该连着看）。
async fn run_turn_with(
    core: &Core,
    provider: &Provider,
    msgs: Vec<(String, Vec<Attachment>)>,
    sid: &str,
    turn: std::sync::Arc<crate::state::Turn>,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let res = turn_once(core, provider, msgs, sid, turn, tx).await;
    if let Err(e) = &res {
        core.state.set_turn_error(sid, format!("{e:#}"));
    }
    // 正常收尾、**按了停止**、队列里还有等着的：都立刻开下一回合把它们全送出去
    // （典型是刚好卡在「交完队列、决定结束」那一瞬间进来的消息，以及一边跑一边
    // 排了几条、然后用户按了停止）。
    //
    // 「停止」停的是**当前这一轮**，不是「把排队的也扔掉」：那几条是用户明确按过
    // 发送的，模型一空下来就该交给它 —— 不然它们会一直卡在「排队中」，没人再来
    // 开回合就永远发不出去。队列空的时候 `continue_with_queued` 自己就是空操作。
    //
    // 出错时**不**接着跑：接着跑只会把队列一条条烧掉（key 失效、限流都一样）。
    // 那几条留在队列里（界面上还是「排队中」，能撤），下一次发消息时按原来的顺序
    // 先走（见 `http::chat`）。
    if res.is_ok() {
        continue_with_queued(core, provider, sid);
    }
    res
}

/// 一个回合本身（`TurnGuard` 在这层里：返回时这个会话的「在跑」就没了）。
///
/// `msgs` 是这个回合一次带进历史的用户消息（正文 + 附件）。通常一条；队列里攒了
/// 好几条时一次全带上 —— 开头这一次模型请求就把它们都看到了，不会一条一条来。
async fn turn_once(
    core: &Core,
    provider: &Provider,
    msgs: Vec<(String, Vec<Attachment>)>,
    sid: &str,
    turn: std::sync::Arc<crate::state::Turn>,
    tx: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let _guard = TurnGuard(core.clone(), sid.to_string());
    // 系统提示词可能被 /api/debug/system-prompt 覆盖过，所以不能直接用常量。
    // 在拿会话写锁之前先取出来，避免锁套锁。
    let system_prompt = core.state.system_prompt();
    // 不活跃的会话消息不在内存里，先从库里读回来（回合进行中不会被放掉，
    // 见 `AppState::evict_idle`）
    core.state.ensure_loaded(sid).await?;
    let title = {
        let mut store = core.state.chat.write().await;
        let Some(sess) = store.get_mut(sid) else {
            anyhow::bail!("没有这个会话: {sid}");
        };
        anyhow::ensure!(sess.resident(), "会话的消息没能读进来: {sid}");
        // 上一回合被打断（进程被杀）留下的半截工具调用，先补上结果
        close_dangling_tail(sess);
        // 历史里只追加两类东西：待补的系统提示词（见 `ensure_system_prompt`）
        // 和这条用户消息。不插任何“每轮重发”的上下文块。
        ensure_system_prompt(sess, &system_prompt);
        // 标题用这一批的第一条（这个会话的第一条用户消息才真正生效）
        if let Some((text, _)) = msgs.first() {
            sess.maybe_title(text);
        }
        // 一条消息装下正文和附件。整批一起进历史 —— 模型一次就看到全部。
        for (text, attachments) in msgs {
            sess.messages.push(ChatMessage::User { text, attachments });
        }
        sess.updated_at = now_ms();
        // 通知栏那一行要显示哪个会话在跑（标题刚定下来，顺手取走，
        // 后面播报的时候不必再回会话里查）
        sess.title.clone()
    };
    core.state.mark_sessions_dirty();
    // 这个回合的实时状态播报器（通知栏那一行）。不接壳时它推的东西会被丢掉，
    // 所以桌面上、测试里都不用管它 —— 见 `status::Reporter`。
    let status = crate::status::Reporter::new(core.state.clone(), sid, &title);
    let res = drive(core, provider, sid, &turn, &status, &tx).await;
    // 结束那一条**最后**推：壳收到它才去发「任务完成」通知（而 `TurnGuard` 那个
    // Drop 一落，前台服务就停了 —— 两条路谁先谁后都得能收场，见壳里的
    // `LiveTurnStatus`）。出错也要推：不推的话通知栏会停在最后一个工具上，
    // 看起来像还在跑。
    status.finish(res.is_ok(), turn.abort_requested());
    res
}

/// 往本回合所属的会话追加一条消息。
///
/// **按 id 找，不用 `active_mut()`**：回合跑在后台，用户随时可能在侧边栏
/// 切到别的会话或新建会话 —— 之前全用 active_mut()，切走之后模型的回复
/// 和工具结果就写进了**另一个**会话，两边都坏掉。
/// 会话中途被删了就丢掉（下一步 stream_one 会报错停下）。
async fn append(core: &Core, sid: &str, m: ChatMessage) {
    let mut store = core.state.chat.write().await;
    match store.get_mut(sid) {
        Some(s) if s.resident() => {
            s.messages.push(m);
            s.updated_at = now_ms();
        }
        // 回合进行中的会话不会被放掉（`evict_idle` 跳过 busy 的）；真到了这里，
        // 往空的 `messages` 里推会让落盘把库里的历史覆盖掉 —— 宁可丢这一条
        Some(_) => tracing::error!("会话 {sid} 的消息不在内存里，丢弃一条追加"),
        None => {}
    }
    drop(store);
    core.state.mark_sessions_dirty();
}

/// 回合结束时把这个会话从「进行中」里拿掉。用 Drop 保证任何路径（包括
/// panic 和提前 return）都会清 —— 否则前端会一直显示「运行中」，这个会话
/// 也再也发不出消息。
struct TurnGuard(Core, String);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.state.end_turn(&self.1);
        self.0.state.mark_sessions_dirty();
    }
}

/// 一轮 LLM 调用的产出
struct StepOutput {
    content: String,
    reasoning: String,
    /// Responses API 的思考条目（原样，下一次请求回传）
    reasoning_items: Vec<Value>,
    calls: Vec<RecordedToolCall>,
    /// 流到一半用户按了停止。已经流出来的正文/思考照样进历史，工具调用作废。
    aborted: bool,
}

impl StepOutput {
    /// 还没拿到任何东西就停了
    fn aborted() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            reasoning_items: Vec::new(),
            calls: Vec::new(),
            aborted: true,
        }
    }
}

/// 真正的 loop：模型 → 工具 → 模型 …
///
/// `status` 是通知栏那一行的播报器（壳没接就是空操作）。挂在这里而不是
/// `stream_one` 里的逐块流上：**阶段切换**（开始想 / 轮到哪个工具 / 工具跑完）
/// 是 drive 才知道的事，而正文的逐块更新由 `status::Reporter` 自己攒。
async fn drive(
    core: &Core,
    provider: &Provider,
    sid: &str,
    turn: &crate::state::Turn,
    status: &crate::status::Reporter,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let message_id = uuid::Uuid::now_v7().to_string();
    send(tx, sse::start(&message_id)).await;
    // 回合一开先说「在想」：这会儿在预填充，一个字都还没出来。
    // 不说的话，通知栏会停在上一回合留下的那一条上。
    status.thinking();

    // 不设轮数上限：任务要多少轮就跑多少轮，"干到一半停下" 不是可接受的
    // 行为。唯一的出口是模型自己不再调工具（且没有排队的消息），或者用户按停止
    //（`POST /api/stop` → `abort_requested`）。
    //
    // `step_no` 只为一件事存在：**第一轮不取队列**（见下面 `deliver_queued`）。
    let mut step_no = 0usize;
    loop {
        if turn.abort_requested() {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }
        // 排队等着交给模型的用户消息，这里交给它。
        //
        // **这是唯一不打断任何东西的时刻**：上一轮的 LLM 调用已经结束、它要的工具
        // 也全跑完了，模型此刻空着。别的地方都不行 —— 正在流的那一步不能往历史里
        // 插东西（请求已经发出去了）。
        //
        // 第一轮例外：那条消息（真有的话）是在这个回合**开始之后**才排进来的 ——
        // 回合刚起步、模型一个字都还没说，没有「空下来」可言。真在这里插，前一条
        // 用户消息后面会跟上一条空的助手消息（前端那里就是一闪而过的空气泡）。
        // 留给第一个步边界就行。
        if step_no > 0 {
            deliver_queued(core, sid, tx, status).await;
        }
        step_no += 1;
        send(tx, sse::start_step()).await;

        let step_out = match stream_one(core, provider, sid, turn, status, tx).await {
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
        if !step.content.is_empty()
            || !step.reasoning.is_empty()
            || !step.calls.is_empty()
            || !step.reasoning_items.is_empty()
        {
            append(
                core,
                sid,
                ChatMessage::Assistant {
                    content: step.content.clone(),
                    reasoning: step.reasoning.clone(),
                    tool_calls: step.calls.clone(),
                    reasoning_items: step.reasoning_items.clone(),
                },
            )
            .await;
        }

        if step.aborted {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }

        let calls = step.calls;
        if calls.is_empty() {
            // 模型不再要求工具 → 除非**还有排队的用户消息**，本回合就结束了。
            // 有的话当场交给它、接着跑（「只要模型空下来就立刻给它」那条路）。
            // 交付和判断是同一步：先查后取的话，中间被撤掉（×）就会白打一次请求，
            // 而且那次请求的历史停在 assistant 上。
            if deliver_queued(core, sid, tx, status).await {
                continue;
            }
            send(tx, sse::finish()).await;
            return Ok(());
        }

        for (n, call) in calls.iter().enumerate() {
            // 用户按了停止：这一批剩下的工具不再执行
            if turn.abort_requested() {
                return stop_with_pending(core, sid, tx, &calls[n..]).await;
            }
            // 工具一律直接执行，没有审批环节。不存在的工具由 execute_tool
            // 返回错误，模型看到「未知工具 X」自己会改。
            //
            // 跑着的时候按停止要立刻生效：丢掉这个 future，bash 的子进程随之
            // 被杀（`kill_on_drop`）。不这样的话，一条 `timeout: 3600` 的命令
            // 按了停止也要等它自己跑完。
            //
            // 通知栏：轮到哪个工具了。放在这儿（参数已经完整）而不是只靠
            // `stream_one` 里名字刚出现的那一下 —— 那一下只能看到半截参数。
            status.tool(&call.name, &call.arguments);
            let outcome = tokio::select! {
                o = execute_tool(core, &call.name, &call.arguments, Some((tx, &call.id)), sid) => o,
                _ = turn.aborted() => {
                    return stop_with_pending(core, sid, tx, &calls[n..]).await;
                }
            };
            send(
                tx,
                tool_output_event(&call.id, &outcome.text, &outcome.images),
            )
            .await;
            append(
                core,
                sid,
                ChatMessage::Tool {
                    tool_call_id: call.id.clone(),
                    content: outcome.text,
                    images: outcome.images,
                },
            )
            .await;
            // 这个工具完事了，但回合还没完（下一轮请求在预填充）。通知栏不能
            // 继续显示「正在跑 apk add」—— 改成「在想」。
            status.tool_done();
        }
        // 工具结果回灌，继续下一轮
    }
}

/// 把排队等着的用户消息交给模型（`AppState::queued`，步边界调一次）。
///
/// 三件事：写进历史（和 `/api/chat` 开头那条走的是同一套）、告诉客户端「这几条
/// 出门了」、以及**换一条助手消息**。
///
/// 为什么非换不可：历史里这两条用户消息后面跟着的是新的一段 assistant
///（`session_view_from` 的合并规则里 User 会打断合并），所以刷新之后看到的是
/// **两条**助手消息。流式这里不换的话，实时看着是一条、刷新变成两条，排版会跳。
async fn deliver_queued(
    core: &Core,
    sid: &str,
    tx: &mpsc::UnboundedSender<Value>,
    status: &crate::status::Reporter,
) -> bool {
    let msgs = core.state.drain_queued(sid);
    if msgs.is_empty() {
        return false;
    }
    let ids: Vec<String> = msgs.iter().map(|m| m.id.clone()).collect();
    for m in msgs {
        let msg = ChatMessage::User {
            text: m.text,
            attachments: m.attachments,
        };
        append(core, sid, msg).await;
    }
    send(tx, sse::queued_delivered(&ids)).await;
    send(tx, sse::start(&uuid::Uuid::now_v7().to_string())).await;
    // 通知栏那行回到「正在想」（它可能还停在某个工具上）
    status.thinking();
    true
}

/// 回合收尾之后队列里还有排队的用户消息：立刻把它们**一次全带上**开下一回合。
///
/// 一次全带，不一条一条来：用户一口气排了好几条，就该让模型一口气看到全部 ——
/// 只发一条的话模型会先答一句，剩下的要等到新回合的下一个步边界，中间白白多出好
/// 几轮输出。历史里它们仍然是各自独立的一条用户消息（界面上也是各自一个气泡）。
///
/// 新回合**没有人订阅那条流**（`tx` 的那一半直接丢掉）：客户端不需要它 ——
/// 它要么已经从上面那个 `data-queued` 知道这条排上了，要么靠 `/api/sessions`
/// 的 `busy` + 轮询 `/api/session` 看结果。推给没人收的通道是浪费，`send` 也
/// 本来就是尽力而为。
pub fn continue_with_queued(core: &Core, provider: &Provider, sid: &str) {
    if !core.state.has_queued(sid) {
        return;
    }
    // 先占坑再取消息：占不到（别的请求刚给它开了新回合）就让那个回合去取，
    // 消息留在队列里不会丢。
    let Some(turn) = core.state.begin_turn(sid) else {
        return;
    };
    let msgs: Vec<(String, Vec<Attachment>)> = core
        .state
        .drain_queued(sid)
        .into_iter()
        .map(|m| (m.text, m.attachments))
        .collect();
    if msgs.is_empty() {
        core.state.end_turn(sid);
        return;
    }
    let (core, provider, sid) = (core.clone(), provider.clone(), sid.to_string());
    tokio::spawn(async move {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        if let Err(e) = run_turn_with(&core, &provider, msgs, &sid, turn, tx).await {
            tracing::warn!("接着跑排队的消息失败了（{sid}）: {e:#}");
        }
    });
}

/// 停在一批工具调用的中间：没跑完 / 没跑的那些也要有结果。
///
/// assistant 那条已经带着这些 tool call 进了历史，缺结果的话下一轮请求
/// 直接 400，这条会话就再也发不出去了。
async fn stop_with_pending(
    core: &Core,
    sid: &str,
    tx: &mpsc::UnboundedSender<Value>,
    pending: &[RecordedToolCall],
) -> anyhow::Result<()> {
    let stopped = ToolOutcome::error("用户已停止，这个工具没有执行完");
    for call in pending {
        send(tx, tool_output_event(&call.id, &stopped.text, &[])).await;
        append(
            core,
            sid,
            ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content: stopped.text.clone(),
                images: Vec::new(),
            },
        )
        .await;
    }
    send(tx, sse::abort("已停止")).await;
    send(tx, sse::finish()).await;
    Ok(())
}

/// 构造一次完整的 **Chat Completions** 请求体（含工具表）。
///
/// 抽成纯函数有两个理由：
/// 1. 让「工具表也是可缓存前缀的一部分」这条不变式**可测** —— 只比
///    `messages` 不够，前一轮的完整请求体必须原样出现在后一轮里。
/// 2. 它直接产出强类型请求，交给 `stream_chat` 一次序列化。
///
/// `pub` 是给 `/api/debug/request` 用的（把模型将看到的东西原样吐出来）。
pub fn build_request(
    sess: &ChatSession,
    platform: Platform,
    model: &str,
    supports_images: bool,
    supports_files: bool,
    effort: Option<&str>,
) -> anyhow::Result<ChatRequest> {
    let messages = build_messages(sess, supports_images, supports_files)?;
    Ok(ChatRequest {
        model: model.to_string(),
        messages,
        tools: tool_definitions(platform),
        stream: true,
        // 要 provider 在流末尾回报 token 用量 —— 这是验证 prompt cache
        // 命中的唯一硬证据。
        stream_options: StreamOptions {
            include_usage: true,
        },
        // 思考强度只是请求参数，不进 messages —— 前缀不变式不受影响
        reasoning_effort: effort.map(str::to_string),
    })
}

/// 构造一次完整的 **Responses API** 请求体。
pub fn build_responses_request(
    sess: &ChatSession,
    platform: Platform,
    model: &str,
    supports_images: bool,
    supports_files: bool,
    effort: Option<&str>,
) -> anyhow::Result<ResponsesRequest> {
    Ok(ResponsesRequest {
        model: model.to_string(),
        input: build_responses_input(sess, supports_images, supports_files),
        tools: tool_definitions_responses(platform),
        stream: true,
        store: false,
        include: &["reasoning.encrypted_content"],
        reasoning: effort.map(|e| Reasoning {
            effort: e.to_string(),
        }),
    })
}

/// 历史 → `input` 条目。和 `build_messages` 是同一份历史的两种编码。
fn build_responses_input(
    sess: &ChatSession,
    supports_images: bool,
    supports_files: bool,
) -> Vec<InputItem> {
    let mut out: Vec<InputItem> = Vec::new();
    for m in &sess.messages {
        match m {
            ChatMessage::System(s) => {
                out.push(message_item("system", MessageContent::Text(s.clone())))
            }
            ChatMessage::User { text, attachments } => out.push(message_item(
                "user",
                user_message_content(text, attachments, supports_images, supports_files),
            )),
            ChatMessage::Assistant {
                content,
                tool_calls,
                reasoning_items,
                ..
            } => {
                // Responses 的 output 顺序：思考条目在最前（原样回传，见
                // `ChatMessage::Assistant::reasoning_items`），再正文，再每个 function_call。
                //
                // 只回传带 `encrypted_content` 的：`store: false` 时服务端不留东西，
                // 光有 id 的条目 OpenAI 会去找、找不到就 400。不带加密内容的
                //（历史里就是没有、或者服务端不给）跳过 —— 少了这一步的思考，别的照旧。
                for item in reasoning_items.iter().filter(|i| replayable(i)) {
                    out.push(InputItem::Raw(item.clone()));
                }
                if !content.is_empty() {
                    out.push(InputItem::AssistantMessage(AssistantItem {
                        kind: "message",
                        role: "assistant",
                        content: vec![OutputPart::Text {
                            text: content.clone(),
                        }],
                    }));
                }
                for tc in tool_calls {
                    out.push(InputItem::FunctionCall(FunctionCallItem {
                        kind: "function_call",
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    }));
                }
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                images,
            } => out.push(InputItem::FunctionCallOutput(FunctionCallOutputItem {
                kind: "function_call_output",
                call_id: tool_call_id.clone(),
                output: tool_output(content, images, supports_images),
            })),
        }
    }
    out
}

/// 这个思考条目能不能原样回传（见 `build_responses_input`）
fn replayable(item: &Value) -> bool {
    item.get("encrypted_content")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
}

fn message_item(role: &str, content: MessageContent) -> InputItem {
    InputItem::Message(MessageItem {
        kind: "message",
        role: role.to_string(),
        content,
    })
}

fn input_text(text: String) -> InputPart {
    InputPart::Text { text }
}

fn input_image(url: std::sync::Arc<str>) -> InputPart {
    InputPart::Image {
        image_url: url,
        detail: "auto".to_string(),
    }
}

fn input_file(name: &str, data_url: std::sync::Arc<str>) -> InputPart {
    InputPart::File {
        file_data: data_url,
        filename: name.to_string(),
    }
}

/// Responses 的用户消息内容（文本 + 图片 / PDF）。
fn user_message_content(
    text: &str,
    attachments: &[Attachment],
    supports_images: bool,
    supports_files: bool,
) -> MessageContent {
    if attachments.is_empty() {
        return MessageContent::Text(text.to_string());
    }
    let mut parts: Vec<InputPart> = Vec::new();
    if !text.is_empty() {
        parts.push(input_text(text.to_string()));
    }
    for a in attachments {
        parts.push(input_text(format!("[附件：{}（{}）]", a.name, a.mime)));
        match a.kind {
            AttachmentKind::Image => {
                if !supports_images {
                    parts.push(input_text(format!(
                        "[当前模型不支持图片输入，{}没有附带]",
                        a.name
                    )));
                    continue;
                }
                match crate::imaging::cached_data_url(&a.path, &a.mime) {
                    Ok(url) => parts.push(input_image(url)),
                    Err(e) => {
                        tracing::warn!("附件读不到了 {}: {e}", a.path);
                        parts.push(input_text(format!(
                            "[附件 {} 已丢失，本轮没能附带]",
                            a.name
                        )));
                    }
                }
            }
            AttachmentKind::Pdf => {
                if !supports_files {
                    parts.push(input_text(format!(
                        "[当前模型不支持 PDF 输入，{}没有附带]",
                        a.name
                    )));
                    continue;
                }
                match crate::imaging::cached_data_url(&a.path, &a.mime) {
                    Ok(url) => parts.push(input_file(&a.name, url)),
                    Err(e) => {
                        tracing::warn!("附件读不到了 {}: {e}", a.path);
                        parts.push(input_text(format!(
                            "[附件 {} 已丢失，本轮没能附带]",
                            a.name
                        )));
                    }
                }
            }
        }
    }
    MessageContent::Parts(parts)
}

/// 工具结果的 Responses 形态：图片直放在 `function_call_output` 里。
fn tool_output(content: &str, images: &[ImagePart], supports_images: bool) -> ToolOutput {
    if images.is_empty() || !supports_images {
        return ToolOutput::Text(content.to_string());
    }
    let mut parts: Vec<InputPart> = vec![input_text(content.to_string())];
    for img in images {
        match crate::imaging::cached_data_url(&img.path, &img.mime) {
            Ok(url) => parts.push(input_image(url)),
            Err(e) => tracing::warn!("图片读不到了 {}: {e}", img.path),
        }
    }
    ToolOutput::Parts(parts)
}

/// 这一段历史组请求时要内联的文件：`(路径, mime)`。和 `build_*` 里的取舍一致
/// （模型不支持的模态不读）。
fn inline_files(sess: &ChatSession, images: bool, pdfs: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for m in &sess.messages {
        match m {
            ChatMessage::User { attachments, .. } => {
                for a in attachments {
                    let want = match a.kind {
                        AttachmentKind::Image => images,
                        AttachmentKind::Pdf => pdfs,
                    };
                    if want {
                        out.push((a.path.clone(), a.mime.clone()));
                    }
                }
            }
            ChatMessage::Tool { images: imgs, .. } if images => {
                out.extend(imgs.iter().map(|i| (i.path.clone(), i.mime.clone())));
            }
            _ => {}
        }
    }
    out
}

/// 记下 provider 报的结束原因（`stop` / `tool_calls` / `length`……），进 `usage` 表。
///
/// 只在非空时覆盖：有些分片会带 `"finish_reason": ""`，别拿它把已经拿到的真原因
/// 冲掉。（`length` 另外还是「被截断」的信号，见 `stream_one` 末尾那处判断。）
fn note_finish(slot: &mut Option<String>, fr: Option<&str>) {
    if let Some(fr) = fr.filter(|s| !s.is_empty()) {
        *slot = Some(fr.to_string());
    }
}

/// 调一次 LLM，把 delta 转成 chunk 推出去，返回本轮产出。
async fn stream_one(
    core: &Core,
    provider: &Provider,
    sid: &str,
    turn: &crate::state::Turn,
    status: &crate::status::Reporter,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<StepOutput> {
    // 这个模型能不能看图 / 吃 PDF：自动判定（models.dev，带缓存 + 落盘）。
    // 在拿会话锁之前算 —— 首次判定要联网，不能拖着锁。
    let supports_images = core.state.supports_images_for(provider).await;
    let supports_files = core.state.supports_files_for(provider).await;

    // 系统提示词是按平台冻结的常量，**这里不做任何插值** —— 见 `system_prompt`。
    // 动态的界面/环境信息在 run_turn 里作为独立消息进历史，保证前缀可缓存。
    //
    // 优先 Responses API（见 `ApiStyle`）：端点不支持（404/405）就退回
    // Chat Completions，并记住这个 base_url|model，后面不再白试。
    // 要内联的图片 / PDF 先在锁外读进缓存（见 `imaging::prewarm`）：
    // 下面组请求时拿着会话的读锁，那时就全是缓存命中。
    let files = {
        let store = core.state.chat.read().await;
        store
            .get(sid)
            .map(|s| inline_files(s, supports_images, supports_files))
            .unwrap_or_default()
    };
    if !files.is_empty() {
        let _ = tokio::task::spawn_blocking(move || crate::imaging::prewarm(&files)).await;
    }

    // 打开流 + 收流，外面套一层重试：限流 / 网关临时出错 / 断网这类临时故障
    // 退避后重来（见 `RETRY_DELAYS`）。只在**还没往前端流出任何内容**时重试 ——
    // 已经流出正文 / 工具调用的话，重来一遍界面上就是两份。
    // 这一步花了多久（含重试，进 `usage` 表）
    let step_started = std::time::Instant::now();
    let mut attempt = 0usize;
    'attempt: loop {
        // 等首包也可能很久（长上下文的预填充），这段也要能停
        let opened = tokio::select! {
            r = open_stream(core, provider, sid, supports_images, supports_files) => r,
            _ = turn.aborted() => return Ok(StepOutput::aborted()),
        };
        let mut rx = match opened {
            Ok(rx) => rx,
            Err(e) if e.is_transient() && attempt < RETRY_DELAYS.len() => {
                if !backoff(tx, turn, attempt, &format!("{e:#}")).await {
                    return Ok(StepOutput::aborted());
                }
                attempt += 1;
                continue 'attempt;
            }
            Err(e) => return Err(e.into()),
        };

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
        let mut reasoning_items: Vec<Value> = Vec::new();
        // 往前端流出过内容没有（决定流断了还能不能重来）
        let mut emitted = false;

        let mut aborted = false;
        loop {
            let item = tokio::select! {
                item = rx.recv() => match item {
                    Some(item) => item,
                    None => break,
                },
                // 流到一半按停止：已经流出来的留着，剩下的不要了
                _ = turn.aborted() => {
                    aborted = true;
                    break;
                }
            };
            let chunk = match item {
                Ok(c) => c,
                // 流在出任何内容之前就断了（连上之后网络掉了、网关掐了连接）：
                // 跟连不上一样，重来
                Err(e) if !emitted && attempt < RETRY_DELAYS.len() => {
                    if !backoff(tx, turn, attempt, &format!("{e:#}")).await {
                        return Ok(StepOutput::aborted());
                    }
                    attempt += 1;
                    continue 'attempt;
                }
                Err(e) => return Err(e),
            };
            // 网关在流里直接报错（限流、上下文超限、response.failed）。不接的话
            // 这一步就是个空回复，回合「正常结束」，用户只看到没反应。
            if let Some(e) = &chunk.error {
                let msg = e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string());
                anyhow::bail!("provider 报错：{msg}");
            }
            if chunk.usage.is_some() {
                usage = chunk.usage.clone();
            }
            if let Some(item) = chunk.reasoning_item {
                reasoning_items.push(item);
            }
            for choice in chunk.choices {
                if let Some(r) = choice.delta.reasoning_text() {
                    emitted = true;
                    reason_acc.push_str(r);
                    // 通知栏：从「跑工具」切回「在想」（幂等，一整段思考只推第一次）
                    status.thinking();
                    for c in blocks.push(BlockKind::Reasoning, r) {
                        send(tx, c).await;
                    }
                }
                if let Some(c) = choice.delta.content.as_deref() {
                    if !c.is_empty() {
                        emitted = true;
                        text_acc.push_str(c);
                        // 通知栏：正在写的最后一行（内部按半秒攒，不是每块都刷）
                        status.streaming(&text_acc);
                        for c in blocks.push(BlockKind::Text, c) {
                            send(tx, c).await;
                        }
                    }
                }
                note_finish(&mut finish, choice.finish_reason.as_deref());
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
                            emitted = true;
                            // 先收尾正在流的思考/正文，工具行才排在它们后面
                            if let Some(c) = blocks.close() {
                                send(tx, c).await;
                            }
                            send(tx, sse::tool_input_start(id, name)).await;
                            // 通知栏：名字一到就显示（参数这时候多半还没流完，
                            // `tool_detail` 抠到哪儿算哪儿）。
                            // 真正开跑之前 `drive` 会再推一次（那时参数才完整）。
                            status.tool(name, args);
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
        // 通知栏：这一步说了什么，结束那一条拿它当回复预览
        status.step_text(&text_acc);

        // 归一化**只在这里做一次**：之后日志、SSE、/api/session、前端拿到的都是
        // 同一个互斥桶 + 算好的命中率（见 `UsageView`）—— 前端不再解析两套字段名。
        if let Some(u) = &usage {
            let view = u.view();
            match view.cache_hit_ratio {
                Some(ratio) => tracing::info!(
                    "prompt cache 命中 {:.1}%（cached={} prompt={} completion={} reasoning={}）",
                    ratio * 100.0,
                    view.cache_read,
                    view.input,
                    view.output,
                    view.reasoning,
                ),
                None => tracing::info!(
                    "prompt={} completion={}（provider 未报缓存字段）",
                    view.input,
                    view.output
                ),
            }
            // 记到这个会话上：输入框那个上下文圆环读它，也进库（`usage` 表）留着分析
            let row = crate::db::UsageRow {
                session_id: sid.to_string(),
                at: now_ms(),
                provider: provider.name.clone(),
                model: provider.model.clone(),
                input: view.input,
                output: view.output,
                cached: u.cached_tokens(),
                cache_write: u.cache_creation_input_tokens,
                reasoning: u.reasoning_tokens(),
                finish: finish.clone(),
                ms: step_started.elapsed().as_millis() as u64,
            };
            let session = core.state.record_usage(&row, &view).await;
            // 这一次的桶 + 记上之后的会话累计（含累计命中率）一起推给前端 ——
            // 前端直接换上，不再自己往上加、也不再自己除
            let mut part = serde_json::to_value(&view)?;
            part["session"] = serde_json::to_value(&session)?;
            send(tx, sse::data_part("usage", part)).await;
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
        // 停在半路：工具调用的参数多半是半截 JSON，一个都不能执行。
        // 已经在界面上露头的那几个，告诉前端作废了，别一直转圈。
        if aborted {
            // `calls` 里只剩有 id 和名字的，正是发过 tool-input-start 的那些
            for c in &calls {
                send(tx, tool_output_event(&c.id, "错误：已停止", &[])).await;
            }
            return Ok(StepOutput {
                content: text_acc,
                reasoning: reason_acc,
                reasoning_items,
                calls: Vec::new(),
                aborted: true,
            });
        }

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
                    tool_output_event(&c.id, "错误：输出被截断，本次工具调用已作废，请重试", &[]),
                )
                .await;
            }
            calls.clear();
        }

        for c in &calls {
            let input: Value = serde_json::from_str(&c.arguments).unwrap_or(json!({}));
            send(tx, sse::tool_input_available(&c.id, &c.name, input)).await;
        }

        return Ok(StepOutput {
            content: text_acc,
            reasoning: reason_acc,
            reasoning_items,
            calls,
            aborted: false,
        });
    }
}

/// LLM 请求遇到临时故障时，第 n 次重试前等多少毫秒。三次都不行就报错。
/// （测试里缩成几毫秒，不然一个用例要等十几秒。）
const RETRY_DELAYS: [u64; 3] = if cfg!(test) {
    [5, 5, 5]
} else {
    [2_000, 5_000, 10_000]
};

/// 重试前：告诉前端在等什么（不然界面就是一段莫名其妙的停顿），然后退避。
/// 等的时候按停止要立刻生效。返回 `false` = 用户停了。
async fn backoff(
    tx: &mpsc::UnboundedSender<Value>,
    turn: &crate::state::Turn,
    attempt: usize,
    why: &str,
) -> bool {
    let ms = RETRY_DELAYS[attempt];
    let secs = ms / 1000;
    let short: String = why.chars().take(160).collect();
    tracing::warn!(
        "LLM 请求失败（{short}），{secs}s 后第 {} 次重试",
        attempt + 1
    );
    send(
        tx,
        sse::data_part(
            "warning",
            json!({
                "message": format!(
                    "请求出错（{short}），{secs} 秒后重试（第 {}/{} 次）",
                    attempt + 1,
                    RETRY_DELAYS.len()
                )
            }),
        ),
    )
    .await;
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => true,
        _ = turn.aborted() => false,
    }
}

/// 打开一条 LLM 流（组请求 + 发出去，等到响应头）。
///
/// 优先 Responses API（见 `ApiStyle`）：端点不支持（404/405）就退回 Chat
/// Completions，并记住这个 base_url|model（落盘），后面不再白试。
/// 组请求出的错（会话被删了之类）包成 `StreamError::Other`，不会被当成临时
/// 故障重试（见 `StreamError::is_transient`）。
async fn open_stream(
    core: &Core,
    provider: &Provider,
    sid: &str,
    supports_images: bool,
    supports_files: bool,
) -> Result<
    mpsc::UnboundedReceiver<anyhow::Result<crate::openai::StreamChunk>>,
    crate::llm::StreamError,
> {
    let chat = || async {
        let (req, session_id) = {
            let store = core.state.chat.read().await;
            let sess = store
                .get(sid)
                .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
            (
                build_request(
                    sess,
                    core.state.platform(),
                    &provider.model,
                    supports_images,
                    supports_files,
                    provider.reasoning_effort.as_deref(),
                )?,
                sess.provider_session.clone(),
            )
        };
        provider.stream_chat(&req, &session_id).await
    };

    match provider.transport() {
        Transport::Chat => chat().await,
        Transport::Responses => {
            let (req, session_id) = {
                let store = core.state.chat.read().await;
                let sess = store
                    .get(sid)
                    .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
                (
                    build_responses_request(
                        sess,
                        core.state.platform(),
                        &provider.model,
                        supports_images,
                        supports_files,
                        provider.reasoning_effort.as_deref(),
                    )?,
                    sess.provider_session.clone(),
                )
            };
            match provider.stream_responses(&req, &session_id).await {
                Err(e) if e.is_unsupported_endpoint() => {
                    core.state.remember_responses_unsupported(provider).await;
                    tracing::info!(
                        "{} 不支持 Responses API，退回 Chat Completions",
                        provider.base_url
                    );
                    chat().await
                }
                other => other,
            }
        }
    }
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
///
/// 这里留尾，而 `bash` 的**最终结果**是「首尾各 100 行」（`linux::Capture::render`），
/// 是故意的：最终结果是给模型看的一次快照，两头都要；这块是给盯着屏幕的人看
/// 的进度，要的是「现在跑到哪了」。
async fn pump_live_output(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    tx: mpsc::UnboundedSender<Value>,
    id: String,
) {
    const LIVE_EVERY: std::time::Duration = std::time::Duration::from_millis(120);
    const LIVE_TAIL: usize = 32 * 1024;

    let mut text = String::new();
    // UTF-8 可能被切在块中间：不完整的尾巴留到下一块
    let mut pending: Vec<u8> = Vec::new();
    let mut dirty = false;
    let mut tick = tokio::time::interval(LIVE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let flush = |text: &str| {
        // 跑到现在的输出，纯文本（「运行中」看 part 的 state，不用另带标记）
        let _ = tx.send(sse::tool_output_preliminary(&id, json!(text)));
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
/// `edit` 的两个模式：`content` 是新建、`edits` 是改；只能给一个。
#[derive(Debug)]
enum EditMode<'a> {
    Create(&'a str),
    Replace,
}

fn edit_mode(args: &Value) -> anyhow::Result<EditMode<'_>> {
    match (args.get("content"), args.get("edits")) {
        (Some(content), None) => {
            let text = content
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("content 必须是字符串"))?;
            Ok(EditMode::Create(text))
        }
        (None, Some(_)) => Ok(EditMode::Replace),
        (Some(_), Some(_)) => {
            anyhow::bail!("edits 和 content 只能给一个：改已有文件用 edits，新建文件用 content")
        }
        (None, None) => {
            anyhow::bail!("缺少 edits 或 content：改已有文件用 edits，新建文件用 content")
        }
    }
}

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

/// `bash` 不给 timeout 时的超时。
///
/// 绝大多数命令几秒就完；长任务（下载 / `apk add` / 编译）模型要自己显式给 ——
/// 工具描述里写了，超时被杀时结果里也会再提醒一次（见 `timeout_hint`）。
const BASH_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// `bash` 的 timeout（秒）：可选。没给（或 null）= 默认 60s；给了但不是正数 →
/// `Err`（模型写错了就告诉它，不偷偷换成默认值）。超过 `MAX_TIMEOUT` 夹到上限。
fn parse_timeout(args: &Value) -> Result<std::time::Duration, &'static str> {
    let v = match args.get("timeout") {
        None | Some(Value::Null) => return Ok(BASH_DEFAULT_TIMEOUT),
        Some(v) => v,
    };
    let secs = v
        .as_f64()
        .filter(|s| s.is_finite() && *s > 0.0)
        .ok_or("timeout 必须是正数（秒）；不给就是默认 60 秒")?;
    let capped = secs.min(linux::MAX_TIMEOUT.as_secs_f64());
    Ok(std::time::Duration::from_secs_f64(capped))
}

/// 超时被杀时追加在输出后面的提醒：用了多少秒、要更久怎么办。
fn timeout_hint(timeout: std::time::Duration, explicit: bool) -> String {
    let secs = timeout.as_secs_f64();
    if explicit {
        format!("[{secs:.0}s 超时被终止，上面的输出可能不全。确实需要更久就重跑并给更大的 timeout（上限 86400）]")
    } else {
        format!(
            "[{secs:.0}s 超时被终止，上面的输出可能不全（没给 timeout，用的是默认值）。长任务重跑时显式给 timeout（秒，上限 86400）；\
             要一直跑的服务用 `setsid … > /tmp/x.log 2>&1 &` 放后台——注意刚被杀的那条连同它起的东西一起死了，要在改过的写法下重新起]"
        )
    }
}

/// 相对路径按会话目录展开（`bash` 的 cwd 就是它）。
///
/// 绝对路径原样返回 —— `resolve_path` 认得 `/workspace/…`、`/sdcard/…`、`/root/…`。
fn in_cwd(path: &str, cwd: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), path)
    }
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

/// `read` 的 numbered：可选布尔开关，默认关。不是布尔（模型给了字符串之类）
/// 就当没给 —— 一个可选开关写错不该让整次 read 失败。
fn parse_numbered(args: &Value) -> bool {
    args.get("numbered")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 工具执行
// ---------------------------------------------------------------------------

/// 一次工具执行的结果：模型看到的**纯文本**（进历史、每一步都重发），外加要内联
/// 给模型的图片（read 读到图）。
///
/// 界面上显示的也是**这同一段文本**，不另备一份结构化字段：退出码、截断、超时、
/// 续读位置这些模型要知道的，本来就写在正文里（`ExecResult::to_text`、`reader` 的
/// 续读提示）。以前包一层 JSON 再把它们各列一遍，一条 `ls` 的结果七成是重复字段，
/// 正文还被转义成 `\n` / `\"`；人和模型看到的也不是同一个东西。
struct ToolOutcome {
    text: String,
    images: Vec<ImagePart>,
}

impl ToolOutcome {
    /// 没有图片的普通结果。
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// 出错：`错误：…`。界面也靠这个开头把卡片染红（见前端 `toolview.ts`）。
    fn error(msg: impl std::fmt::Display) -> Self {
        Self::text(format!("错误：{msg}"))
    }
}

/// 工具结果的 SSE 事件：正文就是模型收到的那段文本；读到的图片给出沙箱里的原图
/// 路径，界面画缩略图（图也是模型看到的东西）。
fn tool_output_event(id: &str, text: &str, images: &[ImagePart]) -> Value {
    let mut ev = sse::tool_output_available(id, json!(text));
    if !images.is_empty() {
        ev["images"] = json!(images.iter().map(|i| &i.source).collect::<Vec<_>>());
    }
    ev
}

/// `live`：(推给前端的通道, 工具调用 id)。给了就把 bash 的输出边跑边推过去。
async fn execute_tool(
    core: &Core,
    name: &str,
    arguments: &str,
    live: Option<(&mpsc::UnboundedSender<Value>, &str)>,
    sid: &str,
) -> ToolOutcome {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

    // 工具的 cwd 和 bash 走同一个函数（`LinuxSandbox::session_cwd`）：手机沙箱里是
    // 这个会话自己的目录，电脑上（宿主模式）是 workspace 本身。两个门必须一致 ——
    // 模型在 bash 里 `pwd` 看到哪，`read x.txt` 就得去哪找。
    let cwd = core
        .state
        .linux()
        .map(|sb| sb.session_cwd(Some(sid)))
        .unwrap_or_else(|| linux::WORKSPACE.to_string());

    // read / bash / edit 都不走设备（`DeviceApi`）：read 和 edit 直接
    // 读写 rootfs 里的文件，bash 起 proot 子进程。
    if name == "read" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolOutcome::error("缺少参数 path");
        }
        let path = &in_cwd(path, &cwd);
        let offset = parse_offset(&args);
        let limit = match parse_limit(&args) {
            Ok(l) => l,
            Err(e) => return ToolOutcome::error(format!("{e:#}")),
        };
        let numbered = parse_numbered(&args);

        return match core.read_any(path, offset, limit, numbered).await {
            // 续读提示（还有几行、用 offset= 几继续）已经在正文末尾了
            Ok(linux::ReadResult::Text(view)) => ToolOutcome::text(view.content),
            Ok(linux::ReadResult::Image(img)) => read_image_outcome(core, path, img).await,
            Err(e) => ToolOutcome::error(format!("{e:#}")),
        };
    }

    if name == "bash" {
        let script = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if script.trim().is_empty() {
            return ToolOutcome::error("缺少参数 command");
        }
        // timeout 可选，默认 60s（见 `BASH_DEFAULT_TIMEOUT`）
        let explicit = !matches!(args.get("timeout"), None | Some(Value::Null));
        let timeout = match parse_timeout(&args) {
            Ok(t) => t,
            Err(why) => return ToolOutcome::error(why),
        };

        let res = match live {
            Some((tx, id)) => {
                let (btx, brx) = mpsc::unbounded_channel();
                let pump = tokio::spawn(pump_live_output(brx, tx.clone(), id.to_string()));
                let r = core.run_linux_live(script, timeout, btx, Some(sid)).await;
                // 等最后一批推完，再发最终结果，保证顺序
                let _ = pump.await;
                r
            }
            None => core.run_linux(script, timeout, Some(sid)).await,
        };
        return match res {
            Ok(r) => {
                // 截断 / 完整输出在哪 / 超时 / 信号 / `[exit N]` 都在正文里（`to_text_with`），
                // 超时那句换成带「下次怎么给 timeout」的版本，只说一遍
                ToolOutcome::text(
                    r.to_text_with(r.timed_out.then(|| timeout_hint(timeout, explicit))),
                )
            }
            Err(e) => ToolOutcome::error(format!("{e:#}")),
        };
    }

    if name == "edit" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolOutcome::error("缺少参数 path");
        }
        let path = &in_cwd(path, &cwd);

        // 两个模式：`edits` 改已有文件，`content` 新建文件。只能给一个 ——
        // 两个都给 / 都不给都报错，不做“聪明”的猜测（猜错了要么盖掉原文、
        // 要么改到错的地方）。
        let mode = match edit_mode(&args) {
            Ok(m) => m,
            Err(e) => return ToolOutcome::error(format!("{e:#}")),
        };
        if let EditMode::Create(content) = mode {
            return match core.create_file(path, content).await {
                Ok(c) => ToolOutcome::text(format!(
                    "已创建 {path}（{} 行 / {} 字节）",
                    c.lines, c.bytes
                )),
                Err(e) => ToolOutcome::error(format!("{e:#}")),
            };
        }

        let edits = match parse_edits(&args) {
            Ok(e) => e,
            Err(e) => return ToolOutcome::error(format!("{e:#}")),
        };

        // 模型看到的就是 diff（`@@` 里有行号，改了几处它自己知道）
        return match core.edit_file(path, &edits).await {
            Ok(out) => ToolOutcome::text(out.patch),
            Err(e) => ToolOutcome::error(format!("{e:#}")),
        };
    }

    // 联网搜索不是一个工具：唯一入口是沙箱里的 `agent search`（见
    // `cli.rs`）。同一个道理 —— 能在一行 shell 里说清的能力，不必再开一个
    // 工具；而且这样它能和 `curl` / `jq` / 循环凑在一起用。

    // 浏览器也不是一个工具：唯一入口是沙箱里的 `agent browser`（见
    // `cli.rs`）。一条 bash 能连做多个动作
    //（`navigate && get_backbone && click`），省的是**整轮 LLM 请求**，工具那条
    // 一次只做一个动作；留着两条路的代价是实打实的：前缀里多 2400 字（工具表的
    // 54%），两处动作表还各自演化（`new_tab URL` 被 CLI 静默吃掉、开出一只空白标签）。
    //
    // 和上面那段设备操作同一个道理：能在 shell 里说清的能力，就别再开一个工具。

    ToolOutcome::error(format!("未知工具 {name}"))
}

/// 图片结果：缩好的字节落盘（历史回放时还要重发），回给模型一段能读的说明
/// （mime、尺寸、大小、原图路径都在里面）；图片本体作为 `images` 走
/// `build_messages` 里那条 carrier。
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
    let size = crate::reader::fmt_size(img.source_bytes);
    let original = format!("{}×{}", img.original_width, img.original_height);
    let mut output = format!("读取图片 {mime}（{original}，{size}");
    if (img.width, img.height) != (img.original_width, img.original_height) {
        output.push_str(&format!("，已缩到 {}×{}", img.width, img.height));
    }
    output.push_str(&format!(
        "）。原文件：{guest_path} —— 要裁剪或看元数据就用这个路径。"
    ));

    if !supports_images {
        output.push_str("\n[当前模型不支持图片输入，这张图没有发给模型]");
        return ToolOutcome::text(output);
    }

    match core.save_image(img.bytes, mime.clone()).await {
        Ok(stored) => ToolOutcome {
            text: output,
            images: vec![ImagePart {
                path: stored.display().to_string(),
                mime,
                source: guest_path.to_string(),
            }],
        },
        Err(e) => {
            tracing::warn!("图片落盘失败: {e:#}");
            output.push_str(&format!("\n[图片没能保存，本轮没发给模型：{e:#}]"));
            ToolOutcome::text(output)
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个假的 Chat Completions 端点：先吐一句话，然后**永远不结束**。
    /// 返回 base_url。
    async fn hanging_provider() -> String {
        use axum::response::sse::{Event as SseEvent, Sse};
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(|| async {
                let first = tokio_stream::iter([Ok::<_, std::convert::Infallible>(
                    SseEvent::default().data(r#"{"choices":[{"delta":{"content":"想到一半"}}]}"#),
                )]);
                let forever = tokio_stream::pending();
                Sse::new(tokio_stream::StreamExt::chain(first, forever))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// 停止必须在**流到一半**就生效，而不是等这一步自己流完 ——
    /// 网络卡住或者模型想很久的时候，那可能是永远。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_interrupts_a_stream_that_never_ends_and_keeps_what_arrived() {
        crate::llm::install_crypto_provider();
        let base = hanging_provider().await;
        let mut p = Provider::compatible("fake", base.clone(), "m", "k");
        p.api_style = crate::llm::ApiStyle::Chat;

        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        let t = core.state.begin_turn(&sid).unwrap();
        // 能力判定直接给缓存，别让测试去连 models.dev
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let turn = {
            let (core, sid, t) = (core.clone(), sid.clone(), t.clone());
            tokio::spawn(async move { run_turn(&core, &p, "你好", vec![], &sid, t, tx).await })
        };
        // 等第一段正文到了再按停止
        loop {
            let c = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("第一段正文一直没来")
                .expect("流提前结束了");
            if c["type"] == "text-delta" {
                break;
            }
        }
        assert!(core.state.is_busy(&sid));
        assert!(core.state.request_abort(&sid));

        tokio::time::timeout(std::time::Duration::from_secs(3), turn)
            .await
            .expect("按了停止，回合没停下来")
            .unwrap()
            .unwrap();

        let store = core.state.chat.read().await;
        let sess = store.active_ref().unwrap();
        assert!(
            matches!(sess.messages.last(), Some(ChatMessage::Assistant { content, .. }) if content == "想到一半"),
            "已经流出来的那半句要留在历史里"
        );
        assert!(
            !core.state.is_busy(&sid),
            "回合结束要把会话从「进行中」里拿掉"
        );
    }

    /// 按顺序回话的假 Chat Completions 端点。`script[n]` 是第 n 次请求的回法：
    /// `Err(状态码)` = 直接报错；`Ok(帧)` = 200 + 这些 SSE 帧（不带 `[DONE]`
    /// 就是「连接在回复结束前断了」）。返回 (base_url, 已收到的请求数)。
    async fn scripted_provider(
        script: Vec<Result<Vec<&'static str>, u16>>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::response::IntoResponse;
        use std::sync::atomic::Ordering;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        let script = std::sync::Arc::new(script);
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move || {
                let (h, script) = (h.clone(), script.clone());
                async move {
                    let n = h.fetch_add(1, Ordering::SeqCst);
                    match script.get(n).cloned().unwrap_or(Err(500)) {
                        Err(code) => (axum::http::StatusCode::from_u16(code).unwrap(), "boom")
                            .into_response(),
                        Ok(frames) => {
                            let body: String =
                                frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                            ([("content-type", "text/event-stream")], body).into_response()
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), hits)
    }

    /// 跑一个回合直到结束，返回 (结果, 推给前端的全部 chunk, 会话里最后一条)。
    async fn run_scripted(base: &str) -> (anyhow::Result<()>, Vec<Value>, Option<ChatMessage>) {
        crate::llm::install_crypto_provider();
        let mut p = Provider::compatible("fake", base, "m", "k");
        p.api_style = crate::llm::ApiStyle::Chat;
        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        let t = core.state.begin_turn(&sid).unwrap();
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&core, &p, "你好", vec![], &sid, t, tx),
        )
        .await
        .expect("回合卡住了");
        let mut chunks = Vec::new();
        while let Ok(c) = rx.try_recv() {
            chunks.push(c);
        }
        let last = core
            .state
            .chat
            .read()
            .await
            .get(&sid)
            .unwrap()
            .messages
            .last()
            .cloned();
        (r, chunks, last)
    }

    const OK_REPLY: [&str; 2] = [
        r#"{"choices":[{"delta":{"content":"好了"},"finish_reason":"stop"}]}"#,
        "[DONE]",
    ];

    /// 临时故障退避重试：503、以及连上之后一个字没出就断了，都重来；
    /// 第三次正常回话，回合照常完成，前端看得到在重试。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transient_llm_failures_are_retried_before_anything_streamed() {
        let (base, hits) =
            scripted_provider(vec![Err(503), Ok(vec![]), Ok(OK_REPLY.to_vec())]).await;
        let (r, chunks, last) = run_scripted(&base).await;
        r.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert!(
            matches!(last, Some(ChatMessage::Assistant { ref content, .. }) if content == "好了"),
            "{last:?}"
        );
        let warnings = chunks
            .iter()
            .filter(|c| c["type"] == "data-warning")
            .count();
        assert_eq!(warnings, 2, "每次重试都要让前端知道在等什么");
        assert!(!chunks.iter().any(|c| c["type"] == "error"));
    }

    /// 不该重试的：鉴权失败（重来也没用）；以及已经流出了内容才断的
    /// （重来的话界面上就是两份）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn permanent_failures_and_half_streamed_replies_are_not_retried() {
        let (base, hits) = scripted_provider(vec![Err(401), Ok(OK_REPLY.to_vec())]).await;
        let (r, _, _) = run_scripted(&base).await;
        assert!(r.is_err());
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "401 不重试"
        );

        let half = vec![r#"{"choices":[{"delta":{"content":"说到一半"}}]}"#];
        let (base, hits) = scripted_provider(vec![Ok(half), Ok(OK_REPLY.to_vec())]).await;
        let (r, _, _) = run_scripted(&base).await;
        assert!(r.is_err(), "流出过内容就断了：报错，不重来");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Responses 端点回了 200 却只发 keep-alive（实测 OpenCode Go 的 deepseek-flash）：
    /// 不能就这么挂着 —— 认成「没有这套 API」，退回 Chat Completions，回合照常完成。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_responses_endpoint_that_only_sends_keepalives_falls_back_to_chat() {
        crate::llm::install_crypto_provider();
        let keepalives = || {
            let (tx, rx) = mpsc::channel::<Result<&'static str, std::io::Error>>(1);
            tokio::spawn(async move {
                while tx.send(Ok(": keep-alive\n\n")).await.is_ok() {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });
            let s = tokio_stream::wrappers::ReceiverStream::new(rx);
            (
                [("content-type", "text/event-stream")],
                axum::body::Body::from_stream(s),
            )
        };
        let app = axum::Router::new()
            .route(
                "/responses",
                axum::routing::post(move || async move { keepalives() }),
            )
            .route(
                "/chat/completions",
                axum::routing::post(|| async {
                    let body: String = OK_REPLY.iter().map(|f| format!("data: {f}\n\n")).collect();
                    ([("content-type", "text/event-stream")], body)
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let p = Provider::compatible("fake", &base, "m", "k"); // api_style = Auto
        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        let t = core.state.begin_turn(&sid).unwrap();
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));
        let (tx, _rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&core, &p, "你好", vec![], &sid, t, tx),
        )
        .await
        .expect("回合卡在只发 keep-alive 的 Responses 端点上了")
        .unwrap();

        let last = core
            .state
            .chat
            .read()
            .await
            .get(&sid)
            .unwrap()
            .messages
            .last()
            .cloned();
        assert!(
            matches!(last, Some(ChatMessage::Assistant { ref content, .. }) if content == "好了"),
            "{last:?}"
        );
        assert!(
            crate::llm::responses_known_unsupported(&p),
            "记住了，下一步不再白等"
        );
    }

    /// 一次「调工具、拿结果、再来一步」的假回话：第一步要一个**不存在的工具**
    ///（`execute_tool` 立刻回「未知工具」——不用沙箱，也不碰网络）。
    const TOOL_CALL_REPLY: [&str; 3] = [
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"nope","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        "[DONE]",
    ];

    /// **排队的消息在下一个步边界就交给模型**，不用等整个回合结束。
    ///
    /// 用户要的是「模型一空下来就给它」：一个回合可能跑十几步、几分钟，等它彻底
    /// 结束才发下一条太晚了。这里让第一步去调一个工具 —— 工具跑完就是步边界，
    /// 那一刻模型手里没活。
    ///
    /// 「交出去了」只能从模型的**输入**里看：所以假端点把每次请求体都留着。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_message_is_handed_over_at_the_next_step_boundary() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        crate::llm::install_crypto_provider();

        let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        // 这个「用户消息」是在回合**跑着**的时候来的：假端点处理第一次请求时把它
        // 排进去（时序上等价于「模型正在干活时用户发了句话」，而且不会打架）。
        let (enq_core, enq_sid) = (core.clone(), sid.clone());
        let (b, n) = (bodies.clone(), calls.clone());
        let app = axum::Router::new().route(
            "/chat/completions",
            axum::routing::post(move |body: String| {
                let (b, n, enq_core, enq_sid) =
                    (b.clone(), n.clone(), enq_core.clone(), enq_sid.clone());
                async move {
                    b.lock().unwrap().push(body);
                    let frames: &[&str] = if n.fetch_add(1, Ordering::SeqCst) == 0 {
                        enq_core.state.enqueue(
                            &enq_sid,
                            crate::state::QueuedMsg {
                                id: "q1".into(),
                                text: "顺便看看磁盘".into(),
                                attachments: vec![],
                            },
                        );
                        &TOOL_CALL_REPLY
                    } else {
                        &OK_REPLY
                    };
                    let body: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
                    ([("content-type", "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut p = Provider::compatible("fake", &base, "m", "k");
        p.api_style = crate::llm::ApiStyle::Chat;
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));

        let (tx, mut rx) = mpsc::unbounded_channel();
        let t = core.state.begin_turn(&sid).unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_turn(&core, &p, "你好", vec![], &sid, t, tx),
        )
        .await
        .expect("回合卡住了")
        .unwrap();

        // 队列空了 = 交出去了
        assert!(core.state.queued_list(&sid).is_empty());

        // 第二次请求里能看到它 —— 这就是「交给模型」的定义。
        // 单独一个块：锁不能跨 await（下面还要读会话），clippy 认块不认 `drop`
        {
            let bodies = bodies.lock().unwrap();
            assert_eq!(bodies.len(), 2, "步边界应该只多打一次请求");
            assert!(
                bodies[1].contains("顺便看看磁盘"),
                "排队那条没进模型的输入：{}",
                bodies[1]
            );
            assert!(
                !bodies[0].contains("顺便看看磁盘"),
                "第一个请求里不该有它（那时它还没排进来）"
            );
        }

        let mut chunks = Vec::new();
        while let Ok(c) = rx.try_recv() {
            chunks.push(c);
        }
        let delivered = chunks
            .iter()
            .find(|c| c["type"] == "data-queued-delivered")
            .expect("得告诉界面这几条出门了");
        assert_eq!(delivered["data"]["ids"][0], "q1");
        let starts: Vec<&str> = chunks
            .iter()
            .filter(|c| c["type"] == "start")
            .map(|c| c["messageId"].as_str().unwrap())
            .collect();
        assert_eq!(starts.len(), 2, "交给模型之后要换一条助手消息");
        assert_ne!(starts[0], starts[1]);

        // 历史形状：两条用户消息、两段助手输出 —— 刷新之后看到的也是这两条
        //（`session_view` 的合并规则里，User 会打断合并）
        let store = core.state.chat.read().await;
        let view = session_view(store.get(&sid).unwrap());
        let roles: Vec<&str> = view.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
        assert_eq!(view[2]["parts"][0]["text"], "顺便看看磁盘");
    }

    /// 回合收尾了但队列里还攒着好几条（按停止、或者刚好卡在「查完队列决定结束」
    /// 那一瞬间进来的消息）：**一次全带上**开下一回合，不能一条一条来（那中间会多
    /// 出好几轮模型输出），更不能烂在队里。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_messages_left_over_after_a_turn_start_the_next_one() {
        crate::llm::install_crypto_provider();
        let (base, hits) =
            scripted_provider(vec![Ok(OK_REPLY.to_vec()), Ok(OK_REPLY.to_vec())]).await;
        let mut p = Provider::compatible("fake", &base, "m", "k");
        p.api_style = crate::llm::ApiStyle::Chat;

        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));
        for (id, text) in [("q1", "接着说"), ("q2", "还有这个")] {
            core.state.enqueue(
                &sid,
                crate::state::QueuedMsg {
                    id: id.into(),
                    text: text.into(),
                    attachments: vec![],
                },
            );
        }

        // 没有回合在跑（队列里却攒着消息）：这就是「按了停止」之后的样子
        continue_with_queued(&core, &p, &sid);

        // 等那个新回合跑完（它没人订阅流，看历史）
        for _ in 0..200 {
            if !core.state.is_busy(&sid) && core.state.queued_list(&sid).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(core.state.queued_list(&sid).is_empty(), "还留在队列里");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "几条要一次全带上，模型只调一次"
        );
        let store = core.state.chat.read().await;
        let msgs = &store.get(&sid).unwrap().messages;
        let users: Vec<&str> = msgs.iter().filter_map(|m| m.user_text()).collect();
        assert_eq!(
            users,
            ["接着说", "还有这个"],
            "两条都要进历史、顺序不变，且中间没有夹着模型的输出：{msgs:?}"
        );
        assert!(
            matches!(msgs.last(), Some(ChatMessage::Assistant { content, .. }) if content == "好了")
        );
    }

    /// Responses API：上一步的思考条目原样放回 input，排在那一步的正文和
    /// 工具调用前面（和服务端给出时的顺序一致）。
    #[test]
    fn responses_requests_replay_reasoning_items_before_the_step() {
        let item =
            json!({ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "e" });
        let bare = json!({ "type": "reasoning", "id": "rs_0", "summary": [] });
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::user("U"),
            ChatMessage::Assistant {
                content: "我去看看".into(),
                reasoning: "先看看目录".into(),
                tool_calls: vec![call("c1")],
                // 光有 id、没有加密内容的回传不了（store: false 时 OpenAI 会 400），跳过
                reasoning_items: vec![bare, item.clone()],
            },
            tool("c1"),
        ];
        let req =
            build_responses_request(&sess, Platform::Android, "m", false, false, None).unwrap();
        let input = serde_json::to_value(&req).unwrap()["input"].clone();
        let kinds: Vec<&str> = input
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message",
                "reasoning",
                "message",
                "function_call",
                "function_call_output"
            ]
        );
        assert_eq!(input[1], item, "原样，一个字段都不改");
        assert_eq!(
            serde_json::to_value(&req).unwrap()["include"],
            json!(["reasoning.encrypted_content"]),
            "要服务端带上加密的思考内容，不然回传不了"
        );
    }

    /// DeepSeek 思考模式：带工具的请求里，历史上**每一条** assistant 消息的
    /// 思考内容都要原样传回（不传就 400，模型也看不到自己上一步想了什么）；
    /// 没有思考内容的那条不带这个字段。
    #[test]
    fn chat_requests_pass_reasoning_back_on_every_assistant_message() {
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::user("U"),
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: "先看看目录".into(),
                tool_calls: vec![call("c1")],
                reasoning_items: Vec::new(),
            },
            tool("c1"),
            ChatMessage::Assistant {
                content: "好了".into(),
                reasoning: "目录里有两个文件".into(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
            ChatMessage::user("再来"),
            ChatMessage::Assistant {
                content: "嗯".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
        ];
        let req = build_request(&sess, Platform::Android, "m", false, false, None).unwrap();
        let v = serde_json::to_value(&req).unwrap();
        let assistants: Vec<&Value> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .collect();
        assert_eq!(assistants[0]["reasoning_content"], "先看看目录");
        assert_eq!(
            assistants[1]["reasoning_content"], "目录里有两个文件",
            "没调工具的那步也要传"
        );
        assert!(
            assistants[2].get("reasoning_content").is_none(),
            "没有思考内容就不带字段"
        );
    }

    fn call(id: &str) -> RecordedToolCall {
        RecordedToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }
    }

    fn tool(id: &str) -> ChatMessage {
        ChatMessage::Tool {
            tool_call_id: id.into(),
            content: "ok".into(),
            images: vec![],
        }
    }

    #[test]
    fn a_dangling_tail_is_closed_in_the_history_itself() {
        let mut sess = ChatSession::default();
        sess.messages.push(ChatMessage::user("跑"));
        sess.messages.push(ChatMessage::Assistant {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![call("c1"), call("c2")],
            reasoning_items: Vec::new(),
        });
        sess.messages.push(tool("c1"));
        close_dangling_tail(&mut sess);
        assert_eq!(sess.messages.len(), 4);
        assert!(
            matches!(&sess.messages[3], ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "c2")
        );

        // 已经完整的不动
        close_dangling_tail(&mut sess);
        assert_eq!(sess.messages.len(), 4);
    }

    fn wire(sess: &ChatSession) -> Vec<serde_json::Value> {
        build_messages(sess, true, true)
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
        ensure_system_prompt(&mut sess, system_prompt(Platform::Android));
        sess.messages.push(ChatMessage::user("第一问"));
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
            reasoning_items: Vec::new(),
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "ok".into(),
            images: vec![],
        });
        ensure_system_prompt(&mut sess, system_prompt(Platform::Android));
        sess.messages.push(ChatMessage::user("第二问"));
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
        sess.messages.push(ChatMessage::user("第一问"));
        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_items: Vec::new(),
        });
        let before = wire(&sess);

        // 版本升级了，下一轮开工前
        ensure_system_prompt(&mut sess, "新提示词");
        let after = wire(&sess);
        assert_eq!(&after[..before.len()], &before[..], "老前缀被动 → 缓存全废");
        assert_eq!(
            after[0]["content"], "旧提示词",
            "第一条必须是旧的，不能替换"
        );
        assert_eq!(after.last().unwrap()["content"], "新提示词");

        // 同一条提示词不能反复追加
        let n = after.len();
        ensure_system_prompt(&mut sess, "新提示词");
        assert_eq!(wire(&sess).len(), n, "重复追加了同一条提示词");

        // 再发一轮，前缀继续单调增长
        sess.messages.push(ChatMessage::user("第二问"));
        let next = wire(&sess);
        assert_eq!(&next[..n], &after[..]);
    }

    #[test]
    fn system_prompt_is_a_frozen_constant_with_no_interpolation() {
        assert!(
            !system_prompt(Platform::Android).contains('{'),
            "system prompt 里出现格式占位符，说明被插值了"
        );
        for dynamic in ["rootfs", "/data/", "Android", "Activity", "1080"] {
            assert!(
                !system_prompt(Platform::Android).contains(dynamic),
                "system prompt 里不该出现运行时才知道的内容: {dynamic}"
            );
        }
    }

    /// 每个平台的提示词 + 工具表：拼出来但**冻结**（同一个平台两次拿到同一串
    /// 字节、没有运行时数据），而且不能叫模型去用它那台机器上没有的东西。
    #[test]
    fn every_platform_gets_a_frozen_prompt_that_matches_its_machine() {
        for p in Platform::ALL {
            let prompt = system_prompt(p);
            let bash = &tools(p)
                .iter()
                .find(|t| t.name == "bash")
                .unwrap()
                .description;
            assert!(
                std::ptr::eq(prompt, system_prompt(p)),
                "{p:?}: 每次都该是同一份"
            );
            for dynamic in [
                "/home/", "/Users/", "/data/", "rootfs", "Arch", "Ubuntu", "1080",
            ] {
                assert!(
                    !prompt.contains(dynamic) && !bash.contains(dynamic),
                    "{p:?}: 不该出现这台机器的具体信息: {dynamic}"
                );
            }
            for door in ["agent browser", "agent search", "agent help"] {
                assert!(prompt.contains(door), "{p:?}: 提示词得告诉模型 {door} 在哪");
            }
            assert!(bash.contains("setsid"), "{p:?}: 后台常驻服务怎么放");
            assert!(bash.contains(p.proc_tools()), "{p:?}: 之后怎么看还剩什么");

            if p.is_host() {
                // 工具表本身就是这台机器的，Alpine / apk 一个字都不该有
                for phone in [
                    "Alpine",
                    "apk add",
                    "agent ps",
                    "agent tap",
                    "/root/",
                    "/sdcard",
                ] {
                    assert!(
                        !prompt.contains(phone) && !bash.contains(phone),
                        "{p:?}: 宿主模式里还在说手机上的 {phone}"
                    );
                }
                assert!(
                    prompt.contains("agent open"),
                    "{p:?}: 电脑上开网页走 agent open"
                );
            } else {
                assert!(bash.contains("Alpine") && bash.contains("apk add"));
                assert!(
                    prompt.contains("agent ps"),
                    "{p:?}: 沙箱里的进程用 agent ps 看"
                );
            }
        }
        let mac = &tools(Platform::Macos)[1].description;
        assert!(
            mac.contains("brew") && mac.contains("BSD"),
            "macOS 要点名 BSD 用户态"
        );
        assert!(!tools(Platform::Linux)[1].description.contains("brew"));
    }

    #[test]
    fn the_request_is_the_history_verbatim() {
        // `build_messages` 就是「历史原样」：不拼、不前置、不插上下文块。
        // 系统提示词是历史里的一条（`ensure_system_prompt` 注的），
        // 所以 请求体长度 == 历史长度，一条不多一条不少。
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, system_prompt(Platform::Android));
        sess.messages.push(ChatMessage::user("问"));
        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_items: Vec::new(),
        });
        sess.messages.push(ChatMessage::user("再问"));

        let w = wire(&sess);
        assert_eq!(w.len(), sess.messages.len(), "请求体里多出了别的东西");
        assert_eq!(w[0]["role"], "system");
        assert_eq!(w[0]["content"], system_prompt(Platform::Android));
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
        ensure_system_prompt(&mut sess, system_prompt(Platform::Android));
        sess.messages.push(ChatMessage::user("第一问"));
        // 转成 Value 是为了断言请求体本身；线上路径不经过这一步
        let req1 = serde_json::to_value(
            build_request(
                &sess,
                Platform::Android,
                "deepseek-v4.1-flash",
                true,
                true,
                None,
            )
            .unwrap(),
        )
        .unwrap();

        sess.messages.push(ChatMessage::Assistant {
            content: "答".into(),
            reasoning: "想".into(),
            tool_calls: vec![RecordedToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
            reasoning_items: Vec::new(),
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "ok".into(),
            images: vec![],
        });
        sess.messages.push(ChatMessage::user("第二问"));
        let req2 = serde_json::to_value(
            build_request(
                &sess,
                Platform::Android,
                "deepseek-v4.1-flash",
                true,
                true,
                None,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            req1["tools"], req2["tools"],
            "工具表变了 → 整段前缀缓存失效"
        );
        assert_eq!(req1["model"], req2["model"]);
        assert_eq!(req1["stream_options"], req2["stream_options"]);
        // 用量上报是缓存命中率的唯一来源，不能因为换成强类型字段就丢了
        assert_eq!(req1["stream_options"], json!({ "include_usage": true }));
        let a = req1["messages"].as_array().unwrap();
        let b = req2["messages"].as_array().unwrap();
        assert_eq!(&b[..a.len()], &a[..], "完整请求体不是前缀关系");
    }

    /// 思考强度：两套协议字段形状不同（Chat 顶层字符串 / Responses 嵌套对象），
    /// 而**默认档必须一个字都不发** —— 严格网关见到不认识的参数会 400。
    #[test]
    fn reasoning_effort_is_tagged_per_protocol_and_omitted_when_unset() {
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, system_prompt(Platform::Android));
        sess.messages.push(ChatMessage::user("嗨"));

        let chat = serde_json::to_value(
            build_request(&sess, Platform::Android, "m", true, true, None).unwrap(),
        )
        .unwrap();
        assert!(chat.get("reasoning_effort").is_none(), "{chat}");
        let resp = serde_json::to_value(
            build_responses_request(&sess, Platform::Android, "m", true, true, None).unwrap(),
        )
        .unwrap();
        assert!(resp.get("reasoning").is_none(), "{resp}");

        let chat = serde_json::to_value(
            build_request(&sess, Platform::Android, "m", true, true, Some("high")).unwrap(),
        )
        .unwrap();
        assert_eq!(chat["reasoning_effort"], "high");
        let resp = serde_json::to_value(
            build_responses_request(&sess, Platform::Android, "m", true, true, Some("low"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(resp["reasoning"]["effort"], "low");

        // 强度只是顶层请求参数，不进 messages —— 前缀不变式不受影响
        assert_eq!(chat["messages"][0]["role"], "system");
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
            ChatMessage::user("看图"),
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![call("c1"), call("c2")],
                reasoning_items: Vec::new(),
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "{}".into(),
                images: vec![ImagePart {
                    path: img.display().to_string(),
                    mime: "image/png".into(),
                    source: "/workspace/a.png".into(),
                }],
            },
            ChatMessage::Tool {
                tool_call_id: "c2".into(),
                content: "{}".into(),
                images: vec![],
            },
        ];

        let v: Vec<Value> = build_messages(&sess, true, true)
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
        let again: Vec<Value> = build_messages(&sess, true, true)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v, again, "带图的历史不是逐字节可复现的");

        // 模型不支持图片时：仍然有这条 carrier，但只有文字说明
        let v: Vec<Value> = build_messages(&sess, false, false)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v[4]["role"], "user");
        let parts = v[4]["content"].as_array().unwrap();
        assert!(parts.iter().all(|p| p["type"] == "text"));

        let _ = std::fs::remove_file(&img);
    }

    /// 用户附件：正文、图片、PDF 要拼在**同一条** user 消息里。
    ///
    /// 为什么要单测：一条消息 = 一个回合边界，附件如果另占一条消息，
    /// 增量同步的切点就可能把正文和附件切开。
    #[test]
    fn user_attachments_ride_on_the_same_user_message() {
        let dir = std::env::temp_dir();
        let png = dir.join(format!("agent-att-img-{}.png", std::process::id()));
        let pdf = dir.join(format!("agent-att-pdf-{}.pdf", std::process::id()));
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nfake").unwrap();
        std::fs::write(&pdf, b"%PDF-1.4\nfake").unwrap();

        let att = |id: &str, path: &std::path::Path, mime: &str, kind| Attachment {
            id: id.into(),
            path: path.display().to_string(),
            mime: mime.into(),
            name: format!("文件.{id}"),
            kind,
        };
        let mut sess = ChatSession::new();
        sess.messages = vec![ChatMessage::User {
            text: "看看这两个".into(),
            attachments: vec![
                att("a.png", &png, "image/png", AttachmentKind::Image),
                att("b.pdf", &pdf, "application/pdf", AttachmentKind::Pdf),
            ],
        }];

        let v: Vec<Value> = build_messages(&sess, true, true)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v.len(), 1, "正文和附件必须在同一条消息里");
        assert_eq!(v[0]["role"], "user");
        let parts = v[0]["content"].as_array().expect("带附件时 content 是数组");
        assert_eq!(parts[0]["type"], "text");
        let kinds: Vec<&str> = parts.iter().map(|p| p["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["text", "text", "image_url", "text", "file"]);
        assert!(parts[2]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
        assert_eq!(parts[4]["file"]["filename"], "文件.b.pdf");
        assert!(parts[4]["file"]["file_data"]
            .as_str()
            .unwrap()
            .starts_with("data:application/pdf;base64,"));

        // 同一份历史要逐字节可复现（prompt cache 靠这个）
        let again: Vec<Value> = build_messages(&sess, true, true)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(v, again);

        // 模型不支持对应模态时降级成文字，绝不发必然 400 的请求
        let v: Vec<Value> = build_messages(&sess, false, false)
            .unwrap()
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let parts = v[0]["content"].as_array().unwrap();
        assert!(parts.iter().all(|p| p["type"] == "text"), "{parts:?}");
        assert_eq!(v.len(), 1, "降级也不能多出消息");

        let _ = std::fs::remove_file(&png);
        let _ = std::fs::remove_file(&pdf);
    }

    /// 带附件的会话：完整视图与「回合边界切分」的增量视图必须一致 ——
    /// 否则滚动到一半刷新会看到重复的附件。
    #[test]
    fn incremental_slice_keeps_attachments_with_their_turn() {
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::user("U1"),
            ChatMessage::Assistant {
                content: "答1".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
            ChatMessage::User {
                text: "U2".into(),
                attachments: vec![Attachment {
                    id: "x.pdf".into(),
                    path: "/nonexistent".into(),
                    mime: "application/pdf".into(),
                    name: "x.pdf".into(),
                    kind: AttachmentKind::Pdf,
                }],
            },
            ChatMessage::Assistant {
                content: "答2".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
        ];
        let full = session_view(&sess);
        for from in 0..=sess.messages.len() {
            if from > 0 && !sess.messages[from - 1].is_user() {
                continue;
            }
            let head = session_view_count_before(&sess, from);
            let tail = session_view_from(&sess, from);
            let joined: Vec<Value> = full[..head].iter().cloned().chain(tail).collect();
            assert_eq!(joined, full, "from={from} 时增量与完整视图不一致");
        }
        assert_eq!(full[2]["parts"][1]["type"], "file");
        assert_eq!(full[2]["parts"][1]["url"], "/api/attachment/x.pdf");
    }

    #[test]
    fn tool_schema_order_and_names_are_fixed() {
        // 工具表也是可缓存前缀的一部分。顺序必须稳定，
        // 所以这里把顺序钉死：谁想改都得先改这个测试。
        //
        // 设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）不作为工具
        // 暴露给模型，它们是沙箱里的命令、由 bash 调用。
        // 浏览器和搜索同理：唯一入口是 `agent browser` / `agent search`。
        let names: Vec<&str> = tools(Platform::Android).iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["read", "bash", "edit"]);
        // 重复调用必须完全一致
        assert_eq!(
            names,
            tools(Platform::Android)
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        );
    }

    /// 删掉的设备工具不能在提示词里留名字。
    ///
    /// 留了模型就会去调一个不存在的工具，拿回「错误：没有名为 device_snapshot
    /// 的工具」，白烧一轮；而且这种「提示词与实际工具表不一致」的 bug 很难在真机上看出来。
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
            assert!(
                !system_prompt(Platform::Android).contains(g),
                "提示词还在提 {g}"
            );
            assert!(
                !tools(Platform::Android).iter().any(|t| t.name == g),
                "工具表里还有 {g}"
            );
        }
    }

    /// 浏览器和联网搜索都只能从**一个门**进：沙箱里的 `agent browser` / `agent search`。
    ///
    /// 工具那条路有实际代价：前缀里多 2400 字（工具表的 54%），两处动作表各自
    /// 漂移（`agent browser new_tab URL` 把 URL 静默吃掉就是这么来的）。搜索同理，
    /// 而且它本来就能和 `curl` / `jq` / 循环凑在一起用。
    #[test]
    fn browser_and_search_are_only_reachable_through_the_sandbox_cli() {
        for tool in ["browser", "search"] {
            assert!(
                !tools(Platform::Android).iter().any(|t| t.name == tool),
                "{tool} 工具又回来了？加回去前先看看它和沙箱命令的重复度"
            );
            assert!(
                system_prompt(Platform::Android).contains(&format!("agent {tool}")),
                "提示词得告诉模型去哪用它（不然它得自己跑 agent help 才发现）"
            );
        }
        // 只属于它们的参数不能再出现在工具表里（工具表是可缓存前缀的一部分）
        let dump = serde_json::to_string(
            &tools(Platform::Android)
                .iter()
                .map(|t| (t.schema)())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        for k in [
            "full_page",
            "max_depth",
            "set_user_agent",
            "find_elements",
            "num_results",
        ] {
            assert!(!dump.contains(k), "工具表里还有删掉的参数 {k}");
        }
    }

    #[test]
    fn tool_schemas_serialize_byte_identically() {
        let once = serde_json::to_string(
            &tools(Platform::Android)
                .iter()
                .map(|t| (t.schema)())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let twice = serde_json::to_string(
            &tools(Platform::Android)
                .iter()
                .map(|t| (t.schema)())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn edit_mode_picks_create_or_replace_and_rejects_ambiguous_args() {
        assert!(matches!(
            edit_mode(&json!({ "content": "x" })),
            Ok(EditMode::Create("x"))
        ));
        assert!(matches!(
            edit_mode(&json!({ "edits": [] })),
            Ok(EditMode::Replace)
        ));

        // 两个都给：不做猜测，直接报错（猜错了要么盖掉原文、要么改错地方）
        let both = edit_mode(&json!({ "content": "x", "edits": [] }))
            .unwrap_err()
            .to_string();
        assert!(both.contains("只能给一个"), "{both}");

        let neither = edit_mode(&json!({ "path": "a" })).unwrap_err().to_string();
        assert!(neither.contains("缺少"), "{neither}");

        let bad = edit_mode(&json!({ "content": 42 }))
            .unwrap_err()
            .to_string();
        assert!(bad.contains("字符串"), "{bad}");
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
        // 缺省 → 第 1 行、读到上限、不加行号
        assert_eq!(parse_offset(&json!({})), 1);
        assert_eq!(parse_limit(&json!({})).unwrap(), None);
        assert_eq!(parse_limit(&json!({ "limit": null })).unwrap(), None);
        assert!(!parse_numbered(&json!({})));
        assert!(!parse_numbered(&json!({ "numbered": null })));

        assert_eq!(parse_offset(&json!({ "offset": 42 })), 42);
        // 非法/越界的 offset 退回第 1 行，不报错（模型偶发写 0）
        assert_eq!(parse_offset(&json!({ "offset": 0 })), 1);
        assert_eq!(parse_offset(&json!({ "offset": -3 })), 1);
        assert_eq!(parse_offset(&json!({ "offset": "x" })), 1);

        assert_eq!(parse_limit(&json!({ "limit": 5 })).unwrap(), Some(5));
        assert_eq!(parse_limit(&json!({ "limit": 0 })).unwrap(), Some(0));
        assert!(parse_limit(&json!({ "limit": -1 })).is_err());
        assert!(parse_limit(&json!({ "limit": "5" })).is_err());

        assert!(parse_numbered(&json!({ "numbered": true })));
        // 类型写错当没给，不报错
        assert!(!parse_numbered(&json!({ "numbered": "true" })));
        assert!(!parse_numbered(&json!({ "numbered": 1 })));
    }

    #[test]
    fn bash_timeout_defaults_to_a_minute_and_is_capped() {
        // 不给 / null → 默认 60s
        assert_eq!(parse_timeout(&json!({})).unwrap(), BASH_DEFAULT_TIMEOUT);
        assert_eq!(
            parse_timeout(&json!({ "timeout": null })).unwrap(),
            BASH_DEFAULT_TIMEOUT
        );
        assert_eq!(BASH_DEFAULT_TIMEOUT.as_secs(), 60);
        // 给了但写错了：报错，不偷偷换成默认值
        assert!(parse_timeout(&json!({ "timeout": 0 })).is_err());
        assert!(parse_timeout(&json!({ "timeout": -5 })).is_err());
        assert!(parse_timeout(&json!({ "timeout": "600" })).is_err());

        // 普通值原样，长时间任务（比如 3 小时下载）完全允许
        assert_eq!(
            parse_timeout(&json!({ "timeout": 600 })).unwrap().as_secs(),
            600
        );
        assert_eq!(
            parse_timeout(&json!({ "timeout": 10800 }))
                .unwrap()
                .as_secs(),
            10800
        );
        // 超过上限就夹到 24h，而不是报错或无限大
        assert_eq!(
            parse_timeout(&json!({ "timeout": 999_999_999 })).unwrap(),
            linux::MAX_TIMEOUT
        );
        assert!(linux::MAX_TIMEOUT >= std::time::Duration::from_secs(6 * 3600));

        // 超时被杀时的提醒要说清用了多少秒；默认值那条还要教它怎么给
        let h = timeout_hint(BASH_DEFAULT_TIMEOUT, false);
        assert!(
            h.contains("60s") && h.contains("默认") && h.contains("timeout"),
            "{h}"
        );
        assert!(!timeout_hint(std::time::Duration::from_secs(600), true).contains("默认"));
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

    /// 界面上的工具结果就是**模型收到的那段原文**，原样；读到的图片另给沙箱里的
    /// 原图路径。
    #[test]
    fn session_view_shows_tool_results_verbatim() {
        let call = |id: &str| RecordedToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        };
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::user("问"),
            ChatMessage::Assistant {
                content: String::new(),
                reasoning: String::new(),
                tool_calls: vec![call("new")],
                reasoning_items: Vec::new(),
            },
            ChatMessage::Tool {
                tool_call_id: "new".into(),
                content: "a.txt\n\n[exit 0]".into(),
                images: vec![ImagePart {
                    path: "/data/img/1.png".into(),
                    mime: "image/png".into(),
                    source: "/workspace/shot.png".into(),
                }],
            },
        ];
        let v = session_view(&sess);
        let parts = v[1]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["output"], json!("a.txt\n\n[exit 0]"));
        assert_eq!(parts[0]["images"], json!(["/workspace/shot.png"]));
    }

    /// 工具结果的 SSE 事件：正文是纯文本；有图才带 `images`。
    #[test]
    fn tool_output_event_is_the_plain_text() {
        let ev = tool_output_event("c1", "错误：缺少参数 path", &[]);
        assert_eq!(ev["output"], json!("错误：缺少参数 path"));
        assert!(ev.get("images").is_none());
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
            ChatMessage::user("问"),
            ChatMessage::Assistant {
                content: "先看看".into(),
                reasoning: "想1".into(),
                tool_calls: vec![call("c1")],
                reasoning_items: Vec::new(),
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "{}".into(),
                images: vec![],
            },
            ChatMessage::Assistant {
                content: "答".into(),
                reasoning: "想2".into(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
            ChatMessage::user("再问"),
            ChatMessage::Assistant {
                content: "再答".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
        ];
        let v = session_view(&sess);
        let roles: Vec<&str> = v.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "user", "assistant"],
            "一个回合一条助手消息"
        );

        let parts: Vec<String> = v[1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                format!(
                    "{}:{}",
                    p["type"].as_str().unwrap(),
                    p["text"].as_str().unwrap_or("")
                )
            })
            .collect();
        assert_eq!(
            parts,
            [
                "reasoning:想1",
                "text:先看看",
                "tool:",
                "reasoning:想2",
                "text:答"
            ]
        );

        let ids: std::collections::HashSet<&str> = v[1]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["id"].as_str())
            .collect();
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
            ChatMessage::user("U1"),
            ChatMessage::Assistant {
                content: "先看看".into(),
                reasoning: "想".into(),
                tool_calls: vec![call("c1")],
                reasoning_items: Vec::new(),
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
                reasoning_items: Vec::new(),
            },
            ChatMessage::user("U2"),
            ChatMessage::Assistant {
                content: "答2".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
        ];
        let full = session_view(&sess);

        // 只在回合边界切（上一条是 User，或 from == 0）
        for from in 0..=sess.messages.len() {
            if from > 0 && !sess.messages[from - 1].is_user() {
                continue;
            }
            let head = session_view_count_before(&sess, from);
            let tail = session_view_from(&sess, from);
            let joined: Vec<Value> = full[..head].iter().cloned().chain(tail).collect();
            assert_eq!(joined, full, "from={from} 时增量结果与完整视图不一致");
        }
    }

    /// 轮询补丁的不变式：客户端手上是**任意时刻**（`messages[..n]`）的视图，
    /// 按前端的规则（`App.svelte` 的 `loadMessages`）套上 `session_patch(n)`，
    /// 必须逐字节等于完整视图。而且回合再长，补丁也只带最后一步。
    #[test]
    fn session_patch_brings_any_snapshot_up_to_date() {
        let call = |id: &str| RecordedToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let tool = |id: &str| ChatMessage::Tool {
            tool_call_id: id.into(),
            content: format!("{{\"output\":\"{id}\"}}"),
            images: vec![],
        };
        let asst =
            |content: &str, reasoning: &str, calls: Vec<RecordedToolCall>| ChatMessage::Assistant {
                content: content.into(),
                reasoning: reasoning.into(),
                tool_calls: calls,
                reasoning_items: Vec::new(),
            };
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::user("U1"),
            asst("先看看", "想", vec![call("c1"), call("c2")]),
            tool("c1"),
            tool("c2"),
            // 什么都没有的一步：视图里不出现，也不算步数
            asst("", "", vec![]),
            asst("", "再想", vec![call("c3")]),
            tool("c3"),
            asst("答1", "", vec![]),
            ChatMessage::System("新提示词".into()),
            ChatMessage::user("U2"),
            asst("", "", vec![call("c4")]),
            tool("c4"),
            asst("答2", "", vec![]),
        ];
        let full = session_view(&sess);

        for n in 0..=sess.messages.len() {
            let mut then = sess.clone();
            then.messages.truncate(n);
            let client = session_view(&then);

            let p = session_patch(&sess, n);
            assert!(p.replace_from <= client.len(), "n={n}");
            let mut fresh = p.messages.clone();
            if let Some(k) = p.keep_parts {
                let old = client[p.replace_from]["parts"].as_array().unwrap();
                let mut parts: Vec<Value> = old[..k].to_vec();
                parts.extend(fresh[0]["parts"].as_array().unwrap().iter().cloned());
                fresh[0]["parts"] = json!(parts);
            }
            let applied: Vec<Value> = client[..p.replace_from]
                .iter()
                .cloned()
                .chain(fresh)
                .collect();
            assert_eq!(applied, full, "n={n} 时补丁结果与完整视图不一致");
        }

        // 回合进行中（服务端刚追加了 c3 的结果，客户端停在它之前）：只重发
        // 最后那一步，前面那步留在客户端
        let mut now = sess.clone();
        now.messages.truncate(8);
        let p = session_patch(&now, 7);
        assert_eq!(p.keep_parts, Some(4), "第一步的思考 + 正文 + 两个工具");
        assert_eq!(p.messages.len(), 1);
        assert_eq!(
            p.messages[0]["parts"].as_array().unwrap().len(),
            2,
            "思考 + c3"
        );
        assert_eq!(p.messages[0]["parts"][1]["state"], "output");
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
        assert_ne!(
            s1[0].1, s2[0].1,
            "第二步的思考不能和第一步同 id，否则前端会接到一起"
        );
        assert_ne!(s1[4].1, s2[3].1, "第二步的正文同理");
    }

    // —— 侧边栏搜索 ——

    fn store_with_two_sessions() -> SessionStore {
        let mut a = ChatSession::new();
        a.title = "Alpine 版本".into();
        a.updated_at = 200;
        a.messages = vec![
            ChatMessage::System("系统提示词里有 apk 这个词，不该被搜到".into()),
            ChatMessage::user("看一下 apk 版本"),
            ChatMessage::Assistant {
                content: "Alpine 3.21\n第二行".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
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
        b.messages = vec![ChatMessage::user("写个 fizzbuzz")];

        SessionStore {
            sessions: vec![a, b],
            active: String::new(),
        }
    }

    #[test]
    fn search_hits_message_text_case_insensitively() {
        let store = store_with_two_sessions();

        let hits = store.search("APK", 3, 60, |_| None);
        assert_eq!(hits.len(), 1, "只搜用户/助手正文，系统提示词和工具结果不算");
        assert_eq!(hits[0]["title"], "Alpine 版本");
        assert_eq!(hits[0]["role"], "user");
        assert_eq!(hits[0]["snippet"], "看一下 apk 版本");

        let hits = store.search("alpine", 3, 60, |_| None);
        assert_eq!(hits[0]["snippet"], "Alpine 3.21 第二行", "换行要拍成空格");
    }

    #[test]
    fn search_is_ordered_by_recency_and_capped() {
        let store = store_with_two_sessions();
        // "a" 两个会话都命中；更新的那个（a，updated_at=200）排前面
        let hits = store.search("a", 3, 60, |_| None);
        assert!(hits.len() >= 2);
        assert_eq!(hits[0]["title"], "Alpine 版本");

        // 每个会话最多 3 条
        let mut many = ChatSession::new();
        many.messages = (0..10)
            .map(|i| ChatMessage::user(format!("命中 {i}")))
            .collect();
        let store = SessionStore {
            sessions: vec![many],
            active: String::new(),
        };
        assert_eq!(store.search("命中", 3, 60, |_| None).len(), 3);
        assert_eq!(store.search("命中", 3, 1, |_| None).len(), 1, "总量封顶");
    }

    #[test]
    fn search_handles_empty_and_multibyte() {
        let store = store_with_two_sessions();
        assert!(store.search("", 3, 60, |_| None).is_empty());
        assert!(store.search("   ", 3, 60, |_| None).is_empty());
        assert!(store.search("不存在的词", 3, 60, |_| None).is_empty());

        // 中文按字符切，不能切出半截 UTF-8
        let mut s = ChatSession::new();
        s.messages = vec![ChatMessage::user(format!(
            "{}测试{}",
            "开头".repeat(100),
            "尾巴".repeat(100)
        ))];
        let store = SessionStore {
            sessions: vec![s],
            active: String::new(),
        };
        let hits = store.search("测试", 3, 60, |_| None);
        assert_eq!(hits.len(), 1);
        let snip = hits[0]["snippet"].as_str().unwrap();
        assert!(snip.starts_with('…') && snip.ends_with('…'));
        assert!(snip.chars().count() < 90);
    }

    /// 小写化会改变字符数的字符（`İ` → 两个字符）在命中位置之前时，
    /// 摘要还得切在原文的正确位置上。
    #[test]
    fn snippet_survives_case_folding_that_changes_length() {
        let text = format!("{}needle{}", "İ".repeat(30), "x".repeat(10));
        let snip = hit_snippet(&text, "NEEDLE").unwrap();
        assert!(snip.contains("needle"), "{snip}");
        assert!(snip.ends_with("xxxxxxxxxx"), "{snip}");
    }

    /// 结束原因只要非空就记。以前只在 `length` 时写值，于是 `usage` 表的 `finish`
    /// 那一列对 `stop` / `tool_calls` 一直是 NULL；空串 / None 不覆盖已拿到的真原因。
    #[test]
    fn the_providers_finish_reason_is_recorded_unless_empty() {
        let mut f = None;
        note_finish(&mut f, Some("tool_calls"));
        assert_eq!(f.as_deref(), Some("tool_calls"));
        note_finish(&mut f, Some(""));
        assert_eq!(f.as_deref(), Some("tool_calls"), "空串不能覆盖");
        note_finish(&mut f, None);
        assert_eq!(f.as_deref(), Some("tool_calls"), "None 不能覆盖");
        note_finish(&mut f, Some("length"));
        assert_eq!(f.as_deref(), Some("length"), "后来的非空值覆盖");
    }

    /// 消息不在内存里的会话：搜索时临时从库里读（`load`），读不出来就跳过。
    #[test]
    fn search_loads_evicted_sessions_on_demand() {
        let mut store = store_with_two_sessions();
        let parked = store.sessions[1].messages.clone();
        let id = store.sessions[1].id.clone();
        store.sessions[1].evict();
        assert!(store.sessions[1].messages.is_empty());

        let mut asked = Vec::new();
        let hits = store.search("fizzbuzz", 3, 60, |sid| {
            asked.push(sid.to_string());
            Some(parked.clone())
        });
        assert_eq!(asked, vec![id]);
        assert_eq!(hits.len(), 1);
        assert!(store.sessions[1].messages.is_empty(), "搜完不装回去");

        assert!(store.search("fizzbuzz", 3, 60, |_| None).is_empty());
    }

    /// 放掉消息之后，侧边栏 / 落盘要的数字照旧。
    #[test]
    fn an_evicted_session_keeps_its_counts() {
        let mut s = ChatSession::new();
        s.messages = vec![
            ChatMessage::System("p".into()),
            ChatMessage::user("一"),
            ChatMessage::user("二"),
        ];
        s.evict();
        assert!(!s.resident());
        assert_eq!((s.message_count(), s.user_count()), (3, 2));
        assert!(s.has_content());

        s.install(vec![ChatMessage::user("一")]);
        assert!(s.resident());
        assert_eq!(s.message_count(), 1);
    }

    // —— 空会话 ——

    /// 建了会话但一句话没说 → 不进侧边栏列表。
    ///
    /// 前端点「新对话」不会让服务端建会话，这里是兜底：
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
            .push(ChatMessage::user("问了"));
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], id.as_str());
        assert!(listed[0]["active"].as_bool().unwrap());

        // 清空过（/api/reset）又回到「不算数」
        store.get_mut(&id).unwrap().clear();
        assert!(store.list().is_empty());
    }

    /// 测排队用：一个假端点 + 一个会话 + 排着一条 `q1`。
    async fn queue_fixture(
        script: Vec<Result<Vec<&'static str>, u16>>,
    ) -> (
        Core,
        Provider,
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        crate::llm::install_crypto_provider();
        let (base, hits) = scripted_provider(script).await;
        let mut p = Provider::compatible("fake", &base, "m", "k");
        p.api_style = crate::llm::ApiStyle::Chat;
        let core = Core::new();
        let sid = core.state.chat.write().await.create();
        *core.state.vision.write().unwrap() = Some((format!("{base}|m"), false));
        *core.state.files.write().unwrap() = Some((format!("{base}|m"), false));
        core.state.enqueue(
            &sid,
            crate::state::QueuedMsg {
                id: "q1".into(),
                text: "排着的".into(),
                attachments: vec![],
            },
        );
        (core, p, sid, hits)
    }

    /// 出错的回合：排队的**留着**（不接着烧掉），错误记下来给界面看。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_turn_keeps_the_queue_and_records_why() {
        let (core, p, sid, hits) = queue_fixture(vec![Err(400)]).await;
        let t = core.state.begin_turn(&sid).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let r = run_turn(&core, &p, "你好", vec![], &sid, t, tx).await;
        assert!(r.is_err());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "不该接着跑"
        );
        assert!(!core.state.is_busy(&sid));
        assert_eq!(core.state.queued_list(&sid).len(), 1, "排队的要留着");
        assert!(core.state.turn_error(&sid).is_some(), "错误要记下来");
        // 下一个回合一开始，上次的错误就不算数了
        let t = core.state.begin_turn(&sid).unwrap();
        assert!(core.state.turn_error(&sid).is_none());
        drop(t);
        core.state.end_turn(&sid);
    }

    /// 按了停止的回合：当前这一轮停下，但**排队的照样交出去**。
    ///
    /// 「停止」停的是这一轮，不是把用户明确按过发送的排队也丢掉 —— 交出去之后
    /// 另起一个回合跑，不能烂在队里（不然它会一直卡在「排队中」发不出去）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stopped_turn_still_hands_over_the_queue() {
        let (core, p, sid, hits) = queue_fixture(vec![Ok(OK_REPLY.to_vec())]).await;
        let t = core.state.begin_turn(&sid).unwrap();
        t.request_abort();
        let (tx, _rx) = mpsc::unbounded_channel();
        run_turn(&core, &p, "你好", vec![], &sid, t, tx)
            .await
            .unwrap();
        // 排队那条在一个 spawn 出来的新回合里跑，等它跑完
        for _ in 0..200 {
            if !core.state.is_busy(&sid) && core.state.queued_list(&sid).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "排队那条要跑（停的是上一轮，不是队列）"
        );
        assert!(!core.state.is_busy(&sid));
        assert!(
            core.state.queued_list(&sid).is_empty(),
            "交出去之后队列要空"
        );
        assert!(core.state.turn_error(&sid).is_none(), "停止不是错误");
    }

    /// 模型说完（不调工具）那一刻排队里有消息：当场交给它，同一个回合接着跑。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_queued_message_is_handed_over_when_the_model_finishes_talking() {
        let (core, p, sid, hits) =
            queue_fixture(vec![Ok(OK_REPLY.to_vec()), Ok(OK_REPLY.to_vec())]).await;
        let t = core.state.begin_turn(&sid).unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        run_turn(&core, &p, "你好", vec![], &sid, t, tx)
            .await
            .unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(core.state.queued_list(&sid).is_empty());
        let store = core.state.chat.read().await;
        let msgs = &store.get(&sid).unwrap().messages;
        assert!(
            matches!(msgs.last(), Some(ChatMessage::Assistant { content, .. }) if content == "好了"),
            "{msgs:?}"
        );
    }
}
