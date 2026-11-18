//! 回合的实时状态：通知栏那一行的内容。
//!
//! 为什么要有它：手机撂在桌上时，人唯一看得见的就是通知栏那一行 —— 只说
//! 「正在处理」不够，得说做到哪一步了（在跑哪条命令 / 在想 / 在写什么）。这些
//! 东西在 agent loop 里全是现成的，只需同时送到通知栏和前端（WebView），
//! 不能只发一边。
//!
//! 形状照抄 OpenMinis（`SessionActivityTracker` + `AgentNotificationContent`）：
//! 那边有一个 tracker 对象当**唯一事实来源**，前台服务订阅它、就地把通知改一遍。
//! 我们的状态在 Rust 里，所以事实来源留在这边：agent loop 在值得看一眼的时刻推一
//! 份快照到**总线**上（`Event::Turn`，见 `state.rs`：前端走 `/api/ws`，手机壳
//! 走 `/api/turns/stream`），订阅者只管画。这样「什么算一个阶段」「细节取哪一行」这些
//! 判断都留在能写测试的地方（见文件末尾）。
//!
//! **频度是这里最要紧的一件事**：正文是逐块流过来的（一条长回复几千块），每块都过
//! 一遍 FFI + 刷一次通知是白费 —— 流式的更新按 [STREAM_INTERVAL] 攒着发，阶段切换
//! （开始想 / 开始跑工具 / 工具结束 / 回合结束）立刻发。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::AppState;
use crate::text;

/// 流式正文最快多久推一次。
///
/// 0.5s：一串汉字一秒钟变两次已经比人眼快了，而通知栏刷新是要钱的
/// （`NotificationManager.notify` 是一次 binder 往返）。
/// 阶段切换不受它限制 —— 那些是「有事发生了」，不是「内容又多了几个字」。
const STREAM_INTERVAL: Duration = Duration::from_millis(500);

/// 细节最多显示多少个字符。通知栏就一行，长了也是被系统截断 —— 但**过边界的东西
/// 必须是有界的**：没必要把一屏的正文推给壳。
const DETAIL_MAX: usize = 80;

/// 从正文里取「最后一行」时最多往回看多少字符。
///
/// 不扫全文：模型可能刚写完一个几千行的文件，而我们要的只是它此刻写在哪儿。
const TAIL_SCAN: usize = 2048;

/// 抠工具参数时最多看开头多少字符（和前端 `summarize` 的 `SUMMARY_SCAN` 同口径）。
///
/// 只看开头：这一行要的是命令 / 路径这种排在前面的短字段，而 `edit` 的参数里
/// 跟着一整份文件内容（几十 KB）。
const ARGS_SCAN: usize = 2048;

/// 一个回合此刻处在哪个阶段。序列化成小写名字（`thinking` …），这是**跨语言约定**：
/// 壳的 `Phase.of()` 按它解析、按它挑默认文案（见壳里的 `strings.xml`）。
///
/// 和 OpenMinis 的 `AgentPhase` 一一对应（COMPLETED/TOOL/THINKING/GENERATING/IDLE）——
/// 少一个 IDLE：我们这边没有「服务活着但什么都没跑」的状态（回合结束就停服务，
/// 见 `AgentForegroundService`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// 模型在输出思考，还没有可见正文
    Thinking,
    /// 可见正文正在流
    Streaming,
    /// 一个工具在跑
    Tool,
    /// 回合结束了（通知栏上停住的那一条）
    Done,
}

