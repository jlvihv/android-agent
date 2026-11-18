//! 工作项（Ticket）：**被派出去的那件事**本身。
//!
//! ## 为什么需要它
//!
//! 一开始这条路是错的：`dispatch` 往一条会话里塞一条消息，就算「派了活」。
//! 会话是**容器** —— 没有状态、没有产出契约、没有终点，可以一直开着改十件事。
//! 于是 Router 派完就对那件事一无所知，只能靠 `list_sessions` 看到
//! 「这条会话忙不忙」这种两状态信号。
//!
//! 而 Router 真正要回答的是：
//!
//! - 这活干完了吗？
//! - 结果在哪？
//! - 需要我做什么吗（等人验收 / 卡住了 / 失败了）？
//! - 我该不该再派一次？
//!
//! 这些都得有个东西承载，那个东西就是这里。
//!
//! ## 与会话的关系
//!
//! 会话仍然是**载体**：真正干活的是那条会话里的一次回合（`session_id`）。
//! 但「这件事到哪一步了」记在 `tickets` 表里，不记在会话上。
//!
//! 一个会话同一时刻只干一件事（回合本来就是串行的），`session_ticket` 表记
//! 「这条会话现在在干哪个工作项」—— Worker 回合结束时靠它找到该汇报给谁。
//!
//! ## 和 dsh 那个原型的关系
//!
//! 概念取自它（Ticket / stage / report / result），但**台面上只有一个人用**，
//! 所以去掉了它那一整套授权模型（Mandate / Policy / 规则匹配 / 准入表格）：
//! 这里「谁派的」就是 Router，「凭什么能派」就是用户在跟它说话。留下的只有
//! 承重的那几条：状态机、产出契约、结果去哪儿。

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::db::Db;

/// 工作项走到哪一步了。
///
/// **`person/*` 一律意味着「没有人类就无法继续」** —— Router 一眼就能算出
/// 「现在有几件事在等我」。这是它做决策的全部基础，也是这个枚举存在的理由：
/// 一个 `busy: bool` 承载不了这些信息。
///
/// 顺序有意义：`stage()` 从前往后匹配第一个成立的（见 `derive`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    /// 派了活，但还没送到 worker（会话还没接上 / 没配模型）
    /// 人：无。这是**短暂态**，正常几秒内就变成 `Working`
    Queued,
    /// 在干
    Working,
    /// worker 说做完了、等人验收（`ready`）
    WaitingReview,
    /// worker 卡住了、要人给个说法（`blocked`）
    Blocked,
    /// 干失败了
    Failed,
    /// 干完了、验收过了
    Done,
    /// 被取消（人不要了 / Router 撤回了）
    Cancelled,
}

impl Stage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Working => "working",
            Self::WaitingReview => "waiting_review",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// 从库里读回来。不认识的一律当 `Failed` —— **不能默认成「还开着」**：
    /// 装回旧版本、或者以后加了新状态时，一个读不懂的状态说成「在干」会让 Router
    /// 永远等一个不会来的结果。说成失败至少会被人看见。
    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "working" => Self::Working,
            "waiting_review" => Self::WaitingReview,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    /// 还开着吗（没结束）。`Done` / `Cancelled` 是终点。
    ///
    /// `Failed` / `Blocked` / `WaitingReview` **仍然是开着的**：等人在那边处理，
    /// 处理完（重新派活）它就继续走。
    pub const fn open(self) -> bool {
        !matches!(self, Self::Done | Self::Cancelled)
    }

    /// 要不要人来处理。
    pub const fn needs_person(self) -> bool {
        matches!(self, Self::WaitingReview | Self::Blocked | Self::Failed)
    }

    /// 给人看的一句话（列表里那一行）。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Queued => "排队中",
            Self::Working => "在干",
            Self::WaitingReview => "待验收",
            Self::Blocked => "被卡住",
            Self::Failed => "失败了",
            Self::Done => "已完成",
            Self::Cancelled => "已取消",
        }
    }
}

/// Worker 汇报的类型（`report` 列）。
///
/// 和 `Stage` 分开：`Stage` 是**现在卡在谁那里**，`report` 是**worker 说了什么**。
/// 两个 `progress` 之间状态不变，但 Router 需要知道「它还在动」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// 做完了，等你验收
    Ready,
    /// 卡住了，要人给说法
    Blocked,
    /// 失败了
    Failed,
    /// 还在动（不算状态变化，只是活着的信号）
    Progress,
}

