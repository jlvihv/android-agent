//! 定时任务：**到点往 Router 的队列里投一句话，把它叫醒**。
//!
//! 它不指挥任何会话。理由：否则就有两套指挥系统（用户经 Router 一套，定时器直接
//! 投给子会话一套），迟早打架。定时任务也不需要知道目标是谁 —— 那是 Router 的判断。
//!
//! ## 只有一份不可否认的事实：水位线
//!
//! **不存「下次触发时间」。** 那个字段一存进来就必然有一条路径忘了推进它 ——
//! 上一版 `ticket::Stage` 的 `Done` 零调用点、所有工作项烂在伪终态里，就是这么来的
//! （见 docs/16）。这里只存 [`ScheduledTask::handled_through`]：**截止这一刻的触发点
//! 都处理过了**。它是水位线，不是状态标签 —— 没有跃迁规则，只有「单调不减」一条，
//! 唯一的写点是 [`Schedules::claim_due`]。
//!
//! 「下次什么时候」每次现算（[`due_at`] / [`next_at`]），只在 HTTP 响应里露脸，
//! 永远不落库。
//!
//! ## 三层
//!
//! | 层 | 在哪 | 干什么 |
//! | --- | --- | --- |
//! | 推导 | [`due_at`] / [`next_at`] | **纯函数**：给定任务 + 现在 + 时区，算出「该不该响、响哪一次」 |
//! | 存储 | [`Schedules`] | 内存表 + 唯一写点（水位线只能从 `claim_due` 过） |
//! | 循环 | [`run`] | 一个 10 秒的 `tokio::interval`，认领 → 落水位线 → 投递 |
//! | 触发 | [`fire`] | 拼正文、调 `router::wake`。**投递目标只有 Router** |
//!
//! ## 为什么不搭落盘定时器那班车
//!
//! `lib.rs` 那个 2 秒的 loop 语义是「**有脏数据才写**」（`sessions_dirty.swap(false)`），
//! 而这里要的是「**每 tick 都看一遍有没有到点**」。混在一起的话，两边都得迁就对方的
//! 周期：调落盘频率会顺手改掉闹钟的准时性，而这段代码主题（内存/落盘）也会散掉。
//!
//! ## 防风暴
//!
//! 水位线落后很久（进程睡了一天、系统时钟被改）时，从水位线往后可能有几十个触发点
//! 都早于现在。**每个 tick 每条任务最多触发一次**，取「现在之前最近的那个」——
//! 不补跑。这一条写在 [`due_at`] 里，有测试钉着。

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use chrono::{FixedOffset, TimeZone as _};

use crate::agent::now_ms;
use crate::state::{AppState, QueuedMsg};
use crate::Core;

/// 最多能有几条定时任务。
///
/// **必须有上限**：Router 有 `schedule_task` 这个工具，模型一时兴起就能给自己排一堆
/// 闹钟；而每条到点都开一轮 LLM 请求。32 是一个人在「定期要做的事」上不可能用完的量。
pub const MAX_SCHEDULES: usize = 32;

/// `prompt` 最多多少字。**超了报错，不截断** —— 截断等于悄悄改掉用户写的那句话。
pub const MAX_PROMPT_CHARS: usize = 2000;

/// 多久看一次表。
///
/// 10 秒：分钟级周期的最大迟到 10 秒（对「每天 9 点」无意义），空转成本是
/// 「一次读锁 + ≤32 次整数比较」。定得比落盘那个 2 秒松，是**故意的**：两边没有
/// 关系，别让人以为调一个会影响另一个。
const TICK: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// 周期
// ---------------------------------------------------------------------------

/// 「每 N 秒」这种固定间隔（下限一分钟）。
///
/// **构造时**就把范围钉住（[`Period::try_new_secs`] 对越界返回 `None`），所以
/// 「每 1 秒一次」在类型层面写不出来 —— 不用在每个调用点各判一遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period(u64);

impl Period {
    /// 一分钟。比它快的周期没有意义：Router 一轮就是一个 LLM 请求。
    pub const MIN_SECS: u64 = 60;
    /// 30 天。比它慢的用 cron 表达（`0 9 1 * *` 之类）。
    pub const MAX_SECS: u64 = 30 * 24 * 60 * 60;

    pub fn try_new_secs(secs: u64) -> Option<Self> {
        (Self::MIN_SECS..=Self::MAX_SECS)
            .contains(&secs)
            .then_some(Self(secs))
    }

    /// 界面上的入口是「每 N 分钟」。
    pub fn try_new_minutes(minutes: u64) -> Option<Self> {
        minutes.checked_mul(60).and_then(Self::try_new_secs)
    }

    pub const fn secs(self) -> u64 {
        self.0
    }

    pub const fn ms(self) -> u64 {
        self.0 * 1000
    }

    /// 给人看的一句话。
    pub fn label(self) -> String {
        if self.0.is_multiple_of(60) {
            format!("每 {} 分钟", self.0 / 60)
        } else {
            format!("每 {} 秒", self.0)
        }
    }
}

/// 一条**解析过**的 cron 表达式（5 或 6 字段，serde 不管它）。
///
/// 解析在构造时做完（[`CronExpr::parse`]）：一个非法表达式进不了
/// [`ScheduledTask`]，所以 `due_at` 里不需要处理「表达式坏了」这种分支。
///
/// 为什么用库（`croner`）而不是自己算：`0 9 * * 1` 这种要处理月末、闰年、DST
/// 边界。手写这些没有一天能写对。见 `Cargo.toml` 那句说明。
#[derive(Debug, Clone)]
pub struct CronExpr {
    /// 原始文本（**回显给用户的就是它**，不是库重新拼出来的那份）
    src: String,
    cron: croner::Cron,
}