/// 一个回合**到哪一步了**：还在跑，还是已经结束、怎么结束的。
///
/// 以前这是三个平铺的字段（`active: bool` + `finished_at_ms: Option<u64>` +
/// `ok: bool` + `stopped: bool`），于是「还在跑但已经有结束时刻」「成功了同时
/// 又是被停的」这些组合全都能写出来，谁读谁得自己判优先级 —— 而三个消费者
/// （`notify::compose`、壳的 `AgentNotifications`、页面）判的顺序还不一样。
///
/// 现在只有三种结束方式，互斥且各自带着自己的结束时刻：非法状态写不出来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 还在跑（没有结束时刻；成功 / 停止都无意义）
    Running,
    /// 正常跑完了
    Done { finished_at_ms: u64 },
    /// 出错了
    Failed { finished_at_ms: u64 },
    /// 用户按了停止（壳据此说「已停止」而不是「出错了」）
    Stopped { finished_at_ms: u64 },
}

impl Outcome {
    /// 还在跑吗（壳的前台服务、`turn_status` 的存活都跟它走）
    pub const fn active(self) -> bool {
        matches!(self, Self::Running)
    }

    /// 结束时刻，只有结束那几条才有
    pub const fn finished_at_ms(self) -> Option<u64> {
        match self {
            Self::Running => None,
            Self::Done { finished_at_ms }
            | Self::Failed { finished_at_ms }
            | Self::Stopped { finished_at_ms } => Some(finished_at_ms),
        }
    }

    /// 跑成功了没有（还在跑、被停的都不算）
    pub const fn ok(self) -> bool {
        matches!(self, Self::Done { .. })
    }

    /// 是不是用户按的停止
    pub const fn stopped(self) -> bool {
        matches!(self, Self::Stopped { .. })
    }

    /// 一个回合跑完了，按结果定它是哪一种。
    ///
    /// `stopped` 优先：按了停止的回合在 loop 里也是「正常返回」，但界面上那句
    /// 该是「已停止」而不是「完成」—— 所以先判它。
    pub fn ended(ok: bool, stopped: bool, finished_at_ms: u64) -> Self {
        if stopped {
            Self::Stopped { finished_at_ms }
        } else if ok {
            Self::Done { finished_at_ms }
        } else {
            Self::Failed { finished_at_ms }
        }
    }
}

/// 推给壳的一份快照。字段刻意全是「原样的事实」，文案在壳那边（`strings.xml`）——
/// 通知栏的话术是界面的事，而这里的字段是 agent loop 的事实。
///
/// 线上 JSON 形状没变（`active` / `finished_at_ms` / `ok` / `stopped` 四个平铺
/// 字段，见 [`SnapshotWire`]）：壳、页面、老版本核心都照旧读得懂。换来的是
/// **核心里**只有一个 `outcome`，写不出自相矛盾的状态。
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// 哪个会话（多会话同时跑时，壳要能分清谁是谁）
    pub sid: String,
    /// 会话标题（侧边栏那一行）。空串 = 还没起标题，壳用兜底文案
    pub title: String,
    pub phase: Phase,
    /// 正在跑的工具名（`bash` / `read` / `edit`），没有就是不带这个字段
    pub tool: Option<String>,
    /// 一行细节：`bash` 的命令、`read`/`edit` 的路径、流式正文的最后一行
    pub detail: Option<String>,
    /// 本回合开始的时刻（**wall clock 毫秒**）。
    ///
    /// 刻意不是 `elapsedRealtime`：通知的 `setWhen` 用的是 wall clock，混着用的话
    /// 计时器的原点落在「开机时刻」，一台开了五天的手机显示 `120:00:00`。
    pub started_at_ms: u64,
    /// 到哪一步了（还在跑 / 完了 / 出错 / 被停）
    pub outcome: Outcome,
}

impl Snapshot {
    /// 还在跑吗
    pub const fn active(&self) -> bool {
        self.outcome.active()
    }

    /// 跑成功了没有
    pub const fn ok(&self) -> bool {
        self.outcome.ok()
    }

    /// 是不是用户按的停止
    pub const fn stopped(&self) -> bool {
        self.outcome.stopped()
    }
}

