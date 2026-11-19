//! Agent 主循环。
//!
//! 决策 D5 选了「自研薄 loop」而不是 `rig`，原因就在这个文件里：
//! 每一个事件都要精确地转成 AI SDK UI Message Stream 的 chunk 推给前端。
//! 框架会把 loop 包起来，反而挡路。
//!
//! 工具调用**一律直接执行**，没有审批环节 —— 服务端因此没有「挂起等回复」
//! 的状态，回合结束就是结束，手机 App 随时被杀都没关系。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
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

/// 工具表每个（平台 × 角色）只建一次。
///
/// 每个 step 都要把整张表序列化进请求（含那段很长的 `bash` 描述），
/// 工具表是**可缓存前缀的一部分**，只建一次也让「字节确定」更保险。
///
/// 索引是 `平台 * 2 + 角色槽位`（见 `Role::slot`）：同一个平台下 worker 和 router
/// 各一份，两份都是从同一个 `Platform` 拼出来的固定字节。
pub fn tools(p: Platform, role: Role) -> &'static [ToolSpec] {
    static TOOLS: [OnceLock<Vec<ToolSpec>>; Platform::SLOTS2] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    TOOLS[p.slot2(role.slot())].get_or_init(|| match role {
        Role::Worker => build_tools(p),
        Role::Router => build_router_tools(p),
    })
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

/// Router 的工具表：**只有编排，没有执行**。
///
/// 这里没有 `bash` / `read` / `edit`，是故意的 —— 也是双层架构成不成立的关键：
/// Router 一旦能执行，就会开始自己干活，然后变成「另一个 Worker」，两层就退化成
/// 一层。靠提示词写「你别自己干」拦不住（DeepSeek 那篇的消融实验就是反面教材：
/// 中性提示下模型 5 次运行一次都不委派）；靠工具表拦得住，因为它根本没那手。
///
/// 五个工具：**看**（`list_sessions` / `read_session`）、**说**（`dispatch`）、
/// **定时**（`schedule_task` / `schedules`）。
/// `answer` 故意没有 —— 正文就是给用户看的话。
///
/// 「定时」那两个是唯一**让 Router 影响未来**的工具（前三个都只作用于当下这一轮）。
/// 它们只往定时器里写一条待办，到点还是投回这里 —— 指挥系统始终只有一套。
fn build_router_tools(_p: Platform) -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "list_sessions",
            description: prompt::list_sessions_description(),
            schema: prompt::list_sessions_schema,
        },
        ToolSpec {
            name: "read_session",
            description: prompt::read_session_description(),
            schema: prompt::read_session_schema,
        },
        ToolSpec {
            name: "dispatch",
            description: prompt::dispatch_description(),
            schema: prompt::dispatch_schema,
        },
        // 定时任务（见 `schedule`）。**两个不是三个**：`schedule_task` 是高频路径，
        // 剩下三个低频 CRUD 收进一个多动作工具 —— 工具面从 3 涨到 5 而不是 6，
        // 而「选错工具」的概率跟工具数一起涨。
        ToolSpec {
            name: "schedule_task",
            description: prompt::schedule_task_description(),
            schema: prompt::schedule_task_schema,
        },
        ToolSpec {
            name: "schedules",
            description: prompt::schedules_description(),
            schema: prompt::schedules_schema,
        },
    ]
}

// ---------------------------------------------------------------------------
// 会话状态
// ---------------------------------------------------------------------------

/// 一条用户角色消息的来源。
///
/// 只做一件事：让代码分得清「这句是人说的」和「这句是子会话的回执」。
/// 它**不进请求体**（`to_openai` / `build_responses_input` 都不读），
/// 所以对 provider 零风险。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// 真人在输入框里打的（也包括 `/api/chat` 来的、脚本发的）
    #[default]
    Human,
    /// **派活**：Router 用 `dispatch` 把这一句投进这条会话（见 `router::deliver`）。
    ///
    /// 和 `Human` 分开是为了一个具体的事：`conclusion`（交付物）只该由**派活驱动的
    /// 回合**更新。用户自己跑去 worker 会话里插一句话，不该把上一份交付物冲掉。
    ///
    /// 对模型来说两者没区别（都是 `user` 角色、原样发出去）—— 它看到的就是
    /// 「用户要我干这个」。
    Dispatch,
    /// 子会话回合结束时推回来的回执（见 `router::report_back`）
    WorkerReport { session_id: String },
    /// **定时器**到点投给 Router 的那句话（见 `schedule`）。
    ///
    /// 为什么不复用别的三个：`Human` 会让一句三天前写下的话取得「用户刚才说过」
    /// 的身份（Router 的提示词里明写着「用户是否同意，以用户在这个对话里说的为准」）；
    /// `Dispatch` 会翻转「这一轮算不算派活」那个判定，进而影响 `conclusion` 的
    /// 覆盖；`WorkerReport` 带 `session_id`、前端会画成「子会话报告」。
    ///
    /// 它只出现在 Router 的会话里（投递目标只有 Router，见 `schedule::fire`）。
    Schedule { task_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatMessage {
    System(String),
    /// 用户消息。`attachments` 是在输入卡片里附的图片 / PDF（没附就是空的）。
    User {
        text: String,
        attachments: Vec<Attachment>,
        /// 这句话是谁送来的。默认 `Human`（老库没有这个字段，`serde(default)` 兜着）。
        ///
        /// **它不发给模型**：`to_openai` / `build_responses_input` 都不读它，
        /// 所以对任何 provider 都是零风险（不需要靠 provider 宽容地忽略未知字段）。
        /// 它的作用是让**代码**分得清「这句话是人说的」还是「这句是子会话的回执」，
        /// 前端也因此不再把回执画成用户气泡。
        ///
        /// 要让**模型**也分得清，得靠正文里的结构化标记（见 `router::report_back`
        /// 拼的 `<worker_report>` 块）—— 那是两件不同的事，这个字段替代不了它。
        #[serde(default)]
        origin: Origin,
    },
    Assistant {
        /// 正文 / 思考（可能几十 KB）。和 `Tool::content` 同理：每一步都会把整段
        /// 历史重新组装进请求，`Arc<str>` 让后续每一步只加引用计数。
        content: std::sync::Arc<str>,
        reasoning: std::sync::Arc<str>,
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
        /// 工具结果正文（bash 输出可能有几十 KB）。用 `Arc<str>`：同一条消息会在
        /// **之后每一步**被重新组装进请求，克隆一次 Arc 比每步 memcpy 整段便宜得多。
        content: std::sync::Arc<str>,
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
            origin: Origin::Human,
        }
    }

    /// 一条**带来源**的用户消息（子会话的回执用它，见 `router::report_back`）。
    pub fn from_worker(text: impl Into<String>, session_id: impl Into<String>) -> Self {
        ChatMessage::User {
            text: text.into(),
            attachments: Vec::new(),
            origin: Origin::WorkerReport {
                session_id: session_id.into(),
            },
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
            ChatMessage::User {
                text, attachments, ..
            } => {
                let mut body = text.clone();
                for a in attachments {
                    let _ = write!(body, "\n[附件：{}（{}）]", a.name, a.mime);
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

/// 会话在双层架构里的角色（见 docs/16-super-agent.md）。
///
/// - `Worker`：干活的那一层。上下文隔离、挂完整执行工具 —— 就是以前唯一的那个东西。
/// - `Router`：协调那一层。承接人类交互、把任务派给 Worker，**不执行具体任务**。
///   靠工具表把它锁住（没有 bash / read / edit），不是靠提示词约束。
///
/// 为什么是一个枚举而不是一个 bool：以后还可能有第三种（比如只看不写的审阅层），
/// 而 `role` 要落盘、要进缓存前缀的选取，多一种就是改一处。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Role {
    /// 干活：完整工具表 + 普通提示词
    #[default]
    Worker,
    /// 协调：只有编排工具 + router 提示词
    Router,
}

impl Role {
    /// 库里存成什么字串。改字串 = 改盘上格式，得配一次迁移。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Router => "router",
        }
    }

    /// 从前缀兼容地读回来：不认识的一律当 Worker。
    ///
    /// **不能报错**：装回旧版本、或者以后加了新角色时旧二进制读到它，一个以
    /// 前能打开的会话不该直接打不开。当 Worker 至少是「跟以前一样」。
    pub fn from_db(s: &str) -> Self {
        match s {
            "router" => Self::Router,
            _ => Self::Worker,
        }
    }

    /// 工具表 / 提示词的槽位下标（见 `tools` / `tool_definitions` 那些 `OnceLock` 数组）。
    pub const fn slot(self) -> usize {
        match self {
            Self::Worker => 0,
            Self::Router => 1,
        }
    }
}

/// 一件派出去的活：谁派的、当初要它干什么、派过几次。
///
/// **要么整份有、要么整份没有**（`ChatSession::dispatch: Option<Dispatch>`）——
/// 以前是三个独立的 `Option`/计数，能写出「有 brief 没 by」这种半份状态，
/// 而迁移恰好会产生那种数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dispatch {
    /// 派它的那个 Router（会话 id）。回合结束时靠它找「这轮结果该告诉谁」。
    pub by: String,
    /// 当初要它干什么（原文，一字不改）。
    ///
    /// ## 为什么这行字要单独存，而不是只留在历史里
    ///
    /// 历史会被压缩、会被 `evict` 掉，压缩之后「当初到底要它干什么」就变成一个
    /// 模糊的摘要了 —— 而这句话是**验收的合同**：判断它干得对不对，靠的是原话，
    /// 不是摘要。这一行独立存在，所以免疫压缩。
    pub brief: String,
    /// 派过几次（同一个会话被补充要求会 +1）。
    pub times: u32,
}

impl Dispatch {
    /// 开一份新的派活记录。
    pub fn new(by: impl Into<String>, brief: impl Into<String>) -> Self {
        Self {
            by: by.into(),
            brief: brief.into(),
            times: 1,
        }
    }

    /// 补充要求：**追加**到 `brief` 末尾，不覆盖原文（那是验收合同）。
    ///
    /// **不动 `times`** —— 派工次数由 `AppState::redispatch_session` 统一 +1
    /// （那里是「再派一次」这个动作的唯一入口）。两处都加就会加两遍。
    pub fn append(&mut self, addition: &str) {
        self.brief.push_str("\n\n---\n\n（补充要求）\n");
        self.brief.push_str(addition);
    }
}

/// 一条派出去的会话「现在什么处境」。**推导出来的，不存。**
///
/// 两个来源合起来算：`AppState::turns`（回合真的在跑）和 `AppState` 的
/// 排队队列（派了还没开跑）。两者都不是 —— 那就是`Idle`。
///
/// 以前这份推导在 `http.rs` 和 `router.rs` 各写一遍，**优先级还是相反的**
/// —— 同一个会话在两个地方会得出不同的标签。现在只有 [`ChatSession::standing`] 一处。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// 回合真的在跑
    Running,
    /// 没在跑，但派了活还没轮到（排队等着交给模型）
    Queued,
    /// 没在跑、没排队 —— 它停下了，等人看一眼
    Idle,
}

impl Standing {
    /// 要不要人来处理。
    ///
    /// **`Queued` 不算**：活刚派下去、正等着开跑，那不是「等你」，是「等它」。
    /// 旧架构里 `Stage::needs_person()` 对 `Queued` 也是 false（保持一致）。
    pub const fn needs_person(self) -> bool {
        matches!(self, Self::Idle)
    }

    /// 机器读的状态名（HTTP 字段 `state`）
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "working",
            Self::Queued => "queued",
            Self::Idle => "idle",
        }
    }

    /// 给人看的一句话
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "在跑",
            Self::Queued => "排队中",
            Self::Idle => "闲着",
        }
    }
}

