//! 协调层（Router）的四个工具：**看**和**说**，没有**做**。
//!
//! 这个文件只放「工具被调用时干什么」。工具表的定义（名字、描述、schema）在
//! `agent::build_router_tools` + `prompt`，两边是一对一的关系：
//!
//! | 工具 | 在这儿的入口 | 干什么 |
//! | --- | --- | --- |
//! | `list_sessions` | [`list_sessions`] | 报有哪些会话、谁在跑 |
//! | `read_session` | [`read_session`] | 读某个会话最近几条，摘要不拉全文 |
//! | `dispatch` | [`dispatch`] | 把任务投进某个会话（不给 id 就新建） |
//!
//! ## 为什么 `dispatch` 走 `enqueue` 而不是直接 `run_turn`
//!
//! 复用 `AppState::enqueue` + `agent::continue_with_queued`，原因有三：
//!
//! 1. **回合串行、跨会话并行**这条不变式已经在那儿写好了：`enqueue` 只是把消息
//!    投进队列，`continue_with_queued` 先试 `begin_turn` 占坑、占不到就把消息留在
//!    队列里让正在跑的那个回合在步边界取走。`dispatch` 自己起回合的话，就得把这
//!    两条规则再写一遍，而且两处一定会走偏。
//! 2. **立即返回**：派完就结束，不等 Worker 做完 —— 这正是 4.1 节要的语义
//!    （Router 的回合不能被一个跑半小时的 Worker 卡住）。
//! 3. **不需要新概念**：没有「派工单」表、没有 Ticket。任务的归属就是
//!    「那条会话里的一条用户消息」，和工作台本来的模型完全一样。
//!
//! 代价：`dispatch` 的返回里拿不到 Worker 的最终结果 —— 结果以那条会话里新
//! 消息的形式存在，用户和 Router 下次唤醒时才看得到。这是有意的：**同步等待就是
//! 把双层塌成一层**。

use std::collections::HashSet;
use std::fmt::Write as _;

use serde_json::Value;

use crate::agent::Role;
use crate::state::{AppState, QueuedMsg};
use crate::Core;

/// 一次 `read_session` 最多看多少条（模型给大了就削到这个）。
const READ_LINES_MAX: usize = 30;
/// `read_session` 不给 `lines` 时看多少条。
const READ_LINES_DEFAULT: usize = 8;
/// `read_session` 里一条消息最多多少字（超了截断，带全文长度）。
const READ_LINE_CHARS: usize = 600;

/// 工具调用的结果：给模型看的那段文本。
pub type Reply = Result<String, String>;

/// `list_sessions`：有哪些会话、谁在跑、最近说了什么。
///
/// **不列 router 自己**（`role = Router` 那些）：Router 不需要看见自己，而且
/// 「派活给 Router」是个无意义的动作 —— 在这里就不给它这个机会。
pub async fn list_sessions(core: &Core) -> Reply {
    let state = &core.state;
    let busy: HashSet<String> = state.busy_sessions().into_iter().collect();
    let store = state.chat.read().await;
    let mut rows: Vec<&crate::agent::ChatSession> = store
        .sessions
        .iter()
        .filter(|s| s.role == Role::Worker && s.has_content())
        .collect();
    // 最近更新的排前面：Router 要「哪个还在动」一眼可见。和侧边栏按创建时间
    // 排不一样 —— 那是给人看的位置稳定，这是给模型看的相关性。
    rows.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

    if rows.is_empty() {
        return Ok("现在还没有任何会话。用 dispatch 不给 session_id 就能开一个。".to_string());
    }

    let mut out = String::from("会话（最近的在前）：\n");
    for s in rows {
        let title = if s.title.is_empty() {
            "新对话"
        } else {
            &s.title
        };
        let running = if busy.contains(&s.id) {
            "（正在跑）"
        } else {
            ""
        };
        // 最后一句话是「它做到哪了」最省 token 的近似：不去读全文，只取消息的
        // 正文头一段。消息不在内存里的会话（stub）就算了，不为了列表去读库。
        let last = latest_line(s);
        let _ = writeln!(
            out,
            "- `{id}` {title}{running}{last}",
            id = s.id,
            title = title,
            running = running,
            last = last,
        );
    }
    Ok(out)
}

