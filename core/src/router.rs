//! 协调层（Router）的工具：**看工作项**、**派工作项**，没有「做」。
//!
//! 这个文件只放「工具被调用时干什么」。工具表的定义（名字、描述、schema）在
//! `agent::build_router_tools` + `prompt`，两边是一对一的关系：
//!
//! | 工具 | 在这儿的入口 | 干什么 |
//! | --- | --- | --- |
//! | `list_tickets` | [`list_tickets`] | 有哪些工作项、各自卡在哪 |
//! | `read_ticket` | [`read_ticket`] | 读一个工作项的详情（含结果全文、执行记录） |
//! | `dispatch` | [`dispatch`] | 开一个工作项并派出去（给了 ticket_id 就是再派一次） |
//!
//! ## 为什么是「工作项」而不是「会话」
//!
//! 一开始这条路是错的：`dispatch` 往一条会话里塞一条消息就算派了活，Router 只能
//! 靠 `list_sessions` 看到「这条会话忙不忙」。而会话是容器 —— 没有状态、没有产出
//! 契约、没有终点。Router 派完就对那件事一无所知。
//!
//! 现在派出去的是 `ticket::Ticket`：有标题、有状态机、有产出契约、有终点。
//! 会话只是它的**载体**（`session_id`）。
//!
//! ## 引用图
//!
//! 每个条目都带 `read: ticket/<id>` 指针（照 dsh 的 `ticketCard`），模型能顺着
//! 从列表走到详情、再走到执行记录。**走一步就断的列表等于没有列表**。
//!
//! ## 为什么 `dispatch` 走 `enqueue` 而不是直接 `run_turn`
//!
//! 复用「回合串行、跨会话并行」那条不变式：`enqueue` 只是把消息投进队列，
//! `continue_with_queued` 先试 `begin_turn` 占坑、占不到就把消息留在队列里让正在
//! 跑的那个回合在步边界取走。自己起回合就得把那两条规则再写一遍，而且两处一定
//! 会走偏。

use std::fmt::Write as _;

use serde_json::Value;

use crate::agent::Role;
use crate::state::{AppState, QueuedMsg};
use crate::ticket::{self, Report, Stage, Ticket};
use crate::Core;

/// `read_ticket` 里执行记录最多给多少条消息。
const TRANSCRIPT_LINES_MAX: usize = 40;
const TRANSCRIPT_LINES_DEFAULT: usize = 12;
/// 执行记录里一条消息最多多少字。
const TRANSCRIPT_LINE_CHARS: usize = 600;
/// `list_tickets` 一次最多列多少个。
const LIST_MAX: usize = 50;

/// 工具调用的结果：给模型看的那段文本。
pub type Reply = Result<String, String>;

// ---------------------------------------------------------------------------
// 工具参数
//
// 每个工具都从一段 JSON 里抠几个字段。以前是九处手写的
// `args.get("x").and_then(Value::as_str).unwrap_or("").trim()`，同一件事写九遍
// —— 改一次语义（比如空串要不要算「没给」）得记得改九处。收敛成三个函数。
// ---------------------------------------------------------------------------

/// 取一个字符串字段（去掉首尾空白）。没给 / 类型不对都是空串。
fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

/// 取一个 bool 字段。没给就是 `false`。
fn arg_flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// 取一个整数并夹到 `[min, max]`；没给 / 类型不对就用 `default`。
fn arg_usize(args: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map_or(default, |n| (n as usize).clamp(min, max))
}

/// `list_tickets`：有哪些工作项、各自卡在哪。
///
/// **只列这个 Router 名下的**。默认只看还开着的 —— Router 平时关心的就是
/// 「还没完的」，翻旧账是另一种需求（`all: true`）。
pub async fn list_tickets(core: &Core, from: &str, args: &Value) -> Reply {
    let state = &core.state;
    let all = arg_flag(args, "all");
    let limit = arg_usize(args, "limit", LIST_MAX, 1, LIST_MAX);

    let db = state.db().ok_or("没有库，工作项不可用")?;
    let tickets = db
        .ticket_list(from, !all, limit)
        .map_err(|e| format!("读工作项失败：{e:#}"))?;
    let (working, waiting) = db
        .ticket_counts(from)
        .map_err(|e| format!("读工作项失败：{e:#}"))?;

    let mut out = String::new();
    if tickets.is_empty() {
        if all {
            out.push_str("还没有任何工作项。用 dispatch 派一个。");
        } else {
            out.push_str("现在没有未完成的工作项。用 dispatch 派一个。");
        }
        return Ok(out);
    }

    // 抬头先给两个数：这是 Router 每次醒来最该先知道的东西
    let _ = writeln!(
        out,
        "{working} 件在干，{waiting} 件在等人{}：",
        if all { "（含已结束的）" } else { "" }
    );
    for t in &tickets {
        let _ = writeln!(out, "- {}", ticket::card(t));
    }
    Ok(out)
}