impl CronExpr {
    /// 解析。错误是一句能直接给人看的话。
    pub fn parse(src: &str) -> Result<Self, String> {
        let src = src.trim();
        if src.is_empty() {
            return Err("cron 表达式是空的".to_string());
        }
        // 5 字段（分 时 日 月 周）是大家认识的那种写法；6 字段带秒，也认
        let cron = src
            .parse::<croner::Cron>()
            .map_err(|e| format!("cron 表达式看不懂（`{src}`）：{e}"))?;
        Ok(Self {
            src: src.to_string(),
            cron,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.src
    }

    /// 「现在之后（不含现在）的下一次」。
    fn next_after(&self, at: chrono::DateTime<FixedOffset>) -> Option<u64> {
        let t = self.cron.find_next_occurrence(&at, false).ok()?;
        Some(t.timestamp_millis().max(0) as u64)
    }

    /// 「现在之前（含现在）的最近一次」。
    fn prev_at_or_before(&self, at: chrono::DateTime<FixedOffset>) -> Option<u64> {
        let t = self.cron.find_previous_occurrence(&at, true).ok()?;
        Some(t.timestamp_millis().max(0) as u64)
    }
}

impl PartialEq for CronExpr {
    fn eq(&self, other: &Self) -> bool {
        // 按原文比：`0 9 * * *` 和 `0 9 * * 0` 派生的东西不一定一样，但**原文**
        // 是用户写的那句话，改一个字就该算改过。
        self.src == other.src
    }
}

impl Eq for CronExpr {}

/// 一条任务多久响一次。
///
/// 两个变体就是两种语义，非法值写不出来：
///
/// - [`Recurrence::Every`]：固定间隔，**锚在创建时刻**（第 k 次 = `created_at + k×周期`）。
///   它能表达 cron 表达不了的东西（每 90 分钟）。
/// - [`Recurrence::Cron`]：挂在**墙上时钟**上（每天 9 点、每周一 9 点）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recurrence {
    Every(Period),
    /// `Box`：`croner::Cron` 解析完之后是 336 字节（一张位图式的字段表），而
    /// `Period` 只有 8 —— 不装箱的话整个枚举 336 字节，`ScheduledTask` 跟着变胖，
    /// 而 tick 里每条任务都要按值比一比（clippy 的 `large_enum_variant` 就是这条）。
    Cron(Box<CronExpr>),
}

impl Recurrence {
    /// 给人看的一句话（界面和工具回显都用它）。
    pub fn label(&self) -> String {
        match self {
            Self::Every(p) => p.label(),
            Self::Cron(c) => format!("cron `{}`", c.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// 一条任务
// ---------------------------------------------------------------------------

/// 一条定时任务（`schedules` 表的一行）。
///
/// 除了 `handled_through`（水位线）之外全是**不可变事实**：改周期 / 改那句话 =
/// 删了重建。没有「编辑」这个动作，也就没有「改到一半」这种中间态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: String,
    /// 到点要投给 Router 的那句话，**原文一字不改**。
    pub prompt: String,
    pub every: Recurrence,
    /// 创建时刻。也是 [`Recurrence::Every`] 那个节奏的锚。
    pub created_at: u64,
    /// 水位线：截止这一刻的触发点都处理过了（投出去了，或按策略放弃）。
    ///
    /// **它不是「上次触发时间」**：暂停 / 恢复时也会被推到 `now`（见
    /// [`Schedules::set_paused`]）。唯一的推进点是 [`Schedules::claim_due`]。
    pub handled_through: u64,
    /// 什么时候停用的。`None` = 启用中。
    ///
    /// 用 `Option` 而不是 `0` 当哨兵（和 `usage.cached`、`machines.relay_addr` 一致）。
    pub paused_at: Option<u64>,
}

impl ScheduledTask {
    /// 现在停着吗。
    pub const fn paused(&self) -> bool {
        self.paused_at.is_some()
    }
}

/// 一次到点的触发（[`Schedules::claim_due`] 的产出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub task_id: String,
    pub prompt: String,
    /// 这一次对应的触发点（已经写进水位线了）
    pub due: u64,
}

// ---------------------------------------------------------------------------
// 推导（纯函数）
// ---------------------------------------------------------------------------

/// 这一次该不该响？该响的话，响的是哪一个触发点？
///
/// **只回「现在之前最近的那一个」**，不补跑 —— 水位线落后 100 个周期也只回一个。
/// 这一条是防风暴的全部实现，别在调用点再写一遍。
///
/// 三个边界（都有测试）：
///
/// - 水位线落后很多 → 回**最近**那一个（不是最早那个，否则一条落后一天的任务会
///   把「9 点」报成「昨天 9 点」）
/// - 水位线在未来（系统时钟被回拨）→ `None`
/// - 现在**正好**卡在触发点上 → 回那个点（cron 用 inclusive 查前一个）
pub fn due_at(task: &ScheduledTask, now: u64, tz_offset_secs: i32) -> Option<u64> {
    if task.paused() {
        return None;
    }
    match &task.every {
        // 固定间隔：整除定位到「now 所在的那一格」。锚在 created_at，所以重启不漂。
        Recurrence::Every(p) => {
            let period = p.ms();
            if now <= task.created_at {
                return None;
            }
            let k = (now - task.created_at) / period;
            if k == 0 {
                return None;
            }
            let due = task.created_at + k * period;
            (due > task.handled_through).then_some(due)
        }
        // cron：问库「现在之前（含现在）的最近一次」。它自己会走日历。
        Recurrence::Cron(c) => {
            let due = c.prev_at_or_before(local(now, tz_offset_secs)?)?;
            (due > task.handled_through).then_some(due)
        }
    }
}

/// 下一次什么时候响（**只给界面看**，不落库）。停用中就是 `None`。
pub fn next_at(task: &ScheduledTask, now: u64, tz_offset_secs: i32) -> Option<u64> {
    if task.paused() {
        return None;
    }
    match &task.every {
        Recurrence::Every(p) => {
            let period = p.ms();
            let k = now.saturating_sub(task.created_at) / period + 1;
            Some(task.created_at + k * period)
        }
        Recurrence::Cron(c) => c.next_after(local(now, tz_offset_secs)?),
    }
}

/// 毫秒时间戳 → 带**固定偏移**的墙上时刻。
///
/// 用固定偏移而不是 IANA 时区库：那要带一份 tzdb（安卓上几 MB），而我们的语义
/// 只需要「此刻这个偏移下的墙上时钟」。跨 DST 的语义差别见 `utc_offset_secs`。
fn local(ms: u64, tz_offset_secs: i32) -> Option<chrono::DateTime<FixedOffset>> {
    // 偏移必须落在 chrono 认可的范围里（±24h）；注入来的值可能离谱
    let tz = FixedOffset::east_opt(tz_offset_secs)?;
    tz.timestamp_millis_opt(ms as i64).single()
}

/// **此刻**本机的 UTC 偏移（秒，东为正）。
///
/// 每次算触发点都重新读，不在启动时定死：用户会旅行、会手动改时区、DST 会切。
/// 读一次是 `localtime_r`（一把全局锁 + 一次区间查找，实测百纳秒级），而调用频率
/// 是**每 tick 一次**（不是每条任务一次）—— 不需要缓存，也就没有「缓存什么时候失效」
/// 这个问题。
///
/// # DST 的语义
///
/// 用「此刻的偏移」把整个搜索窗口当一个固定偏移来算。跨 DST 边界那一刻会有
/// 最多一小时的偏差（那一天的那一次早/晚一小时）。要精确的话得带时区数据库，
/// 而这不是这个功能的量级 —— 明说过，别指望它。
pub fn utc_offset_secs(at_ms: u64) -> i32 {
    libc_offset_secs(at_ms).unwrap_or_else(fallback_offset)
}

/// 从宿主 libc 读偏移。读不出来（少见）就返回 `None`，由调用方用注入值兜。
///
/// `allow(deprecated)`：`libc::time_t` 在 musl 1.2 / 新版 bionic 上从 32 位变 64 位，
/// libc crate 已经把那个别名标了 deprecated。但 `localtime_r` 的签名就是要它 ——
/// 我们要的正是「跟着这个目标平台的 time_t 宽度走」（下面那段 32 位溢出检查靠它）。
/// 换成写死 `i64` 会在 32 位安卓（armv7 / x86）上和 C 侧签名对不上。
#[allow(deprecated)]
fn libc_offset_secs(at_ms: u64) -> Option<i32> {
    let secs = (at_ms / 1000) as i64;
    // 32 位 time_t（armv7 安卓）装不下 2038 之后的时刻：直接放弃，用注入的兜底。
    if std::mem::size_of::<libc::time_t>() < 8 && secs > i64::from(i32::MAX) {
        return None;
    }
    let t: libc::time_t = secs as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `localtime_r` 只写我们给的这块 `tm`，成功返回同一个指针、失败返回空。
    if unsafe { libc::localtime_r(&t, tm.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: 上面确认写成功了。
    let tm = unsafe { tm.assume_init() };
    // `tm_gmtoff` 是 GNU/musl/bionic 都有的扩展字段（本 crate 只编 Linux/安卓/macOS）
    i32::try_from(tm.tm_gmtoff).ok()
}

/// 安卓壳注入的偏移（分钟）。**只是兜底**：`localtime_r` 读不到时才用它
/// （32 位 time_t 的 2038 之后、或者 bionic 那边时区没配）。
///
/// 它不在 `AppState` 里而是模块级原子值：读它的地方是**纯函数**
/// [`utc_offset_secs`]，而那个函数要能被测试直接调，不该先凑一个 `AppState`。
static FALLBACK_OFFSET_MINUTES: AtomicI32 = AtomicI32::new(0);

/// 安卓壳把系统时区送进来（`AndroidEnv.tz_offset_minutes`，见 `ffi`）。
pub fn set_fallback_offset_minutes(minutes: i32) {
    FALLBACK_OFFSET_MINUTES.store(minutes, Ordering::Relaxed);
}

fn fallback_offset() -> i32 {
    FALLBACK_OFFSET_MINUTES.load(Ordering::Relaxed) * 60
}

// ---------------------------------------------------------------------------
// 存储
// ---------------------------------------------------------------------------

/// 定时任务表（内存那份）。挂在 `AppState::schedules` 上。
///
/// 和 `mcp` / `api_styles` 同构：内存是权威的读副本，写的时候一同进库。表本来就
/// 小（上限 32 行），tick 里**一条 SQL 都不发**。
#[derive(Default)]
pub struct Schedules {
    tasks: RwLock<Vec<ScheduledTask>>,
}

impl Schedules {
    /// 从库里读进来的那一份装上（启动时调一次）。
    pub fn load(&self, list: Vec<ScheduledTask>) {
        if let Ok(mut g) = self.tasks.write() {
            *g = list;
        }
    }

    /// 全部任务（**按创建时间**，界面顺序稳定）。
    pub fn list(&self) -> Vec<ScheduledTask> {
        let Ok(g) = self.tasks.read() else {
            return Vec::new();
        };
        let mut out = g.clone();
        out.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    pub fn get(&self, id: &str) -> Option<ScheduledTask> {
        self.tasks.read().ok()?.iter().find(|t| t.id == id).cloned()
    }

    /// 建一条。**唯一的创建入口**（HTTP 和 Router 工具都走它，就没有两套校验）。
    pub fn create(
        &self,
        st: &AppState,
        prompt: &str,
        every: Recurrence,
    ) -> Result<ScheduledTask, String> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err("要说什么？`prompt` 是空的".to_string());
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(format!(
                "那句话太长了（{} 字，上限 {MAX_PROMPT_CHARS}）。精简一下，别让它变成一篇文章。",
                prompt.chars().count()
            ));
        }
        let now = now_ms();
        let task = ScheduledTask {
            id: uuid::Uuid::now_v7().to_string(),
            prompt: prompt.to_string(),
            every,
            created_at: now,
            // 新任务的水位线 = 创建时刻：**从下一条周期开始响**，不会一建出来就立刻
            // 响一次（「每 30 分钟」不该在建的那一刻就吵）。
            handled_through: now,
            paused_at: None,
        };
        // 上限检查放在写锁里：两个并发请求同时看到「只有 31 条」不能各建一条。
        // **锁里只有内存操作**：SQLite 写盘几毫秒到几十毫秒（手机闪存），而在锁里
        // 做的话这段时间 `claim_due`（10 秒 tick 的唯一入口）会被挡住 —— 同一个文件
        // 里 `persist_cursor` 特意用了 `spawn_blocking`，两处该一致。
        {
            let mut g = self
                .tasks
                .write()
                .map_err(|_| "定时任务表坏了".to_string())?;
            if g.len() >= MAX_SCHEDULES {
                return Err(format!(
                    "已经有 {MAX_SCHEDULES} 条定时任务了（上限）。先用 `schedules` 删掉几条不用的。"
                ));
            }
            g.push(task.clone()); // 先占位（锁内零 IO）
        }
        // 出锁再写库。写失败就把刚占的位撤掉 —— 内存和库不能对不上
        // （否则界面上多一条「存不下来」的任务，重启之后就消失了）。
        if let Some(db) = st.db() {
            if let Err(e) = db.put_schedule(&task) {
                if let Ok(mut g) = self.tasks.write() {
                    g.retain(|t| t.id != task.id);
                }
                return Err(format!("存不下来：{e:#}"));
            }
        }
        Ok(task)
    }

    /// 停用 / 恢复。
    ///
    /// **两个方向都推水位线到 `now`**：停用期间积下来的触发点不补跑，恢复之后从
    /// 下一个整点开始 —— 否则「停了一周再开」会立刻炸出一堆提醒。
    pub fn set_paused(
        &self,
        st: &AppState,
        id: &str,
        paused: bool,
    ) -> Result<ScheduledTask, String> {
        let now = now_ms();
        // 锁里只改内存（理由同 `create`）。改之前把**原来那一条**整个留着，
        // 写库失败时用它整份换回去 —— 比逐个字段还原可靠（少一个字段就是脏数据）。
        let (before, after) = {
            let mut g = self
                .tasks
                .write()
                .map_err(|_| "定时任务表坏了".to_string())?;
            let Some(t) = g.iter_mut().find(|t| t.id == id) else {
                return Err(format!("没有这条定时任务：`{id}`"));
            };
            let paused_at = paused.then_some(now);
            if t.paused_at == paused_at {
                // 已经是这个状态了：什么都不做（别白写一次库）
                return Ok(t.clone());
            }
            let before = t.clone();
            t.paused_at = paused_at;
            t.handled_through = t.handled_through.max(now);
            (before, t.clone())
        };
        if let Some(db) = st.db() {
            if let Err(e) = db.put_schedule(&after) {
                if let Ok(mut g) = self.tasks.write() {
                    if let Some(t) = g.iter_mut().find(|t| t.id == id) {
                        *t = before;
                    }
                }
                return Err(format!("存不下来：{e:#}"));
            }
        }
        Ok(after)
    }

    /// 删掉一条。返回它原来在不在。
    ///
    /// 顺序和 `create` / `set_paused` 相反：**先删库、再删内存**。
    ///
    /// 理由是回滚代价不对称：删内存是 `remove` 一个元素，删库那一行得能整份写回去
    /// （`put_schedule` 只写它认的那几列），而「内存里还有、库里没了」这种错位
    /// 比反过来的更安全 —— 内存是权威副本，下一轮 `claim_due` 还会让它响一次，
    /// 而不是**悄悄少了一条用户的闹钟**。
    ///
    /// 锁里仍然只有内存操作：`try_write` 一次拿到就做，拿不到（有人正在 `claim_due`）
    /// 就等一小会儿 —— 删库已经成功了，这一步不能失败。
    pub fn delete(&self, st: &AppState, id: &str) -> Result<bool, String> {
        let exists = self.get(id).is_some();
        if !exists {
            return Ok(false);
        }
        if let Some(db) = st.db() {
            db.delete_schedule(id)
                .map_err(|e| format!("删不掉：{e:#}"))?;
        }
        // 删完库再改内存。这里用阻塞的 `write()`（不是 `try_write`）：库里那一行
        // 已经没了，内存这份**必须**跟着消失，否则下一次 tick 会投出一条
        // 「已经不该存在」的提醒。等一小会儿比留下脏数据好。
        let mut g = self
            .tasks
            .write()
            .map_err(|_| "定时任务表坏了".to_string())?;
        g.retain(|t| t.id != id);
        Ok(true)
    }

    /// **认领**到点的任务：挑出来，并把水位线推到那个触发点。
    ///
    /// 这是水位线的**唯一写点**（除了暂停/恢复那两个「跟上现在」）。在锁里推进水位线
    /// 本身就是幂等栅栏：同一次触发不会被认领两遍，即使以后把 [`fire`] 改成
    /// `tokio::spawn` 也一样。
    ///
    /// 时区只读一次（不是每条任务读一次）。
    pub fn claim_due(&self, now: u64) -> Vec<Claim> {
        let tz = utc_offset_secs(now);
        let Ok(mut g) = self.tasks.write() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for t in g.iter_mut() {
            if let Some(due) = due_at(t, now, tz) {
                t.handled_through = due;
                out.push(Claim {
                    task_id: t.id.clone(),
                    prompt: t.prompt.clone(),
                    due,
                });
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 循环
// ---------------------------------------------------------------------------

/// 后台循环：每 [`TICK`] 看一次表。
///
/// 顺序是**先落水位线、再投递**：崩在中间的话，这一次提醒丢了；反过来的话会
/// 投两次（同一次提醒开两个回合、白烧两份 token）。宁可少叫一次。
pub async fn run(core: Core) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` 的第一次 tick 立刻返回：跳过它，别在启动的那一刻就看表
    tick.tick().await;
    loop {
        tick.tick().await;
        let claims = core.state.schedules.claim_due(now_ms());
        if claims.is_empty() {
            // 绝大多数 tick 走这里：没有分配、没有 IO、没有 await
            continue;
        }
        persist_cursor(&core, &claims).await;
        for c in &claims {
            fire(&core, c).await;
        }
    }
}

/// 把新的水位线写进库（一次一批）。
async fn persist_cursor(core: &Core, claims: &[Claim]) {
    let Some(db) = core.state.db() else {
        return;
    };
    let rows: Vec<(String, u64)> = claims.iter().map(|c| (c.task_id.clone(), c.due)).collect();
    // SQLite 写盘是阻塞 IO，别坐 worker 上
    let res = tokio::task::spawn_blocking(move || {
        for (id, due) in rows {
            db.set_schedule_cursor(&id, due)?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("定时任务水位线没写进去（重启后可能重复一次）：{e:#}"),
        Err(e) => tracing::warn!("定时任务水位线任务挂了：{e}"),
    }
}

/// 到点投进去的那段正文。
///
/// **标记是给模型看的唯一线索**：`Origin` 不进请求体（见 `agent::Origin`），而 API 层
/// 的 `user` 角色带不了「这是闹钟」这个字段。和 `<worker_report>` 一个道理
/// （见 `router::report_back`）。三句话各管一件：
///
/// 1. `<schedule task="…">` 划出边界，并带上 task id（模型要停它时用得上）
/// 2. 原话**一字不改**（那是用户当初写的）
/// 3. 明说「不是用户此刻说的」+ 给它「不派」这个选项（否则它会为了响应而硬派）
fn body_for(c: &Claim) -> String {
    let mut body = String::with_capacity(c.prompt.len() + 320);
    let _ = write!(
        body,
        "<schedule task=\"{id}\">\n{prompt}\n</schedule>\n\
         （上面这条是**定时器**到点投的，不是用户此刻说的：它是当初设这条闹钟时写下的原话。\
         该不该派活、派给谁，你自己判断；不派就直接说明，别为了响应它而硬派。）",
        id = c.task_id,
        prompt = c.prompt.trim(),
    );
    body
}

/// 投一条到 Router 的队列里，把它叫醒。
///
/// **唯一的目标是 Router** —— 它自己决定派不派活、派给谁。定时任务不碰子会话。
async fn fire(core: &Core, c: &Claim) {
    let router = crate::router::ensure(&core.state).await;
    // 正文里的标记是**给模型**的唯一线索：`Origin` 不进请求体（见 `agent::Origin`），
    // 而 API 层的 `user` 角色带不了「这是闹钟」这个字段。和 `<worker_report>` 一个道理。
    let msg = QueuedMsg {
        id: uuid::Uuid::now_v7().to_string(),
        text: body_for(c),
        attachments: Vec::new(),
        // 第四个变体：让**代码**也分得清「用户此刻说的」和「闹钟说的」
        origin: crate::agent::Origin::Schedule {
            task_id: c.task_id.clone(),
        },
    };
    if crate::router::wake(core, &router, msg).is_err() {
        // 队列满了：**丢掉这一次，只 warn**，不重试。重试的话每 10 秒撞一次，
        // 日志会变成噪音，而且真正的「Router 卡住了」会被淹没。
        tracing::warn!(
            "Router 的队列满了，定时任务 {} 这一次（{}）丢了",
            c.task_id,
            crate::text::clip(&c.prompt, 60)
        );
    }
}

// ---------------------------------------------------------------------------
// 给人 / 给模型的入口
//
// 两份入口（HTTP 和 Router 的工具）**都只是调上面那几个方法**，一行校验都不重复
// —— 否则「工具说能建、HTTP 说不能」这种不一致迟早出现。
// ---------------------------------------------------------------------------

/// 一句给模型看的回显：这条任务什么时候会响。
fn describe(task: &ScheduledTask, now: u64, tz: i32) -> String {
    let next = match next_at(task, now, tz) {
        Some(n) => format!(
            "下一次：{}",
            local(n, tz).map_or_else(|| n.to_string(), |d| d.format("%Y-%m-%d %H:%M").to_string())
        ),
        None => "现在停着".to_string(),
    };
    format!(
        "`{}` {} —— {next}。到点会把那句话投给协调层。",
        task.id,
        task.every.label()
    )
}

/// 建。**唯一入口**：HTTP 和 Router 工具都走它。
///
/// 错误是一句能直接给人看的话（HTTP 把状态码配成 400，工具原样回给模型）。
pub fn create(
    st: &AppState,
    prompt: &str,
    every: Option<u64>,
    cron: Option<&str>,
) -> Result<ScheduledTask, String> {
    // 「两种周期二选一」在类型上是 `Recurrence`，但入口收的是两个 Option：
    // 这里是**唯一**把它们折叠成一个 `Recurrence` 的地方。
    let every = match (every, cron.map(str::trim).filter(|s| !s.is_empty())) {
        (Some(m), None) => Recurrence::Every(
            Period::try_new_minutes(m)
                .ok_or_else(|| format!("每 {m} 分钟不行：最短 1 分钟，最长 30 天"))?,
        ),
        (None, Some(c)) => Recurrence::Cron(Box::new(CronExpr::parse(c)?)),
        (Some(_), Some(_)) => {
            return Err(
                "`every_minutes` 和 `cron` 只能给一个（它们是两种不同的周期语义）".to_string(),
            )
        }
        (None, None) => return Err("要给周期：`every_minutes`（每 N 分钟）或者 `cron`".to_string()),
    };
    st.schedules.create(st, prompt, every)
}

/// 停 / 恢复 / 删。三个动作都只认 id。
pub fn set_state(st: &AppState, id: &str, action: &str) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("要给 `schedule_id`（`list` 里反引号里的那个串）".to_string());
    }
    match action {
        "pause" => {
            let t = st.schedules.set_paused(st, id, true)?;
            Ok(format!(
                "已停用 {}。到点不会再叫你了，`resume` 可以恢复。",
                t.id
            ))
        }
        "resume" => {
            let t = st.schedules.set_paused(st, id, false)?;
            let now = now_ms();
            Ok(format!(
                "已恢复。{}",
                describe(&t, now, utc_offset_secs(now))
            ))
        }
        "delete" => {
            if st.schedules.delete(st, id)? {
                Ok(format!("已删掉 `{id}`。"))
            } else {
                Err(format!("没有这条定时任务：`{id}`"))
            }
        }
        other => Err(format!(
            "不认识的 action `{other}`（list / pause / resume / delete）"
        )),
    }
}

/// 全部任务的**给人看的一段**（HTTP 的 JSON 和工具的回显都从这份数据来）。
pub fn rows(st: &AppState, now: u64, tz: i32) -> Vec<serde_json::Value> {
    st.schedules
        .list()
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "prompt": t.prompt,
                "every": t.every.label(),
                "everyMinutes": match &t.every {
                    Recurrence::Every(p) => Some(p.secs() / 60),
                    Recurrence::Cron(_) => None,
                },
                "cron": match &t.every {
                    Recurrence::Cron(c) => Some(c.as_str()),
                    Recurrence::Every(_) => None,
                },
                "paused": t.paused(),
                "createdAt": t.created_at,
                // **现算的**，永远不落库（见模块文档）
                "nextAt": next_at(t, now, tz),
            })
        })
        .collect()
}

/// `schedule_task`：Router 建一条定时任务。
pub async fn tool_schedule_task(core: &Core, args: &serde_json::Value) -> crate::router::Reply {
    let st = &core.state;
    // 参数解析走 `router` 那三个既有的小函数（同一套语义：没给就是空 / false / 默认）
    let prompt = crate::router::arg_str_pub(args, "prompt");
    let every = args
        .get("every_minutes")
        .and_then(serde_json::Value::as_u64);
    let cron = crate::router::arg_str_pub(args, "cron");
    let task = create(st, prompt, every, (!cron.is_empty()).then_some(cron))?;
    let now = now_ms();
    Ok(format!(
        "{}到点我会被叫醒，自己判断要不要派活。（这句是给用户看的话，就不用再重复了。）",
        describe(&task, now, utc_offset_secs(now))
    ))
}

/// `schedules`：多动作（list / pause / resume / delete）。
pub async fn tool_schedules(
    core: &Core,
    _from: &str,
    args: &serde_json::Value,
) -> crate::router::Reply {
    let st = &core.state;
    let action = crate::router::arg_str_pub(args, "action");
    let id = crate::router::arg_str_pub(args, "schedule_id");
    match action {
        "" | "list" => {
            let now = now_ms();
            let tz = utc_offset_secs(now);
            let list = st.schedules.list();
            if list.is_empty() {
                return Ok("还没有定时任务。（要建就用 `schedule_task`。）".to_string());
            }
            let mut out = format!("{} 条定时任务：\n", list.len());
            for t in &list {
                let _ = writeln!(out, "- {}", describe(t, now, tz));
            }
            Ok(out)
        }
        "pause" | "resume" | "delete" => set_state(st, id, action),
        other => Err(format!(
            "不认识的 action `{other}`（list / pause / resume / delete）"
        )),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-06-01T00:00:00Z（一个周一）。测试全用它当基准。
    const BASE: u64 = 1_780_272_000_000;
    const H: i32 = 3600;
    const DAY: u64 = 86_400_000;

    fn task(every: Recurrence, created_at: u64, handled_through: u64) -> ScheduledTask {
        ScheduledTask {
            id: "t1".into(),
            prompt: "看看那台机器还活着没".into(),
            every,
            created_at,
            handled_through,
            paused_at: None,
        }
    }

    fn daily9() -> Recurrence {
        Recurrence::Cron(Box::new(CronExpr::parse("0 9 * * *").unwrap()))
    }

    // ---- 防风暴的那三条边界 ------------------------------------------------

    /// 某个本地墙上时刻（年-月-日 时:分）对应的毫秒时间戳。
    ///
    /// 测试里要断言「响的就是那一个 9 点」，就得先能算出一个 9 点是哪个时刻 ——
    /// 用一个和被测代码无关的算法（`chrono` 的 `and_utc` 减偏移），不然是自证。
    fn local_at(y: i32, mo: u32, d: u32, h: u32, mi: u32, off_secs: i32) -> u64 {
        let naive = chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap();
        (naive.and_utc().timestamp_millis() - off_secs as i64 * 1000) as u64
    }

    /// 水位线落后 100 个周期 → **只回一个**，而且回的是**最近**那个。
    ///
    /// 变异验证：如果 `due_at` 改成「从水位线往后数第一个」（或循环攒齐所有错过的），
    /// 这条会挂 —— 那条表达式本身就是「补跑」。
    #[test]
    fn a_long_overdue_task_yields_exactly_one_claim_at_the_latest_slot() {
        // BASE 是 2026-06-01T00:00Z，也就是 +08:00 的**早上 8 点** —— 那天还没到 9 点。
        let now = BASE + 100 * DAY;
        let t = task(daily9(), BASE, BASE);
        let due = due_at(&t, now, 8 * H).expect("落后 100 天也该响");

        // now 在 +08:00 是 2026-09-09 08:00，最近的那个 9 点是**前一天**（09-08）9 点
        let now_local = local(now, 8 * H).unwrap();
        assert_eq!(
            now_local.format("%Y-%m-%d %H:%M").to_string(),
            "2026-09-09 08:00"
        );
        assert_eq!(
            due,
            local_at(2026, 9, 8, 9, 0, 8 * H),
            "要报最近那一次，不是最早那次"
        );
        // 而且它确实是「刚刚过去」的那一个（不到一天）
        assert!(now - due < DAY, "错过的那些不能攒着补跑");

        // 认领一次只产生 1 个 claim
        let s = Schedules::default();
        s.load(vec![t]);
        assert_eq!(s.claim_due(now).len(), 1, "落后 100 个周期也只响一次");
        // 同一个时刻再认领一次：水位线已经在那个点上，不再响
        assert_eq!(s.claim_due(now).len(), 0, "同一次触发不该认领两遍");
    }

    /// 水位线在未来（系统时钟被回拨）→ `None`，不触发。
    #[test]
    fn a_watermark_in_the_future_silences_the_task() {
        let now = BASE + DAY;
        let t = task(daily9(), BASE, now + 10 * DAY);
        assert_eq!(due_at(&t, now, 8 * H), None);
        let s = Schedules::default();
        s.load(vec![t]);
        assert!(s.claim_due(now).is_empty());

        // 固定间隔那条同理
        let t2 = task(
            Recurrence::Every(Period::try_new_minutes(30).unwrap()),
            BASE,
            now + 10 * DAY,
        );
        assert_eq!(due_at(&t2, now, 8 * H), None);
    }

    /// 现在**正好**卡在触发点上 → 就是它（cron 用 inclusive 查前一个）。
    ///
    /// 变异验证：把 `find_previous_occurrence(_, true)` 的 `inclusive` 改成 `false`，
    /// 「正好卡点上」会返回**上一次**（前一天的 9 点）——这条会挂。
    #[test]
    fn landing_exactly_on_a_slot_fires_that_slot() {
        let nine_1st = local_at(2026, 6, 1, 9, 0, 8 * H);
        let nine_2nd = local_at(2026, 6, 2, 9, 0, 8 * H);

        // 水位线停在创建时刻（06-01 08:00），现在正好 06-01 09:00 → 就是这一点
        let t = task(daily9(), BASE, BASE);
        assert_eq!(
            due_at(&t, nine_1st, 8 * H),
            Some(nine_1st),
            "9:00 那一刻算在「现在之前（含现在）」里"
        );

        // 水位线已经推到 06-01 09:00 之后：差 1 毫秒不算到，刚过也不算重放
        let seen = task(daily9(), BASE, nine_1st);
        assert_eq!(due_at(&seen, nine_1st, 8 * H), None, "同一格不能响两遍");
        assert_eq!(due_at(&seen, nine_2nd - 1, 8 * H), None, "差 1 毫秒还没到");
        assert_eq!(due_at(&seen, nine_2nd, 8 * H), Some(nine_2nd), "到了就是它");
    }

    // ---- 固定间隔 ---------------------------------------------------------

    #[test]
    fn every_period_is_anchored_at_creation_and_never_drifts() {
        let p = Period::try_new_minutes(30).unwrap();
        let t = task(Recurrence::Every(p), BASE, BASE);
        // 还没到第一次
        assert_eq!(due_at(&t, BASE + 60_000, 0), None);
        // 正好 30 分钟
        assert_eq!(due_at(&t, BASE + 30 * 60_000, 0), Some(BASE + 30 * 60_000));
        // 45 分钟：响的是第 30 分钟那一格
        assert_eq!(due_at(&t, BASE + 45 * 60_000, 0), Some(BASE + 30 * 60_000));
        // 水位线推上去之后：下一格
        let t = task(Recurrence::Every(p), BASE, BASE + 30 * 60_000);
        assert_eq!(due_at(&t, BASE + 45 * 60_000, 0), None, "这一格响过了");
        assert_eq!(due_at(&t, BASE + 60 * 60_000, 0), Some(BASE + 60 * 60_000));
    }

    #[test]
    fn a_paused_task_never_fires() {
        let mut t = task(daily9(), BASE, BASE);
        t.paused_at = Some(BASE);
        assert_eq!(due_at(&t, BASE + 100 * DAY, 8 * H), None);
        assert_eq!(next_at(&t, BASE, 8 * H), None);
    }

    // ---- `next_at` 只是给界面看的 -----------------------------------------

    #[test]
    fn next_at_walks_forward_from_the_watermark() {
        let t = task(daily9(), BASE, BASE);
        let n = next_at(&t, BASE, 8 * H).expect("停用才没有下一次");
        let d = local(n, 8 * H).unwrap();
        assert_eq!(
            (d.format("%H:%M").to_string()),
            "09:00",
            "每天 9 点：下一次就该是 9 点，实际 {d}"
        );
        assert!(n > BASE);
    }

    // ---- 时区 -------------------------------------------------------------

    /// **同一个任务 + 同一个 now，换时区就是换墙上时刻**。
    ///
    /// 这就是那条产品决策：在北京设的「每天 9 点」，飞到伦敦之后响的是伦敦的 9 点。
    #[test]
    fn the_same_task_follows_the_current_offset() {
        let t = task(daily9(), BASE, BASE);
        let beijing = due_at(&t, BASE + DAY, 8 * H).unwrap();
        let london = due_at(&t, BASE + DAY, 0).unwrap();
        assert_ne!(beijing, london, "两个时区不能算出同一个时刻");
        assert_eq!(
            local(beijing, 8 * H).unwrap().format("%H:%M").to_string(),
            "09:00"
        );
        assert_eq!(
            local(london, 0).unwrap().format("%H:%M").to_string(),
            "09:00"
        );
        // 北京时间 9 点 = UTC 1 点，比伦敦的 9 点早 8 小时
        assert_eq!(london - beijing, 8 * H as u64 * 1000);
    }

    /// `utc_offset_secs` 在真实宿主上读得出东西来（读库里没有 tzdb 也能用）。
    #[test]
    fn the_host_offset_is_readable_and_sane() {
        let off = utc_offset_secs(now_ms());
        // 全世界现实里的偏移都在 ±14 小时以内
        assert!((-14 * H..=14 * H).contains(&off), "宿主偏移离谱：{off} 秒");
        assert_eq!(off % 60, 0, "偏移应该是整分钟：{off} 秒");
    }

    // ---- 构造期校验（非法值写不出来）--------------------------------------

    #[test]
    fn period_rejects_anything_outside_its_range() {
        assert!(Period::try_new_secs(59).is_none(), "比一分钟快的不许");
        assert!(Period::try_new_secs(60).is_some());
        assert!(Period::try_new_minutes(0).is_none());
        assert!(Period::try_new_minutes(1).is_some());
        assert!(Period::try_new_minutes(30 * 24 * 60).is_some());
        assert!(Period::try_new_minutes(30 * 24 * 60 + 1).is_none());
        // 溢出不能 panic
        assert!(Period::try_new_minutes(u64::MAX).is_none());
    }

    // ---- 存储 / 认领（真库）------------------------------------------------

    fn core_with_db_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-sched-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn core_with_db(tag: &str) -> Core {
        let core = Core::new();
        core.set_data_dir(core_with_db_dir(tag));
        core
    }

    /// 建 → 落库 → 重启后还在（水位线和暂停状态都保住）。
    #[tokio::test]
    async fn a_task_survives_a_restart_with_its_watermark() {
        let dir = core_with_db_dir("restart");
        let core = Core::new();
        core.set_data_dir(dir.clone());
        let t = create(&core.state, "看看那台机器", Some(30), None).expect("建得了");
        assert_eq!(
            t.every,
            Recurrence::Every(Period::try_new_minutes(30).unwrap())
        );
        assert!(!t.paused());
        // 新任务的水位线 = 创建时刻：**不会一建出来就立刻响**
        assert_eq!(t.handled_through, t.created_at);
        assert_eq!(
            due_at(&t, t.created_at + 60_000, 0),
            None,
            "建出来一分钟内不该响"
        );

        // 模拟一次触发（把水位线推上去）
        {
            let s = &core.state.schedules;
            let later = t.created_at + 31 * 60_000;
            let claims = s.claim_due(later);
            assert_eq!(claims.len(), 1);
            assert_eq!(claims[0].due, t.created_at + 30 * 60_000);
            // 后台那个 `persist_cursor` 做的事，这里直接调（测试不等 10 秒的 tick）
            core.state
                .db()
                .unwrap()
                .set_schedule_cursor(&t.id, claims[0].due)
                .unwrap();
        }

        // 「重启」：同一个数据目录、一个新的 Core
        let core2 = Core::new();
        core2.set_data_dir(dir);
        let list = core2.state.schedules.list();
        assert_eq!(list.len(), 1, "重启后任务还在");
        assert_eq!(list[0].id, t.id);
        assert_eq!(list[0].prompt, "看看那台机器");
        assert_eq!(
            list[0].handled_through,
            t.created_at + 30 * 60_000,
            "水位线要跟到库里"
        );
    }

    /// 停用 / 恢复：两个方向都推水位线到「现在」，所以停一周再开不会炸出一堆。
    ///
    /// 要钉的是**次数**（一周里错过的那 7 次不能补跑），不是某个具体时刻 ——
    /// `set_paused` 用的是真实的 `now_ms()`，测试没法注入时钟。
    #[tokio::test]
    async fn pausing_and_resuming_skips_the_gap_instead_of_replaying_it() {
        let core = core_with_db("pause");
        let t = create(&core.state, "每天的活", None, Some("0 9 * * *")).expect("建得了 cron");
        assert!(matches!(&t.every, Recurrence::Cron(_)));
        let real_now = now_ms();

        let paused = core
            .state
            .schedules
            .set_paused(&core.state, &t.id, true)
            .unwrap();
        assert!(paused.paused());
        assert!(
            paused.handled_through >= real_now.min(t.created_at),
            "停用时水位线跟上现在"
        );

        // 停用期间过了一周：一声不响（`claim_due` 里 `paused` 先挡掉）
        let week_later = real_now + 7 * DAY;
        assert!(core.state.schedules.claim_due(week_later).is_empty());

        // 恢复：水位线推到现在，之后**最多响一次**（不是补那 7 次）
        core.state
            .schedules
            .set_paused(&core.state, &t.id, false)
            .unwrap();
        let resumed = core.state.schedules.get(&t.id).unwrap();
        assert!(!resumed.paused());
        assert!(resumed.handled_through >= real_now);
        let claims = core.state.schedules.claim_due(week_later);
        assert_eq!(claims.len(), 1, "停用一周后恢复只能响一次，不能补跑 7 次");
        // 而且响的是「刚才过去的那一个 9 点」，不是一周前那个
        assert!(week_later - claims[0].due < DAY, "要报最近那次");
    }

    /// 删掉之后库里也没了。
    #[tokio::test]
    async fn delete_removes_it_from_memory_and_from_the_db() {
        let core = core_with_db("delete");
        let t = create(&core.state, "临时盯着", Some(60), None).unwrap();
        assert!(core.state.schedules.delete(&core.state, &t.id).unwrap());
        assert!(core.state.schedules.get(&t.id).is_none());
        assert!(
            !core.state.schedules.delete(&core.state, &t.id).unwrap(),
            "删两次返回 false"
        );
        assert!(
            core.state.db().unwrap().schedules().unwrap().is_empty(),
            "库里也没了"
        );
    }

    // ---- 写库失败时内存不留脏数据 -----------------------------------------
    //
    // 三处写操作都是「锁里只改内存 → 出锁再写库」（`create` / `set_paused`，
    // `delete` 反过来：先写库再改内存）。写库失败必须让内存也回到原样 ——
    // 否则界面上多一条（或少一条）任务、重启之后又不见了，比直接报错糟得多。
    //
    // 造失败的办法：库是真的（`set_data_dir` 开出来的），但把 `schedules` 表
    // `DROP` 掉 —— 之后任何写都必然 `no such table`。这比「只读目录」可靠
    // （SQLite 在只读目录上会退回内存库，那样根本不会失败）。

    #[test]
    fn create_rolls_back_the_placeholder_when_the_db_write_fails() {
        let core = Core::new();
        // 没有库 → `st.db()` 是 None → **不写库也不失败**。这不是我们要测的路径，
        // 所以先放一个真库，再把它的表删掉（写入必失败）。
        core.set_data_dir(core_with_db_dir("rollback-create"));
        core.state
            .db()
            .unwrap()
            .with(|c| c.execute_batch("DROP TABLE schedules"))
            .unwrap();

        let before = core.state.schedules.list().len();
        let err = core
            .state
            .schedules
            .create(
                &core.state,
                "这条存不进去",
                Recurrence::Every(Period::try_new_minutes(30).unwrap()),
            )
            .expect_err("表都没了，写库必然失败");
        assert!(err.contains("存不下来"), "{err}");
        assert_eq!(
            core.state.schedules.list().len(),
            before,
            "写库失败后内存里**不能**留下那条"
        );
        assert!(
            core.state
                .schedules
                .list()
                .iter()
                .all(|t| t.prompt != "这条存不进去"),
            "占位要真的撤掉"
        );
    }

    #[test]
    fn set_paused_restores_the_previous_row_when_the_db_write_fails() {
        let core = Core::new();
        core.set_data_dir(core_with_db_dir("rollback-pause"));
        let t = create(&core.state, "原本是启用的", Some(30), None).unwrap();
        assert!(!t.paused());

        core.state
            .db()
            .unwrap()
            .with(|c| c.execute_batch("DROP TABLE schedules"))
            .unwrap();

        let err = core
            .state
            .schedules
            .set_paused(&core.state, &t.id, true)
            .expect_err("写不进去就该报错");
        assert!(err.contains("存不下来"), "{err}");
        let after = core
            .state
            .schedules
            .get(&t.id)
            .expect("还在（失败的只是写库）");
        assert!(!after.paused(), "内存要整份还原成「启用中」");
        assert_eq!(after, t, "还原成**一模一样**的那一行（含水位线）");
    }

    #[test]
    fn delete_does_not_touch_memory_when_the_db_write_fails() {
        let core = Core::new();
        core.set_data_dir(core_with_db_dir("rollback-delete"));
        let t = create(&core.state, "删不掉的那条", Some(30), None).unwrap();

        core.state
            .db()
            .unwrap()
            .with(|c| c.execute_batch("DROP TABLE schedules"))
            .unwrap();

        let err = core
            .state
            .schedules
            .delete(&core.state, &t.id)
            .expect_err("写不进去就该报错");
        assert!(err.contains("删不掉"), "{err}");
        assert!(
            core.state.schedules.get(&t.id).is_some(),
            "库里删失败时内存**必须**留着 —— 悄悄少一条用户的闹钟比多响一次糟"
        );
    }

    // ---- 入口校验（只有一份）----------------------------------------------

    /// HTTP 和 Router 工具都走 `create`：所以校验只可能有一份。
    #[tokio::test]
    async fn the_create_entry_point_rejects_every_bad_shape() {
        let core = core_with_db("create");
        let st = &core.state;
        // 两种周期必须二选一
        assert!(create(st, "话", None, None)
            .unwrap_err()
            .contains("要给周期"));
        assert!(create(st, "话", Some(30), Some("0 9 * * *"))
            .unwrap_err()
            .contains("只能给一个"));
        // 空 / 太长
        assert!(create(st, "   ", Some(30), None)
            .unwrap_err()
            .contains("空的"));
        let long = "字".repeat(MAX_PROMPT_CHARS + 1);
        let err = create(st, &long, Some(30), None).unwrap_err();
        assert!(err.contains("太长了"), "{err}");
        assert!(
            err.contains(&MAX_PROMPT_CHARS.to_string()),
            "要说清上限：{err}"
        );
        // **恰好**到上限可以（不是「超了就静默截断」，而是边界本身合法）
        assert!(create(st, &"字".repeat(MAX_PROMPT_CHARS), Some(30), None).is_ok());
        // 周期越界
        assert!(create(st, "话", Some(0), None).is_err());
        assert!(create(st, "话", Some(30 * 24 * 60 + 1), None).is_err());
        // cron 看不懂就报，不静默换成别的
        assert!(create(st, "话", None, Some("每天九点")).is_err());
    }

    /// 上限是硬的：第 33 条被拒（而且是要人话，不是 panic）。
    #[tokio::test]
    async fn the_33rd_task_is_refused() {
        let core = core_with_db("cap");
        for i in 0..MAX_SCHEDULES {
            create(&core.state, &format!("第 {i} 条"), Some(60), None)
                .unwrap_or_else(|e| panic!("第 {i} 条该建得了：{e}"));
        }
        let err = create(&core.state, "第 33 条", Some(60), None).unwrap_err();
        assert!(
            err.contains(&MAX_SCHEDULES.to_string()),
            "要说清上限：{err}"
        );
        assert_eq!(core.state.schedules.list().len(), MAX_SCHEDULES);
    }

    /// **到点之后真的投到 Router 的队列里**，而且带的是 `Origin::Schedule`。
    ///
    /// 这条走完整条路：`claim_due` → `fire` → `router::wake` → `AppState::enqueue`。
    /// 钉住三件事：
    ///   1. 投给**Router**（不是子会话 —— 定时任务不指挥任何会话）
    ///   2. `origin` 是 `Schedule`（代码分得清「闹钟说的」）
    ///   3. 正文带 `<schedule>` 标记（模型分得清）
    #[tokio::test]
    async fn firing_wakes_the_router_with_a_marked_message() {
        let core = core_with_db("fire");
        let t = create(&core.state, "看看那台机器", Some(60), None).unwrap();
        // 没配模型：消息入队但不开回合（`wake` 的唯一分支），队列里查得到
        core.state.clear_llm();

        let now = t.created_at + 61 * 60_000;
        let claims = core.state.schedules.claim_due(now);
        assert_eq!(claims.len(), 1);
        fire(&core, &claims[0]).await;

        // Router 被建出来了，消息在**它的**队列里
        let router = crate::router::ensure(&core.state).await;
        let q = core.state.queued_list(&router);
        assert_eq!(q.len(), 1, "投给了 Router 的队列");
        assert_eq!(
            q[0].origin,
            crate::agent::Origin::Schedule {
                task_id: t.id.clone()
            },
            "origin 必须是 Schedule（不能是 Human / Dispatch / WorkerReport）"
        );
        assert!(q[0].text.contains("<schedule"), "正文要有边界标记");
        assert!(q[0].text.contains("看看那台机器"), "原话要一字不改");

        // **没有**任何子会话被建出来：定时任务不指挥谁干活
        let workers = {
            let store = core.state.chat.read().await;
            store
                .sessions
                .iter()
                .filter(|s| s.role == crate::agent::Role::Worker)
                .count()
        };
        assert_eq!(workers, 0, "定时任务不许直接指挥子会话");
    }

    /// 队列满了：**丢掉这一次、只 warn**，不重试、不 panic。
    #[tokio::test]
    async fn a_full_router_queue_drops_one_tick_instead_of_retrying() {
        let core = core_with_db("full");
        let t = create(&core.state, "话", Some(60), None).unwrap();
        core.state.clear_llm();
        let router = crate::router::ensure(&core.state).await;
        // 把 Router 的队列灌满
        for i in 0..AppState::MAX_QUEUED {
            core.state
                .enqueue(
                    &router,
                    QueuedMsg {
                        id: format!("q{i}"),
                        text: "占位".into(),
                        attachments: Vec::new(),
                        origin: crate::agent::Origin::Human,
                    },
                )
                .unwrap_or_else(|m| panic!("没满，却排不进去：{}", m.id));
        }
        let now = t.created_at + 61 * 60_000;
        let claims = core.state.schedules.claim_due(now);
        assert_eq!(claims.len(), 1, "水位线照样推进（这一次算处理过了）");
        fire(&core, &claims[0]).await; // 不 panic
        assert_eq!(core.state.queued_list(&router).len(), AppState::MAX_QUEUED);
        assert!(
            core.state.schedules.claim_due(now).is_empty(),
            "不重试：下一次 tick 也不会再投"
        );
    }

    /// 正文里的标记：模型只能靠它分清「闹钟说的」和「用户说的」（`Origin` 不进请求体）。
    ///
    /// 这条钉的是 `fire` 拼的那段字面量。改成纯正文的话会挂。
    #[test]
    fn the_injected_body_marks_itself_as_a_clock() {
        let c = Claim {
            task_id: "t-1".into(),
            prompt: "看看那台机器还活着没".into(),
            due: BASE,
        };
        let body = body_for(&c);
        assert!(body.starts_with("<schedule task=\"t-1\">"), "{body}");
        assert!(body.contains("</schedule>"), "边界要闭合：{body}");
        assert!(
            body.contains("看看那台机器还活着没"),
            "原话要一字不改：{body}"
        );
        assert!(body.contains("定时器"), "要明说这是定时器投的：{body}");
        assert!(
            body.contains("不是用户此刻说的"),
            "要明说「不是用户此刻说的」：{body}"
        );
        assert!(
            body.contains("别为了响应它而硬派"),
            "要给它「不派」这个选项：{body}"
        );
    }

    #[test]
    fn cron_expressions_are_parsed_once_and_kept_verbatim() {
        for ok in [
            "0 9 * * *",
            "*/15 * * * *",
            "0 9 * * 1-5",
            "30 8 * * 0",
            "0 0 9 * * *",
        ] {
            let c = CronExpr::parse(ok).unwrap_or_else(|e| panic!("`{ok}` 该能解析：{e}"));
            assert_eq!(c.as_str(), ok, "原文要原样留着（回显给用户的是它）");
        }
        for bad in ["", "   ", "garbage", "0 9 * *", "99 9 * * *"] {
            assert!(CronExpr::parse(bad).is_err(), "`{bad}` 不该通过");
        }
    }
}