/// 会话最后一条有内容的消息，压成一行（没有就空串）。
fn latest_line(s: &crate::agent::ChatSession) -> String {
    if !s.resident() {
        return String::new();
    }
    for m in s.messages.iter().rev() {
        let (who, text) = match m {
            crate::agent::ChatMessage::User { text, .. } => ("用户", text.as_str()),
            crate::agent::ChatMessage::Assistant { content, .. } => ("助手", content.as_ref()),
            _ => continue,
        };
        let t = text.trim();
        if t.is_empty() {
            continue;
        }
        return format!("，{who}最后说：{}", clip(t, 80));
    }
    String::new()
}

/// `read_session`：某个会话最近几条消息。
///
/// **按需从库里读**（`ensure_loaded`），读完不会把它常驻在内存里 —— Router 翻
/// 十个会话不该让内存涨十倍。这条是 DeepSeek 那篇 Inline & Spill 的最小版本：
/// 窗口里只放「最近几条 + 截断」，要看更多就再调一次。
pub async fn read_session(core: &Core, args: &Value) -> Reply {
    let state = &core.state;
    let id = args
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if id.is_empty() {
        return Err("缺少参数 session_id".to_string());
    }
    let lines = args
        .get("lines")
        .and_then(Value::as_u64)
        .map_or(READ_LINES_DEFAULT, |n| {
            (n as usize).clamp(1, READ_LINES_MAX)
        });

    // 权限：只能看 worker。看 router（包括自己）没有意义，也不该能看。
    {
        let store = state.chat.read().await;
        match store.get(id) {
            Some(s) if s.role == Role::Worker => {}
            Some(_) => return Err(format!("`{id}` 是协调层会话，不归你读")),
            None => return Err(format!("没有这个会话: `{id}`")),
        }
    }
    // 消息可能不在内存里（stub）：读回来。回合正在跑的那个会话已经在内存里，
    // `ensure_loaded` 对它是空操作。
    state
        .ensure_loaded(id)
        .await
        .map_err(|e| format!("读会话失败：{e:#}"))?;

    let busy = state.busy_sessions().iter().any(|s| s == id);
    let store = state.chat.read().await;
    let Some(s) = store.get(id) else {
        return Err(format!("没有这个会话: `{id}`"));
    };
    let title = if s.title.is_empty() {
        "新对话"
    } else {
        &s.title
    };
    let total = s.message_count();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "会话「{title}」（`{id}`）{run}，共 {total} 条消息。最近 {lines} 条：",
        run = if busy { "，正在跑" } else { "" },
    );
    if !s.resident() {
        // 读过了还是 stub：说明这会话没有消息（不该发生，但别骗模型）
        out.push_str("（没有可读的消息）\n");
        return Ok(out);
    }
    let msgs = &s.messages;
    let start = msgs.len().saturating_sub(lines);
    if start > 0 {
        let _ = writeln!(out, "（前面还有 {start} 条没显示）");
    }
    for m in &msgs[start..] {
        match m {
            crate::agent::ChatMessage::User { text, .. } => {
                let _ = writeln!(out, "- 用户：{}", clip(text.trim(), READ_LINE_CHARS));
            }
            crate::agent::ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                let c = content.trim();
                if !c.is_empty() {
                    let _ = writeln!(out, "- 助手：{}", clip(c, READ_LINE_CHARS));
                } else if !tool_calls.is_empty() {
                    let names: Vec<&str> = tool_calls.iter().map(|t| t.name.as_str()).collect();
                    let _ = writeln!(out, "- 助手：调用 {}", names.join("、"));
                } else if !reasoning.trim().is_empty() {
                    let _ = writeln!(out, "- 助手：（在思考）");
                }
            }
            // 工具结果是它自己的噪音，Router 只要知道「它做到哪了」，不要管道原文
            crate::agent::ChatMessage::Tool { .. } => {}
            crate::agent::ChatMessage::System(_) => {}
        }
    }
    Ok(out)
}