/// `Snapshot` 的线上形状（见 [`Snapshot`] 的说明）。
///
/// 只在这一层出现：序列化时由 `outcome` 摊开成四个字段，反序列化时再收回来。
/// 反序列化要**宽容**：对端可能是老版本核心（那时 `stopped` 和 `ok` 可以同时
/// 为真），所以按「停止 > 失败 > 完成」的优先级收，和壳那边的判断顺序一致。
#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotWire {
    sid: String,
    title: String,
    phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    started_at_ms: u64,
    active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_at_ms: Option<u64>,
    ok: bool,
    stopped: bool,
}

impl From<&Snapshot> for SnapshotWire {
    fn from(s: &Snapshot) -> Self {
        Self {
            sid: s.sid.clone(),
            title: s.title.clone(),
            phase: s.phase,
            tool: s.tool.clone(),
            detail: s.detail.clone(),
            started_at_ms: s.started_at_ms,
            active: s.outcome.active(),
            finished_at_ms: s.outcome.finished_at_ms(),
            ok: s.outcome.ok(),
            stopped: s.outcome.stopped(),
        }
    }
}

impl From<SnapshotWire> for Snapshot {
    fn from(w: SnapshotWire) -> Self {
        // `finished_at_ms` 缺席时用 0：老核心 / 半截帧不该让整条状态读不出来
        let at = w.finished_at_ms.unwrap_or(0);
        let outcome = if w.active {
            Outcome::Running
        } else {
            Outcome::ended(w.ok, w.stopped, at)
        };
        Self {
            sid: w.sid,
            title: w.title,
            phase: w.phase,
            tool: w.tool,
            detail: w.detail,
            started_at_ms: w.started_at_ms,
            outcome,
        }
    }
}

impl serde::Serialize for Snapshot {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        SnapshotWire::from(self).serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for Snapshot {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        SnapshotWire::deserialize(d).map(Self::from)
    }
}

/// 一个回合的状态播报器。`run_turn` 建一个，agent loop 拿着它推。
///
/// 拿在手里的是 `Arc<AppState>` 而不是 `&Core`：推的时候只用到总线这一个字段。
pub struct Reporter {
    state: Arc<AppState>,
    sid: String,
    title: String,
    started_at_ms: u64,
    last: Mutex<Last>,
}

/// 上一次推的是什么。用来做两件事：同样的东西不重复推、流式内容按时间攒着推。
#[derive(Default)]
struct Last {
    /// 上一次真推出去的那份（完全相同就不推了）
    sent: Option<Snapshot>,
    /// 上一次真推出去的时刻
    at: Option<Instant>,
    /// 最后一段可见正文的预览，结束那一条拿它当「回复预览」。
    /// 每一步流完时记一次（见 [Reporter::step_text]），所以它一直是「模型最后说的话」。
    reply: Option<String>,
}

impl Last {
    fn phase(&self) -> Option<Phase> {
        self.sent.as_ref().map(|s| s.phase)
    }

    /// 同一个流式阶段里、离上一次推送还不到 [STREAM_INTERVAL]
    fn throttled(&self) -> bool {
        self.phase() == Some(Phase::Streaming)
            && self.at.is_some_and(|at| at.elapsed() < STREAM_INTERVAL)
    }
}

impl Reporter {
    /// `title` 是调用方在拿会话锁时顺手取的（这里不再回头查库）。
    pub fn new(state: Arc<AppState>, sid: &str, title: &str) -> Self {
        Self {
            state,
            sid: sid.to_string(),
            title: title.to_string(),
            started_at_ms: crate::agent::now_ms(),
            last: Mutex::new(Last::default()),
        }
    }

    /// 模型开始（重新）生成：一步开始了，或者一个工具刚跑完。
    ///
    /// 每个思考 delta 都会调到这里，所以阶段没变就直接返回（不拼快照）。
    pub fn thinking(&self) {
        // **一步之内**又吐了一段思考：已经开始出正文的话**不切回 Thinking**。
        // 有些 provider 会把 reasoning / content 交错着发，切来切去就是每个 chunk
        // 一次通知栏刷新（binder 往返）。真正的新一步走 `new_step`。
        if matches!(
            self.last_phase(),
            Some(Phase::Thinking) | Some(Phase::Streaming)
        ) {
            return;
        }
        self.send(self.running(Phase::Thinking, None, None), false);
    }