/// 当前会话。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    /// **给人看的短编号**（`#7`）。`None` = 还没有号（老数据，`load()` 里补）。
    ///
    /// 为什么必须**存**而不是算：
    ///
    /// - 从 `Vec` 的下标算 → 删掉一条后面全错位，同一个 `#7` 昨天指 A 今天指 B。
    /// - 从 SQLite 的 `rowid` 算 → `sessions` 表的主键是 `id TEXT`，rowid 是隐式的
    ///   `max(rowid)+1`；**删掉最大的那条之后，新插入的行会复用同一个 rowid**
    ///   （`db.rs` 的 `DELETE FROM sessions` 真的会发生）。复用比没有更糟。
    /// - 从 `created_at` 后几位算（上一版干过）→ 同一毫秒建的两条会撞。
    ///
    /// 所以发一个号、存下来、**永不回收**（删掉 `#7`，`#7` 就永久空缺）。
    /// 唯一的写点是 [`SessionStore::push_new`]（受 `chat` 写锁保护）。
    ///
    /// 它也**不是状态**：没有跃迁规则，写一次就不再改 —— 和 `brief` / `conclusion`
    /// 那类「不可变记录」同性质。
    #[serde(default)]
    pub seq: Option<u64>,
    /// 标题：取第一条用户输入的前若干字（Claude 的侧边栏就是这样）
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    /// 这个会话在双层架构里是哪一层（见 `Role`）。
    ///
    /// **它决定提示词和工具表**，所以是缓存前缀的一部分：建会话那一刻定下来，
    /// 之后不改（换角色就是换会话）。跟着会话走、落盘（`db.rs` 的 `role` 列）。
    pub role: Role,
    pub messages: Vec<ChatMessage>,
    /// 给 provider 做 prompt cache 路由用的稳定会话 ID
    pub provider_session: String,
    /// 派活信息。`None` = **这条会话不是被派的**（用户在输入框里自己开的）。
    ///
    /// 三个字段以前是三个平铺的 `Option`（`brief` / `dispatched_by` / `dispatches`），
    /// 于是「有 brief 没 by」「有 by 没 brief」这些写一半的状态都能表示 ——
    /// 而 `db.rs` 的迁移恰好会产生那种数据（老 ticket 缺列）。收成一个 `Option`
    /// 之后，**要么整份有、要么整份没有**，类型上就写不出半份。
    pub dispatch: Option<Dispatch>,
    /// 最近一次回合结束时它说的那段话（给人看的一句话结论）。
    ///
    /// 和 `brief` 同理：历史压缩之后，「它最后说成了什么」不能靠摘要。
    /// 空串 = 还没跑过回合。
    pub conclusion: String,
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

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl ChatSession {
    pub fn new() -> Self {
        Self::with_role(Role::Worker)
    }

    pub fn with_role(role: Role) -> Self {
        Self::with_id(role, uuid::Uuid::now_v7().to_string())
    }

    /// 同 [`Self::with_role`]，但**指定 id**。
    ///
    /// 只在一处用：Router 被删之后**用原来的 id 重建**（`router::ensure`）——
    /// 派出去的会话记着 `dispatched_by = <旧 Router id>`，换个新 id 就等于
    /// 把它们全变成孤儿（新的 Router 一条都看不到）。复用旧 id 是让那层归属
    /// 关系在重建之后仍然成立的最省事的办法。
    ///
    /// 注意 id 是**会话身份**，不是内容：这条重建出来的会话历史是空的，
    /// 只是**名字没变**。所以「之前派过什么」仍然查得到（那些会话还在，
    /// `dispatched_by` 仍然指着这个名字），而「之前聊过什么」是真的没了。
    pub fn with_id(role: Role, id: String) -> Self {
        let now = now_ms();
        Self {
            id,
            // 号由 `SessionStore::push_new` 发（那里才拿得到全局水位线）
            seq: None,
            title: String::new(),
            created_at: now,
            updated_at: now,
            role,
            messages: Vec::new(),
            provider_session: new_provider_session(),
            dispatch: None,
            conclusion: String::new(),
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

    /// 这条会话现在什么处境。**唯一**的推导入口（见 [`Standing`]）。
    ///
    /// `busy` 从 `AppState::turns` 来（回合真的在跑）；`queued` 从排队队列来
    /// （派了、还没轮到）。两个都由调用方传进来，因为这个类型不持有 `AppState`。
    ///
    /// 优先级：**在跑 > 排队 > 闲着**。一条会话永远可以继续，所以没有终态。
    pub fn standing(&self, busy: bool, queued: bool) -> Standing {
        if busy {
            return Standing::Running;
        }
        if queued {
            return Standing::Queued;
        }
        Standing::Idle
    }

    /// 派这条会话的 Router（`None` = 不是派出去的）。
    pub fn dispatched_by(&self) -> Option<&str> {
        self.dispatch.as_ref().map(|d| d.by.as_str())
    }

    /// 当初的要求原文（`None` = 不是派出去的）。
    pub fn brief(&self) -> Option<&str> {
        self.dispatch.as_ref().map(|d| d.brief.as_str())
    }

    /// 派过几次（`None` = 不是派出去的）。
    pub fn dispatch_times(&self) -> Option<u32> {
        self.dispatch.as_ref().map(|d| d.times)
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
///
/// ## 为什么不是「一律小写化再比」
///
/// 搜索是**在会话锁里**跑的（`AppState::search` 由 `http::search_sessions` 拿着
/// 读锁调），而抽屉那边 180ms 防抖、打字快就是背靠背几次。原实现每条消息都
/// 展开三份 `Vec<char>`（原文 / 小写 / 来源下标）再逐窗口比：省库 40MB 正文实测
/// 一次搜索 193ms，手机上还要乘几倍，这期间任何回合追加消息都得等。
///
/// 现在按**查询词本身**分派（见 `find_fold` / `fold_kind`），只有真需要折叠时
/// 才付代价：没有大小写的查询走字面 `str::find`（memmem，不分配）；纯 ASCII 的
/// 走字节级折小写；只有带非 ASCII 大小写的查询（`Ａ` / `Σ` / `İ`）才逐字符折叠。
/// 任何一条路都不为整段历史建 `Vec<char>`。
fn hit_snippet(text: &str, needle: &str) -> Option<String> {
    const BEFORE: usize = 20;
    const AFTER: usize = 60;

    // 命中位置用 (起, 止) 两个**字符**下标表示
    let (at, last) = find_fold(text, needle)?;

    let start = at.saturating_sub(BEFORE);
    let end = last + 1 + AFTER;

    // 单趟、只走到窗口末尾：不把整条消息 `collect` 成 `Vec<char>`，也不先
    // `chars().count()` 数一遍全长（一条消息可能几十 KB，命中一下就要一份
    // 完整的字符表）。`end` 不用先 clamp —— 按 `i < end` 收就行。
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    let mut more = false;
    for (i, c) in text.chars().enumerate() {
        if i >= end {
            more = true;
            break;
        }
        if i >= start {
            // 换行拍平成空格，否则摘要里会带出多行
            s.push(if c == '\n' { ' ' } else { c });
        }
    }
    if more {
        s.push('…');
    }
    Some(s)
}

/// `hit_snippet` 的定位部分：返回命中区间的首尾**字符**下标（不是字节）。
///
/// 语义以**折叠后的第一次出现**为准（原实现就是这样：整段小写化后取第一个
/// `windows` 命中）。这和「字面 `str::find`」不是一回事 —— `… 1aY … A …` 里
/// 查 `A`，折叠后的第一次出现在前面那个 `a` 上。所以字面命中**不能**直接当
/// 答案，它只是快速候选（随机对拍抓到过这一点）。
///
/// 分两层，第二层只在必要时走（省库 40MB 正文 --release 实测：常见查询
/// 2–30ms，原来一律 200ms 以上）：
///
/// 1. **字节级 ASCII 折叠**（`ascii_fold_find`）：只折 ASCII 字母，非 ASCII
///    字节按原样比。什么情况下能定案？
///    - 查询**纯 ASCII** —— 它只能匹配 ASCII 串，字节级就是完备的；
///    - 查询**不含任何大小写字符**（中文、标点、emoji）—— 折叠不改变任何
///      字符，字面 `str::find` 就是答案（`str::find` 走 memmem，最快）；
///    - 其余（查询里有全角字母、`Σ`、`İ` 这类）才需要精确折叠。
/// 2. **精确折叠**（`exact_fold_find`）：逐 `char` 算 `to_lowercase`，
///    能把 `İ` → `i̇`（一个变两个）、`Ａ` → `ａ` 算对。
///
/// `pub(crate)`：`router` 的短名候选提示和 `list_sessions` 的 `query` 搜索都要它
/// （同一套大小写不敏感语义 —— 两处各写一份的话，「搜得到但提示里没有」这种
/// 不一致迟早出现）。
pub(crate) fn find_fold(text: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    match fold_kind(needle) {
        // 查询没有大小写：字面命中就是折叠后的第一次出现
        FoldKind::None => {
            let at = text.find(needle)?;
            let start = text[..at].chars().count();
            Some((start, start + needle.chars().count() - 1))
        }
        FoldKind::Ascii => {
            ascii_fold_find(text.as_bytes(), needle.as_bytes()).map(|(from, len)| {
                let b = text.as_bytes();
                // 字节下标 → 字符下标：数「非续字节」（UTF-8 里一个字符一个）
                let at = b[..from].iter().filter(|&&x| x & 0xC0 != 0x80).count();
                let n = b[from..from + len]
                    .iter()
                    .filter(|&&x| x & 0xC0 != 0x80)
                    .count();
                (at, at + n.saturating_sub(1))
            })
        }
        FoldKind::Unicode => exact_fold_find(text, needle),
    }
}

/// 查询词需要哪种折叠（见 `find_fold`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FoldKind {
    /// 一个大小写字符都没有：折叠不改变任何字符，字面查找就够
    None,
    /// 只有 ASCII 字母的大小写差异：字节级折叠完备
    Ascii,
    /// 有非 ASCII 的大小写字符（`Ａ` / `Σ` / `İ`…）：必须精确折叠
    Unicode,
}

/// 一个字符有没有大小写（大写、小写都推出它自己）。
///
/// 用「`to_lowercase`/`to_uppercase` 是不是原样返回」判，而不是 `is_uppercase`
/// 之类：后者对 `Σ`、`ǅ`（标题大小写）、切罗基文那些不完整。
fn caseless(c: char) -> bool {
    c.to_lowercase().eq(std::iter::once(c)) && c.to_uppercase().eq(std::iter::once(c))
}

fn fold_kind(needle: &str) -> FoldKind {
    if needle.chars().all(caseless) {
        // 折叠管不到它。注意「折成别的无大小写字符」的情况不存在（穷举过
        // BMP：无大小写的字符折叠后还是无大小写字符）
        return FoldKind::None;
    }
    if needle.is_ascii() {
        return FoldKind::Ascii;
    }
    FoldKind::Unicode
}

/// 在 `hay` 里按「ASCII 字母不分大小写、其余字节原样」找 `needle`。
/// 返回 `(起始字节下标, 长度)`。
///
/// 只看首字节的候选位置再逐个比：不命中时比逐位置开窗快得多。`needle` 是
/// 合法 `&str`，首字节一定不是续字节，所以命中的起点一定落在字符边界上。
fn ascii_fold_find(hay: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    let n = needle.len();
    if n == 0 || hay.len() < n {
        return None;
    }
    let first = needle[0].to_ascii_lowercase();
    let mut i = 0usize;
    while i + n <= hay.len() {
        let start = i + hay[i..=hay.len() - n]
            .iter()
            .position(|&b| b.to_ascii_lowercase() == first)?;
        if hay[start..start + n]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some((start, n));
        }
        i = start + 1;
    }
    None
}

/// 真正的 Unicode 小写化匹配（慢，但能把 `İ`、全角字母这类算对）。
///
/// 窗口里每一项是「小写字符 + 它来自原文第几个字符」：小写化可能把一个字符
/// 变成两个（`İ` → `i̇`），所以不能假设一个原文字符对应一个窗口项。用定长
/// `VecDeque` 当环 —— 满了弹头推尾，不复位整段。
fn exact_fold_find(text: &str, needle: &str) -> Option<(usize, usize)> {
    let pat: Vec<char> = needle.to_lowercase().chars().collect();
    if pat.is_empty() {
        return None;
    }
    let mut run: std::collections::VecDeque<(char, usize)> =
        std::collections::VecDeque::with_capacity(pat.len());
    for (i, c) in text.chars().enumerate() {
        for l in c.to_lowercase() {
            if run.len() == pat.len() {
                run.pop_front();
            }
            run.push_back((l, i));
            if run.len() == pat.len() && run.iter().map(|(c, _)| *c).eq(pat.iter().copied()) {
                return Some((run.front()?.1, run.back()?.1));
            }
        }
    }
    None
}

/// 多个会话。侧边栏的「最近对话」/ 搜索结果都从它来。
///
/// **没有 `active` 字段**（2026-11 删的）。「用户眼前看的是哪条会话」归界面管
/// （地址栏 `/s/<id>`），服务端不该存一份自己的答案 —— 以前那份 `active` 会跟
/// 前端打架：多端同时开着时互相抢，`agentctl` 还得手动把用户那条切回去。
///
/// 现在需要「不指定 id 时用哪条」的地方（`agentctl --session active`、
/// `/api/reset`、`/api/debug/request`、以及下次启动恢复现场）统一走
/// `AppState::last_session`，那是一份**纯提示**：可以被显式写，但任何操作都
/// 不会顺手改它。
#[derive(Default, Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: Vec<ChatSession>,
    /// 下一个可用的短编号（进程内的发号水位线）。
    ///
    /// **不落库**：它从每条会话的 `seq` 恢复（`seq` 才是事实），见
    /// [`Self::recover_seq`]。落库就成了第二个真相，两者迟早对不上。
    #[serde(skip)]
    high_seq: u64,
}

impl SessionStore {
    /// 把会话放进表里，**并给它发一个号**（`seq`）。
    ///
    /// **这是新会话进入 store 的唯一入口**，也是 `seq` 的唯一写点。号在
    /// `chat` 写锁保护下发（调用方都拿着写锁），所以不存在「读 max + 1」的竞态。
    pub fn push_new(&mut self, mut s: ChatSession) -> String {
        let id = s.id.clone();
        // 已经带了号（Router 用旧 id 重建时从旧记录抄过来的那种）就不再发
        if s.seq.is_none() {
            s.seq = Some(self.next_seq());
        }
        self.sessions.push(s);
        id
    }

    /// 下一个号。**只增不减**（删掉的号永不回收）。
    fn next_seq(&mut self) -> u64 {
        self.high_seq = self.high_seq.max(self.max_seq()).saturating_add(1);
        self.high_seq
    }

    fn max_seq(&self) -> u64 {
        self.sessions
            .iter()
            .filter_map(|s| s.seq)
            .max()
            .unwrap_or(0)
    }

    /// 从会话表里已有的号恢复发号水位线（`load()` 之后调一次），并**给没号的会话补号**
    /// （老数据、`seq` 列加进来之前建的）。
    ///
    /// 补号顺序是 `created_at`（再拿 id 兜底），于是老会话的号大致等于它们建出来的
    /// 先后 —— 不是精确的历史顺序，但这批号本来就是补救的，**连续不重复**才是要求。
    ///
    /// 返回**补过号的那几条 id**。调用方（`Db::load`）要用它把落盘水位线里的
    /// `seq` 改回 `None` —— 否则库里的行还是 NULL、而水位线说「序号已经有了」，
    /// **补的号永远写不进库**（每次重启都重新补一遍，号还可能不一样）。
    pub fn recover_seq(&mut self) -> Vec<String> {
        self.high_seq = self.max_seq();
        // 没号的按创建时间排，一条一条接着发
        let mut missing: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.seq.is_none())
            .map(|(i, _)| i)
            .collect();
        if missing.is_empty() {
            return Vec::new();
        }
        missing.sort_by(|&a, &b| {
            let (x, y) = (&self.sessions[a], &self.sessions[b]);
            x.created_at
                .cmp(&y.created_at)
                .then_with(|| x.id.cmp(&y.id))
        });
        let mut assigned = Vec::with_capacity(missing.len());
        for i in missing {
            let n = self.next_seq();
            self.sessions[i].seq = Some(n);
            assigned.push(self.sessions[i].id.clone());
        }
        assigned
    }

    /// 找一条会话的号（`None` = 它没有 id 也没有号）。
    pub fn by_seq(&self, seq: u64) -> Option<&ChatSession> {
        self.sessions.iter().find(|s| s.seq == Some(seq))
    }

    /// 当前会话，没有就建一个
    pub fn get(&self, id: &str) -> Option<&ChatSession> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ChatSession> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// 建一条空会话，返回它的 id。**不改任何「当前会话」**（见上面的说明）。
    pub fn create(&mut self) -> String {
        self.push_new(ChatSession::new())
    }

    /// 删掉并交还这个会话（调用方要清它引用的文件）。
    pub fn delete(&mut self, id: &str) -> Option<ChatSession> {
        let at = self.sessions.iter().position(|s| s.id == id);
        at.map(|i| self.sessions.remove(i))
    }

    /// 给侧边栏用的列表，**按创建时间**排（新建的在最上面）。
    ///
    /// 以前按 `updated_at`（最近聊过的在前），结果是**正在说话的会话会在列表里往上
    /// 跳**：两个会话各跑一个回合时，它们来回换位 —— 抽屉开着就能看见列表在动，
    /// 点错是迟早的事。创建时间不会变，位置就是死的（新建的插到最上面，其余不动）。
    ///
    /// **空会话不进列表**：前端点「新对话」不同，那是本地清空视图、服务端
    /// 到第一句话才建会话（见 `POST /api/chat` 的 `fresh`）。这里是兜底 ——
    /// 调试端/CLI 直接 `POST /api/sessions` 建的、`/api/reset` 清空后的、
    /// 以及 `/api/reset` 清空后的，都是没内容的，不该在抽屉里占位。
    pub fn list(&self) -> Vec<Value> {
        self.list_page(None, 0).0
    }

    /// 侧边栏的一页会话（游标分页），返回 `(这一页, 后面还有没有, 下一页的游标)`。
    ///
    /// 为什么分页：一条 157 字节上下，一千条会话就是 150 KB —— 而抽屉一屏只看得见
    /// 十条。全部传过去等于每次都白花十几倍流量（手机上还是几百毫秒），前端也要
    /// 一次挂一千个 DOM 节点。`limit = 0` 保持老行为（全给），CLI / 脚本这么用。
    ///
    /// 游标是上一页最后一条的 `(created_at, id)`：排序是「创建时间倒序 + id 倒序」，
    /// 所以「排在它后面」就是二元组更小的那些 —— 新建会话插到最上面也不会让下一页
    /// 错位（离线游标不会漂，不需要 offset）。
    pub fn list_page(
        &self,
        before: Option<(u64, &str)>,
        limit: usize,
    ) -> (Vec<Value>, bool, Option<String>) {
        self.list_page_with(before, limit, |_| None)
    }

    /// 同 [`Self::list_page`]，但每行额外带一个 `state` / `label`。
    ///
    /// `standing` 由调用方给：它需要 `busy` / `queued`，而那两样在 `AppState`
    /// 里（这个类型持有的是会话表，不是运行时状态）。传 `None` 就不带这两个
    /// 字段 —— 内部调用（`list()`）不需要它们。
    ///
    /// **状态只由 `ChatSession::standing` 算**，不在这里另写一遍判断：
    /// 那份推导已经在 `http.rs` 的 dispatched 列表和测试里用了，多一份就会走偏。
    pub fn list_page_with(
        &self,
        before: Option<(u64, &str)>,
        limit: usize,
        standing: impl Fn(&ChatSession) -> Option<crate::agent::Standing>,
    ) -> (Vec<Value>, bool, Option<String>) {
        // 空会话不进列表（「新对话」点了没说话不该占位），但 **Router 例外**：
        // 它必须一直看得见 —— 它是双层架构的入口，用户得能点回它。而且它通常
        // 就是空的（派出去的活由 worker 干，它自己不攒消息）。
        let mut v: Vec<&ChatSession> = self
            .sessions
            .iter()
            .filter(|s| s.has_content() || s.role == Role::Router)
            .collect();
        // created_at 撞车（同一毫秒建的）时拿 id 兜底：顺序必须**确定** ——
        // 同一个列表反复拉，每次都得一模一样，不然抽屉又会自己动
        v.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        if let Some((ts, id)) = before {
            let at = v
                .iter()
                .position(|s| (s.created_at, s.id.as_str()) < (ts, id))
                .unwrap_or(v.len());
            v.drain(..at);
        }
        let has_more = limit > 0 && v.len() > limit;
        if has_more {
            v.truncate(limit);
        }
        // 下一页从这一页**最后一条**之后开始。游标里带着 `created_at` 和 id：
        // 只看 id 或者只看时间都不行（同一毫秒建的两条得能分开）
        let next = v
            .last()
            .filter(|_| has_more)
            .map(|s| format!("{}:{}", s.created_at, s.id));
        let rows = v
            .into_iter()
            .map(|s| {
                let mut row = json!({
                    "id": s.id,
                    // 短编号（`#7`）。界面在标题旁边显示它 —— 用户看到 `#7`
                    // 就能对 Router 说「#7 那个改一下」。`null` = 还没发号。
                    "seq": s.seq,
                    "title": if s.title.is_empty() { "新对话" } else { &s.title },
                    "updatedAt": s.updated_at,
                    "count": s.user_count(),
                    // 前端靠它把协调层那条画得不一样（图标 / 固定在顶上 / 不能删）。
                    // 不给的话界面就得靠「标题叫什么」去猜，那是猜不准的。
                    "role": s.role.as_str(),
                });
                // `state` / `label` 只在调用方给了推导时才有（见 `list_page_with`）。
                // 前端拿 `label` 直接显示，不在那边再写一遍中文映射。
                if let Some(st) = standing(s) {
                    row["state"] = json!(st.as_str());
                    row["label"] = json!(st.label());
                }
                row
            })
            .collect();
        (rows, has_more, next)
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
                    ChatMessage::Assistant { content, .. } => ("assistant", content.as_ref()),
                    // 系统提示词和工具结果不参与搜索：前者不是对话，后者是噪音
                    _ => continue,
                };
                let Some(snippet) = hit_snippet(text, needle) else {
                    continue;
                };
                out.push(json!({
                    "id": s.id,
                    "title": if s.title.is_empty() { "新对话" } else { &s.title },
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
            content: prompt::INTERRUPTED.into(),
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
    let mut budget = ImageBudget::new(sess);
    let mut i = 0;
    while i < msgs.len() {
        // 带附件的用户消息：正文 + 图片 / PDF 内容块拼在一条消息里。
        // 回执（`origin == WorkerReport`）和普通用户消息在这里一视同仁：**原样发**。
        // `origin` 只给本地用（见 `ChatMessage::User.origin`），不进请求体。
        if let ChatMessage::User {
            text, attachments, ..
        } = &msgs[i]
        {
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
        let mut carriers: Vec<(ImagePart, bool)> = Vec::new();
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
                        carriers.push((img.clone(), budget.next()));
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        for (img, inline) in carriers {
            out.push(image_carrier(&img, supports_images, inline));
        }
    }
    pass_back_empty_reasoning(&mut out);
    Ok(out)
}

/// 思考模型：**每一条** assistant 消息都带 `reasoning_content`，这一步没想就传 `""`。
///
/// 思考模型也会有「这一步没思考、直接调工具」的时候，思考内容是空的 —— 以前空的
/// 就整个省掉字段，DeepSeek 的严格后端就回 400：
///
/// ```text
/// The `reasoning_content` in the thinking mode must be passed back to the API.
/// ```
///
/// 实测（jl 上那次）：失败的请求里当前回合有一条 assistant 没带这个字段，前面几个
/// 回合也有同样缺的却都过了 —— 网关后面不同上游宽严不一，所以不能指望它总是放过。
///
/// 「是不是思考模型」看历史里**有没有一条带思考内容**：不吐思考的模型（OpenAI 的
/// Chat Completions）历史里一条都没有，字段照旧一个都不出现，不给它塞陌生字段。
fn pass_back_empty_reasoning(msgs: &mut [Message]) {
    let thinking = msgs.iter().any(|m| {
        matches!(
            m,
            Message::Assistant {
                reasoning_content: Some(_),
                ..
            }
        )
    });
    if !thinking {
        return;
    }
    for m in msgs {
        if let Message::Assistant {
            reasoning_content, ..
        } = m
        {
            reasoning_content.get_or_insert_with(Default::default);
        }
    }
}

/// 工具读到的图，请求里只内联最近这么几张（更早的换成一句文字说明）。
///
/// 全都内联的话，图多的会话请求体会一路涨到网关的上限：jl 上一次 3D 建模，
/// 模型反复渲染、看图，53 张 PNG 把请求撑到 50MB，网关直接回
/// `413 length limit exceeded` —— 原样重发必然还是 413，会话就此卡死。
/// 老的渲染图模型也用不上，要再看可以重新 `read`。
const KEEP_IMAGES: usize = 4;

/// 按顺序数历史里工具读到的图，决定每一张内联不内联（见 `KEEP_IMAGES`）。
///
/// 截断点**按整块往前挪**：内联的张数在 `KEEP_IMAGES` 到 `2 * KEEP_IMAGES - 1`
/// 之间摆动，每多读 `KEEP_IMAGES` 张图才挪一次。每挪一次前缀就变了、prompt
/// cache 断一次，一张一挪的话每步都断。截断点只由历史推导，请求仍然是确定的。
struct ImageBudget {
    /// 从第几张（从 0 数）开始内联
    from: usize,
    seen: usize,
}

impl ImageBudget {
    fn new(sess: &ChatSession) -> Self {
        let total: usize = sess
            .messages
            .iter()
            .map(|m| match m {
                ChatMessage::Tool { images, .. } => images.len(),
                _ => 0,
            })
            .sum();
        Self {
            from: total.saturating_sub(KEEP_IMAGES) / KEEP_IMAGES * KEEP_IMAGES,
            seen: 0,
        }
    }

    /// 历史里的下一张工具图要不要内联
    fn next(&mut self) -> bool {
        let inline = self.seen >= self.from;
        self.seen += 1;
        inline
    }
}

/// 把一张工具返回的图片包成一条 `user` 消息。
///
/// 为什么是 user 消息：Chat Completions 的 tool 消息内容只能是字符串，
/// 工具结果里的图片没有别的合法位置可挂。Responses API 没这个问题（图片
/// 直接放在 `function_call_output` 里，见 `build_responses_input`）。
fn image_carrier(img: &ImagePart, supports_images: bool, inline: bool) -> Message {
    // 同一步读了好几张图时，靠路径分清哪张是哪张（tool_call_id 对模型没意义）
    let label = format!("[read {}]", img.source);

    let mut parts: Vec<Part> = vec![text_part(label)];
    if !inline {
        parts.push(text_part(prompt::IMAGE_EVICTED.to_string()));
    } else if supports_images {
        // 走缓存：每一步都要重新组装请求，不能每步都把同一张图重新 base64。
        // 字节仍然逐字一致（文件不变），prompt cache 照命中。
        match crate::imaging::cached_data_url(&img.path, &img.mime) {
            Ok(url) => parts.push(image_part(url)),
            Err(e) => {
                tracing::warn!("图片读不到了 {}: {e}", img.path);
                parts.push(text_part("[图片文件丢了，没能附带]".to_string()));
            }
        }
    } else {
        parts.push(text_part("[当前模型看不了图，没有附带]".to_string()));
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
                    parts.push(text_part("[当前模型看不了图，没有附带]".to_string()));
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
pub fn tool_definitions(p: Platform, role: Role) -> std::sync::Arc<[Tool]> {
    static DEFS: [OnceLock<std::sync::Arc<[Tool]>>; Platform::SLOTS2] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    DEFS[p.slot2(role.slot())]
        .get_or_init(|| {
            tools(p, role)
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
fn tool_definitions_responses(p: Platform, role: Role) -> std::sync::Arc<[crate::responses::Tool]> {
    static DEFS: [OnceLock<std::sync::Arc<[crate::responses::Tool]>>; Platform::SLOTS2] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    DEFS[p.slot2(role.slot())]
        .get_or_init(|| {
            tools(p, role)
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
            let mut v = json!({
                "id": m.id,
                "role": "user",
                "steps": 0,
                "done": false,
                "parts": user_view_parts(&m.text, &m.attachments),
            });
            // **排队中的那一条也要带来源**。以前这里不看 `origin`：一条等着交给
            // 模型的回执在界面上是一个「用户气泡」，交出去之后又变成「子会话报告」
            // —— 同一个东西在排队前后换了个样子。`fromWorker` / `fromSchedule` 和
            // `session_view_range_clipped` 用的是同一组键（前端只认这两组）。
            match &m.origin {
                Origin::WorkerReport { session_id } => v["fromWorker"] = json!(session_id),
                Origin::Schedule { task_id } => v["fromSchedule"] = json!(task_id),
                Origin::Human | Origin::Dispatch => {}
            }
            v
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

/// 视图里的**回合边界**：`(视图下标, DB 下标)`，第一个总是 `(0, 0)`。
///
/// `session_view_range` 的两个端点只能取这里的下标（或者末尾）：边界之间那些
/// assistant 消息是**并进上一条**的（一个回合的多步合成一条），从中间切开就会被
/// 当成新的一条，跟整段视图对不上。
///
/// 「一次取多少」和「从哪切」是两件事：条数是客户端说了算（它只渲染得了一屏），
/// 切点只能落在这里。
pub fn session_boundaries(sess: &ChatSession) -> Vec<(usize, usize)> {
    let mut out = vec![(0usize, 0usize)];
    let mut n = 0usize;
    let mut last_assistant = false;
    for (i, m) in sess.messages.iter().enumerate() {
        match m {
            // User 一定开新的一条（合并规则里 User 打断 assistant 的合并）。
            // 记的是**它之前**的视图条数 —— 也就是它自己在新视图里的下标：
            // 它自己还没算进去
            ChatMessage::User { .. } => {
                last_assistant = false;
                out.push((n, i));
                n += 1;
            }
            // assistant 只在「上一条不是 assistant」时起一条 —— 所以它从来不是切点
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
            _ => {}
        }
    }
    out
}

/// 尾部窗口的起点（DB 下标）：**一定是回合边界**，它之后至少 `tail` 条视图消息
/// （不够就到 0 —— 整段就这么点）。
///
/// 多给一点没关系（最多多一个回合），少给才会让界面缺内容。`tail = 0` = 整段
/// （脚本 / 调试那条路照旧）。
pub fn session_tail_start(sess: &ChatSession, tail: usize) -> usize {
    let total = session_view_count_before(sess, sess.messages.len());
    if tail == 0 || total <= tail {
        return 0;
    }
    let want = total - tail;
    session_boundaries(sess)
        .into_iter()
        .rfind(|(count, _)| *count <= want)
        .map_or(0, |(_, i)| i)
}

/// 往前翻一页：`(窗口起点, 窗口终点)`（都是 DB 下标、都是回合边界）。
///
/// 这一窗**最多** `limit` 条视图消息，终点不超过 `before` —— 也就是客户端手上
/// 那窗的起点（上一次响应里的 `from`）。所以一页接一页往前要，拼起来正好是
/// 完整视图，既不重也不要漏（有测试盯着）。
pub fn session_page(sess: &ChatSession, before: usize, limit: usize) -> (usize, usize) {
    let bounds = session_boundaries(sess);
    let total = session_view_count_before(sess, sess.messages.len());
    let upto = before.min(total);
    // 终点：视图下标 ≤ upto 的最后一个边界（正常就正好是 upto 那个）
    let end = bounds
        .iter()
        .rfind(|(count, _)| *count <= upto)
        .copied()
        .unwrap_or((0, 0));
    let want = end.0.saturating_sub(limit);
    let start = bounds
        .iter()
        .rfind(|(count, _)| *count <= want)
        .map_or(0, |&(_, i)| i);
    (start, end.1)
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
    session_view_range(sess, from, sess.messages.len())
}

/// 同 [`session_view_from`]，但把 reasoning / tool 的正文截到 `cap`（界面那条路用）。
pub fn session_view_from_clipped(sess: &ChatSession, from: usize, cap: usize) -> Vec<Value> {
    session_view_range_clipped(sess, from, sess.messages.len(), cap)
}

/// 同 `session_view`，但只要 `messages[from..upto]` 那一段。
///
/// 两个端点都必须是**回合边界**（见 [`session_boundaries`]）——
/// 从中间切开的话，开头那个 assistant 会被当成新的一条，而整段视图里它是并进上一条的。
///
/// 存在的理由：往前翻的时候只发这一窗，不把整段历史（含每个 50KB 的 bash 输出）
/// 重新发一遍 —— 客户端一屏也就几十条，给多了它也不渲染。
pub fn session_view_range(sess: &ChatSession, from: usize, upto: usize) -> Vec<Value> {
    session_view_range_clipped(sess, from, upto, usize::MAX)
}

/// 单条 part 的**正文**在首屏最多给这么多字符（reasoning 的 `text`、tool 的 `output`）。
///
/// 实测一个 10 条消息的窗口能到 190 KB，其中 reasoning 197 KB、tool 输出 177 KB
/// （同一份数据里两者有重叠计数），而真正给人看的 `text` 只有 **7.5 KB —— 4%**。
/// 前端对 reasoning 只渲染**一行摘要**（`ThoughtLine`），tool 只有一个折叠行；
/// 全文只在点开面板时才看。所以首屏把 96% 的字节传了过去，屏幕上根本没有。
///
/// 400 字够放下摘要行 + 面板里的开头一段；剩下的点开时再取（见 `part_full`）。
pub const PART_PREVIEW_CHARS: usize = 400;

/// 工具**输出**首屏一个字都不给。
///
/// 它在前端**从不显示**：工具行只有图标 + 摘要（摘要读的是 `input` 的
/// `command`/`path`），输出全文只在点开面板时才看。所以「留前 400 字」纯属浪费
/// —— 反正点开要重新请求一次，那 400 字一行也用不上。
///
/// 唯一从 output 里读出来的东西是「这一行要不要标红」（`failed`：开头
/// `错误：` 或末尾 `[exit N]`，N≠0）。那个在服务端算好，作为 `failed: true`
/// 一起发 —— 比传 400 字再让前端截尾判一遍省得多，也不会因为截断把
/// 末尾的 `[exit 1]` 切掉而误判成功（这个坑真踩过）。
pub const OUTPUT_ON_WIRE: bool = false;

/// 工具**参数**（`input`）在首屏最多序列化成这么多字符。
///
/// 第一版只截了 `output`，漏了 `input` —— 实测这个会话里 `bash` 的参数合计
/// **57 KB**（30 条命令），比截断后的 output（29 KB）还大一倍。而屏幕上：
///   - 工具行只显示 `command` 的**第一行**（`summarize`）
///   - 参数明细只在点开面板时才看（`describeInput`）
///
/// 所以首屏没有任何理由把 30 条完整命令都传一遍。
const TOOL_INPUT_PREVIEW_CHARS: usize = 2000;

/// 单个长字段（`command` / `content` …）在首屏最多留这么多字符。
///
/// 和 `TOOL_INPUT_PREVIEW_CHARS` 是两件事：那个管「整个 input 序列化后多长」，
/// 这个管「其中某一个字段多长」。后者才是绝大多数情况 —— 106 个工具、每个参数
/// 都不到 2 KB，但合计 71 KB，因为它们是一条条几百字的命令。
/// 工具行只显示 `command` 的第一行，留 200 字（够看出是什么命令）足够。
const LONG_FIELD_CHARS: usize = 200;

/// 这次工具调用算不算失败 —— 和前端 `lib/toolview.ts` 的 `failed()` **同一套判定**。
///
/// 放在服务端算的理由：首屏不再传 output 正文（见 [`OUTPUT_ON_WIRE`]），
/// 而工具行要不要标红就看这个。传一个布尔比传 400 字让前端自己截尾判便宜，
/// 而且不会因为把末尾的 `[exit 1]` 切掉而误判成成功（这个坑真踩过：一条
/// 400 字之后才输出 `[exit 1]` 的命令，界面不标红）。
fn tool_failed(output: &str) -> bool {
    if output.starts_with("错误：") {
        return true;
    }
    // `[exit N]` 在**末尾**（允许尾随空白）
    match output.trim_end().rfind("[exit ") {
        Some(at) => {
            let rest = &output[at + 6..];
            rest.split(']')
                .next()
                .and_then(|n| n.trim().parse::<i32>().ok())
                .is_some_and(|n| n != 0)
        }
        None => false,
    }
}

/// 把一个 part 的某个正文字段截到 `cap` 个字符，截了就标 `truncated` + `chars`。
///
/// 按**字符**不是字节：中日文一个字 3 字节，按字节切会把一个汉字劈成两半
/// （`String::truncate` 会在非边界上 panic，而这里连 panic 都不该有）。
/// 前端要拿到全长才知道「还有多少没显示」，所以 `chars` 是无条件记的。
fn clip_part(mut part: Value, total: usize, cap: usize, field: &str) -> Value {
    if total <= cap || cap == usize::MAX {
        return part;
    }
    if let Some(Value::String(text)) = part.get(field) {
        // 留一点余量给省略号，前端不会自动加
        let cut: String = text.chars().take(cap).collect();
        part[field] = json!(format!("{cut}…"));
        part["truncated"] = json!(true);
        part["chars"] = json!(total);
    }
    part
}

/// 把 `part["input"]` 截成一个「够看的预览」。
///
/// 和 [`clip_part`] 不同：`input` 是结构体（不是字符串），不能按字符切。
/// 做法是**整个换成一段文本预览**：
///   - 短参数（序列化后 ≤ cap）原样保留，前端不用管；
///   - 长的就换成 `{"__preview": "<前 cap 字符>"}` 这种形状，前端认 `truncated`
///     就知道「这不是完整参数，点开面板再取」。
///
/// 为什么不保留完整结构再砍某个字段：不同工具的参数形状差得远（`bash` 是
/// `command`、`edit` 是 `edits`/`content`、`read` 是 `path`），逐个字段判断
/// 既容易漏又难维护。整个预览一段就够了 —— 首屏只需要 `summarize()` 那一个
/// 短字段（它还会从 `inputText` 兜底）。
fn clip_input(mut part: Value, cap: usize) -> Value {
    if cap == usize::MAX {
        return part;
    }
    // 把 `input` **取出来**：`part` 是自有值，`remove` 就够了 —— 以前是
    // `part.get("input").cloned()`，把整个参数对象再深拷一份（`edit` 的参数里
    // 跟着整份文件内容，几十 KB）。
    let Some(input) = part.as_object_mut().and_then(|o| o.remove("input")) else {
        return part;
    };

    // **先看单个长字段**：`bash` 的 `command` 经常是一整段脚本（几百到几千字），
    // 而工具行只显示它的第一行。整体序列化不到 cap 不代表没问题 —— 实测这个
    // 会话里 106 个工具的参数合计 71 KB，单个最大才 3.4 KB，**每一个都「没超
    // 2000」**，总量却失控了。所以长字段单独截，阈值比整体小得多。
    let mut preview = serde_json::Map::new();
    let mut cut_any = false;
    if let Some(obj) = input.as_object() {
        for (k, v) in obj {
            let is_long_field = matches!(
                k.as_str(),
                "command" | "content" | "text" | "query" | "oldText" | "newText"
            );
            if let Some(text) = is_long_field.then(|| v.as_str()).flatten() {
                if text.chars().count() > LONG_FIELD_CHARS {
                    let mut short: String = text.chars().take(LONG_FIELD_CHARS).collect();
                    short.push('…');
                    preview.insert(k.clone(), json!(short));
                    cut_any = true;
                    continue;
                }
            }
            preview.insert(k.clone(), v.clone());
        }
    }

    // 一个长字段都没有：原样返回。**这一步以前不是提前的** —— 以前先
    // `serde_json::to_string(&input)` 把整段参数序列化一遍（几十 KB）+ 数一遍字符，
    // 然后发现没东西要截，把 `part` 原样返回。白做两次 O(内容长度) 的活。
    if !cut_any {
        part["input"] = input;
        return part;
    }

    // 到这儿才需要「原始有多长」（前端显示「原文 N 字」）
    let full = serde_json::to_string(&input).unwrap_or_default();
    let full_len = full.chars().count();

    // 字段截过之后**仍然**超 cap 的（比如一百个短字段堆在一起），才退化成一整段
    // 文本预览。判断用截断后的实际大小 —— 用原始 `full_len` 的话，一条长命令
    // 明明已经被砍到 200 字了，还会被再套一层 `__preview`（测试抓到过）。
    //
    // 量长度直接序列化 `&preview`（`Map` 本身就是 `Serialize`），不再像以前那样
    // 为了量一次大小把整张 map `clone()` 成一个 `Value`。
    let after = serde_json::to_string(&preview)
        .map(|v| v.chars().count())
        .unwrap_or(0);
    if after > cap {
        let mut slim = serde_json::Map::new();
        // `remove` 而不是 `get().clone()`：这几个键下面的值马上就没人要了
        for key in ["command", "path", "name", "query", "url"] {
            if let Some(v) = preview.remove(key) {
                slim.insert(key.into(), v);
            }
        }
        slim.insert(
            "__preview".into(),
            json!(full.chars().take(cap).collect::<String>()),
        );
        slim.insert("chars".into(), json!(full_len));
        preview = slim;
    }
    part["input"] = Value::Object(preview);
    part["inputTruncated"] = json!(true);
    part["inputChars"] = json!(full_len);
    part
}

/// 同 [`session_view_range`]，但把 reasoning / tool 的正文截到
/// [`PART_PREVIEW_CHARS`]，并带上 `truncated: true` 和 `chars`（全长）。
///
/// `cap` 传 `usize::MAX` 就是不截（调试脚本、`session_view` 那条老路照旧要全文）。
pub fn session_view_range_clipped(
    sess: &ChatSession,
    from: usize,
    upto: usize,
    cap: usize,
) -> Vec<Value> {
    let len = sess.messages.len();
    let from = from.min(len);
    let upto = upto.clamp(from, len);
    // 工具结果按 id 索引：只需要切片内的（一个回合的 tool 结果都在它自己后面）
    let mut results: HashMap<&str, (&str, &[ImagePart])> = HashMap::new();
    for m in &sess.messages[from..upto] {
        if let ChatMessage::Tool {
            tool_call_id,
            content,
            images,
        } = m
        {
            results.insert(tool_call_id.as_str(), (content.as_ref(), images.as_slice()));
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for (i, m) in sess.messages[from..upto]
        .iter()
        .enumerate()
        .map(|(k, m)| (from + k, m))
    {
        match m {
            // 系统提示词（第一条，以及后续的“提示词升级”）是给模型看的，不给用户看
            ChatMessage::System(_) => {}

            ChatMessage::User {
                text,
                attachments,
                origin,
            } => {
                let mut v = json!({
                    "id": format!("u{i}"),
                    "role": "user",
                    "steps": 0,
                    "done": true,
                    "parts": user_view_parts(text, attachments),
                });
                // 回执不是人说的话：告诉前端，别把它画成用户气泡。
                // 老客户端不认这个字段也没关系（它只知道 user/assistant）。
                match origin {
                    crate::agent::Origin::WorkerReport { session_id } => {
                        v["fromWorker"] = json!(session_id);
                    }
                    // 定时器投的：同理，用户没说过这些话（`queued_view` 也认这个键）
                    crate::agent::Origin::Schedule { task_id } => {
                        v["fromSchedule"] = json!(task_id);
                    }
                    crate::agent::Origin::Human | crate::agent::Origin::Dispatch => {}
                }
                out.push(v);
            }

            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut parts: Vec<Value> = Vec::new();
                if !reasoning.is_empty() {
                    parts.push(clip_part(
                        json!({ "type": "reasoning", "id": format!("r{i}"), "text": reasoning }),
                        reasoning.chars().count(),
                        cap,
                        "text",
                    ));
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
                    let mut part = json!({
                        "type": "tool",
                        "toolCallId": tc.id,
                        "toolName": tc.name,
                        "inputText": "",
                        "input": serde_json::from_str::<Value>(&tc.arguments)
                            .unwrap_or_else(|_| json!(tc.arguments)),
                        // 没有输出就是还在跑（服务端此刻正在执行）
                        "state": if output.is_some() { "output" } else { "ready" },
                        "images": images,
                    });
                    // 还在跑就**不带** `output`，别写成 `null`：前端的类型是 `output?: string`，
                    // 只认「没有」。踩过：`null` 进了 `failed()` 的 `startsWith`，渲染时抛错把
                    // Svelte 那一轮更新整个打断 —— 切回一个工具正跑着的会话，界面停在上一个
                    // 会话的内容上、没有停止键，直到那个工具跑完。
                    if let Some(o) = output {
                        // **首屏不带 output 正文**（见 `OUTPUT_ON_WIRE`）：
                        //   - 截断版（界面那条路，cap != MAX）：一个字都不给，
                        //     只给 `failed` 和 `chars`（面板里显示「原文 N 字」用）；
                        //   - 不截断版（调试脚本 / 老客户端）：原样给全文。
                        part["chars"] = json!(o.chars().count());
                        if !OUTPUT_ON_WIRE && cap != usize::MAX {
                            part["failed"] = json!(tool_failed(o));
                            part["output"] = json!("");
                        } else {
                            // **先写进去再截**：`clip_part` 是拿 `part[field]` 去切的，
                            // 字段还没写时它什么都不做（踩过：工具输出原样发了全文，
                            // 190 KB 的窗口里 tool 占 177 KB 一点没省）。
                            part["output"] = json!(o);
                            part = clip_part(part, o.chars().count(), cap, "output");
                        }
                    }
                    // 参数也要截：第一版漏了这条，实测 bash 的参数合计 57 KB，
                    // 比截断后的 output 还大一倍（见 TOOL_INPUT_PREVIEW_CHARS）。
                    if cap != usize::MAX {
                        part = clip_input(part, TOOL_INPUT_PREVIEW_CHARS);
                    }
                    parts.push(part);
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

/// 按视图里的 part id 取**全文**（首屏那条路是截断过的，见 [`PART_PREVIEW_CHARS`]
/// 和 [`LONG_FIELD_CHARS`]）。
///
/// **一次返回所有被截过的字段**，不是一次只要一个：
/// ```json
/// {"text": "…"}                      // reasoning / 正文
/// {"output": "…", "input": {…}}      // 工具：输出和参数一起给
/// ```
/// 早先按 `?input=1` 拆成两条路，输入输出都长的工具点一下会并发两条往返 ——
/// 同一份数据的两个字段没理由拆两次，而且调用方还要自己管两份缓存。
///
/// id 的两种来源：
/// - `r{i}` / `t{i}` —— `i` 是 `ChatMessage` 的下标（`session_view_range` 里
///   `format!("r{i}")` 那个 `i`），直接在 `sess.messages[i]` 上取对应字段；
/// - 别的一律当工具调用 id，在整段里找那个 `tool_calls[].id`（工具调用可能落在
///   回合里任意一条 assistant 上）。
///
/// 找不到返回 `None`（会话被删、id 打错、或者那条消息已经被回收）。
pub fn part_full(sess: &ChatSession, id: &str) -> Option<Value> {
    // `r3` / `t3`：前缀 + DB 下标
    if let Some(rest) = id.strip_prefix('r') {
        if let Ok(i) = rest.parse::<usize>() {
            if let Some(ChatMessage::Assistant { reasoning, .. }) = sess.messages.get(i) {
                return Some(json!({ "text": reasoning }));
            }
        }
    }
    if let Some(rest) = id.strip_prefix('t') {
        if let Ok(i) = rest.parse::<usize>() {
            if let Some(ChatMessage::Assistant { content, .. }) = sess.messages.get(i) {
                return Some(json!({ "text": content }));
            }
        }
    }
    // 工具：参数在那条 assistant 上，输出在它后面某条 Tool 上 —— 两个一起给
    let mut i = None;
    for (k, m) in sess.messages.iter().enumerate() {
        if let ChatMessage::Assistant { tool_calls, .. } = m {
            if let Some(tc) = tool_calls.iter().find(|tc| tc.id == id) {
                i = Some((k, tc.arguments.clone()));
                break;
            }
        }
    }
    let (i, arguments) = i?;
    let mut out = json!({
        "input": serde_json::from_str::<Value>(&arguments)
            .unwrap_or_else(|_| json!(arguments)),
    });
    // 结果可能在它后面任意一条 Tool 消息上（一个回合的 tool 结果都在后面）
    if let Some(content) = sess.messages[i + 1..].iter().find_map(|m| match m {
        ChatMessage::Tool {
            tool_call_id,
            content,
            ..
        } if tool_call_id == id => Some(content.clone()),
        _ => None,
    }) {
        out["output"] = json!(content);
    }
    Some(out)
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
            messages: session_view_from_clipped(sess, client_len, PART_PREVIEW_CHARS),
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
            messages: session_view_from_clipped(sess, client_len, PART_PREVIEW_CHARS),
        };
    };

    let keep: usize = msgs[turn_start..last].iter().map(view_parts_len).sum();
    let prior_steps = assistants.len() - 1;
    // 这条增量也截断：一个回合能长到几十步、每步一个 50KB 的 bash 输出，而用户
    // 此刻多半并没有打开那个面板。要全文时走 `/api/part`。
    let mut out = session_view_from_clipped(sess, last, PART_PREVIEW_CHARS);
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
    tx: mpsc::Sender<Value>,
) -> anyhow::Result<()> {
    run_turn_with(
        core,
        provider,
        vec![(user_text.to_string(), attachments, Origin::Human)],
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
    msgs: Vec<(String, Vec<Attachment>, Origin)>,
    sid: &str,
    turn: std::sync::Arc<crate::state::Turn>,
    tx: mpsc::Sender<Value>,
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
    msgs: Vec<(String, Vec<Attachment>, Origin)>,
    sid: &str,
    turn: std::sync::Arc<crate::state::Turn>,
    tx: mpsc::Sender<Value>,
) -> anyhow::Result<()> {
    let _guard = TurnGuard(core.clone(), sid.to_string());
    // 不活跃的会话消息不在内存里，先从库里读回来（回合进行中不会被放掉，
    // 见 `AppState::evict_idle`）
    core.state.ensure_loaded(sid).await?;
    // 系统提示词可能被 /api/debug/system-prompt 覆盖过，所以不能直接用常量。
    // 它**按角色分**（router 那份是另一条不可覆盖的常量），所以要先知道这个会话
    // 是谁 —— 单独拿一次读锁问一声，在写锁之前，避免锁套锁
    //（`system_prompt` 里会读别的 RwLock）。
    let role = {
        let store = core.state.chat.read().await;
        match store.get(sid) {
            Some(s) => s.role,
            None => anyhow::bail!("没有这个会话: {sid}"),
        }
    };
    let system_prompt = core.state.system_prompt_for(role);
    // 这一轮里有没有**派活**（`dispatch` 投进来的）？
    //
    // 在外面算：`msgs` 会被下面的块消费掉，而 `report_back` 在块外要用。
    // 队列里可能混着两种（派活 + 用户插话），只要有一条是派活就算 ——
    // 那说明这一轮确实在推进那件活。
    let dispatched = msgs
        .iter()
        .any(|(_, _, origin)| matches!(origin, Origin::Dispatch));
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
        if let Some((text, _, _)) = msgs.first() {
            sess.maybe_title(text);
        }
        // 一条消息装下正文和附件。整批一起进历史 —— 模型一次就看到全部。
        for (text, attachments, origin) in msgs {
            sess.messages.push(ChatMessage::User {
                text,
                attachments,
                origin,
            });
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
    // 双层架构的闭环：如果这个会话是 Router 派出去的，把结果送回去
    //（见 `router::report_back`）。不是派出去的会话它什么都不做。
    //
    // **用户自己按的停止不回报**：那是人不想让它继续了，不是「做完了」或
    // 「失败了」，往 Router 塞一条反而多一次没意义的唤醒。
    // 用户按的停止**也要记**：那不是「做完」也不是「失败」，是中途中止 ——
    // 工作项得知道，不然它会永远挂在「在干」上。
    let said = last_assistant_text(core, sid).await;
    crate::router::report_back(
        core,
        sid,
        &said,
        res.is_ok(),
        turn.abort_requested(),
        dispatched,
    )
    .await;
    res
}

/// 把一条会话最近 `lines` 条消息压成给人看的一段（Router 的「执行记录」）。
///
/// 和 `last_assistant_text` 放在一起：两个都是「会话消息 → 给人看的一段文字」，
/// 属于会话自己的知识，不该让 `router` 模块去摸 `ChatMessage` 的内部结构
///（它现在只调这个函数，不知道消息是 enum、有几个变体）。
///
/// 工具结果只留「调用过」：它是过程的噪音，Router 要的是「它做到哪了」。
pub async fn session_digest(
    core: &Core,
    session_id: &str,
    lines: usize,
    line_chars: usize,
) -> Result<String, String> {
    let state = &core.state;
    // 消息可能不在内存里（stub）：读回来
    state
        .ensure_loaded(session_id)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let store = state.chat.read().await;
    let Some(s) = store.get(session_id) else {
        return Err("载体会话已经不在了".to_string());
    };
    if !s.resident() {
        return Ok("（没有可读的消息）".to_string());
    }
    let clip = |t: &str| crate::text::clip(t, line_chars);
    let msgs = &s.messages;
    let start = msgs.len().saturating_sub(lines);
    let mut out = format!("执行记录（最近 {lines} 条，共 {} 条）：\n", msgs.len());
    if start > 0 {
        let _ = writeln!(out, "（前面还有 {start} 条没显示）");
    }
    for m in &msgs[start..] {
        match m {
            ChatMessage::User { text, .. } => {
                let _ = writeln!(out, "- 派活：{}", clip(text.trim()));
            }
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let c = content.trim();
                if !c.is_empty() {
                    let _ = writeln!(out, "- 它：{}", clip(c));
                } else if !tool_calls.is_empty() {
                    let names: Vec<&str> = tool_calls.iter().map(|t| t.name.as_str()).collect();
                    let _ = writeln!(out, "- 它：调用 {}", names.join("、"));
                } else if !reasoning.trim().is_empty() {
                    let _ = writeln!(out, "- 它：（在思考）");
                }
            }
            // 工具输出是它自己的噪音，这里只要「跑过」
            ChatMessage::Tool { .. } => {}
            ChatMessage::System(_) => {}
        }
    }
    Ok(out)
}

/// 这个会话最后一条助手消息的正文（回报给 Router 用）。
///
/// 只要正文，不要工具调用和思考：Router 关心的是「它做完说了什么」，不是它
/// 中间跑了多少条命令（那些在它自己的会话里，要看再 `read_session`）。
///
/// **一个字都不截**。worker 认认真真干完一件事，末尾那段就是全部的交付说明 ——
/// 截断它等于把交付物撕掉一半。上下文顶满了是另一个问题（有 compaction 管），
/// 不该在这里用截断来「预防」。
async fn last_assistant_text(core: &Core, sid: &str) -> String {
    let store = core.state.chat.read().await;
    let Some(s) = store.get(sid) else {
        return String::new();
    };
    if !s.resident() {
        return String::new();
    }
    for m in s.messages.iter().rev() {
        if let ChatMessage::Assistant { content, .. } = m {
            let t = content.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    String::new()
}

/// 往本回合所属的会话追加一条消息。
///
/// **按 id 找**：回合跑在后台，用户随时可能在侧边栏切到别的会话或新建会话
/// —— 以前用「当前会话」去找，切走之后模型的回复
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
        // 这一轮里最大的那些临时块（请求体的 base64、工具输出的整段捕获、
        // 图片解码的中间图）到这附近都已经放掉了。
        //
        // **只有安卓需要在这儿还一次**：bionic/scudo 攥着空闲块不放，`mallopt`
        // 是那边唯一的入口。桌面是 musl 静态二进制（见 README 那段），
        // 超过阈值的块直接 `mmap`、`free` 即 `munmap` —— 没有东西可还，所以这段
        // 在桌面上**根本不编进去**（不是调一个空函数）。见 `trim` 的文件头。
        #[cfg(target_os = "android")]
        crate::trim::release_soon();
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
    /// 流出过内容之后连接断了（`Some(原因)`）。和 `aborted` 一样：已经流出来的进历史、
    /// 没写完的工具调用作废 —— 但回合不结束，`drive` 退避后接着下一步。
    interrupted: Option<String>,
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
            interrupted: None,
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
    tx: &mpsc::Sender<Value>,
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
    // 连着几步都是流到一半断掉的（见 `StepOutput::interrupted`）。正常走完一步就清零；
    // 连着断满 `RETRY_DELAYS` 那么多次就不再接着跑了 —— 多半不是偶发的网络抖动
    let mut interrupts = 0usize;
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
        // 新的一步：「想」是允许从「写」切回来的（见 `Reporter::new_step`）。
        // 一步**之内**的思考 delta 走 `Reporter::thinking`，它不会再切回去 ——
        // 有些 provider 把 reasoning / content 交错着发，切来切去就是每个 chunk
        // 一次通知栏刷新。
        status.new_step();

        let step_out = match stream_one(core, provider, sid, turn, status, tx).await {
            Ok(c) => c,
            Err(e) => {
                send(tx, sse::error(&format!("{e:#}"))).await;
                send(tx, sse::finish()).await;
                return Err(e);
            }
        };
        send(tx, sse::finish_step()).await;
        let StepOutput {
            content,
            reasoning,
            reasoning_items,
            calls,
            aborted,
            interrupted,
        } = step_out;

        // 不管有没有工具调用，这一轮的 assistant 输出都要进历史。
        // 三个大字段直接 move 进去（以前每个 step 都 clone 一遍几十 KB 的正文/思考）；
        // `calls` 后面还要拿去执行工具，只能 clone 这一份。
        if !content.is_empty()
            || !reasoning.is_empty()
            || !calls.is_empty()
            || !reasoning_items.is_empty()
        {
            append(
                core,
                sid,
                ChatMessage::Assistant {
                    content: content.into(),
                    reasoning: reasoning.into(),
                    tool_calls: calls.clone(),
                    reasoning_items,
                },
            )
            .await;
        }

        if aborted {
            send(tx, sse::abort("已停止")).await;
            send(tx, sse::finish()).await;
            return Ok(());
        }

        // 流到一半断了：已经流出来的上面进了历史，退避后接着下一步 —— 模型看得到自己
        // 说到哪了，从那接着做（界面上只是多出新的，不会有两份）
        if let Some(why) = interrupted {
            if interrupts >= RETRY_DELAYS.len() {
                send(tx, sse::error(&why)).await;
                send(tx, sse::finish()).await;
                anyhow::bail!("{why}");
            }
            if !backoff(tx, turn, interrupts, &why, Retry::Continue).await {
                send(tx, sse::abort("已停止")).await;
                send(tx, sse::finish()).await;
                return Ok(());
            }
            interrupts += 1;
            continue;
        }
        interrupts = 0;

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
                    content: outcome.text.into(),
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
    tx: &mpsc::Sender<Value>,
    status: &crate::status::Reporter,
) -> bool {
    let msgs = core.state.drain_queued(sid);
    if msgs.is_empty() {
        return false;
    }
    // 先把 id **移**出来（`msgs` 随后要按值消费给 `append`）。以前是
    // `msgs.iter().map(|m| m.id.clone())` —— 每条排队消息多一次 String 分配，
    // 而这两个 id 本来就要跟着 `data-queued-delivered` 发给前端，无其它引用。
    let mut ids: Vec<String> = Vec::with_capacity(msgs.len());
    for m in msgs {
        ids.push(m.id);
        let msg = ChatMessage::User {
            text: m.text,
            attachments: m.attachments,
            origin: m.origin,
        };
        append(core, sid, msg).await;
    }
    send(tx, sse::queued_delivered(&ids)).await;
    send(tx, sse::start(&uuid::Uuid::now_v7().to_string())).await;
    // 通知栏那行回到「正在想」（它可能还停在某个工具上）。这是新的一步，
    // 允许从「写」切回来。
    status.new_step();
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
    let msgs: Vec<(String, Vec<Attachment>, Origin)> = core
        .state
        .drain_queued(sid)
        .into_iter()
        .map(|m| (m.text, m.attachments, m.origin))
        .collect();
    if msgs.is_empty() {
        core.state.end_turn(sid);
        return;
    }
    let (core, provider, sid) = (core.clone(), provider.clone(), sid.to_string());
    tokio::spawn(async move {
        // 这个回合没有订阅者（见函数注释）：有界通道、容量小一点，推不进去就算了。
        let (tx, rx) = mpsc::channel(1);
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
    tx: &mpsc::Sender<Value>,
    pending: &[RecordedToolCall],
) -> anyhow::Result<()> {
    let stopped = ToolOutcome::error("用户已停止，这个工具没有执行完");
    let stopped_text: std::sync::Arc<str> = stopped.text.as_str().into();
    for call in pending {
        send(tx, tool_output_event(&call.id, &stopped.text, &[])).await;
        append(
            core,
            sid,
            ChatMessage::Tool {
                tool_call_id: call.id.clone(),
                content: stopped_text.clone(),
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
        tools: tool_definitions(platform, sess.role),
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
        tools: tool_definitions_responses(platform, sess.role),
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
    let mut budget = ImageBudget::new(sess);
    for m in &sess.messages {
        match m {
            ChatMessage::System(s) => {
                out.push(message_item("system", MessageContent::Text(s.clone())));
            }
            // 同 `build_messages`：回执与普通用户消息一视同仁，原样发。
            ChatMessage::User {
                text, attachments, ..
            } => out.push(message_item(
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
                            text: content.to_string(),
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
                output: tool_output(content, images, supports_images, &mut budget),
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
                    parts.push(input_text("[当前模型看不了图，没有附带]".to_string()));
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
fn tool_output(
    content: &str,
    images: &[ImagePart],
    supports_images: bool,
    budget: &mut ImageBudget,
) -> ToolOutput {
    if images.is_empty() || !supports_images {
        budget.seen += images.len();
        return ToolOutput::Text(content.to_string());
    }
    let mut parts: Vec<InputPart> = vec![input_text(content.to_string())];
    for img in images {
        if !budget.next() {
            parts.push(input_text(prompt::IMAGE_EVICTED.to_string()));
            continue;
        }
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
    let mut budget = ImageBudget::new(sess);
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
                for i in imgs {
                    if budget.next() {
                        out.push((i.path.clone(), i.mime.clone()));
                    }
                }
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
    tx: &mpsc::Sender<Value>,
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
                if !backoff(tx, turn, attempt, &format!("{e:#}"), Retry::Again).await {
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
        // 流出过内容之后断了：原因（见 `StepOutput::interrupted`）
        let mut interrupted: Option<String> = None;

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
                    if !backoff(tx, turn, attempt, &format!("{e:#}"), Retry::Again).await {
                        return Ok(StepOutput::aborted());
                    }
                    attempt += 1;
                    continue 'attempt;
                }
                // 已经流出过内容才断（网关掐了长连接，实测：`peer closed connection
                // without sending TLS close_notify`）：整步重来的话界面上就是两份。
                // 流出来的留着、没写完的工具调用作废，交给 `drive` 接着下一步
                Err(e) if emitted => {
                    interrupted = Some(format!("{e:#}"));
                    break;
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
                    for c in blocks.push(BlockKind::Reasoning, r).into_iter().flatten() {
                        send(tx, c).await;
                    }
                }
                if let Some(c) = choice.delta.content.as_deref() {
                    if !c.is_empty() {
                        emitted = true;
                        text_acc.push_str(c);
                        // 通知栏：正在写的最后一行（内部按半秒攒，不是每块都刷）
                        status.streaming(&text_acc);
                        for c in blocks.push(BlockKind::Text, c).into_iter().flatten() {
                            send(tx, c).await;
                        }
                    }
                }
                note_finish(&mut finish, choice.finish_reason.as_deref());
                if let Some(tcs) = choice.delta.tool_calls {
                    for tc in tcs {
                        let i = tc.index as usize;
                        // 上限：`acc` 是按 index 下标存的，而 index 是**服务端给的**。
                        // 畸形（或坏网关改写）的 index 就是一个 OOM 开关：
                        // `u32::MAX` 会让下面这个 while 一直 push 到 300 GB。
                        // 几十个工具调用已经远超任何模型的实际行为，超过就当
                        // 这一条不能用 —— 丢掉比整个进程被 OOM 杀掉强。
                        const MAX_TOOL_CALLS: usize = 64;
                        if i >= MAX_TOOL_CALLS {
                            tracing::warn!(
                                "工具调用 index={i} 超出预期（>{MAX_TOOL_CALLS}），忽略"
                            );
                            continue;
                        }
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
                interrupted: None,
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

        // 连接断在半路：工具调用的参数多半是半截 JSON，一个都不能执行（同上）。
        // 已经在界面上露头的告诉前端作废了；`drive` 下一步会让模型重新来
        if interrupted.is_some() {
            for c in &calls {
                send(
                    tx,
                    tool_output_event(&c.id, "错误：连接中断，这个工具调用没写完，已作废", &[]),
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
            interrupted,
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

/// 退避之后做什么（只影响跟前端说的那句话）
#[derive(Clone, Copy)]
enum Retry {
    /// 这一步整个重来（还什么都没流出来）
    Again,
    /// 流出过内容才断：留着已有的，接着下一步
    Continue,
}

/// 重试前：告诉前端在等什么（不然界面就是一段莫名其妙的停顿），然后退避。
/// 等的时候按停止要立刻生效。返回 `false` = 用户停了。
async fn backoff(
    tx: &mpsc::Sender<Value>,
    turn: &crate::state::Turn,
    attempt: usize,
    why: &str,
    kind: Retry,
) -> bool {
    let ms = RETRY_DELAYS[attempt];
    let secs = ms / 1000;
    let short: String = why.chars().take(160).collect();
    let (what, then) = match kind {
        Retry::Again => ("请求出错", "重试"),
        Retry::Continue => ("连接中断", "接着跑"),
    };
    tracing::warn!(
        "LLM {what}（{short}），{secs}s 后{then}（第 {} 次）",
        attempt + 1
    );
    send(
        tx,
        sse::data_part(
            "warning",
            json!({
                "message": format!(
                    "{what}（{short}），{secs} 秒后{then}（第 {}/{} 次）",
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
    /// 追加一段内容。返回要发出的 chunk：可能先收尾上一块、再开新块（最多 3 个）。
    ///
    /// 用固定长度的栈数组而不是 `Vec`：这是**每个 delta** 都走一次的路径，一条长
    /// 回复几千块，每块一个堆分配（哪怕只有一个元素）纯属白花。
    fn push(&mut self, kind: BlockKind, delta: &str) -> [Option<Value>; 3] {
        let mut out: [Option<Value>; 3] = [None, None, None];
        let mut n = 0usize;
        if self.open.as_ref().map(|(k, _)| *k) != Some(kind) {
            if let Some(v) = self.close() {
                out[n] = Some(v);
                n += 1;
            }
            let id = match kind {
                BlockKind::Text => format!("t-{}", uuid::Uuid::now_v7()),
                BlockKind::Reasoning => format!("r-{}", uuid::Uuid::now_v7()),
            };
            out[n] = Some(match kind {
                BlockKind::Text => sse::text_start(&id),
                BlockKind::Reasoning => sse::reasoning_start(&id),
            });
            n += 1;
            self.open = Some((kind, id));
        }
        let (_, id) = self.open.as_ref().expect("刚开过块");
        out[n] = Some(match kind {
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

async fn send(tx: &mpsc::Sender<Value>, v: Value) {
    if v.get("preliminary").and_then(Value::as_bool) == Some(true) {
        // 阶段性工具输出：前端对它是整份替换，满了直接丢（下一帧会覆盖）。
        let _ = tx.try_send(v);
        return;
    }
    // 通道有界（见 `http::CHUNK_QUEUE`）：正文/思考 delta 要保序，所以满了就等。
    // 但客户端可能挂着连接**永远不读**（WebView 被挂起、连接半死）—— 等太久就
    // 当这一帧丢了，回合照样往下跑；界面下一次 `/api/session` 会把历史补齐。
    const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let _ = tokio::time::timeout(SEND_TIMEOUT, tx.send(v)).await;
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
    tx: mpsc::Sender<Value>,
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
        // 跑到现在的输出，纯文本（「运行中」看 part 的 state，不用另带标记）。
        // `try_send`：这是阶段性结果，客户端读不过来就丢掉这一帧（下一帧会覆盖）。
        let _ = tx.try_send(sse::tool_output_preliminary(&id, json!(text)));
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
        .and_then(serde_json::Value::as_i64)
        .map_or(1, |n| n.max(1) as usize)
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
        .and_then(serde_json::Value::as_bool)
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

/// Router 那五个工具的分派。不是 Router 的工具就返回 `None`，交给下面原来的逻辑。
///
/// 认名字、不认角色：**工具表才是权限**（router 的表里没有 read/bash/edit），
/// 这里只是「这个名字归 `router` 模块管」。多一层角色判断反而多一处要同步的地方。
async fn router_tool(core: &Core, name: &str, args: &Value, sid: &str) -> Option<ToolOutcome> {
    let out = match name {
        "list_sessions" => crate::router::list_sessions(core, sid, args).await,
        "read_session" => crate::router::read_session(core, sid, args).await,
        // 传 sid：新会话要记「谁派的」，做完才知道汇报给谁
        "dispatch" => crate::router::dispatch(core, sid, args).await,
        // 定时任务（见 `schedule`）：和上面三个一样，只是「写一条待办」，不执行任何东西
        "schedule_task" => crate::schedule::tool_schedule_task(core, args).await,
        "schedules" => crate::schedule::tool_schedules(core, sid, args).await,
        _ => return None,
    };
    Some(match out {
        Ok(text) => ToolOutcome::text(text),
        Err(e) => ToolOutcome::error(e),
    })
}

/// `live`：(推给前端的通道, 工具调用 id)。给了就把 bash 的输出边跑边推过去。
async fn execute_tool(
    core: &Core,
    name: &str,
    arguments: &str,
    live: Option<(&mpsc::Sender<Value>, &str)>,
    sid: &str,
) -> ToolOutcome {
    let args: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

    // 工具的 cwd 和 bash 走同一个函数（`LinuxSandbox::session_cwd`）：手机沙箱里是
    // 这个会话自己的目录，电脑上（宿主模式）是 workspace 本身。两个门必须一致 ——
    // 模型在 bash 里 `pwd` 看到哪，`read x.txt` 就得去哪找。
    let cwd = core.state.linux().map_or_else(
        || linux::WORKSPACE.to_string(),
        |sb| sb.session_cwd(Some(sid)),
    );

    // ---- Router 的几个工具（见 `router.rs`）------------------------------
    //
    // 放在最前面，而且**不碰 cwd / 沙箱**：Router 不执行，`session_cwd` 那种
    // 「这个会话的目录在哪」的问题对它是没有意义的。这也是一条防线 ——
    // router 的工具表里没有 read/bash/edit，万一模型凭记忆硬调，这里
    // 会落到末尾的「未知工具」，而不是进到下面那几条执行路径里。
    if let Some(out) = router_tool(core, name, &args, sid).await {
        return out;
    }

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

/// 图片结果：缩好的字节落盘（历史回放时还要重发），回给模型一句说明（格式、原尺寸、
/// 大小，缩过的话发过去多大）；图片本体作为 `images` 走 `build_messages` 里那条 carrier
/// （Responses 协议直接放在工具结果里）。
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
        .is_none_or(|p| core.state.cached_supports_images(&p));

    // 只说图本身：格式、原尺寸、大小，缩过的话发过去的是多大（在图上估坐标要用）。
    // 路径不回显 —— 就是模型自己刚传进来的那个。
    let mime = img.mime.clone();
    let size = crate::reader::fmt_size(img.source_bytes);
    let mut output = format!(
        "图片 {mime}，{}×{}，{size}",
        img.original_width, img.original_height
    );
    if (img.width, img.height) != (img.original_width, img.original_height) {
        let _ = write!(output, "（缩到 {}×{} 发给你）", img.width, img.height);
    }

    if !supports_images {
        output.push_str("。当前模型看不了图，这张没发给你");
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
            let _ = write!(output, "。图没能保存，这次没发给你：{e:#}");
            ToolOutcome::text(output)
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// **`standing()` 是唯一的推导入口，优先级写死在一处。**
    ///
    /// 以前 `http.rs` 和 `router.rs` 各写一份，优先级还是相反的 —— 同一个会话
    /// 在两个地方会得出不同标签。
    #[test]
    fn standing_has_one_priority_order() {
        let s = ChatSession::with_role(Role::Worker);
        // 在跑 > 排队 > 闲着
        assert_eq!(s.standing(false, false), Standing::Idle);
        assert_eq!(s.standing(false, true), Standing::Queued);
        assert_eq!(s.standing(true, false), Standing::Running);
        // busy 和 queued 同时为真时，在跑赢（它确实在跑）
        assert_eq!(s.standing(true, true), Standing::Running);
    }

    /// **`Standing` 只有三种值**：一条会话永远可以继续，所以没有终态。
    /// 这条钉住「闲着是唯一的不在跑状态」。
    #[test]
    fn standing_has_exactly_three_states() {
        let all = [Standing::Running, Standing::Queued, Standing::Idle];
        let names: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
        assert_eq!(names, vec!["working", "queued", "idle"]);
    }

    /// **`Queued` 不算「等你」。** 派了活、还没轮到，那不是等你，是等它。
    ///
    /// 旧架构 `Stage::needs_person()` 对 `Queued` 也是 false；一版实现里
    /// 用 `!busy` 之外再加一个条件判定，于是 `dispatch` 之后、回合真正开跑之前
    /// 那个窗口里会话被算进「等你」—— 计数会抖。
    #[test]
    fn a_queued_session_does_not_need_a_person() {
        assert!(!Standing::Queued.needs_person(), "排队中不该算等人");
        assert!(Standing::Idle.needs_person(), "停下了才等人");
        assert!(!Standing::Running.needs_person());
    }

    /// `Option<Dispatch>` 让「半份派活记录」写不出来：要么整份有、要么没有。
    #[test]
    fn a_dispatch_record_is_all_or_nothing() {
        let mut s = ChatSession::with_role(Role::Worker);
        assert_eq!(s.dispatched_by(), None);
        assert_eq!(s.brief(), None);
        assert_eq!(s.dispatch_times(), None);

        s.dispatch = Some(Dispatch::new("router-1", "把这件事干了"));
        assert_eq!(s.dispatched_by(), Some("router-1"));
        assert_eq!(s.brief(), Some("把这件事干了"));
        assert_eq!(s.dispatch_times(), Some(1));

        // 追加：原文留着，次数不动（次数由 reopen_session 记）
        s.dispatch.as_mut().unwrap().append("补充一句");
        assert_eq!(s.dispatch_times(), Some(1), "append 不该动次数");
        let b = s.brief().unwrap();
        assert!(b.contains("把这件事干了") && b.contains("补充一句"), "{b}");
    }

    use super::*;

    /// 侧边栏分页：游标翻页不重不漏、顺序和全量一致。
    ///
    /// 这条挡的是「用 offset 分页」那个坑 —— 翻页期间新建会话会插到最前面，
    /// offset 会让下一页重复一行、漏掉一行。
    #[test]
    fn session_list_pages_by_cursor() {
        let mut store = SessionStore::default();
        // 5 条会话，创建时间递减（越新越靠前）。故意有两对撞同一毫秒的
        for i in 0..5u64 {
            let mut s = ChatSession::new();
            s.created_at = 1000 - (i / 2);
            s.updated_at = s.created_at;
            s.title = format!("会话{i}");
            // 有内容才进列表
            s.messages.push(ChatMessage::user(format!("hi {i}")));
            store.sessions.push(s);
        }
        let all = store.list();
        assert_eq!(all.len(), 5, "5 条都该在列表里");

        // 一页 2 条：按游标一页页翻，拼起来必须和全量**逐条相同**
        let mut seen: Vec<String> = vec![];
        let mut cursor: Option<(u64, String)> = None;
        loop {
            let (rows, more, next) =
                store.list_page(cursor.as_ref().map(|(t, id)| (*t, id.as_str())), 2);
            assert!(rows.len() <= 2);
            // 第一次的游标就是这一页最后一条
            if let Some(c) = &next {
                let (ts, id) = c.split_once(':').unwrap();
                cursor = Some((ts.parse().unwrap(), id.to_string()));
            }
            seen.extend(rows.iter().map(|r| r["id"].as_str().unwrap().to_string()));
            if !more {
                break;
            }
            assert!(next.is_some(), "还有下一页就必须给游标");
        }
        let expect: Vec<String> = all
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(seen, expect, "翻完所有页 = 全量，不重不漏");

        // `limit = 0` 是老行为：一次全给
        let (rows, more, next) = store.list_page(None, 0);
        assert_eq!(rows.len(), 5);
        assert!(!more);
        assert!(next.is_none());
    }

    /// 摘要窗口之外，**最原始的那版**实现（一律全量小写化 + 双向 range）。
    ///
    /// 只用来对拍：新实现三条路径（原样命中 / 滑动窗口 / 全量折叠）在下面所有
    /// 用例上必须和它给出**逐字相同**的结果 —— 搜索摘要长什么样是用户直接看到的，
    /// 不能因为性能改动就变。
    fn reference(text: &str, needle: &str) -> Option<String> {
        const BEFORE: usize = 20;
        const AFTER: usize = 60;
        let raw: Vec<char> = text.chars().collect();
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

    /// 三条路径 + 一堆边界，和原实现逐字对拍。
    #[test]
    fn snippet_matches_the_original_implementation() {
        let long_cn = "前面有很多字".repeat(20);
        let cases = [
            ("第一行\n这里有 KEYWORD 在中间", "KEYWORD"),
            ("第一行\n这里有 KEYWORD 在中间", "keyword"),
            ("只有关键字", "关键"),
            ("没有命中", "zzz"),
            ("任意", ""),
            ("prefix HelloWorld suffix", "helloworld"),
            ("prefix HelloWorld suffix", "HelloWorld"),
            ("前缀 İstanbul 后缀", "istanbul"),
            ("前缀 İstanbul 后缀", "İstanbul"),
            ("İX", "x"),
            ("ＡＢＣ 全角", "ＡＢＣ"),
            ("İİİİİİİİİİ target İİİİİİİİİİ", "target"),
            (&format!("{long_cn}\n目标字串\n{long_cn}"), "目标字串"),
            (&format!("{long_cn}{long_cn}{long_cn}tail"), "tail"),
            (&format!("{long_cn}中间{long_cn}"), "中间"),
            ("\n\n\n换行开头和结尾\n\n", "换行"),
            ("aaaa", "a"),
            ("😀emoji😀", "emoji"),
            ("😀emoji😀", "😀"),
        ];
        for (text, needle) in cases {
            assert_eq!(
                hit_snippet(text, needle),
                reference(text, needle),
                "和原实现不一致：text={text:?} needle={needle:?}"
            );
        }
    }

    /// 用户看得见的那几条行为（不依赖对拍也能读懂）。
    #[test]
    fn snippet_shows_context_and_folds_case() {
        // 长文本：前后都截了就都带省略号，换行拍平成空格
        let long = format!("{}KEYWORD{}", "前".repeat(40), "后".repeat(200));
        let s = hit_snippet(&long, "KEYWORD").unwrap();
        assert!(s.contains("KEYWORD"), "摘要里必须有命中词：{s}");
        assert!(!s.contains('\n'), "摘要不能带换行：{s}");
        assert!(
            s.starts_with('…') && s.ends_with('…'),
            "两头都要标截断：{s}"
        );

        // 短文本：整条都在窗口里，不多加省略号
        assert_eq!(hit_snippet("只有关键字", "关键").unwrap(), "只有关键字");
        assert!(hit_snippet("没有命中", "zzz").is_none());
        assert!(hit_snippet("任意", "").is_none(), "空查询不命中");

        // 忽略大小写（滑动窗口那条路），但摘要回原文
        let s = hit_snippet("prefix HelloWorld suffix", "helloworld").unwrap();
        assert!(s.contains("HelloWorld"), "要回原文，不是小写版：{s}");

        // 多字节字符不能在窗口里被切碎
        let body = "前面一堆字".repeat(10);
        let s = hit_snippet(&format!("{body}目标字串{body}"), "目标字串").unwrap();
        assert!(s.contains("目标字串"));
        assert!(!s.contains('\u{FFFD}'), "不能出现替换符（切碎了）：{s}");
    }

    /// 随机对拍：固定种子的伪随机文本 + 查询，逐例和原实现比。
    ///
    /// 手写用例只能覆盖想到的情况；三层路径（原样 / 字节折叠 / 精确折叠）
    /// 边界多，靠随机拼（中英混排、大小写、全角、emoji、换行、控字符）能兜住
    /// 「某一层漏判」这类回归 —— 这正是这次改动最容易出事的地方。
    #[test]
    fn snippet_matches_the_original_on_random_input() {
        // xorshift：不引依赖，结果可复现
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "abAB中文İıＡＡéÉ😀XYZ\n\t 12，。ǅǈΣσςϴᲐႠＡＢａｂ𝔘𝕒"
            .chars()
            .collect();
        for round in 0..5000 {
            let len = (rnd() % 120) as usize;
            let text: String = (0..len)
                .map(|_| alphabet[(rnd() as usize) % alphabet.len()])
                .collect();
            let qlen = 1 + (rnd() % 4) as usize;
            let needle: String = (0..qlen)
                .map(|_| alphabet[(rnd() as usize) % alphabet.len()])
                .collect();
            assert_eq!(
                hit_snippet(&text, &needle),
                reference(&text, &needle),
                "第 {round} 轮不一致：text={text:?} needle={needle:?}"
            );
        }
    }

    /// 搜索是按会话排的：命中的会话要带角色和片段，`per_session` / `limit` 生效。
    #[test]
    fn search_reports_hits_with_role_and_respects_limits() {
        let mut store = SessionStore::default();
        let id = store.create();
        let s = store.get_mut(&id).unwrap();
        s.messages.push(ChatMessage::user("找一个 needle 吧"));
        s.messages.push(ChatMessage::Assistant {
            content: "这里也有 needle".into(),
            reasoning: String::new().into(),
            tool_calls: Vec::new(),
            reasoning_items: Vec::new(),
        });
        s.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "tool 里的 needle 不算".into(),
            images: vec![],
        });

        let hits = store.search("needle", 3, 60, |_| None);
        assert_eq!(hits.len(), 2, "工具结果不参与搜索：{hits:?}");
        let roles: Vec<&str> = hits.iter().map(|h| h["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["user", "assistant"], "按会话里的顺序返回");

        // 每个会话只回一条时，第二条不该出现
        assert_eq!(store.search("needle", 1, 60, |_| None).len(), 1);
        assert!(store.search("   ", 3, 60, |_| None).is_empty(), "空查询");
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
            reasoning: String::new().into(),
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
            let bash = &tools(p, Role::Worker)
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
        let mac = &tools(Platform::Macos, Role::Worker)[1].description;
        assert!(
            mac.contains("brew") && mac.contains("BSD"),
            "macOS 要点名 BSD 用户态"
        );
        assert!(!tools(Platform::Linux, Role::Worker)[1]
            .description
            .contains("brew"));
    }

    /// **回执永远给全文，一条都不降级。**
    ///
    /// 用户的红线是「不许截断」，而且明确否决了「降到历史里换指针」这种做法 ——
    /// 那套（`KEEP_REPORTS` / `ReportBudget` / `demoted`)已经整个删掉了。
    /// 这个测试钉住的是：历史里哪怕堆了几十条回执，**每一条都原样出现在请求里**，
    /// 没有任何一条被换成指针或摘要。
    ///
    /// 上下文膨胀是另一个问题（以后单独一轮解决），不该以牺牲「看得见全文」为代价。
    #[test]
    fn every_report_is_sent_in_full_no_matter_how_many() {
        let mut sess = ChatSession::with_role(Role::Router);
        ensure_system_prompt(&mut sess, crate::prompt::router_description());

        // 塞 20 条（远超任何「保留最近 N 条」的阈值，如果还有那种逻辑就会露出来）
        const N: usize = 20;
        for i in 0..N {
            sess.messages.push(ChatMessage::from_worker(
                format!(
                    "<worker_report session=\"s{i}\" failed=\"false\">\n\
                     第 {i} 条的正文，这一句必须原样出现在请求里。\n\
                     </worker_report>"
                ),
                format!("s{i}"),
            ));
        }

        let joined: String = build_messages(&sess, true, false)
            .unwrap()
            .iter()
            .map(|m| match m {
                Message::User {
                    content: UserContent::Text(t),
                } => t.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        for i in 0..N {
            assert!(
                joined.contains(&format!("第 {i} 条的正文")),
                "第 {i} 条被降级/截断了 —— 回执必须永远全文"
            );
        }
        assert!(
            !joined.contains("demoted"),
            "不该再有任何降级标记：{joined}"
        );
        assert!(
            !joined.contains("已不在上下文里"),
            "不该再有任何替指针文案：{joined}"
        );

        // Responses API 那条路也得一样
        let input = build_responses_input(&sess, true, false);
        let wire = serde_json::to_string(&input).unwrap();
        for i in 0..N {
            assert!(
                wire.contains(&format!("第 {i} 条的正文")),
                "Responses 路径上第 {i} 条也该是全文"
            );
        }
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

    /// **空的 Router 会话也要出现在侧边栏列表里。**
    ///
    /// 普通空会话不进列表是对的（「新对话」点了没说话不该占位），但 Router 必须
    /// 例外：它是双层架构的入口，用户得能点回它；而它通常就是空的（活由 worker 干，
    /// 它自己不攒消息）。不例外的话界面会显示「还没有对话」—— Router 明明在，
    /// 地址栏也指着它。
    #[test]
    fn an_empty_router_is_listed_but_an_empty_worker_is_not() {
        let mut st = SessionStore::default();
        // 空的 worker（不该出现）、空的 router（该出现）、一个有内容的 worker
        let empty_worker = st.create();
        let router = ChatSession::with_role(Role::Router);
        let rid = st.push_new(router);
        let busy = st.create();
        st.get_mut(&busy)
            .unwrap()
            .messages
            .push(ChatMessage::user("干活"));

        let ids: Vec<String> = st
            .list_page(None, 0)
            .0
            .iter()
            .map(|v| v["id"].as_str().unwrap().to_string())
            .collect();

        assert!(ids.contains(&rid), "空的 Router 必须在列表里：{ids:?}");
        assert!(
            !ids.contains(&empty_worker),
            "空的 worker 不该在列表里：{ids:?}"
        );
        assert!(ids.contains(&busy), "有内容的 worker 要在：{ids:?}");
    }

    /// **Router 的工具表里绝不能有执行工具** —— 这是双层架构的命根子。
    ///
    /// 靠提示词说「你别自己干活」拦不住（DeepSeek 那篇的消融实验：中性提示下
    /// 模型 5 次运行一次都没委派）；拦住它的是「它手里根本没有那手」。这条测试
    /// 一旦红了，说明有人往 router 表里加了 read/bash/edit —— 那时候双层就
    /// 已经悄悄退化成一层了，而且在行为上完全看不出来。
    #[test]
    fn router_tools_cannot_execute() {
        for p in Platform::ALL {
            let names: Vec<&str> = tools(p, Role::Router).iter().map(|t| t.name).collect();
            for forbidden in ["bash", "read", "edit"] {
                assert!(
                    !names.contains(&forbidden),
                    "{p:?} 的 router 表里出现了 `{forbidden}`：协调层不能有执行能力"
                );
            }
            // 五个编排工具都在（少一个就是某个能力悄悄没了）：
            // 看（list_sessions / read_session）、派（dispatch）、定时（schedule_task / schedules）。
            // 并且**没有** `answer`：模型的正文本来就是给用户看的那句话，
            // 专门为说话开一个工具只会白多一轮请求。
            for required in [
                "list_sessions",
                "read_session",
                "dispatch",
                "schedule_task",
                "schedules",
            ] {
                assert!(
                    names.contains(&required),
                    "{p:?} 的 router 表里缺少 `{required}`"
                );
            }
            assert_eq!(names.len(), 5, "{p:?} 的 router 工具表只有五个：{names:?}");
            // 反过来的那一半：worker 表里不该有编排工具（它没有派活的对象）
            let worker: Vec<&str> = tools(p, Role::Worker).iter().map(|t| t.name).collect();
            for orchestration in [
                "list_sessions",
                "read_session",
                "dispatch",
                "schedule_task",
                "schedules",
            ] {
                assert!(
                    !worker.contains(&orchestration),
                    "{p:?} 的 worker 表里出现了 `{orchestration}`：那是协调层的事"
                );
            }
        }
    }

    /// Router 的提示词里**不能出现 worker 才有的东西**。
    ///
    /// 提示词里说有 `bash`、工具表里没有，是最坏的一种不一致：模型会一遍遍
    /// 试着调一个不存在的工具。
    #[test]
    fn router_prompt_does_not_promise_execution() {
        let prompt = crate::prompt::router_description();
        assert!(prompt.contains("dispatch"), "得告诉它怎么派活");
        assert!(prompt.contains("不亲自干活"), "得说清它自己不做");
        // 「没有 shell / 读文件 / 改文件的能力」这句是明说的，不是反话
        assert!(prompt.contains("没有 shell"), "得明说它没有 shell");
    }

    /// 前缀不变式对 **Router 也成立**：同一角色两轮之间必须是前缀关系。
    ///
    /// 工具表、提示词都按角色分槽，这里守的是「分槽之后没有串味」：router 的第
    /// 二轮请求必须以第一轮为前缀，且两轮的工具表都是 router 那张。
    #[test]
    fn router_request_is_prefix_stable_and_uses_router_tools() {
        let mut sess = ChatSession::with_role(Role::Router);
        ensure_system_prompt(&mut sess, crate::prompt::router_description());
        sess.messages.push(ChatMessage::user("帮我看看库在哪"));
        let turn1 = wire(&sess);

        sess.messages.push(ChatMessage::Assistant {
            content: "".into(),
            reasoning: String::new().into(),
            tool_calls: vec![RecordedToolCall {
                id: "c1".into(),
                name: "list_sessions".into(),
                arguments: "{}".into(),
            }],
            reasoning_items: Vec::new(),
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "c1".into(),
            content: "- abc 会话1".into(),
            images: vec![],
        });
        sess.messages.push(ChatMessage::user("那派给它"));
        let turn2 = wire(&sess);

        assert!(turn2.len() > turn1.len());
        assert_eq!(&turn2[..turn1.len()], &turn1[..], "router 的前缀也不许变");

        // 工具表也是 router 那张：没有一个执行工具
        let names: Vec<&str> = tools(Platform::Android, Role::Router)
            .iter()
            .map(|t| t.name)
            .collect();
        assert!(names.contains(&"dispatch"), "router 得能派活");
        assert!(!names.contains(&"bash"), "router 不执行");
    }

    #[test]
    fn tool_schemas_serialize_byte_identically() {
        let once = serde_json::to_string(
            &tools(Platform::Android, Role::Worker)
                .iter()
                .map(|t| (t.schema)())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let twice = serde_json::to_string(
            &tools(Platform::Android, Role::Worker)
                .iter()
                .map(|t| (t.schema)())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(once, twice);
    }

    /// **排队中的那一条也要带来源。**
    ///
    /// 以前 `queued_view` 不看 `origin`：一条等着交给模型的回执在界面上是一个
    /// 「用户气泡」，交出去之后才变成「子会话报告」—— 同一个东西在排队前后换了
    /// 个样子。定时器投的那条同理（`fromSchedule`）。
    ///
    /// 变异验证：把 `queued_view` 里那个 `match` 删掉，这条会挂。
    #[test]
    fn a_queued_message_carries_its_origin_to_the_client() {
        let queued = |id: &str, origin: Origin| crate::state::QueuedMsg {
            id: id.into(),
            text: "一句话".into(),
            attachments: Vec::new(),
            origin,
        };
        let view = queued_view(&[
            queued("q1", Origin::Human),
            queued(
                "q2",
                Origin::WorkerReport {
                    session_id: "s-1".into(),
                },
            ),
            queued(
                "q3",
                Origin::Schedule {
                    task_id: "t-1".into(),
                },
            ),
        ]);
        assert_eq!(view.len(), 3);
        assert!(view[0].get("fromWorker").is_none(), "人说的话没有来源标记");
        assert!(view[0].get("fromSchedule").is_none());
        assert_eq!(
            view[1]["fromWorker"],
            json!("s-1"),
            "排队中的回执也要标出来"
        );
        assert_eq!(
            view[2]["fromSchedule"],
            json!("t-1"),
            "排队中的闹钟也要标出来"
        );
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
                reasoning: String::new().into(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            },
            ChatMessage::user("U2"),
            ChatMessage::Assistant {
                content: "答2".into(),
                reasoning: String::new().into(),
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

    /// 一个长会话：`turns` 个回合，每个回合 user + assistant（思考 + 工具调用）
    /// + tool 结果 + assistant（正文）—— 视图里每回合就两条（user + 合并的 assistant）。
    fn long_session(turns: usize) -> ChatSession {
        let mut sess = ChatSession::new();
        sess.messages.push(ChatMessage::System("sys".into()));
        for t in 0..turns {
            sess.messages.push(ChatMessage::user(format!("问{t}")));
            sess.messages.push(ChatMessage::Assistant {
                content: String::new().into(),
                reasoning: format!("想{t}").into(),
                tool_calls: vec![RecordedToolCall {
                    id: format!("c{t}"),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                reasoning_items: Vec::new(),
            });
            sess.messages.push(ChatMessage::Tool {
                tool_call_id: format!("c{t}"),
                content: format!("输出{t}").into(),
                images: vec![],
            });
            sess.messages.push(ChatMessage::Assistant {
                content: format!("答{t}").into(),
                reasoning: String::new().into(),
                tool_calls: vec![],
                reasoning_items: Vec::new(),
            });
        }
        sess
    }

    /// 尾部窗口：起点必须是回合边界，给的**不能少于**要的（少了界面就缺内容），
    /// 也不能多给太多（多给一个回合就到顶了）。
    #[test]
    fn tail_window_starts_on_a_turn_boundary_and_covers_the_tail() {
        let sess = long_session(12);
        let full = session_view(&sess);
        let total = full.len();
        assert_eq!(total, 24, "12 个回合 × 2 条");

        for tail in 0..=total + 5 {
            let start = session_tail_start(&sess, tail);
            assert!(
                start == 0 || matches!(sess.messages[start], ChatMessage::User { .. }),
                "tail={tail} 的起点不在回合边界上"
            );
            let from = session_view_count_before(&sess, start);
            if tail == 0 {
                // `0` 是老语义：整段（脚本 / 调试那条路）——它不算「窗口」，不参与下面
                // 那个「不能多给」的上界
                assert_eq!(start, 0, "tail=0 必须给整段");
                continue;
            }
            let got = total - from;
            assert!(got >= tail.min(total), "tail={tail} 只给了 {got} 条");
            assert!(got <= tail.min(total) + 2, "tail={tail} 多给了 {got} 条");
            // 切出来的那一段必须和整段视图逐字对得上（合并规则一致）
            assert_eq!(
                session_view_range(&sess, start, sess.messages.len()),
                full[from..].to_vec(),
                "tail={tail} 的窗口和整段视图对不上"
            );
        }
        // `tail = 0` 是老语义：整段
        assert_eq!(
            session_view_count_before(&sess, session_tail_start(&sess, 0)),
            0
        );
    }

    // —— 侧边栏搜索 ——

    // —— 空会话 ——
}

#[cfg(test)]
mod bench_search {
    //! 搜索这条链的**量级**基准：不是断言的测试，是给人看的。
    //!
    //! `cargo test --release -- --ignored --nocapture search_cost`。
    //! 数据按省库实测规模造（40MB 正文、50k 条消息），省得靠感觉猜。
    use super::*;

    /// 造一段和真实会话同量级的历史（40MB 正文、约 5 万条消息）。
    fn realistic() -> SessionStore {
        let unit = "这是一段中文回复，在真实历史里通常是几百字到几千字不等。".repeat(10);
        let n = 40 * 1024 * 1024 / unit.len();
        let mut store = SessionStore::default();
        let id = store.create();
        let s = store.get_mut(&id).unwrap();
        s.messages.reserve(n);
        for i in 0..n {
            if i % 2 == 0 {
                s.messages.push(ChatMessage::user(unit.clone()));
            } else {
                s.messages.push(ChatMessage::Assistant {
                    content: unit.as_str().into(),
                    reasoning: String::new().into(),
                    tool_calls: Vec::new(),
                    reasoning_items: Vec::new(),
                });
            }
        }
        store
    }

    #[test]
    #[ignore = "量级基准，不是断言；手动跑 --ignored --nocapture"]
    fn search_cost_on_a_realistic_history() {
        let store = realistic();
        let total: usize = store
            .sessions
            .iter()
            .flat_map(|s| s.messages.iter())
            .map(|m| match m {
                ChatMessage::User { text, .. } => text.len(),
                ChatMessage::Assistant { content, .. } => content.len(),
                _ => 0,
            })
            .sum();
        println!("历史 {} MB / {} 条消息", total / 1048576, {
            store.sessions[0].messages.len()
        });

        // 命中（原样命中快路）：抽屉里最常见的「搜一个中文词」
        let t = std::time::Instant::now();
        let hits = store.search("中文回复", 3, 60, |_| None);
        println!("命中查询    ：{:?}（{} 条）", t.elapsed(), hits.len());

        // 不命中（要扫完全部）：打错字 / 找不存在的东西
        let t = std::time::Instant::now();
        let hits = store.search("不存在的词", 3, 60, |_| None);
        println!("不命中查询  ：{:?}（{} 条）", t.elapsed(), hits.len());

        // 大小写折叠那条路（ASCII 查询打中文历史，全都对不上 -> 走滑动窗口）
        let t = std::time::Instant::now();
        let hits = store.search("HELLO", 3, 60, |_| None);
        println!("大小写折叠  ：{:?}（{} 条）", t.elapsed(), hits.len());
    }

    // --- part 截断（首屏只给 400 字，点开面板再取全文） ---

    /// 造一条带 reasoning + tool 的会话，正文都超过 400 字。
    fn big_session() -> ChatSession {
        let mut sess = ChatSession::new();
        let reasoning = "想".repeat(1000);
        let content = "回答".repeat(10);
        let output = "输出行\n".repeat(2000);
        sess.messages.push(ChatMessage::user("问一句"));
        sess.messages.push(ChatMessage::Assistant {
            // 三个都不再用了：直接 move 进来，不用 clone
            content: content.into(),
            reasoning: reasoning.into(),
            tool_calls: vec![RecordedToolCall {
                id: "call_abc".into(),
                name: "bash".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            }],
            reasoning_items: vec![],
        });
        sess.messages.push(ChatMessage::Tool {
            tool_call_id: "call_abc".into(),
            content: output.into(),
            images: vec![],
        });
        sess
    }

    #[test]
    fn 首屏那条路会截断长正文() {
        let sess = big_session();
        let view = session_view_from_clipped(&sess, 0, PART_PREVIEW_CHARS);
        let parts = view[1]["parts"].as_array().expect("parts");
        let reasoning = parts
            .iter()
            .find(|p| p["type"] == "reasoning")
            .expect("有 reasoning");
        let tool = parts.iter().find(|p| p["type"] == "tool").expect("有 tool");

        // reasoning 仍然给前 400 字（那一行摘要要用它渲染）
        assert_eq!(reasoning["truncated"], serde_json::json!(true));
        assert_eq!(reasoning["chars"], serde_json::json!(1000));
        // **工具输出首屏一个字都不给**（前端从不显示它，只在面板里看）：
        // 只留 `chars`（面板显示「原文 N 字」）和 `failed`（工具行标红用）
        assert_eq!(tool["output"], serde_json::json!(""));
        assert_eq!(tool["chars"], serde_json::json!(8000));
        assert_eq!(tool["failed"], serde_json::json!(false));

        // 实际长度：400 字 + 一个省略号。**按字符不是按字节** —— 中文一个字 3 字节，
        // 按字节切会把汉字劈成两半。
        let rt = reasoning["text"].as_str().unwrap();
        assert_eq!(rt.chars().count(), PART_PREVIEW_CHARS + 1);
        assert!(rt.ends_with('…'));

        // 短的正文不动：这条 content 只有 20 字
        let text = parts.iter().find(|p| p["type"] == "text").expect("有 text");
        assert!(text.get("truncated").is_none());
        assert!(text.get("chars").is_none());
    }

    #[test]
    fn 失败判定和前端那套一致() {
        // 首屏不再传 output 正文，工具行要不要标红全靠这个布尔，
        // 所以它的判定规则必须和前端 `failed()` 一字不差。
        assert!(tool_failed("错误：no such file"));
        assert!(tool_failed("some output\n[exit 1]"));
        assert!(tool_failed("some output\n[exit 1]\n")); // 尾随空白
        assert!(!tool_failed("ok\n[exit 0]"));
        assert!(!tool_failed("就是一段没有退出码的输出"));
        assert!(!tool_failed(""));
        // 末尾那个才算；正文里提到 `[exit 1]` 不算
        assert!(!tool_failed("[exit 1] 这行在开头\n最后是 [exit 0]"));
    }

    #[test]
    fn 不截断那条路给全文() {
        // `session_view_from`（调试脚本 / 老客户端走的）必须还是全文
        let sess = big_session();
        let view = session_view_from(&sess, 0);
        let parts = view[1]["parts"].as_array().unwrap();
        let reasoning = parts.iter().find(|p| p["type"] == "reasoning").unwrap();
        assert!(reasoning.get("truncated").is_none());
        assert_eq!(reasoning["text"].as_str().unwrap().chars().count(), 1000);
    }

    #[test]
    fn 工具参数超长也截但摘要字段留着() {
        // 实测 `bash` 的参数合计能到 57 KB（30 条命令）。前端工具行只显示
        // `command` 的第一行，所以那个字段必须活下来 —— 否则整屏的工具行
        // 都变成「执行命令」。
        let mut sess = ChatSession::new();
        let long = format!(
            r#"{{"command":"ls -la\n{}","timeout":30}}"#,
            "x".repeat(4000)
        );
        sess.messages.push(ChatMessage::user("跑"));
        sess.messages.push(ChatMessage::Assistant {
            content: String::new().into(),
            reasoning: String::new().into(),
            tool_calls: vec![RecordedToolCall {
                id: "call_big".into(),
                name: "bash".into(),
                arguments: long,
            }],
            reasoning_items: vec![],
        });
        let view = session_view_from_clipped(&sess, 0, PART_PREVIEW_CHARS);
        let tool = &view[1]["parts"].as_array().unwrap()[0];
        assert_eq!(tool["inputTruncated"], json!(true));
        // 摘要字段还在，截到 LONG_FIELD_CHARS（前端 `summarize` 自己取第一行）
        let cmd = tool["input"]["command"].as_str().unwrap();
        assert!(cmd.starts_with("ls -la"));
        assert_eq!(cmd.chars().count(), LONG_FIELD_CHARS + 1); // + 省略号
                                                               // 整体没超 cap，所以不该出现 `__preview`
        assert!(tool["input"].get("__preview").is_none());
        assert_eq!(tool["inputChars"].as_u64().unwrap(), 4035);
        // 短参数的**原样保留**，不该被换成预览
        let mut small = ChatSession::new();
        small.messages.push(ChatMessage::Assistant {
            content: String::new().into(),
            reasoning: String::new().into(),
            tool_calls: vec![RecordedToolCall {
                id: "call_small".into(),
                name: "read".into(),
                arguments: r#"{"path":"/tmp/a"}"#.into(),
            }],
            reasoning_items: vec![],
        });
        let v2 = session_view_from_clipped(&small, 0, PART_PREVIEW_CHARS);
        let t2 = &v2[0]["parts"].as_array().unwrap()[0];
        assert!(t2.get("inputTruncated").is_none());
        assert_eq!(t2["input"]["path"], json!("/tmp/a"));
    }

    #[test]
    fn 取全文两种id都认() {
        let sess = big_session();
        // reasoning：`r{i}`，i 是 DB 下标（这条是 messages[1]）
        assert_eq!(
            part_full(&sess, "r1").unwrap()["text"],
            json!("想".repeat(1000))
        );
        // 正文：`t{i}`
        assert_eq!(
            part_full(&sess, "t1").unwrap()["text"],
            json!("回答".repeat(10))
        );
        // 不存在
        assert!(part_full(&sess, "nope").is_none());
        assert!(part_full(&sess, "r99").is_none());
    }

    #[test]
    fn 工具的参数和输出一次全给() {
        // 输入输出都长的工具，点开面板**一条请求**就该拿到两样。
        // 早先按 `?input=1` 拆成两条路，点一下并发两次往返 —— 同一份数据的两个
        // 字段没理由拆两次。
        let sess = big_session();
        let got = part_full(&sess, "call_abc").expect("有这个工具");
        assert_eq!(got["output"], json!("输出行\n".repeat(2000)));
        // 参数是解析好的 JSON（不是一段字符串）
        assert_eq!(got["input"]["command"], json!("ls"));
        // 两个字段在同一个响应里，调用方一次就够
        assert!(got.get("output").is_some() && got.get("input").is_some());
    }

    #[test]
    fn 补丁和首屏用同一个上限() {
        // 增量那条路也用同一个 cap：否则刷新拿到 400 字、流式拿到全文，
        // 用户会看到同一段思考在两个时刻长度不一样。
        let sess = big_session();
        let head = session_view_from_clipped(&sess, 0, PART_PREVIEW_CHARS);
        let patch = session_patch(&sess, 0);
        // replace_from 都是 0 时两边应该是一样的形状
        assert_eq!(patch.replace_from, 0);
        assert_eq!(patch.messages[1]["parts"], head[1]["parts"]);
    }
}
