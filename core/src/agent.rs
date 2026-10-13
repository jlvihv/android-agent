//! Agent 主循环。
//!
//! 决策 D5 选了「自研薄 loop」而不是 `rig`，原因就在这个文件里：
//! 每一个事件都要精确地转成 AI SDK UI Message Stream 的 chunk 推给前端。
//! 框架会把 loop 包起来，反而挡路。
//!
//! 工具调用**一律直接执行**，没有审批环节 —— 服务端因此没有「挂起等回复」
//! 的状态，回合结束就是结束，手机 App 随时被杀都没关系。

use std::borrow::Cow;
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
            description: "读取文本或图片。读取文本时，输出截断到 2000 行或 50KB，被截断时末尾会写明用哪个 offset 续读；结果里的 totalLines / totalBytes 是整个文本文件的行数和字节数，据此决定继续读还是改用 bash 的 rg/sed 局部读。读取图片时（png/jpg/gif/webp/bmp）可能会缩放（offset/limit 对图片无效），结果里的 totalBytes 是原图片文件大小。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 /workspace）" },
                        "offset": { "type": "integer", "description": "起始行号，从 1 开始（可选，默认 1）" },
                        "limit": { "type": "integer", "description": "最多读取的行数（可选，默认读到截断上限）" },
                        "numbered": { "type": "boolean", "description": "给每行加 cat -n 同款行号（可选，默认关）" }
                    },
                    "required": ["path"]
                })
            },
        },
        ToolSpec {
            name: "bash",
            description: "在 Alpine Linux 沙箱里跑 bash 命令（cwd=/workspace；每次新进程，文件系统持久、shell 状态不持久）。输出超过 2000 行或 50KB 时，给**首 100 行 + [省略 N 行] + 尾 100 行**，完整输出存临时文件并返回路径，要看中间那段就去 read 它。已预装 bash/curl/git/gh/jq/rg/fd/tree/unzip/python3/diff/patch。Python 包优先 `apk add py3-*`；后台服务要重定向 stdout/stderr。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "要执行的 shell 命令" },
                        "timeout": { "type": "number", "description": "超时秒数。先估算这条命令要跑多久，上限 86400（24h）。" }
                    },
                    "required": ["command", "timeout"]
                })
            },
        },
        ToolSpec {
            name: "edit",
            description: "改文件：edits 做精确替换（一个或多个；相邻改动合并成一条，不要为了够到远处的改动而在 oldText 里塞大段没变的内容）。新建文件：只给 content（全文），文件已存在会报错。edits 与 content 互斥。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 /workspace）" },
                        "edits": {
                            "type": "array",
                            "description": "一条或多条替换（改已有文件时用）",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string", "description": "要替换的原文；必须在原文件中唯一，且不能与其他 edits[].oldText 重叠" },
                                    "newText": { "type": "string", "description": "替换成的内容" }
                                },
                                "required": ["oldText", "newText"]
                            }
                        },
                        "content": { "type": "string", "description": "新文件的全文（新建时用；父目录会按需创建，文件已存在则报错）" }
                    },
                    "required": ["path"]
                })
            },
        },
        ToolSpec {
            name: "browser",
            description: "在真浏览器（WebView）里打开网页并操作它。多标签：new_tab 开一只新的（默认操作总是落在当前标签上），list_tabs 看有哪些、当前是哪只。典型顺序：navigate → get_backbone（拿到可点的 selector 和页面结构）→ click / type → 再 get_backbone 或 get_readable 确认。读文章用 get_readable，别用 get_text（后者带导航/页脚）。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "navigate=打开 url；get_backbone=读页面结构（含可点的 selector，**这是你「看」页面的主要方式**）；get_readable=抽正文（文章页用它，比 get_text 干净得多）；get_text=读整页/某元素的文本；click=点 selector 或坐标；type=往 selector 里输入；press=按键（Enter 会提交表单、 Tab 移焦）；find_elements=列出 selector 匹配到的元素（几个、各自坐标）；scroll=滚动；screenshot=截图（full_page=true 截整页）；set_user_agent=切桌面/移动版；back=后退；wait_for_dom_stable=等动态内容安定下来；js=在页面里执行一段 JS（返回值 JSON 化后给你）；new_tab=开新标签（可带 url）；list_tabs=列出所有标签；switch_tab / close_tab=切到 / 关掉某只标签",
                            "enum": ["navigate", "get_backbone", "get_readable", "get_text", "click", "type", "press", "find_elements", "scroll", "screenshot", "set_user_agent", "back", "wait_for_dom_stable", "js", "new_tab", "list_tabs", "switch_tab", "close_tab"]
                        },
                        "url": { "type": "string", "description": "navigate / new_tab 的目标。没写协议头就按 https 补。沙箱里的文件（比如自己写的网页）用 sandbox:/workspace/…/index.html 打开" },
                        "selector": { "type": "string", "description": "CSS selector。get_backbone 给出的 sel 可以直接用" },
                        "text": { "type": "string", "description": "type 要输入的文本" },
                        "key": { "type": "string", "description": "press 要按的键", "enum": ["Enter", "Tab", "Escape", "Backspace", "Delete", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "PageUp", "PageDown", "Home", "End"] },
                        "user_agent": { "type": "string", "description": "set_user_agent 的档（会连着视口一起换，然后重载）", "enum": ["mobile", "desktop"] },
                        "full_page": { "type": "boolean", "description": "screenshot：true = 整页（上限 8000px），否则只截视口" },
                        "tab": { "type": "integer", "description": "对哪只标签操作（默认当前那只）。给了就先切过去；id 从 new_tab / list_tabs 来。close_tab 不带 tab 就是关当前这只" },
                        "x": { "type": "integer", "description": "click 的 X 坐标（和 y 一起用，代替 selector）" },
                        "y": { "type": "integer", "description": "click 的 Y 坐标" },
                        "script": { "type": "string", "description": "js 动作要执行的代码。可以是 async，允许 await 和顶层 return" },
                        "direction": { "type": "string", "description": "scroll 方向", "enum": ["up", "down"] },
                        "amount": { "type": "integer", "description": "scroll 每次滚多少像素（默认 500）" },
                        "max_depth": { "type": "integer", "description": "get_backbone 的树深度（默认 5）" },
                        "timeout": { "type": "integer", "description": "wait_for_dom_stable 等几秒（默认 10，上限 30）" }
                    },
                    "required": ["action"]
                })
            },
        },
        ToolSpec {
            name: "search",
            description: "联网搜索（Exa）：给一句自然语言描述的检索意图，拿回标题 + 链接 + 摘要片段。写检索意图而不是关键词。",
            schema: || {
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "检索意图（自然语言）" },
                        "num_results": { "type": "integer", "description": "要几条（默认 5，上限 20）" }
                    },
                    "required": ["query"]
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
    /// 用户在输入卡片里附了文件的用户消息（图片 / PDF）。
    ///
    /// 单独一个变体是为了**向后兼容**：老会话里存的是 `User(String)`，
    /// 照样能读；带附件的才有这个形状。不能把 `User` 改成带字段的结构体
    /// 变体 —— 那样老 SQLite 里的 `{"User":"..."}` 就解析不了了。
    UserFiles {
        text: String,
        #[serde(default)]
        attachments: Vec<Attachment>,
    },
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
    /// 是不是一条用户消息（带不带附件都算）。回合边界、标题、计数都靠它 ——
    /// 别在各处直接 `matches!(ChatMessage::User(_))`，会把 `UserFiles` 漏掉。
    pub fn is_user(&self) -> bool {
        matches!(self, ChatMessage::User(_) | ChatMessage::UserFiles { .. })
    }

    /// 用户消息的正文（非用户消息返回 None）。
    pub fn user_text(&self) -> Option<&str> {
        match self {
            ChatMessage::User(t) => Some(t),
            ChatMessage::UserFiles { text, .. } => Some(text),
            _ => None,
        }
    }

    /// 转成 Chat Completions 的一条消息。
    ///
    /// 带附件的 `UserFiles` 正常路径由 `build_messages` 处理（它知道模型
    /// 支不支持图片/PDF）；这里只保证单独调用时也不炸，正文里点名有哪些附件。
    fn to_openai(&self) -> Message {
        match self {
            ChatMessage::System(s) => Message::System { content: s.clone() },
            ChatMessage::User(s) => Message::User {
                content: UserContent::Text(s.clone()),
            },
            ChatMessage::UserFiles { text, attachments } => {
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
                tool_calls,
                ..
            } => Message::Assistant {
                content: (!content.is_empty()).then(|| content.clone()),
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
                    ChatMessage::User(t) => ("user", t.as_str()),
                    ChatMessage::UserFiles { text, .. } => ("user", text.as_str()),
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
  卸载不丢）。成品放这里，按任务建子目录；虚拟环境/依赖/编译产物放 /root\n\
  （重装会清空）。整个内部存储挂在 /sdcard，可读写用户文件 —— 动它之前先说明，\n\
  删除或覆盖先确认；不确定就先 `ls /sdcard`（可能没挂上）。\n\
- 给用户看成品时：写 `sandbox:` 链接，如 `[打开](sandbox:/workspace/报告/index.html)`；\n\
  图片/音频/视频都用图片语法，能渲染 `![说明](sandbox:…)`。做完主动给链接，别只写路径。\n\
- 沙箱里的 `agent` 命令可查看/操作自己：`agent state|request|tools`、\n\
  `agent command`（按键/点击/滑动/启动应用/打开 URI），`agent help` 看全部。\n\
\n\
回答要言简意赅。";

/// 回合被打断时留下的工具调用结果。
///
/// 正常情况下停止会把没跑完的调用补上结果（见 `drive`），这里是兜底：
/// 进程在两步之间被杀、或者老版本留下的会话。
const INTERRUPTED: &str = r#"{"error":"没有结果：回合被中断（用户停止或 App 被关闭）"}"#;

/// 历史 + 给没有结果的工具调用补上「被中断」。
///
/// 两家协议都要求每个 tool call 后面跟着它的结果，缺一个整个请求就 400 ——
/// 那条会话从此再也发不出消息。补的内容由历史推导、字节确定，不影响前缀缓存。
/// 绝大多数时候没有缺口，直接借用原历史，不拷贝。
fn with_tool_results(msgs: &[ChatMessage]) -> Vec<Cow<'_, ChatMessage>> {
    let mut out: Vec<Cow<'_, ChatMessage>> = Vec::with_capacity(msgs.len());
    let mut i = 0;
    while i < msgs.len() {
        out.push(Cow::Borrowed(&msgs[i]));
        let ChatMessage::Assistant { tool_calls, .. } = &msgs[i] else {
            i += 1;
            continue;
        };
        i += 1;
        let mut answered: HashSet<&str> = HashSet::new();
        while let Some(ChatMessage::Tool { tool_call_id, .. }) = msgs.get(i) {
            answered.insert(tool_call_id);
            out.push(Cow::Borrowed(&msgs[i]));
            i += 1;
        }
        for tc in tool_calls
            .iter()
            .filter(|tc| !answered.contains(tc.id.as_str()))
        {
            out.push(Cow::Owned(ChatMessage::Tool {
                tool_call_id: tc.id.clone(),
                content: INTERRUPTED.to_string(),
                images: Vec::new(),
            }));
        }
    }
    out
}

/// 历史末尾那步如果有没结果的工具调用，就地补上（回合开始前调）。
///
/// 只补**末尾**：会话是只追加落盘的（`db.rs` 的水位线），往中间插会让库和
/// 内存对不上。中间的缺口（老会话）交给 `with_tool_results` 在组请求时补。
/// 落进历史而不只是组请求时补，是为了界面上那几个工具不再一直转圈。
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
            content: INTERRUPTED.to_string(),
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
    let msgs = with_tool_results(&sess.messages);
    let mut out: Vec<Message> = Vec::with_capacity(msgs.len());
    let mut i = 0;
    while i < msgs.len() {
        // 带附件的用户消息：正文 + 图片 / PDF 内容块拼在一条消息里。
        if let ChatMessage::UserFiles { text, attachments } = &*msgs[i] {
            out.push(user_message(
                text,
                attachments,
                supports_images,
                supports_files,
            ));
            i += 1;
            continue;
        }
        if !matches!(*msgs[i], ChatMessage::Tool { .. }) {
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
            match &*msgs[i] {
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
        "[{} 返回的图片（tool_call_id: {tool_call_id}）；原文件：{}]",
        img.tool, img.source
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
pub fn tool_definitions() -> std::sync::Arc<[Tool]> {
    static DEFS: OnceLock<std::sync::Arc<[Tool]>> = OnceLock::new();
    DEFS.get_or_init(|| {
        tools()
            .iter()
            .map(|t| Tool {
                kind: "function".into(),
                function: FunctionDef {
                    name: t.name.to_string(),
                    description: t.description.to_string(),
                    parameters: (t.schema)(),
                },
            })
            .collect()
    })
    .clone()
}

/// Responses API 的工具表：**扁平**的 `{"type":"function","name":…}`。
fn tool_definitions_responses() -> std::sync::Arc<[crate::responses::Tool]> {
    static DEFS: OnceLock<std::sync::Arc<[crate::responses::Tool]>> = OnceLock::new();
    DEFS.get_or_init(|| {
        tools()
            .iter()
            .map(|t| crate::responses::Tool {
                kind: "function",
                name: t.name.to_string(),
                description: t.description.to_string(),
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
            ChatMessage::User(_) | ChatMessage::UserFiles { .. } => {
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

    let parse = |s: &str| -> Value { serde_json::from_str(s).unwrap_or_else(|_| json!(s)) };

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

            ChatMessage::UserFiles { text, attachments } => out.push(json!({
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
/// assistant 消息 —— 一个回合的多步都并在它里面。以前的做法是退回回合起点、
/// 把整个回合重发一遍：回合不限步数，100 步、每步一个 50KB 的 bash 输出，
/// 轮询一次就是几 MB（服务端拼、客户端 parse、整条消息重新渲染），越跑越慢。
///
/// 现在只退回到 `client_len` 之前**最后一步**（那一步的工具结果可能是新到的），
/// 并告诉客户端：那条 assistant 消息的前 `keep_parts` 个 part 留着，后面换成
/// 新发的。重发量和回合长度无关。
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
    let _guard = TurnGuard(core.clone(), sid.to_string());
    // 系统提示词可能被 /api/debug/system-prompt 覆盖过，所以不能直接用常量。
    // 在拿会话写锁之前先取出来，避免锁套锁。
    let system_prompt = core.state.system_prompt();
    // 不活跃的会话消息不在内存里，先从库里读回来（回合进行中不会被放掉，
    // 见 `AppState::evict_idle`）
    core.state.ensure_loaded(sid).await?;
    {
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
        // 一条消息装下正文和附件（有附件时才用 `UserFiles`，空附件保持老形状，
        // 前缀不变）。
        if attachments.is_empty() {
            sess.messages.push(ChatMessage::User(user_text.into()));
        } else {
            sess.messages.push(ChatMessage::UserFiles {
                text: user_text.into(),
                attachments,
            });
        }
        sess.maybe_title(user_text);
        sess.updated_at = now_ms();
    }
    core.state.mark_sessions_dirty();
    drive(core, provider, sid, &turn, &tx).await
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
    calls: Vec<RecordedToolCall>,
    /// 流到一半用户按了停止。已经流出来的正文/思考照样进历史，工具调用作废。
    aborted: bool,
}

/// 真正的 loop：模型 → 工具 → 模型 …
async fn drive(
    core: &Core,
    provider: &Provider,
    sid: &str,
    turn: &crate::state::Turn,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let message_id = uuid::Uuid::now_v7().to_string();
    send(tx, sse::start(&message_id)).await;

    // 不设轮数上限：任务要多少轮就跑多少轮，"干到一半停下" 不是可接受的
    // 行为。唯一的出口是模型自己不再调工具，或者用户按停止
    //（`POST /api/stop` → `abort_requested`）。
    loop {
        if turn.abort_requested() {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }
        send(tx, sse::start_step()).await;

        let step_out = match stream_one(core, provider, sid, turn, tx).await {
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

        if step.aborted {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }

        let calls = step.calls;
        if calls.is_empty() {
            // 模型不再要求工具 → 本回合结束
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
            let outcome = tokio::select! {
                o = execute_tool(core, &call.name, &call.arguments, Some((tx, &call.id))) => o,
                _ = turn.aborted() => {
                    return stop_with_pending(core, sid, tx, &calls[n..]).await;
                }
            };
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
    let stopped = json!({ "error": "用户已停止，这个工具没有执行完" });
    for call in pending {
        send(tx, sse::tool_output_available(&call.id, stopped.clone())).await;
        append(
            core,
            sid,
            ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content: stopped.to_string(),
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
    model: &str,
    supports_images: bool,
    supports_files: bool,
    effort: Option<&str>,
) -> anyhow::Result<ChatRequest> {
    let messages = build_messages(sess, supports_images, supports_files)?;
    Ok(ChatRequest {
        model: model.to_string(),
        messages,
        tools: tool_definitions(),
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
    model: &str,
    supports_images: bool,
    supports_files: bool,
    effort: Option<&str>,
) -> anyhow::Result<ResponsesRequest> {
    Ok(ResponsesRequest {
        model: model.to_string(),
        input: build_responses_input(sess, supports_images, supports_files),
        tools: tool_definitions_responses(),
        stream: true,
        store: false,
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
    for m in with_tool_results(&sess.messages) {
        match &*m {
            ChatMessage::System(s) => {
                out.push(message_item("system", MessageContent::Text(s.clone())))
            }
            ChatMessage::User(s) => out.push(message_item("user", MessageContent::Text(s.clone()))),
            ChatMessage::UserFiles { text, attachments } => out.push(message_item(
                "user",
                user_message_content(text, attachments, supports_images, supports_files),
            )),
            ChatMessage::Assistant {
                content,
                tool_calls,
                ..
            } => {
                // Responses 的 output 顺序：先正文，再每个 function_call。
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
            ChatMessage::UserFiles { attachments, .. } => {
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

/// 调一次 LLM，把 delta 转成 chunk 推出去，返回本轮产出。
async fn stream_one(
    core: &Core,
    provider: &Provider,
    sid: &str,
    turn: &crate::state::Turn,
    tx: &mpsc::UnboundedSender<Value>,
) -> anyhow::Result<StepOutput> {
    // 这个模型能不能看图 / 吃 PDF：自动判定（models.dev，带缓存 + 落盘）。
    // 在拿会话锁之前算 —— 首次判定要联网，不能拖着锁。
    let supports_images = core.state.supports_images_for(provider).await;
    let supports_files = core.state.supports_files_for(provider).await;

    // SYSTEM_PROMPT 是静态常量，**这里不做任何插值** —— 见它上面的说明。
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

    let open = async {
        let rx = match provider.transport() {
            Transport::Responses => {
                let (req, session_id) = {
                    let store = core.state.chat.read().await;
                    let sess = store
                        .get(sid)
                        .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
                    (
                        build_responses_request(
                            sess,
                            &provider.model,
                            supports_images,
                            supports_files,
                            provider.reasoning_effort.as_deref(),
                        )?,
                        sess.provider_session.clone(),
                    )
                };
                match provider.stream_responses(&req, &session_id).await {
                    Ok(rx) => rx,
                    Err(e) if e.is_unsupported_endpoint() => {
                        crate::llm::mark_responses_unsupported(provider);
                        tracing::info!(
                            "{} 不支持 Responses API，退回 Chat Completions",
                            provider.base_url
                        );
                        let (req, session_id) = {
                            let store = core.state.chat.read().await;
                            let sess = store
                                .get(sid)
                                .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
                            (
                                build_request(
                                    sess,
                                    &provider.model,
                                    supports_images,
                                    supports_files,
                                    provider.reasoning_effort.as_deref(),
                                )?,
                                sess.provider_session.clone(),
                            )
                        };
                        provider.stream_chat(&req, &session_id).await?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Transport::Chat => {
                let (req, session_id) = {
                    let store = core.state.chat.read().await;
                    let sess = store
                        .get(sid)
                        .ok_or_else(|| anyhow::anyhow!("会话已被删除，回合停止"))?;
                    (
                        build_request(
                            sess,
                            &provider.model,
                            supports_images,
                            supports_files,
                            provider.reasoning_effort.as_deref(),
                        )?,
                        sess.provider_session.clone(),
                    )
                };
                provider.stream_chat(&req, &session_id).await?
            }
        };
        Ok::<_, anyhow::Error>(rx)
    };
    // 等首包也可能很久（长上下文的预填充），这段也要能停
    let mut rx = tokio::select! {
        r = open => r?,
        _ = turn.aborted() => {
            return Ok(StepOutput {
                content: String::new(),
                reasoning: String::new(),
                calls: Vec::new(),
                aborted: true,
            });
        }
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
        let chunk = item?;
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
    // 停在半路：工具调用的参数多半是半截 JSON，一个都不能执行。
    // 已经在界面上露头的那几个，告诉前端作废了，别一直转圈。
    if aborted {
        // `calls` 里只剩有 id 和名字的，正是发过 tool-input-start 的那些
        for c in &calls {
            send(
                tx,
                sse::tool_output_available(&c.id, json!({ "error": "已停止" })),
            )
            .await;
        }
        return Ok(StepOutput {
            content: text_acc,
            reasoning: reason_acc,
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
        aborted: false,
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
        let numbered = parse_numbered(&args);

        return match core.read_any(path, offset, limit, numbered).await {
            Ok(linux::ReadResult::Text(view)) => ToolOutcome::json(json!({
                "output": view.content,
                "truncated": view.truncated,
                "totalLines": view.total_lines,
                "totalBytes": view.total_bytes,
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
            Ok(r) => {
                let text = r.to_text();
                let urls = crate::urls::extract(&text);
                let mut out = json!({
                    "output": text,
                    "exit_code": r.code,
                    "truncated": r.truncated,
                    "timed_out": r.timed_out,
                    "signal": r.signal,
                    "full_output_path": r.full_output_path,
                    "wall_time_seconds": (r.duration_ms as f64) / 1000.0,
                });
                // 输出里的网址（见 `urls` 模块）：要么用 agent-open 打开，要么在
                // 回复里给用户（用户在聊天里点）。没有就不带这个字段 —— 每条
                // 结果都会留在历史里、每一步都重发，空数组也是钱
                if !urls.is_empty() {
                    out["urls"] = json!(urls);
                }
                ToolOutcome::json(out)
            }
            Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
    }

    if name == "edit" {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 path" }));
        }

        // 两个模式：`edits` 改已有文件，`content` 新建文件。只能给一个 ——
        // 两个都给 / 都不给都报错，不做“聪明”的猜测（猜错了要么盖掉原文、
        // 要么改到错的地方）。
        let mode = match edit_mode(&args) {
            Ok(m) => m,
            Err(e) => return ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
        if let EditMode::Create(content) = mode {
            return match core.create_file(path, content).await {
                Ok(c) => ToolOutcome::json(json!({
                    "ok": true,
                    "message": format!("已创建 {path}（{} 行 / {} 字节）", c.lines, c.bytes),
                })),
                Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
            };
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

    if name == "search" {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        if query.trim().is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 query" }));
        }
        let num = args
            .get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as usize;

        return match core.search_web(query, num).await {
            Ok(found) => ToolOutcome::json(json!({
                "ok": true,
                "output": format_hits(&found),
                "count": found.hits.len(),
                "provider": found.provider,
                "truncated": false,
            })),
            Err(e) => ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };
    }

    if name == "browser" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        if action.is_empty() {
            return ToolOutcome::json(json!({ "error": "缺少参数 action" }));
        }
        // `__` 开头的是壳和核心之间的内部动作（取消、打开 URI），不给模型
        if action.starts_with("__") {
            return ToolOutcome::json(json!({ "error": format!("未知动作 {action}") }));
        }

        let res = match core.browser(&args.to_string()).await {
            Ok(v) => v,
            Err(e) => return ToolOutcome::json(json!({ "error": format!("{e:#}") })),
        };

        if res.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            let why = res
                .get("text")
                .or_else(|| res.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("浏览器动作失败");
            return ToolOutcome::json(json!({ "error": why }));
        }

        let mut text = res
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // 每一跳都把落地点报出来：模型下一步要发请求/点链接时靠它判断自己在哪
        if let Some(url) = res
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            text.push_str(&format!("\nURL: {url}"));
        }

        let mut out = ToolOutcome::json(json!({
            "ok": true,
            "output": text,
            "truncated": false,
        }));

        // 截图（只有 screenshot 动作有）：走 `read` 同一条图片管线 ——
        // 解码、缩到 2000、重新编码后落盘，所以历史里存的是压过的那份。
        if let Some(p) = res.get("image").and_then(|v| v.as_str()) {
            match browser_image(core, p).await {
                Some(img) => {
                    out.result["imagePath"] = json!(p);
                    out.images.push(img);
                }
                None => {
                    let t = out.result["output"].as_str().unwrap_or("").to_string();
                    out.result["output"] = json!(format!("{t}\n[截图没能读出来，没发给模型]"));
                }
            }
        }

        return out;
    }

    // 设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）现在**不作为工具**
    // 暴露给模型，以后收进沙箱里的一个命令、由 bash 调用。
    ToolOutcome::json(json!({ "error": format!("未知工具 {name}") }))
}

/// 搜索结果 → 模型看的文本。
///
/// 这段文字会进历史、进提示词缓存，所以**按钱算**：每条片段截到 400 字、
/// 整段封顶 4000 字。要更多就让模型自己挑一条去 curl / browser 读全文 ——
/// 把十篇全文（几十 KB）塞进历史才是真浪费。
fn format_hits(found: &crate::search::Found) -> String {
    if found.hits.is_empty() {
        return "没搜到结果。换个说法，或者用 browser 打开搜索引擎自己翻。".to_string();
    }
    let mut out = String::new();
    let mut chars = 0usize;
    for (i, h) in found.hits.iter().enumerate() {
        let snip: String = h.snippet.chars().take(400).collect();
        let item = format!("{}. {}\n   {}\n   {}\n", i + 1, h.title, h.url, snip);
        chars += item.chars().count();
        out.push_str(&item);
        if chars > 4000 {
            out.push_str("…（结果太长，已截断；调小 num_results 或换更准的说法）\n");
            break;
        }
    }
    out
}

/// 浏览器截图 → 挂到工具结果上。
///
/// Kotlin 把截图落在沙箱里的 `/root/.browser/…`，这里按**沙箱路径**读进来
/// （`read_any` 会把它映射到 rootfs），复用 `read` 的图片管线：解码 → 缩到
/// 2000 → 重编码 → 落盘。返回 `None` 表示这张图没拿到（调用方在文本里标明）。
async fn browser_image(core: &Core, guest_path: &str) -> Option<ImagePart> {
    let img = match core.read_any(guest_path, 0, None, false).await {
        Ok(crate::linux::ReadResult::Image(img)) => img,
        Ok(_) => {
            tracing::warn!("浏览器截图不是图片: {guest_path}");
            return None;
        }
        Err(e) => {
            tracing::warn!("读浏览器截图失败 {guest_path}: {e:#}");
            return None;
        }
    };

    let supports_images = core
        .state
        .llm()
        .map(|p| core.state.cached_supports_images(&p))
        .unwrap_or(true);
    if !supports_images {
        return None;
    }

    let mime = img.mime.clone();
    match core.save_image(img.bytes, mime.clone()).await {
        Ok(stored) => Some(ImagePart {
            path: stored.display().to_string(),
            mime,
            source: guest_path.to_string(),
            tool: "browser".to_string(),
        }),
        Err(e) => {
            tracing::warn!("浏览器截图落盘失败: {e:#}");
            None
        }
    }
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
        return ToolOutcome::json(json!({
            "output": output,
            "imagePath": guest_path,
            "imageMime": mime,
            "totalBytes": img.source_bytes,
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
                "totalBytes": img.source_bytes,
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
                "totalBytes": img.source_bytes,
                "truncated": false,
            }))
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

    /// 停在一批工具中间（或进程被杀）之后，下一轮请求里每个 tool call
    /// 都必须有结果 —— 缺一个，provider 就 400，这条会话再也发不出去。
    #[test]
    fn every_tool_call_has_a_result_even_after_an_interrupted_turn() {
        let mut sess = ChatSession::default();
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("跑三个".into()));
        sess.messages.push(ChatMessage::Assistant {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![call("c1"), call("c2"), call("c3")],
        });
        sess.messages.push(tool("c1"));
        // c2、c3 没有结果；后面已经又来了一句（老会话的形状）
        sess.messages.push(ChatMessage::User("继续".into()));

        let w = wire(&sess);
        let ids: Vec<&str> = w
            .iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| m["tool_call_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["c1", "c2", "c3"]);
        assert_eq!(
            w.last().unwrap()["role"],
            "user",
            "补的结果要紧跟在那批调用后面"
        );

        let r =
            serde_json::to_value(build_responses_request(&sess, "m", true, true, None).unwrap())
                .unwrap();
        let outs = r["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["type"] == "function_call_output")
            .count();
        assert_eq!(outs, 3);
    }

    #[test]
    fn a_dangling_tail_is_closed_in_the_history_itself() {
        let mut sess = ChatSession::default();
        sess.messages.push(ChatMessage::User("跑".into()));
        sess.messages.push(ChatMessage::Assistant {
            content: String::new(),
            reasoning: String::new(),
            tool_calls: vec![call("c1"), call("c2")],
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
        let req1 = serde_json::to_value(
            build_request(&sess, "deepseek-v4.1-flash", true, true, None).unwrap(),
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
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "ok".into(),
            images: vec![],
        });
        sess.messages.push(ChatMessage::User("第二问".into()));
        let req2 = serde_json::to_value(
            build_request(&sess, "deepseek-v4.1-flash", true, true, None).unwrap(),
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
        ensure_system_prompt(&mut sess, SYSTEM_PROMPT);
        sess.messages.push(ChatMessage::User("嗨".into()));

        let chat =
            serde_json::to_value(build_request(&sess, "m", true, true, None).unwrap()).unwrap();
        assert!(chat.get("reasoning_effort").is_none(), "{chat}");
        let resp =
            serde_json::to_value(build_responses_request(&sess, "m", true, true, None).unwrap())
                .unwrap();
        assert!(resp.get("reasoning").is_none(), "{resp}");

        let chat =
            serde_json::to_value(build_request(&sess, "m", true, true, Some("high")).unwrap())
                .unwrap();
        assert_eq!(chat["reasoning_effort"], "high");
        let resp = serde_json::to_value(
            build_responses_request(&sess, "m", true, true, Some("low")).unwrap(),
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
        sess.messages = vec![ChatMessage::UserFiles {
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
            ChatMessage::User("U1".into()),
            ChatMessage::Assistant {
                content: "答1".into(),
                reasoning: String::new(),
                tool_calls: vec![],
            },
            ChatMessage::UserFiles {
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
        // 设备操作（读界面 / 点 / 输入 / 按键 / 启动应用）暂时**不作为工具**
        // 暴露给模型，以后收进沙箱里的一个命令、由 bash 调用。
        let names: Vec<&str> = tools().iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["read", "bash", "edit", "browser", "search"]);
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
        let twice =
            serde_json::to_string(&tools().iter().map(|t| (t.schema)()).collect::<Vec<_>>())
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
    fn bash_timeout_is_required_and_capped() {
        // 缺失 / 非法都不能偷偷用一个短默认值
        assert!(parse_timeout(&json!({})).is_none());
        assert!(parse_timeout(&json!({ "timeout": null })).is_none());
        assert!(parse_timeout(&json!({ "timeout": 0 })).is_none());
        assert!(parse_timeout(&json!({ "timeout": -5 })).is_none());
        assert!(parse_timeout(&json!({ "timeout": "600" })).is_none());

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
            ChatMessage::Assistant {
                content: "先看看".into(),
                reasoning: "想1".into(),
                tool_calls: vec![call("c1")],
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
            },
            ChatMessage::User("再问".into()),
            ChatMessage::Assistant {
                content: "再答".into(),
                reasoning: String::new(),
                tool_calls: vec![],
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
            };
        let mut sess = ChatSession::new();
        sess.messages = vec![
            ChatMessage::System("sys".into()),
            ChatMessage::User("U1".into()),
            asst("先看看", "想", vec![call("c1"), call("c2")]),
            tool("c1"),
            tool("c2"),
            // 什么都没有的一步：视图里不出现，也不算步数
            asst("", "", vec![]),
            asst("", "再想", vec![call("c3")]),
            tool("c3"),
            asst("答1", "", vec![]),
            ChatMessage::System("新提示词".into()),
            ChatMessage::User("U2".into()),
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
            .map(|i| ChatMessage::User(format!("命中 {i}")))
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
        s.messages = vec![ChatMessage::User(format!(
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
            ChatMessage::User("一".into()),
            ChatMessage::User("二".into()),
        ];
        s.evict();
        assert!(!s.resident());
        assert_eq!((s.message_count(), s.user_count()), (3, 2));
        assert!(s.has_content());

        s.install(vec![ChatMessage::User("一".into())]);
        assert!(s.resident());
        assert_eq!(s.message_count(), 1);
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