    /// 新的一步开始了（或一个工具刚跑完）：这时允许从 `Streaming` 切回 `Thinking`。
    pub fn new_step(&self) {
        if self.last_phase() == Some(Phase::Thinking) {
            return;
        }
        self.send(self.running(Phase::Thinking, None, None), false);
    }

    /// 可见正文又长了一截。`text` 是这**一步**到目前为止的全部可见正文。
    ///
    /// 只有跟上一次推出去的隔了 [STREAM_INTERVAL] 才真的推（也只有那时才算预览）——
    /// 中间那些块的效果是「下次推的时候更新」。
    pub fn streaming(&self, text: &str) {
        if self.last.lock().map_or(true, |l| l.throttled()) {
            return;
        }
        self.send(
            self.running(Phase::Streaming, None, reply_preview(text)),
            false,
        );
    }

    /// 一步流完了：记下这一步的正文预览，结束那一条要拿它当「回复预览」。
    ///
    /// 放在步尾而不是每块都记：节流会吞掉最后半秒的块，但这里拿的是整步的正文。
    pub fn step_text(&self, text: &str) {
        let Some(preview) = reply_preview(text) else {
            return;
        };
        if let Ok(mut last) = self.last.lock() {
            last.reply = Some(preview);
        }
    }

    /// 开始跑一个工具。`args` 可能**还没流完**（模型正在写参数）——
    /// 那就先显示工具名，参数抠到哪儿算哪儿。
    pub fn tool(&self, name: &str, args: &str) {
        let detail = tool_detail(name, args);
        self.send(
            self.running(Phase::Tool, Some(name.to_string()), detail),
            false,
        );
    }

    /// 工具跑完了，等下一步（回到 `thinking`）。不这么推的话，通知栏会一直停在
    /// 上一条命令上 —— 那期间模型正在想下一步，说的却是「正在跑 apk add」。
    pub fn tool_done(&self) {
        self.new_step();
    }

    /// 回合结束。`stopped` = 用户按了停止，`ok` = 没出错。
    ///
    /// 这一条是**最后一条**：壳收到它就去发「任务完成」那条通知。
    pub fn finish(&self, ok: bool, stopped: bool) {
        let reply = self.last.lock().ok().and_then(|g| g.reply.clone());
        self.send(
            Snapshot {
                detail: reply,
                outcome: Outcome::ended(ok, stopped, crate::agent::now_ms()),
                ..self.running(Phase::Done, None, None)
            },
            // 结束那一条必须落地：它是壳发「任务完成」通知的依据
            true,
        );
    }

    fn last_phase(&self) -> Option<Phase> {
        self.last.lock().ok().and_then(|l| l.phase())
    }

    /// 还在跑的那几个阶段共用的形状（一定是 `Running`：结束时刻、成败都还没意义）。
    fn running(&self, phase: Phase, tool: Option<String>, detail: Option<String>) -> Snapshot {
        Snapshot {
            sid: self.sid.clone(),
            title: self.title.clone(),
            phase,
            tool,
            detail,
            started_at_ms: self.started_at_ms,
            outcome: Outcome::Running,
        }
    }

    /// 真正推一份快照出去。返回有没有推（测试用）。
    ///
    /// `force` = 忽略节流（结束那一条必须落地）。节流只管「同一个流式阶段里的
    /// 重复推送」：阶段切换（想 → 写、工具 → 想）是「有事发生了」，再快也得让壳看到。
    fn send(&self, snap: Snapshot, force: bool) -> bool {
        {
            let Ok(mut last) = self.last.lock() else {
                return false;
            };
            if last.sent.as_ref() == Some(&snap) {
                return false;
            }
            if !force && snap.phase == Phase::Streaming && last.throttled() {
                return false;
            }
            last.sent = Some(snap.clone());
            last.at = Some(Instant::now());
        }
        self.state.push_turn_status(snap);
        true
    }
}