impl Report {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Progress => "progress",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ready" => Some(Self::Ready),
            "blocked" => Some(Self::Blocked),
            "failed" => Some(Self::Failed),
            "progress" => Some(Self::Progress),
            _ => None,
        }
    }

    /// 这个汇报把工作项推进到哪个状态。`None` = **状态不变**。
    ///
    /// `Progress` 返回 `None`，不是 `Working`：它有两种来源，含义相反 ——
    /// ①「还在动」（中途汇报），状态该保持原样；②「用户按了停止」（中途中止），
    /// 那时工作项不该被说成「在干」（没人干了，也不会有人再汇报）。
    /// 一开始把它硬映射成 `Working`，于是停止之后界面永远显示「在干」。
    pub const fn stage(self) -> Option<Stage> {
        match self {
            Self::Ready => Some(Stage::WaitingReview),
            Self::Blocked => Some(Stage::Blocked),
            Self::Failed => Some(Stage::Failed),
            Self::Progress => None,
        }
    }
}

/// 一个工作项。
#[derive(Debug, Clone)]
pub struct Ticket {
    /// 插入顺序（SQLite 的 rowid）。**给人看的编号就是它** —— uuid 太长、
    /// 而且 v7 的前缀是时间戳，同毫秒建的两个会撞。
    pub rowid: i64,
    pub id: String,
    pub title: String,
    /// 要做什么。Worker 只拿到这一段（它看不到 Router 的对话）
    pub brief: String,
    pub session_id: String,
    pub router_id: String,
    pub stage: Stage,
    /// 最新一次汇报的正文
    pub result: String,
    /// 最新一次汇报的类型（没汇报过是 `None`）
    pub report: Option<Report>,
    pub ended_at: u64,
    pub ended_by: String,
    pub dispatches: u32,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Ticket {
    /// 编号：`#3`（给人看）。`id` 是 uuid，太长，不适合嘴上说。
    pub fn number(&self) -> i64 {
        self.rowid
    }

    /// 还开着吗
    pub fn open(&self) -> bool {
        self.stage.open()
    }

    /// 结果的一句话摘要（列表里那一行）。
    ///
    /// 取**第一行非空的**，压到 `max` 字。以前有两个几乎一样的方法
    /// （`preview` 取全文再截、`headline` 取第一行），调用点各挑一个，读的人
    /// 得先比一遍才知道差别 —— 合成一个，语义写在一处。
    ///
    /// 为什么是第一行而不是全文：一段 3000 字带表格的报告在列表里铺开就是把
    /// 界面吃掉了（踩过）。要看全文的去 `read_ticket` / 那条会话。
    pub fn summary(&self, max: usize) -> String {
        let first = self
            .result
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        clip(first, max)
    }
}

/// 把一段文本压到 `max` 字（按字符，不按字节）。
pub fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…（共 {n} 字）")
}

// ---------------------------------------------------------------------------
// 库
// ---------------------------------------------------------------------------