/// `dispatch`：把任务投进某个会话（不给 id 就新建一个）。
///
/// 返回**立即**给出 —— 任务只是排进了那条会话的队列。真正开跑由
/// `continue_with_queued` 负责（它会先试 `begin_turn`；那条会话正忙就把消息留在
/// 队列里，等它的回合在步边界取）。
pub async fn dispatch(core: &Core, args: &Value) -> Reply {
    dispatch_from(core, "", args).await
}

/// `dispatch` 的实现。`by` 是派活的那个会话（Router）；空串 = 不知道（直接调的测试）。
///
/// 分成两层是因为「谁派的」不在工具参数里（模型不该能指定它）—— 它由
/// **调用这个工具的会话**决定，从 `execute_tool` 的 `sid` 传进来。
pub async fn dispatch_from(core: &Core, by: &str, args: &Value) -> Reply {
    let state = &core.state;
    let dispatch_id = uuid::Uuid::now_v7().to_string();
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if task.is_empty() {
        return Err("缺少参数 task".to_string());
    }
    let want = args
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // **先看有没有模型**，再动会话：没配模型时把活派出去是「派了也没人跑」，
    // 那就不该先建一个空会话、再把它丢在那儿（它没内容、不进列表，只在内存里
    // 占一个位置 —— 用户看不见，也删不掉）。顺序反过来的话，连点几次 dispatch
    // 就攒一堆看不见的空 worker。
    let Some(provider) = state.llm() else {
        return Err("还没配置模型，派出去的活没人能跑（先去设置里配一个）".to_string());
    };

    // 找目标会话，没有就新建一个 worker。
    let (sid, created) = {
        let mut store = state.chat.write().await;
        if want.is_empty() {
            let id = store.create(); // = worker（见 `SessionStore::create`）
            if !title.is_empty() {
                if let Some(s) = store.get_mut(&id) {
                    s.title = title.chars().take(60).collect();
                }
            }
            (id, true)
        } else {
            match store.get(&want) {
                Some(s) if s.role == Role::Worker => (want.clone(), false),
                Some(_) => return Err(format!("`{want}` 是协调层会话，不能往里派活")),
                None => return Err(format!("没有这个会话: `{want}`")),
            }
        }
    };
    state.mark_sessions_dirty();

    // 投进队列（`enqueue` 的唯一入口，见它和 `continue_with_queued` 的注释）。
    let msg = QueuedMsg {
        id: uuid::Uuid::now_v7().to_string(),
        text: task.to_string(),
        attachments: Vec::new(),
    };
    if state.enqueue(&sid, msg).is_err() {
        return Err(format!(
            "那个会话排队等着交给模型的消息太多了（上限 {}），等它跑完再派",
            AppState::MAX_QUEUED
        ));
    }

    // 记一笔「谁派出去的」：Worker 做完要把结果送回 Router（`report_back`），
    // 而一个回合可能跑很久、中间核心被杀过 —— 只在内存里记的话，重启后这条活
    // 就断了。落库之后重启也能接上。
    if let Some(db) = state.db() {
        if let Err(e) = db.set_dispatch(&sid, by, &dispatch_id, crate::agent::now_ms()) {
            tracing::warn!("记派工失败（不影响派活，只是做完可能回不到 Router）: {e:#}");
        }
    }

    // 试着立刻开跑：那条会话空着就现在开，正忙就把消息留在队列里等它。
    // `continue_with_queued` 自己会先试 `begin_turn`，所以这里不需要判断忙不忙。
    crate::agent::continue_with_queued(core, &provider, &sid);

    Ok(if created {
        format!("已经开了一个新会话 `{sid}`，把这件事交给它去做；做完会有消息回来。")
    } else {
        format!("已经派给 `{sid}`，它会接着那边的上下文做；做完会有消息回来。")
    })
}