/// 从（可能还没写完的）工具参数 JSON 里抠一个字符串字段。
///
/// 为什么不等参数流完：模型写一条长命令要好几秒，那几秒里通知栏是空的。前端
/// `MessageList.summarize` 也是这个思路（`partialArgs`）—— 有区别的是它按正则取
/// **所有**字段，我们只要一个，所以这里手写扫描（Rust 侧不想为此引 regex）。
///
/// 没写完的字符串**返回已有的部分**（不是 None）：边写边显示正是要的效果。
/// 真拿不到（键还没出现）才是 None。
pub fn string_arg(args: &str, key: &str) -> Option<String> {
    let hay = &args[..head_end(args, ARGS_SCAN)];
    let needle = format!("\"{key}\"");
    let at = hay.find(&needle)? + needle.len();
    let rest = hay[at..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;

    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            // 收尾引号：这是完整的值
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() < 4 {
                        return Some(out);
                    }
                    match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        Some(c) => out.push(c),
                        // 代理对（`\ud83d\ude00`）不拼了：半个人家也看不懂，
                        // 真出现就到此为止
                        None => return Some(out),
                    }
                }
                Some(other) => out.push(other),
                // 末尾是半个转义（`\`）—— 下一块才知道是什么，先不猜
                None => return Some(out),
            },
            c => out.push(c),
        }
    }
    // 参数还在流，字符串还没收尾
    Some(out)
}

/// 工具那一行的细节：`bash` 取命令行第一行，`read` / `edit` 取路径。
///
/// 这里是**原样的事实**（命令 / 路径），「读取 / 编辑」这类动词在壳的
/// `strings.xml` 里 —— 通知栏的话术是界面的事。
pub fn tool_detail(name: &str, args: &str) -> Option<String> {
    let raw = match name {
        // 一份脚本可以好几行，而通知栏只有一行 —— 取第一行（和前端同一口径）
        "bash" => string_arg(args, "command")?,
        "read" | "edit" => string_arg(args, "path")?,
        _ => return None,
    };
    let line = first_line(&raw);
    (!line.is_empty()).then_some(text::cap(line, DETAIL_MAX))
}

/// 回复预览：正文**尾部**最后一个非空行。
///
/// 取尾部而不是头部：正在写的时候最后一行才是「它此刻在做什么」。markdown 的记号
/// 在通知栏上是噪音（`## 结果` / `**加粗**`），去掉。
pub fn reply_preview(text: &str) -> Option<String> {
    let start = tail_start(text, TAIL_SCAN);
    let last = text[start..]
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    // 一次遍历同时做三件事：去 markdown 记号（`*` / `` ` `` / `_`）、把连续空白
    // 压成一个空格、丢掉首尾空白。以前是 collapsed() 分配一个 String、
    // `.replace()` 再分配一个、`.trim().to_string()` 再分配一个 —— 而这条路径
    // 每个流式块都会走（见 `Reporter::streaming` 的节流）。
    let clean = text::squeeze(last.trim_start_matches('#'), |c| {
        !matches!(c, '*' | '`' | '_')
    });
    (!clean.is_empty()).then(|| text::cap(&clean, DETAIL_MAX))
}