impl Db {
    /// 开一个工作项。返回它的 id。
    #[allow(clippy::too_many_arguments)]
    pub fn ticket_create(
        &self,
        id: &str,
        title: &str,
        brief: &str,
        session_id: &str,
        router_id: &str,
        at: u64,
    ) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO tickets(id, title, brief, session_id, router_id, stage, result, report,
                                     ended_at, ended_by, dispatches, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, '', '', 0, '', 1, ?7, ?7)",
                rusqlite::params![
                    id,
                    title,
                    brief,
                    session_id,
                    router_id,
                    Stage::Queued.as_str(),
                    at as i64,
                ],
            )?;
            Ok(())
        })
    }

    /// 把一个工作项挂到会话上（`session_ticket`）。**覆盖**旧的：
    /// 会话被派了新活，旧的那个工作项这时候已经记下过结果了。
    pub fn ticket_bind_session(
        &self,
        session_id: &str,
        ticket_id: &str,
        at: u64,
    ) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO session_ticket(session_id, ticket_id, updated_at) VALUES(?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                   ticket_id = excluded.ticket_id,
                   updated_at = excluded.updated_at",
                rusqlite::params![session_id, ticket_id, at as i64],
            )?;
            Ok(())
        })
    }

    /// 这条会话现在在干哪个工作项（Worker 回合结束时要它找汇报对象）。
    pub fn ticket_of_session(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT ticket_id FROM session_ticket WHERE session_id = ?1")?;
            let mut rows = stmt.query([session_id])?;
            Ok(match rows.next()? {
                Some(r) => Some(r.get(0)?),
                None => None,
            })
        })
    }

    /// 回合真的开始了：`Queued` → `Working`。
    ///
    /// **这是唯一一处把工作项推到「在干」的地方**，而且必须绑在回合开始
    /// （`begin_turn` 之后）—— 一开始没有这一步，工作项从建出来到 worker 干完
    /// 一直停在 `Queued`，界面就一直显示「排队中」，而实际上早就在跑了。
    ///
    /// 只从 `Queued` 推（带 `WHERE stage = 'queued'`）：一个已经在 `WaitingReview`
    /// 或者 `Blocked` 的工作项被再派一次时是 `dispatch` 自己把它改回 `Queued` 的，
    /// 这里不抢。这条条件也是幂等的保证 —— 重复调用不会把 `Failed` 拉回 `Working`。
    pub fn ticket_started(&self, id: &str, at: u64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE tickets SET stage = ?2, updated_at = ?3
                 WHERE id = ?1 AND stage = ?4",
                rusqlite::params![
                    id,
                    Stage::Working.as_str(),
                    at as i64,
                    Stage::Queued.as_str()
                ],
            )?;
            Ok(())
        })
    }

    /// 记一次汇报：正文 + 类型（+ 状态，当这次汇报决定了新状态时）。
    ///
    /// 状态推不推由 `Report::stage()` 决定 —— 它返回 `None` 的汇报（`progress`）
    /// 只记正文和时刻，不动状态。
    pub fn ticket_report(
        &self,
        id: &str,
        report: Report,
        text: &str,
        at: u64,
    ) -> anyhow::Result<()> {
        self.with(|c| match report.stage() {
            Some(stage) => {
                c.execute(
                    "UPDATE tickets SET result = ?2, report = ?3, stage = ?4, updated_at = ?5 WHERE id = ?1",
                    rusqlite::params![id, text, report.as_str(), stage.as_str(), at as i64],
                )?;
                Ok(())
            }
            None => {
                c.execute(
                    "UPDATE tickets SET result = ?2, report = ?3, updated_at = ?4 WHERE id = ?1",
                    rusqlite::params![id, text, report.as_str(), at as i64],
                )?;
                Ok(())
            }
        })
    }

    /// 中途中止：用户按了停止 / 回合被打断。**没人再干了**，所以不能留在 `Working`。
    ///
    /// 单独一个跃迁而不是靠 `Report::Progress` 推状态：那个既表示「还在动」也表示
    /// 「被中止」，含义相反；让汇报类型去顺带决定状态就成了隐式状态机。
    pub fn ticket_interrupted(&self, id: &str, text: &str, at: u64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE tickets SET result = ?2, report = ?3, stage = ?4, updated_at = ?5
                 WHERE id = ?1 AND stage IN ('queued', 'working')",
                rusqlite::params![
                    id,
                    text,
                    Report::Progress.as_str(),
                    Stage::Blocked.as_str(),
                    at as i64
                ],
            )?;
            Ok(())
        })
    }

    /// 结束一个工作项。
    pub fn ticket_end(&self, id: &str, by: &str, at: u64) -> anyhow::Result<()> {
        let stage = if by == "cancelled" {
            Stage::Cancelled
        } else {
            Stage::Done
        };
        self.with(|c| {
            c.execute(
                "UPDATE tickets SET stage = ?2, ended_at = ?3, ended_by = ?4, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, stage.as_str(), at as i64, by],
            )?;
            Ok(())
        })
    }

    /// 再派一次（同一个工作项补充要求 / 重试）：次数 +1，状态回到排队。
    pub fn ticket_redispatch(&self, id: &str, at: u64) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE tickets SET dispatches = dispatches + 1, stage = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, Stage::Queued.as_str(), at as i64],
            )?;
            Ok(())
        })
    }

    /// 读一个工作项。
    pub fn ticket_get(&self, id: &str) -> anyhow::Result<Option<Ticket>> {
        let mut found = self.ticket_query(
            "SELECT rowid, id, title, brief, session_id, router_id, stage, result, report,
                    ended_at, ended_by, dispatches, created_at, updated_at
             FROM tickets WHERE id = ?1",
            rusqlite::params![id],
        )?;
        Ok(found.pop())
    }

    /// 某个 Router 名下的工作项（新的在前）。
    ///
    /// `open_only`：只要还开着的 —— Router 平时关心的就是「还没完的」。
    pub fn ticket_list(
        &self,
        router_id: &str,
        open_only: bool,
        limit: usize,
    ) -> anyhow::Result<Vec<Ticket>> {
        let sql = if open_only {
            "SELECT rowid, id, title, brief, session_id, router_id, stage, result, report,
                    ended_at, ended_by, dispatches, created_at, updated_at
             FROM tickets
             WHERE router_id = ?1 AND stage NOT IN ('done', 'cancelled')
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        } else {
            "SELECT rowid, id, title, brief, session_id, router_id, stage, result, report,
                    ended_at, ended_by, dispatches, created_at, updated_at
             FROM tickets
             WHERE router_id = ?1
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        };
        self.ticket_query(sql, rusqlite::params![router_id, limit as i64])
    }

    /// 「有几件事在等人」和「有几件在干」—— Router 每次醒来最该先知道的两个数。
    pub fn ticket_counts(&self, router_id: &str) -> anyhow::Result<(usize, usize)> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT stage, COUNT(*) FROM tickets
                 WHERE router_id = ?1 AND stage NOT IN ('done', 'cancelled')
                 GROUP BY stage",
            )?;
            let mut rows = stmt.query([router_id])?;
            let (mut working, mut waiting) = (0usize, 0usize);
            while let Some(r) = rows.next()? {
                let stage = Stage::parse(&r.get::<_, String>(0)?);
                let n: i64 = r.get(1)?;
                if stage.needs_person() {
                    waiting += n as usize;
                } else if stage.open() {
                    working += n as usize;
                }
            }
            Ok((working, waiting))
        })
    }

    fn ticket_query(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> anyhow::Result<Vec<Ticket>> {
        self.with(|c| {
            let mut stmt = c.prepare(sql)?;
            let mut rows = stmt.query(params)?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push(Ticket {
                    rowid: r.get(0)?,
                    id: r.get(1)?,
                    title: r.get(2)?,
                    brief: r.get(3)?,
                    session_id: r.get(4)?,
                    router_id: r.get(5)?,
                    stage: Stage::parse(&r.get::<_, String>(6)?),
                    result: r.get(7)?,
                    report: Report::parse(&r.get::<_, String>(8)?),
                    ended_at: r.get::<_, i64>(9)?.max(0) as u64,
                    ended_by: r.get(10)?,
                    dispatches: r.get::<_, i64>(11)?.max(0) as u32,
                    created_at: r.get::<_, i64>(12)?.max(0) as u64,
                    updated_at: r.get::<_, i64>(13)?.max(0) as u64,
                });
            }
            Ok(out)
        })
    }
}