/// `read_ticket`：一个工作项的详情。
///
/// 三段：**它是什么**（标题 / 要求 / 状态 / 结果）、**它干过什么**（执行记录，
/// `what=transcript` 时给）、以及**下一步能做什么**。
///
/// 「结果」和「执行记录」分开是有意的：Router 关心的是「它做完了说了什么」，
/// 中间跑了多少条命令是另一种信息，混在一起就是噪音。默认只给前两段的摘要，
/// 要过程得显式要。
pub async fn read_ticket(core: &Core, from: &str, args: &Value) -> Reply {
    let state = &core.state;
    let id = arg_str(args, "ticket_id");
    if id.is_empty() {
        return Err("缺少参数 ticket_id（`list_tickets` 给的那些反引号里的串）".to_string());
    }
    let db = state.db().ok_or("没有库，工作项不可用")?;
    let t = db
        .ticket_get(id)
        .map_err(|e| format!("读工作项失败：{e:#}"))?
        .ok_or_else(|| format!("没有这个工作项: `{id}`"))?;
    // 权限：只能看自己名下的。不是自己的当作不存在（不泄露别人有什么）。
    if t.router_id != from {
        return Err(format!("没有这个工作项: `{id}`"));
    }

    let mut out = String::new();
    let _ = writeln!(out, "工作项 #{} `{}`", t.number(), t.id);
    let _ = writeln!(out, "标题：{}", t.title);
    let _ = writeln!(out, "状态：{}", t.stage.label());
    let _ = writeln!(out, "派过 {} 次，载体会话 `{}`", t.dispatches, t.session_id);
    let _ = writeln!(out, "要求：{}", t.brief);
    if t.result.trim().is_empty() {
        let _ = writeln!(out, "结果：（还没汇报）");
    } else {
        let _ = writeln!(out, "结果（{}）：", report_label(t.report));
        let _ = writeln!(out, "{}", t.result.trim());
    }

    // 执行记录：要了才给。它是「过程」，默认不给
    let want = arg_str(args, "what");
    if want == "transcript" {
        let lines = arg_usize(
            args,
            "lines",
            TRANSCRIPT_LINES_DEFAULT,
            1,
            TRANSCRIPT_LINES_MAX,
        );
        let _ = writeln!(out);
        match crate::agent::session_digest(core, &t.session_id, lines, TRANSCRIPT_LINE_CHARS).await
        {
            Ok(text) => out.push_str(&text),
            Err(e) => {
                let _ = writeln!(out, "执行记录读不出来：{e}");
            }
        }
    } else {
        let _ = writeln!(
            out,
            "（要看它中间跑了什么：`read_ticket {{ticket_id: \"{}\", what: \"transcript\"}}`）",
            t.id
        );
    }

    // 下一步能做什么：按状态给，别让模型自己猜
    let _ = writeln!(out);
    match t.stage {
        Stage::WaitingReview => {
            let _ = writeln!(
                out,
                "它做完了，等验收。看结果可以；要它接着改就再派一次（dispatch 带 ticket_id）。"
            );
        }
        Stage::Blocked | Stage::Failed => {
            let _ = writeln!(
                out,
                "它卡住了/失败了。看结果里说的原因；要它继续就再派一次（dispatch 带 ticket_id）。"
            );
        }
        Stage::Working | Stage::Queued => {
            let _ = writeln!(out, "它还在跑。等它的汇报，不要重复派同一个活。");
        }
        Stage::Done | Stage::Cancelled => {
            let _ = writeln!(out, "这件事已经结束了（{}）。", stage_end(&t));
        }
    }
    Ok(out)
}

fn report_label(r: Option<Report>) -> &'static str {
    match r {
        Some(Report::Ready) => "做完了",
        Some(Report::Blocked) => "卡住了",
        Some(Report::Failed) => "失败了",
        Some(Report::Progress) => "进行中",
        None => "汇报",
    }
}