/// Worker 的一个回合结束了：把结果送回派它的那个 Router。
///
/// 这是双层的**闭环**，也是唯一一处 Worker 能「主动」影响 Router 的地方。没有它，
/// `dispatch` 就是单向的 —— Router 派出去、再也听不到回音，用户得自己去翻每个人的
/// 会话才知道做完了没有。
///
/// 什么时候不送：
/// - 这个会话不是被派出去的（`dispatched` 表里没有它）—— 用户自己开的会话不该
///   凭空往 Router 里塞消息；
/// - 它派出去的 Router 会话已经没了；
/// - 这一回合**失败了**也送：失败正是 Router 最该知道的（照 `notify` 那条路，
///   失败不送的话 Router 会一直以为它还在干）。
///
/// 送法：往 Router 的队列里排一条（和 `dispatch` 派活走同一条路）—— Router 空着
/// 就立刻开一轮，正忙就在步边界取。**不直接调 `run_turn`**：那样就绕过了
/// 串行/并行那套规则。
pub async fn report_back(core: &Core, from: &str, said: &str, ok: bool) {
    let state = &core.state;
    let Some(db) = state.db() else { return };
    let Ok(Some((router, _dispatch_id))) = db.dispatch_of(from) else {
        return; // 不是派出去的会话（或没有库）：不关它的事
    };
    // Router 还在不在、还是不是 router
    {
        let store = state.chat.read().await;
        match store.get(&router) {
            Some(s) if s.role == Role::Router => {}
            _ => return,
        }
    }
    let title = {
        let store = state.chat.read().await;
        store
            .get(from)
            .map(|s| {
                if s.title.is_empty() {
                    "新对话".to_string()
                } else {
                    s.title.clone()
                }
            })
            .unwrap_or_else(|| "一个会话".to_string())
    };
    // 给它一段能直接看懂的话：谁的、成没成、说了什么。
    let body = if ok {
        format!("「{title}」做完了，说：\n{said}")
    } else {
        format!("「{title}」这一轮失败了，说：\n{said}")
    };
    let msg = QueuedMsg {
        id: uuid::Uuid::now_v7().to_string(),
        text: body,
        attachments: Vec::new(),
    };
    if state.enqueue(&router, msg).is_err() {
        tracing::warn!("Router 的队列满了，这条回执丢了（{from} → {router}）");
        return;
    }
    // 和 `dispatch` 一样：能开就跑，正忙就排队。
    if let Some(provider) = state.llm() {
        crate::agent::continue_with_queued(core, &provider, &router);
    }
}