/// 这条会话这一轮如果是在干工作项，把它推到「在干」。
///
/// 在 `agent::turn_once` 的开头调 —— **回合真的开跑了**那一刻。`dispatch` 只是
/// 把消息排进队列（那条会话可能正忙着），在那里标「在干」是撒谎。
///
/// 失败只记日志：工作项状态不准不该让一轮活跑不起来。
pub async fn mark_started(core: &crate::Core, session_id: &str) {
    let Some(db) = core.state.db() else { return };
    let Ok(Some(tid)) = db.ticket_of_session(session_id) else {
        return; // 不是在干工作项（用户自己开的会话 / Router 自己）
    };
    if let Err(e) = db.ticket_started(&tid, crate::agent::now_ms()) {
        tracing::warn!("工作项没能标成「在干」（界面会一直显示排队中）: {e:#}");
    }
}

/// 渲染一个工作项的一行（给模型看的）。
///
/// 形状照抄 dsh 的 `ticketCard`：**紧凑 + 带 `read:` 指针**。
/// 每个条目都要能顺着指针走到下一个对象 —— 否则模型的探索走一步就断了。
pub fn card(t: &Ticket) -> String {
    let mut line = format!("`{}` {} [{}]", t.id, t.title, t.stage.label());
    if let Some(r) = t.report {
        if r == Report::Progress {
            let _ = write!(line, "（还在动）");
        }
    }
    let head = t.summary(160);
    if !head.is_empty() {
        let _ = write!(line, "：{head}");
    }
    if t.dispatches > 1 {
        let _ = write!(line, "（派过 {} 次）", t.dispatches);
    }
    let _ = write!(line, "  → read: ticket/{}", t.id);
    line
}