fn stage_end(t: &Ticket) -> &'static str {
    if t.ended_by == "cancelled" {
        "取消"
    } else {
        "完成"
    }
}

/// `dispatch`：开一个工作项并派出去。
///
/// - 不给 `ticket_id` → **新建**一个工作项，给一个 worker 会话去干
/// - 给了 `ticket_id` → **再派一次**（补充要求 / 让它接着改）：工作项还是那个，
///   派工次数 +1，状态回到「排队中」
///
/// 返回**立即**给出 —— 活只是排进了那条会话的队列。真正开跑由
/// `continue_with_queued` 负责（它会先试 `begin_turn`；那条会话正忙就把消息留在
/// 队列里等它）。同步等待就是把双层塌成一层。
pub async fn dispatch(core: &Core, from: &str, args: &Value) -> Reply {
    let state = &core.state;
    let task = arg_str(args, "task");
    if task.is_empty() {
        return Err(
            "缺少参数 task（要它做什么，写成不知道前因后果的人也能看懂的样子）".to_string(),
        );
    }
    let router = from;
    let db = state.db().ok_or("没有库，工作项不可用")?;

    // 再派一次：工作项已经在。借 `args` 里的那一截就行（`args` 在这个函数里活着），
    // 以前为绕借用检查把它 to_string 了一份。
    let again = arg_str(args, "ticket_id");

    if !again.is_empty() {
        let t = db
            .ticket_get(again)
            .map_err(|e| format!("读工作项失败：{e:#}"))?
            .ok_or_else(|| format!("没有这个工作项: `{again}`"))?;
        if t.router_id != router {
            return Err(format!("没有这个工作项: `{again}`"));
        }
        if !t.open() {
            return Err(format!(
                "工作项 #{} 已经结束了（{}）。要重开就新建一个，别往结束的上头派。",
                t.number(),
                t.stage.label()
            ));
        }
        // 会话没了（被删了）就新开一个：工作项还是同一个，换个载体
        let sid = ensure_session(core, &t.session_id, &t.title).await?;
        return redispatch(core, &db, &t, &sid, task).await;
    }

    // 新建
    let given = arg_str(args, "title");
    let title = if given.is_empty() {
        // 没给标题就用要求的前一段：模型经常只给 task，不能因此失败
        ticket::clip(task.lines().next().unwrap_or("未命名"), 40)
    } else {
        ticket::clip(given, 60)
    };

    // **先看有没有模型**，再动会话：没配模型时派出去也没人跑，那就不该先建一个
    // 空会话再丢在那儿（它没内容、不进列表，用户看不见也删不掉）。
    let Some(provider) = state.llm() else {
        return Err("还没配置模型，派出去的活没人能跑（先去设置里配一个）".to_string());
    };

    // 新工作项一个自己的会话（上下文隔离：它看不到 Router 的对话）。
    // `ensure_session("")` = 「没有现成的，新建一个」—— 和再派一次时
    // 「沿用原来的、没了就新建」是同一段逻辑，不写两遍。
    let sid = ensure_session(core, "", &title).await?;

    let tid = uuid::Uuid::now_v7().to_string();
    let now = crate::agent::now_ms();
    if let Err(e) = db.ticket_create(&tid, &title, task, &sid, router, now) {
        return Err(format!("建工作项失败：{e:#}"));
    }
    if let Err(e) = db.ticket_bind_session(&sid, &tid, now) {
        tracing::warn!("工作项和会话没绑定上（它的汇报会丢）: {e:#}");
    }

    // 投活 + 开跑
    if let Err(e) = deliver(core, &sid, task, &provider) {
        // 派不出去就把工作项标成失败，别让它挂在「排队中」永远不动
        let _ = db.ticket_report(&tid, Report::Failed, &e, crate::agent::now_ms());
        return Err(e);
    }

    Ok(format!(
        "开了工作项 #{}（`{tid}`），交给一条新会话去干。它做完会把结果送回来。",
        db.ticket_number(&tid).ok().flatten().unwrap_or(0)
    ))
}