/// 把一段文本压到 `max` 字（按字符，不按字节 —— 中文不该被切在半个字上）。
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…（共 {n} 字）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ChatMessage, ChatSession};

    fn session() -> ChatSession {
        ChatSession::new()
    }

    /// `dispatch` 不给 session_id 时**建一个 worker**（不是 router），
    /// 并且把任务排进它的队列 —— 这是 L1 真正的功能点。
    ///
    /// 配一个假 provider：`continue_with_queued` 会真的去开回合（然后因为连不上
    /// 而失败，那是它的事，与本测试无关）；这里只验证「会话建对了、消息排进去了」。
    #[tokio::test]
    async fn dispatch_creates_a_worker_and_queues_the_task() {
        let core = Core::new();
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));

        let out = dispatch(&core, &serde_json::json!({ "task": "把 README 改一下" }))
            .await
            .expect("配了模型就该派得出去");

        // 新建的那个会话：是 worker，而且是唯一一个
        let store = core.state.chat.read().await;
        assert_eq!(store.sessions.len(), 1, "应当正好建了一个会话");
        let s = &store.sessions[0];
        assert_eq!(s.role, Role::Worker, "派活的目标必须是 worker");
        assert!(out.contains(&s.id), "返回里要写明派给了谁：{out}");
    }

    /// 没配模型时 `dispatch` 必须**什么都不建**，直接报错。
    ///
    /// 反过来的话，连点几次就攒一堆没内容的空 worker：它们不进侧边栏（空会话
    /// 不列），用户看不见也删不掉，只在内存和下次落盘时碍事。
    #[tokio::test]
    async fn dispatch_without_a_model_creates_nothing() {
        let core = Core::new();
        let err = dispatch(&core, &serde_json::json!({ "task": "随便干点什么" }))
            .await
            .expect_err("没模型就不该派得出去");
        assert!(err.contains("模型"), "错误里要说是模型的事：{err}");
        assert!(
            core.state.chat.read().await.sessions.is_empty(),
            "派不出去就不该留下空会话"
        );
    }

    /// `dispatch` 不能往 router 会话里派活 —— 那会让协调层自己变成执行者。
    #[tokio::test]
    async fn dispatch_refuses_to_target_a_router_session() {
        let core = Core::new();
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        let router = {
            let mut store = core.state.chat.write().await;
            let s = crate::agent::ChatSession::with_role(Role::Router);
            let id = s.id.clone();
            store.sessions.push(s);
            id
        };
        let err = dispatch(
            &core,
            &serde_json::json!({ "task": "派给我自己", "session_id": router }),
        )
        .await
        .expect_err("不能派给 router");
        assert!(err.contains("协调层"), "错误要说清为什么不行：{err}");
    }

    /// `read_session` 读不到 router 会话（那是它自己的层），也读不到不存在的会话。
    #[tokio::test]
    async fn read_session_refuses_routers_and_unknown_ids() {
        let core = Core::new();
        let router = {
            let mut store = core.state.chat.write().await;
            let s = crate::agent::ChatSession::with_role(Role::Router);
            let id = s.id.clone();
            store.sessions.push(s);
            id
        };
        let e = read_session(&core, &serde_json::json!({ "session_id": router }))
            .await
            .expect_err("不该能读 router");
        assert!(e.contains("协调层"), "{e}");

        let e = read_session(&core, &serde_json::json!({ "session_id": "没有这个" }))
            .await
            .expect_err("不该能读不存在的会话");
        assert!(e.contains("没有这个会话"), "{e}");
    }

    /// `list_sessions` **不列 router 自己** —— 它不需要看见自己，也不该有
    /// 「派活给 router」的念头。同时 worker 要列出来。
    #[tokio::test]
    async fn list_sessions_hides_the_router_and_shows_workers() {
        let core = Core::new();
        let (worker, router) = {
            let mut store = core.state.chat.write().await;
            let w = store.create();
            store
                .get_mut(&w)
                .unwrap()
                .messages
                .push(crate::agent::ChatMessage::user("干活"));
            let r = crate::agent::ChatSession::with_role(Role::Router);
            let rid = r.id.clone();
            store.sessions.push(r);
            (w, rid)
        };
        let out = list_sessions(&core).await.unwrap();
        assert!(out.contains(&worker), "worker 要列出来：{out}");
        assert!(!out.contains(&router), "router 不该出现在自己的列表里");
    }

    /// 派出去的活做完，要能把结果**送回 Router**。
    ///
    /// 这是双层唯一一条反向的路（Worker → Router）。没有它，`dispatch` 就是
    /// 单向的：Router 派出去、再也听不到回音。
    /// 带库的核心（`report_back` 要查派工记录，没库就什么都不做）。
    ///
    /// **不配 provider**：`report_back` 末尾会尝试 `continue_with_queued` 开一轮，
    /// 配了的话真回合会立刻把队列取走（连不上端点、然后失败），测试就查不到那条
    /// 回执了。不配 provider 时它什么都不做 —— 队列原样留着，正好可以断言。
    fn core_with_db(tag: &str) -> Core {
        let core = Core::new();
        let dir = std::env::temp_dir().join(format!("agent-router-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        core.set_data_dir(dir);
        core
    }

    #[tokio::test]
    async fn a_dispatched_worker_reports_back_to_its_router() {
        let core = core_with_db("report");
        // 一个 router + 一个 worker，并在库里记下「worker 是 router 派的」
        let (router, worker) = {
            let mut store = core.state.chat.write().await;
            let r = crate::agent::ChatSession::with_role(Role::Router);
            let rid = r.id.clone();
            store.sessions.push(r);
            let w = store.create();
            // worker 得有点内容，不然它不进列表、也不像一个真的干了活的会话
            store
                .get_mut(&w)
                .unwrap()
                .messages
                .push(crate::agent::ChatMessage::user("干活"));
            (rid, w)
        };
        core.state
            .db()
            .unwrap()
            .set_dispatch(&worker, &router, "d-1", 0)
            .unwrap();

        report_back(&core, &worker, "改好了，PR 已提交", true).await;

        // Router 的队列里该有一条回执
        let q = core.state.queued_list(&router);
        assert_eq!(q.len(), 1, "Router 没收到回执（队列长度 {}）", q.len());
        assert!(
            q[0].text.contains("改好了"),
            "回执要带上 Worker 说的话：{}",
            q[0].text
        );
        assert!(
            q[0].text.contains("做完"),
            "成功的回执要说做完了：{}",
            q[0].text
        );
    }

    /// 不是派出去的会话（用户自己开的）不该凭空往 Router 里塞消息。
    #[tokio::test]
    async fn an_undispatched_session_reports_nowhere() {
        let core = Core::new();
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        let (router, worker) = {
            let mut store = core.state.chat.write().await;
            let r = crate::agent::ChatSession::with_role(Role::Router);
            let rid = r.id.clone();
            store.sessions.push(r);
            (rid, store.create())
        };
        // **不**记派工：这个会话是自己开的
        report_back(&core, &worker, "我干完了", true).await;
        assert!(
            core.state.queued_list(&router).is_empty(),
            "不是派出去的会话不该有回执"
        );
    }

    /// 失败的回合**也要**回报 —— 失败正是 Router 最该知道的。
    #[tokio::test]
    async fn a_failed_turn_reports_back_as_a_failure() {
        let core = core_with_db("fail");
        let (router, worker) = {
            let mut store = core.state.chat.write().await;
            let r = crate::agent::ChatSession::with_role(Role::Router);
            let rid = r.id.clone();
            store.sessions.push(r);
            let w = store.create();
            store
                .get_mut(&w)
                .unwrap()
                .messages
                .push(crate::agent::ChatMessage::user("干活"));
            (rid, w)
        };
        core.state
            .db()
            .unwrap()
            .set_dispatch(&worker, &router, "d-2", 0)
            .unwrap();

        report_back(&core, &worker, "连不上数据库", false).await;
        let q = core.state.queued_list(&router);
        assert_eq!(q.len(), 1);
        assert!(
            q[0].text.contains("失败"),
            "失败要说明是失败：{}",
            q[0].text
        );
    }

    #[test]
    fn clip_counts_chars_not_bytes() {
        // 10 个汉字 = 30 字节；按字节切会切出半个字
        let s = "一二三四五六七八九十";
        assert_eq!(clip(s, 5), "一二三四五…（共 10 字）");
        assert_eq!(clip("abc", 5), "abc");
    }

    #[test]
    fn latest_line_skips_empty_and_tools() {
        let mut s = session();
        s.messages.push(ChatMessage::User {
            text: "先问一句".into(),
            attachments: Vec::new(),
        });
        // 一条没有正文、只有工具调用的助手消息，不该被当成「最后说的话」
        s.messages.push(ChatMessage::Assistant {
            content: "".into(),
            reasoning: "".into(),
            tool_calls: vec![crate::agent::RecordedToolCall {
                id: "t".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            reasoning_items: Vec::new(),
        });
        let line = latest_line(&s);
        assert!(line.contains("用户"), "{line}");
        assert!(line.contains("先问一句"), "{line}");
    }

    #[test]
    fn latest_line_is_empty_for_a_resident_session_with_nothing_said() {
        // stub 会话（消息不在内存）不为了列表去读库
        let mut s = session();
        s.messages.clear();
        s.stub = Some(crate::agent::Stub { len: 3, users: 1 });
        assert_eq!(latest_line(&s), "");
    }
}