/// 第一行非空的内容（还没压空白、没截断）。
///
/// 「压成一行 + 截到 `DETAIL_MAX`」在 `text` 模块里（`text::one_line` /
/// `text::cap`）—— 以前这里另写了一份内容一样的 `collapsed`，同一条通知和
/// 同一条搜索摘要的空白折叠规则各走一路，改一处忘一处就会漂。
fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// 「最多看头部 `max` 字节」的终点，落在合法字符边界上。
fn head_end(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 「最多看尾部 `max` 字节」的起点，落在合法字符边界上。
fn tail_start(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return 0;
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases(rx: &mut tokio::sync::broadcast::Receiver<crate::state::Event>) -> Vec<Phase> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let crate::state::Event::Turn { status } = ev {
                out.push(status.phase);
            }
        }
        out
    }

    /// 一步之内 reasoning / content 交错出现时，阶段不能在两者之间跳 ——
    /// 每跳一次就是一次通知栏刷新（binder 往返）。`thinking()` 是「又吐了一段
    /// 思考」，不切回 Thinking；`new_step()` 才允许。
    #[test]
    fn interleaved_reasoning_does_not_flap_the_phase() {
        let st = AppState::new();
        let mut rx = st.tx.subscribe();
        let r = Reporter::new(st, "s1", "标题");

        r.new_step(); // 新的一步：Thinking
        r.streaming("正文");
        // 同一步里的思考 delta：不该把阶段切回去
        r.thinking();
        r.streaming("正文更多");
        r.thinking();

        assert_eq!(phases(&mut rx), vec![Phase::Thinking, Phase::Streaming]);

        // 新的一步（工具跑完 / 下一轮）：允许切回 Thinking
        r.new_step();
        assert_eq!(phases(&mut rx), vec![Phase::Thinking]);
    }

    /// 结束那一条的四种说法**互斥**：`outcome` 一个字段决定全部，
    /// 所以「还在跑但有结束时刻」「成功了又是被停的」这些组合写不出来。
    #[test]
    fn an_outcome_never_contradicts_itself() {
        let cases = [
            (Outcome::Running, true, false, false, None),
            (
                Outcome::Done { finished_at_ms: 7 },
                false,
                true,
                false,
                Some(7),
            ),
            (
                Outcome::Failed { finished_at_ms: 8 },
                false,
                false,
                false,
                Some(8),
            ),
            (
                Outcome::Stopped { finished_at_ms: 9 },
                false,
                false,
                true,
                Some(9),
            ),
        ];
        for (o, active, ok, stopped, at) in cases {
            assert_eq!(o.active(), active, "{o:?}");
            assert_eq!(o.ok(), ok, "{o:?}");
            assert_eq!(o.stopped(), stopped, "{o:?}");
            assert_eq!(o.finished_at_ms(), at, "{o:?}");
        }
        // 「停止」优先于「成功」：loop 里按了停止也是正常返回
        assert_eq!(
            Outcome::ended(true, true, 1),
            Outcome::Stopped { finished_at_ms: 1 }
        );
    }

    /// 线上 JSON 形状没变（壳、页面读的还是那四个平铺字段）。
    #[test]
    fn the_wire_shape_keeps_the_flat_booleans() {
        let st = AppState::new();
        let r = Reporter::new(st, "s1", "标题");
        r.new_step();
        r.finish(true, false);
        let snap = r.last.lock().unwrap().sent.clone().unwrap();
        let v = serde_json::to_value(&snap).unwrap();
        for k in ["active", "ok", "stopped", "finished_at_ms", "phase"] {
            assert!(v.get(k).is_some(), "少了 {k}：{v}");
        }
        assert_eq!(v["active"], serde_json::json!(false));
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["stopped"], serde_json::json!(false));

        // 回得去（壳 → 核心的那条路 / 中继转发）
        let back: Snapshot = serde_json::from_value(v).unwrap();
        assert_eq!(back.outcome, snap.outcome);

        // 老核心发来的「既成功又被停」：按「停止」收，不造出非法状态
        let old = serde_json::json!({
            "sid": "s", "title": "t", "phase": "done", "started_at_ms": 1,
            "active": false, "finished_at_ms": 2, "ok": true, "stopped": true
        });
        let got: Snapshot = serde_json::from_value(old).unwrap();
        assert_eq!(got.outcome, Outcome::Stopped { finished_at_ms: 2 });
    }
}