/// 往已有工作项再派一次。
async fn redispatch(
    core: &Core,
    db: &std::sync::Arc<crate::db::Db>,
    t: &Ticket,
    sid: &str,
    task: &str,
) -> Reply {
    let state = &core.state;
    let Some(provider) = state.llm() else {
        return Err("还没配置模型，派出去的活没人能跑".to_string());
    };
    // 要求变了：工作项的描述跟着更新（它下一步要看的是最新的要求）
    if let Err(e) = db.ticket_redispatch(&t.id, crate::agent::now_ms()) {
        return Err(format!("改工作项失败：{e:#}"));
    }
    if let Err(e) = db.ticket_bind_session(sid, &t.id, crate::agent::now_ms()) {
        tracing::warn!("工作项和会话没绑定上: {e:#}");
    }
    if let Err(e) = deliver(core, sid, task, &provider) {
        let _ = db.ticket_report(&t.id, Report::Failed, &e, crate::agent::now_ms());
        return Err(e);
    }
    Ok(format!(
        "又派给工作项 #{}（`{}`）了，还是那条会话接着干。",
        t.number(),
        t.id
    ))
}

/// 确认这条会话还在（不在了就新开一个，标题沿用）。
async fn ensure_session(core: &Core, want: &str, title: &str) -> Result<String, String> {
    let state = &core.state;
    // 空串 = 「一定新建」。先短路掉，省一次没用处的读锁，也让意图写在最前面。
    if !want.is_empty() {
        let store = state.chat.read().await;
        if store.get(want).is_some_and(|s| s.role == Role::Worker) {
            return Ok(want.to_string());
        }
    }
    // 写锁里再查一次：两个请求同时发现「没有」，不能各建一个
    let mut store = state.chat.write().await;
    if !want.is_empty() && store.get(want).is_some_and(|s| s.role == Role::Worker) {
        return Ok(want.to_string());
    }
    let s = crate::agent::ChatSession::with_role(Role::Worker);
    let id = s.id.clone();
    store.sessions.push(s);
    if let Some(s) = store.get_mut(&id) {
        s.title = title.to_string();
    }
    drop(store);
    state.mark_sessions_dirty();
    Ok(id)
}

/// 把活投进会话并开跑（`enqueue` + `continue_with_queued`）。
fn deliver(
    core: &Core,
    sid: &str,
    task: &str,
    provider: &crate::llm::Provider,
) -> Result<(), String> {
    let msg = QueuedMsg {
        id: uuid::Uuid::now_v7().to_string(),
        text: task.to_string(),
        attachments: Vec::new(),
    };
    if core.state.enqueue(sid, msg).is_err() {
        return Err(format!(
            "那条会话排队等着交给模型的消息太多了（上限 {}），等它跑完再派",
            AppState::MAX_QUEUED
        ));
    }
    crate::agent::continue_with_queued(core, provider, sid);
    Ok(())
}

/// Worker 的一个回合结束了：把结果记进工作项，并让 Router 知道。
///
/// 这是双层的**闭环**，也是唯一一处 Worker 能影响 Router 的地方。
///
/// 记什么：最后说的那段话（`result`）+ 一句进度判断（`report`）。判断的规则
/// 是**确定性的**，不让模型猜（它此刻已经不在推理了）：
///
/// - 回合正常结束 → `ready`（做完了，等人验收）
/// - 回合出错 → `failed`
/// - 用户按了停止 → `progress`（不是「做完」也不是「失败」，是中途中止）
///
/// 然后往 Router 的队列里排一条**结构化的**回执（带 ticket id 和状态），
/// 而不是一句散文 —— Router 醒来要知道「哪个工作项、什么状态、结果在哪」。
pub async fn report_back(core: &Core, from_session: &str, said: &str, ok: bool, stopped: bool) {
    let state = &core.state;
    let Some(db) = state.db() else { return };
    let Ok(Some(tid)) = db.ticket_of_session(from_session) else {
        return; // 这条会话不是在干工作项（用户自己开的）
    };
    let Ok(Some(t)) = db.ticket_get(&tid) else {
        return;
    };
    // 已经结束的工作项不再收汇报（结束了就是结束了）
    if !t.open() {
        return;
    }
    let text = if said.trim().is_empty() {
        if stopped {
            "（中途中止）".to_string()
        } else if ok {
            "（这一轮没有输出）".to_string()
        } else {
            "这一轮失败了".to_string()
        }
    } else {
        said.to_string()
    };
    let now = crate::agent::now_ms();
    // **停止走单独的跃迁**：它不是「做完」也不是「失败」，而是「没人再干了」。
    // 走 `ticket_report(Progress)` 的话状态不变，工作项会永远停在「在干」。
    let written = if stopped {
        db.ticket_interrupted(&tid, &text, now)
    } else {
        db.ticket_report(
            &tid,
            if ok { Report::Ready } else { Report::Failed },
            &text,
            now,
        )
    };
    if let Err(e) = written {
        tracing::warn!("记工作项汇报失败（Router 会收不到）: {e:#}");
        return;
    }
    // **重新读一遍**：上面那个 `t` 是写入**之前**的快照，`stage` 还是旧的
    //（典型是 `Queued`）。拿旧状态去告诉 Router「现在是什么状态」就是在撒谎。
    let Ok(Some(t)) = db.ticket_get(&tid) else {
        return;
    };

    // 让 Router 知道。**结构化**：哪个工作项、什么状态、结果在哪
    let router = t.router_id.clone();
    {
        let store = state.chat.read().await;
        match store.get(&router) {
            Some(s) if s.role == Role::Router => {}
            _ => return, // Router 没了就只记账，不叫醒谁
        }
    }
    // **全文给，一个字都不截。**
    //
    // 一开始照 dsh 抄了个 600 字上限 + 「要看全文就调 read_ticket」的指针。两处都蠢：
    //
    // 1. dsh 那边是团队共享一个 Router、几十个活同时汇报，才需要抠 token；
    //    这里是一个人、一个 Router，worker 干完一件事写的那段报告就是**全部的
    //    交付说明**，截断它等于把交付物撕掉一半。
    // 2. 短的回执还附一句「要看全文就调工具」，是白让模型多烧一轮请求去读同一段话。
    //
    // 如果某天报告真的大到顶爆上下文，那是 compaction 的事（`eta-compaction` 那一层），
    // 不该在回执这里用截断来「预防」—— 那只会让人看不到自己写的东西。
    let body = format!(
        "工作项 #{}「{}」{}。\n{}",
        t.number(),
        t.title,
        t.stage.label(),
        text.trim(),
    );
    let msg = QueuedMsg {
        id: uuid::Uuid::now_v7().to_string(),
        text: body,
        attachments: Vec::new(),
    };
    if state.enqueue(&router, msg).is_err() {
        tracing::warn!("Router 的队列满了，工作项 #{} 的回执丢了", t.number());
        return;
    }
    if let Some(provider) = state.llm() {
        crate::agent::continue_with_queued(core, &provider, &router);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ChatMessage, ChatSession};

    fn core_with_db(tag: &str) -> Core {
        let core = Core::new();
        let dir = std::env::temp_dir().join(format!("agent-ticket-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        core.set_data_dir(dir);
        core
    }

    /// 建一个 Router 会话，返回 (core, router_id)
    async fn with_router(tag: &str) -> (Core, String) {
        let core = core_with_db(tag);
        let rid = {
            let mut store = core.state.chat.write().await;
            let s = ChatSession::with_role(Role::Router);
            let id = s.id.clone();
            store.sessions.push(s);
            id
        };
        (core, rid)
    }

    #[tokio::test]
    async fn dispatch_creates_a_ticket_with_state_not_just_a_session() {
        let (core, rid) = with_router("create").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));

        let out = dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "把 README 的错别字改掉", "title": "改错别字" }),
        )
        .await
        .expect("配了模型就该派得出去");
        assert!(out.contains("工作项 #"), "返回要说清开了哪个工作项：{out}");

        // 库里真的有一个工作项，状态是「排队中」
        let db = core.state.db().unwrap();
        let list = db.ticket_list(&rid, true, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].stage, Stage::Queued);
        assert_eq!(list[0].title, "改错别字");
        assert!(list[0].open());
    }

    #[tokio::test]
    async fn dispatch_without_a_model_creates_no_ticket_and_no_session() {
        let (core, rid) = with_router("nomodel").await;
        let err = dispatch(&core, &rid, &serde_json::json!({ "task": "随便干点什么" }))
            .await
            .expect_err("没模型就不该派得出去");
        assert!(err.contains("模型"), "{err}");
        // 库里没有工作项，也没有多出来的会话
        let db = core.state.db().unwrap();
        assert!(db.ticket_list(&rid, false, 10).unwrap().is_empty());
        let store = core.state.chat.read().await;
        assert_eq!(store.sessions.len(), 1, "只有那个 Router，没多建 worker");
    }

    #[tokio::test]
    async fn a_worker_report_moves_the_ticket_and_tells_the_router() {
        let (core, rid) = with_router("report").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();

        // 把 provider 清掉再汇报：配着的话 `continue_with_queued` 会立刻开一轮
        // 把回执取走（连不上端点、然后失败），队列里就查不到那条了。
        core.state.clear_llm();
        // Worker 干完了
        report_back(&core, &t.session_id, "改好了，PR 已提交", true, false).await;

        let after = db.ticket_get(&t.id).unwrap().unwrap();
        assert_eq!(after.stage, Stage::WaitingReview, "做完该是「待验收」");
        assert!(after.result.contains("改好了"));

        // Router 收到一条**结构化**回执（带工作项身份）
        let q = core.state.queued_list(&rid);
        assert_eq!(q.len(), 1);
        assert!(
            q[0].text.contains("工作项 #"),
            "要带工作项身份：{}",
            q[0].text
        );
        assert!(q[0].text.contains("待验收"), "要带状态：{}", q[0].text);
        // **全文给，不截断、不要模型再调工具**
        assert!(q[0].text.contains("改好了"), "要带结果原文：{}", q[0].text);
        assert!(
            !q[0].text.contains("read_ticket"),
            "回执就是全部内容，不该再要一次工具调用：{}",
            q[0].text
        );
    }

    /// **回合真的开跑时，工作项必须从「排队中」变成「在干」。**
    ///
    /// 这是线上抓到的 bug：`dispatch` 之后的几秒（有时几分钟，那条会话正忙）里
    /// 界面显示「排队中」是对的；但**开始跑了之后还显示「排队中」就是撒谎**。
    /// 原来没有任何地方把 stage 设成 `Working` —— 工作项从建出来一直挂到 worker
    /// 干完，中间全程 `Queued`。
    #[tokio::test]
    async fn the_ticket_turns_working_when_its_turn_really_starts() {
        let (core, rid) = with_router("started").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        assert_eq!(t.stage, Stage::Queued, "派完还没跑，该是排队中");

        // 模拟回合开跑（`agent::turn_once` 开头做的就是这件事）
        crate::ticket::mark_started(&core, &t.session_id).await;
        assert_eq!(
            db.ticket_get(&t.id).unwrap().unwrap().stage,
            Stage::Working,
            "开跑了就该是「在干」"
        );

        // 幂等：再来一次不会把已经是别的状态的工作项拉回来
        db.ticket_report(&t.id, Report::Ready, "做完了", 0).unwrap();
        crate::ticket::mark_started(&core, &t.session_id).await;
        assert_eq!(
            db.ticket_get(&t.id).unwrap().unwrap().stage,
            Stage::WaitingReview,
            "已经待验收的不该被「开跑」拉回在干"
        );
    }

    /// **用户按停止 = 没人再干了**，不能留在「在干」。
    ///
    /// `Report::Progress` 有两种含义（还在动 / 被中止），让它顺带决定状态就是
    /// 隐式状态机。停止走单独的 `ticket_interrupted`。
    #[tokio::test]
    async fn a_stopped_turn_never_leaves_the_ticket_working() {
        let (core, rid) = with_router("stopped2").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        crate::ticket::mark_started(&core, &t.session_id).await;
        assert_eq!(db.ticket_get(&t.id).unwrap().unwrap().stage, Stage::Working);

        core.state.clear_llm();
        report_back(&core, &t.session_id, "干到一半被叫停", true, true).await;

        let after = db.ticket_get(&t.id).unwrap().unwrap();
        assert_eq!(
            after.stage,
            Stage::Blocked,
            "停下了就该等人，不能留在「在干」"
        );
        assert!(after.stage.needs_person(), "停下的事该有人看一眼");
    }

    /// **回执不截断。** worker 干完一件事写的那段报告就是全部的交付说明 ——
    /// 截断它等于把交付物撕掉一半，还逼模型再发一轮工具调用去读回来。
    #[tokio::test]
    async fn a_long_report_is_delivered_whole() {
        let (core, rid) = with_router("whole").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        core.state.clear_llm(); // 让回执留在队列里可查

        let long = "很长的结果。".repeat(500); // 3000 字
        report_back(&core, &t.session_id, &long, true, false).await;
        let q = core.state.queued_list(&rid);
        assert_eq!(q.len(), 1);
        assert!(
            q[0].text.contains(&long),
            "3000 字的报告要被**整段**送到，不能截（实际 {} 字）",
            q[0].text.chars().count()
        );
        assert!(!q[0].text.contains("…（共"), "不许截断");
    }

    #[tokio::test]
    async fn a_failed_turn_marks_the_ticket_failed() {
        let (core, rid) = with_router("fail").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();

        report_back(&core, &t.session_id, "连不上数据库", false, false).await;
        assert_eq!(db.ticket_get(&t.id).unwrap().unwrap().stage, Stage::Failed);
    }

    #[tokio::test]
    async fn a_stopped_turn_does_not_claim_the_work_is_done() {
        let (core, rid) = with_router("stop").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();

        report_back(&core, &t.session_id, "干到一半被叫停", true, true).await;
        let after = db.ticket_get(&t.id).unwrap().unwrap();
        // 中途中止**不是**「做完了」—— 那是人不想让它继续，不是 ready
        assert_ne!(after.stage, Stage::WaitingReview);
        assert_eq!(after.report, Some(Report::Progress));
    }

    /// **中止之后不能说「还在动」。**
    ///
    /// `ticket_interrupted` 把 `report` 记成 `progress`（中止也是进度汇报）同时把
    /// stage 推到 `Blocked`。以前 `card()` 只看 `report`，于是列表里那一行是
    /// 「被卡住（还在动）」—— 两句话互相打脸，模型也会误以为不用管它。
    #[tokio::test]
    async fn a_stopped_ticket_is_not_still_moving() {
        let (core, rid) = with_router("moving").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();

        // 还在跑的时候，一条 progress 确实意味着「还在动」
        crate::ticket::mark_started(&core, &t.session_id).await;
        db.ticket_report(&t.id, Report::Progress, "跑到一半", 1).unwrap();
        let moving = db.ticket_get(&t.id).unwrap().unwrap();
        assert!(moving.still_moving(), "在跑的时候该说还在动");
        assert!(ticket::card(&moving).contains("（还在动）"));

        core.state.clear_llm();
        report_back(&core, &t.session_id, "干到一半被叫停", true, true).await;
        let stopped = db.ticket_get(&t.id).unwrap().unwrap();
        assert_eq!(stopped.stage, Stage::Blocked);
        assert!(
            !stopped.still_moving(),
            "停下了就不是「还在动」：stage={:?} report={:?}",
            stopped.stage, stopped.report
        );
        let line = ticket::card(&stopped);
        assert!(
            !line.contains("（还在动）"),
            "词不能自相矛盾（被卡住又说还在动）：{line}"
        );
    }

    #[tokio::test]
    async fn list_shows_counts_and_read_pointers() {
        let (core, rid) = with_router("list").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "做甲", "title": "甲" }),
        )
        .await
        .unwrap();
        let out = list_tickets(&core, &rid, &serde_json::json!({}))
            .await
            .unwrap();
        assert!(out.contains("件在干"), "抬头要有数：{out}");
        assert!(out.contains("甲"), "{out}");
        assert!(
            out.contains("read: ticket/"),
            "每个条目要带 read 指针：{out}"
        );
    }

    #[tokio::test]
    async fn read_ticket_gives_the_result_and_hides_the_transcript_by_default() {
        let (core, rid) = with_router("read").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "干点活", "title": "一件事" }),
        )
        .await
        .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        report_back(&core, &t.session_id, "做完了，结果是 X", true, false).await;

        let out = read_ticket(&core, &rid, &serde_json::json!({ "ticket_id": t.id }))
            .await
            .unwrap();
        assert!(out.contains("结果是 X"), "要能看到结果：{out}");
        assert!(out.contains("待验收"), "{out}");
        // 默认不给执行记录（那是过程），但要告诉它怎么要
        assert!(
            !out.contains("执行记录（最近"),
            "默认不该倒出执行记录：{out}"
        );
        assert!(
            out.contains("what: \"transcript\""),
            "要告诉它怎么要：{out}"
        );
    }

    #[tokio::test]
    async fn a_router_cannot_read_or_dispatch_another_routers_ticket() {
        let (core, rid) = with_router("acl").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();

        // 别人的 Router 看它：当作不存在（不泄露有什么）
        let err = read_ticket(
            &core,
            "another-router",
            &serde_json::json!({ "ticket_id": t.id }),
        )
        .await
        .expect_err("不该能读别人的");
        assert!(err.contains("没有这个工作项"), "{err}");
    }

    #[tokio::test]
    async fn redispatch_keeps_the_same_ticket() {
        let (core, rid) = with_router("again").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "干点活", "title": "一件事" }),
        )
        .await
        .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        report_back(&core, &t.session_id, "做完了", true, false).await;

        // 再派一次（让它接着改）：还是同一个工作项，派工次数 +1
        let out = dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "再改一下第二段", "ticket_id": t.id }),
        )
        .await
        .unwrap();
        assert!(out.contains("又派给"), "{out}");
        let after = db.ticket_get(&t.id).unwrap().unwrap();
        assert_eq!(after.dispatches, 2);
        assert_eq!(
            db.ticket_list(&rid, true, 10).unwrap().len(),
            1,
            "没有变成两个"
        );
    }

    #[tokio::test]
    async fn cannot_redispatch_a_finished_ticket() {
        let (core, rid) = with_router("ended").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        db.ticket_end(&t.id, "done", 0).unwrap();

        let err = dispatch(
            &core,
            &rid,
            &serde_json::json!({ "task": "再干", "ticket_id": t.id }),
        )
        .await
        .expect_err("结束的不该能再派");
        assert!(err.contains("已经结束"), "{err}");
    }

    #[test]
    fn stage_round_trips_and_unknown_is_never_open() {
        for st in [
            Stage::Queued,
            Stage::Working,
            Stage::WaitingReview,
            Stage::Blocked,
            Stage::Failed,
            Stage::Done,
            Stage::Cancelled,
        ] {
            assert_eq!(Stage::parse(st.as_str()), st);
        }
        // 读不懂的状态读成 `Failed`，不是「还在干」：说成「在干」会让 Router
        // 永远等一个不会来的结果；说成失败至少会被人看见、能被处理。
        // 注意 `Failed` 本身仍然是**开着的**（等人处理），所以这里断的是
        // 「它不是 Working / Queued」那个意思。
        assert_eq!(Stage::parse("something-new"), Stage::Failed);
        assert_ne!(Stage::parse("something-new"), Stage::Working);
        assert!(Stage::parse("something-new").needs_person());
    }

    #[test]
    fn only_person_stages_need_a_person() {
        assert!(Stage::WaitingReview.needs_person());
        assert!(Stage::Blocked.needs_person());
        assert!(Stage::Failed.needs_person());
        assert!(!Stage::Working.needs_person());
        assert!(!Stage::Queued.needs_person());
        assert!(!Stage::Done.needs_person());
    }

    #[test]
    fn clip_counts_chars_not_bytes() {
        assert_eq!(
            ticket::clip("一二三四五六七八九十", 5),
            "一二三四五…（共 10 字）"
        );
        assert_eq!(ticket::clip("abc", 5), "abc");
    }

    /// 不是派出去的会话（用户自己开的）不该凭空往 Router 里塞消息。
    #[tokio::test]
    async fn an_undispatched_session_reports_nowhere() {
        let (core, rid) = with_router("undispatched").await;
        let sid = {
            let mut store = core.state.chat.write().await;
            store.create()
        };
        report_back(&core, &sid, "我干完了", true, false).await;
        assert!(core.state.queued_list(&rid).is_empty());
    }

    /// 会话里的对话内容不该影响工作项 —— 工作项自己记着 `result`。
    #[tokio::test]
    async fn the_ticket_keeps_its_own_result_even_if_the_session_is_gone() {
        let (core, rid) = with_router("gone").await;
        core.state
            .set_llm(crate::llm::Provider::openai("sk-test-not-used"));
        dispatch(&core, &rid, &serde_json::json!({ "task": "干点活" }))
            .await
            .unwrap();
        let db = core.state.db().unwrap();
        let t = db.ticket_list(&rid, true, 10).unwrap().pop().unwrap();
        report_back(&core, &t.session_id, "结果很重要", true, false).await;
        // 会话被删了
        {
            let mut store = core.state.chat.write().await;
            store.delete(&t.session_id);
        }
        // 工作项的结果还在（它没存在会话里）
        let after = db.ticket_get(&t.id).unwrap().unwrap();
        assert!(after.result.contains("结果很重要"));
        let _ = ChatMessage::user("x"); // 用一下导入
    }
}
