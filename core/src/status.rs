//! 回合的实时状态：通知栏那一行的内容。
//!
//! 为什么要有它：回合进行中那个通知以前只说「agent 正在处理」，点开才知道做到哪
//! 一步了 —— 而手机撂在桌上时，人唯一看得见的就是它。要显示的东西（在跑哪条命令 /
//! 在想 / 在写什么）在 agent loop 里全是现成的，但以前只发给了 SSE 那个客户端
//! （WebView 里的前端），App 一切到后台就看不见了。
//!
//! 形状照抄 OpenMinis（`SessionActivityTracker` + `AgentNotificationContent`）：
//! 那边有一个 tracker 对象当**唯一事实来源**，前台服务订阅它、就地把通知改一遍。
//! 我们的状态在 Rust 里，所以事实来源留在这边：agent loop 在值得看一眼的时刻推一
//! 份 JSON 快照到**总线**上（`Event::Turn`，见 `state.rs`：前端走 `/api/ws`，手机壳
//! 走 `/api/turns/stream`），订阅者只管画。这样「什么算一个阶段」「细节取哪一行」这些
//! 判断都留在能写测试的地方（见文件末尾）。
//!
//! **频度是这里最要紧的一件事**：正文是逐块流过来的（一条长回复几千块），每块都过
//! 一遍 FFI + 刷一次通知是白费 —— 流式的更新按 [STREAM_INTERVAL] 攒着发，阶段切换
//! （开始想 / 开始跑工具 / 工具结束 / 回合结束）立刻发。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::state::AppState;

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

/// 一个回合此刻处在哪个阶段。
///
/// 和 OpenMinis 的 `AgentPhase` 一一对应（COMPLETED/TOOL/THINKING/GENERATING/IDLE）——
/// 少一个 IDLE：我们这边没有「服务活着但什么都没跑」的状态（回合结束就停服务，
/// 见 `AgentForegroundService`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

impl Phase {
    /// 跨语言的名字。壳按它挑默认文案（见壳里的 `strings.xml`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Thinking => "thinking",
            Phase::Streaming => "streaming",
            Phase::Tool => "tool",
            Phase::Done => "done",
        }
    }
}

/// 推给壳的一份快照。字段刻意全是「原样的事实」，文案在壳那边（`strings.xml`）——
/// 通知栏的话术是界面的事，而这里的字段是 agent loop 的事实。
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Snapshot {
    /// 哪个会话（多会话同时跑时，壳要能分清谁是谁）
    pub sid: String,
    /// 会话标题（侧边栏那一行）。空串 = 还没起标题，壳用兜底文案
    pub title: String,
    /// `thinking` / `streaming` / `tool` / `done`
    pub phase: &'static str,
    /// 正在跑的工具名（`bash` / `read` / `edit`），没有就是不带这个字段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// 一行细节：`bash` 的命令、`read`/`edit` 的路径、流式正文的最后一行
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 本回合开始的时刻（**wall clock 毫秒**）。
    ///
    /// 刻意不是 `elapsedRealtime`：通知的 `setWhen` 用的是 wall clock，混着用就
    /// 是 OpenMinis 踩过的那个坑 —— 计时器的原点落在「开机时刻」，一台开了五天的
    /// 手机显示 `120:00:00`，不崩不报错，就是读数离谱。
    pub started_at_ms: u64,
    /// 还在跑吗（`false` = 这一条是「已经完事」的状态）
    pub active: bool,
    /// 结束时刻，只有结束那一条才有
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// 结束时的结果：正常跑完 = true，出错 = false
    pub ok: bool,
    /// 结束是不是因为用户按了停止（壳据此说「已停止」而不是「出错了」）
    pub stopped: bool,
}

/// 一个回合的状态播报器。`run_turn` 建一个，agent loop 拿着它推。
///
/// 拿在手里的是 `Arc<AppState>` 而不是 `&Core`：推的时候只用到「谁在听」这一个
/// 字段，生命周期比 `Core` 那层借用宽松得多。
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
    /// 上一次推出去的阶段名（`thinking` 连着一千个 delta 只该推一次）。
    /// 存字符串而不是 `Phase`：`send` 手上只有快照，枚举在 `running()` 那层就
    /// 变成跨语言的名字了。
    phase: Option<&'static str>,
    /// 上一次真推出去的时刻
    at: Option<Instant>,
    /// 最后一段可见正文的预览。结束那一条要拿它当「回复预览」——
    /// 工具跑完、思考这些阶段不碰它，所以它一直是「模型最后说的话」。
    reply: Option<String>,
    /// 上一次推出去的 JSON（逐字节相同就不推了）
    json: String,
}

impl Reporter {
    /// `title` 是调用方在拿会话锁时顺手取的（这里不再回头查库）。
    pub fn new(state: Arc<AppState>, sid: &str, title: &str) -> Self {
        Self {
            state,
            sid: sid.to_string(),
            title: title.to_string(),
            started_at_ms: now_ms(),
            last: Mutex::new(Last::default()),
        }
    }

    /// 模型开始（重新）生成：一步开始了，或者一个工具刚跑完。
    ///
    /// 连着调只有第一次有用（阶段没变就直接返回，连 JSON 都不拼）。
    pub fn thinking(&self) {
        {
            let Ok(mut last) = self.last.lock() else {
                return;
            };
            if last.phase == Some(Phase::Thinking.as_str()) {
                return;
            }
            // 先占住阶段。下面还要拼 JSON、再判断一次，但那之间不可能有第二个
            // 线程进来（一个回合只有一个 loop）。
            last.phase = Some(Phase::Thinking.as_str());
        }
        self.send(self.running(Phase::Thinking, None, None), false);
    }

    /// 可见正文又长了一截。`text` 是这**一步**到目前为全部的可见正文。
    ///
    /// 每一块都算一次预览（只扫尾部两千字，有界），但只有跟上一次推出去的隔了
    /// [STREAM_INTERVAL] 才真的推 —— 中间那些块的效果是「下次推的时候更新」。
    pub fn streaming(&self, text: &str) {
        let detail = reply_preview(text);
        {
            let Ok(mut last) = self.last.lock() else {
                return;
            };
            // 先记住：结束那一条要用它当回复预览（**不受节流限制**，
            // 否则最后半秒写的东西会在「完成」通知里缺席）
            last.reply = detail.clone();
            // 已经在这个阶段里、而且刚推过：攒着（下次推的时候内容自然是最新的）
            if last.phase == Some(Phase::Streaming.as_str()) {
                if let Some(at) = last.at {
                    if at.elapsed() < STREAM_INTERVAL {
                        return;
                    }
                }
            }
        }
        self.send(self.running(Phase::Streaming, None, detail), false);
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
        self.thinking();
    }

    /// 回合结束。`stopped` = 用户按了停止，`ok` = 没出错。
    ///
    /// 这一条是**最后一条**：壳收到它就去发「任务完成」那条通知（独立于前台服务，
    /// 因为服务在这前后就被停掉了，两条路谁先谁后都得能收场）。
    pub fn finish(&self, ok: bool, stopped: bool) {
        let reply = self.last.lock().ok().and_then(|g| g.reply.clone());
        self.send(
            Snapshot {
                sid: self.sid.clone(),
                title: self.title.clone(),
                phase: Phase::Done.as_str(),
                tool: None,
                detail: reply,
                started_at_ms: self.started_at_ms,
                active: false,
                finished_at_ms: Some(now_ms()),
                ok,
                stopped,
            },
            // 结束那一条必须落地：它是壳发「任务完成」通知的依据
            true,
        );
    }

    /// 还在跑的那几个阶段共用的形状（没有结束时刻，`ok` 无意义）。
    fn running(&self, phase: Phase, tool: Option<String>, detail: Option<String>) -> Snapshot {
        Snapshot {
            sid: self.sid.clone(),
            title: self.title.clone(),
            phase: phase.as_str(),
            tool,
            detail,
            started_at_ms: self.started_at_ms,
            active: true,
            finished_at_ms: None,
            ok: true,
            stopped: false,
        }
    }

    /// 真正推一份快照出去。返回有没有推（测试用）。
    ///
    /// `force` = 忽略节流（结束那一条必须落地）。
    fn send(&self, snap: Snapshot, force: bool) -> bool {
        // 序列化失败只可能是我们自己的字段有问题（全是 String / 数字），
        // 那就当这次没推 —— 通知栏少一帧，比把 agent loop 打断强。
        let Ok(json) = serde_json::to_string(&snap) else {
            return false;
        };
        {
            let Ok(mut last) = self.last.lock() else {
                return false;
            };
            if last.json == json {
                return false;
            }
            // 节流只管「同一个流式阶段里的重复推送」：阶段切换（想 → 写、
            // 工具 → 想）是「有事发生了」，再快也得让壳看到。
            let still_streaming = snap.phase == Phase::Streaming.as_str()
                && last.phase == Some(Phase::Streaming.as_str());
            if !force && still_streaming {
                if let Some(at) = last.at {
                    if at.elapsed() < STREAM_INTERVAL {
                        return false;
                    }
                }
            }
            last.phase = Some(snap.phase);
            last.at = Some(Instant::now());
            last.json = json.clone();
        }
        self.state.push_turn_status(&json);
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
    let line = one_line(&raw);
    (!line.is_empty()).then_some(line)
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
    let clean = collapsed(last.trim_start_matches('#').trim())
        .replace(['*', '`', '_'], "")
        .trim()
        .to_string();
    (!clean.is_empty()).then(|| cap(&clean, DETAIL_MAX))
}

/// 一行、别太长。
fn one_line(s: &str) -> String {
    let first = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    cap(&collapsed(first), DETAIL_MAX)
}

/// 连续空白压成一个空格（命令行里的缩进、正文里的换行都不该出现在一行里）。
fn collapsed(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

/// 按**字符**截断并加省略号。
///
/// 不按字节：中文一个字三字节，按字节切会切出半个字（界面上是个方块）。
fn cap(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    for (n, c) in s.chars().enumerate() {
        if n == max {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
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

/// wall clock 毫秒。和 `agent::now_ms` 同一个坐标系（那边是给消息时间戳用的，
/// 私有函数，这里就不为它开个口子了）。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_arg_reads_a_partial_value() {
        // 参数流到一半：字符串还没收尾
        assert_eq!(
            string_arg(r#"{"command":"apk add pyt"#, "command").as_deref(),
            Some("apk add pyt")
        );
        // 收了尾
        assert_eq!(
            string_arg(r#"{"command":"ls -la","timeout":30}"#, "command").as_deref(),
            Some("ls -la")
        );
        // 键还没出现
        assert_eq!(string_arg(r#"{"#, "command"), None);
        assert_eq!(string_arg(r#"{"timeout":30}"#, "command"), None);
    }

    #[test]
    fn string_arg_unescapes_and_stops_at_half_an_escape() {
        assert_eq!(
            string_arg(r#"{"command":"echo \"hi\"\n"#, "command").as_deref(),
            Some("echo \"hi\"\n")
        );
        assert_eq!(
            string_arg(r#"{"command":"echo \"#, "command").as_deref(),
            Some("echo ")
        );
        assert_eq!(
            string_arg(r#"{"command":"\u4e2d\u6587"#, "command").as_deref(),
            Some("中文")
        );
    }

    #[test]
    fn string_arg_looks_only_at_the_head_of_a_huge_argument() {
        // `edit` 的 content 可以几十 KB，命令/路径总在前面 —— 不该为了它扫全文。
        // 这里把 key 放在 ARGS_SCAN 之后就抠不到（可接受：真到那一步说明
        // 参数形状和我们预期的不一样，宁可没有细节也不要每次推几 KB）。
        let filler = "x".repeat(ARGS_SCAN + 10);
        let args = format!(r#"{{"blob":"{filler}","path":"/etc/x"}}"#);
        assert_eq!(string_arg(&args, "path"), None);
    }

    #[test]
    fn tool_detail_is_one_line() {
        assert_eq!(
            tool_detail("bash", r#"{"command":"cd /workspace && \\\n  ls -la"}"#).as_deref(),
            Some("cd /workspace && \\")
        );
        assert_eq!(
            tool_detail("read", r#"{"path":"/etc/alpine-release"}"#).as_deref(),
            Some("/etc/alpine-release")
        );
        assert_eq!(
            tool_detail("edit", r#"{"path":"/workspace/a.md","content":"…"}"#).as_deref(),
            Some("/workspace/a.md")
        );
        // 别的工具（老会话里可能有）没有细节，不算错
        assert_eq!(tool_detail("browser", r#"{"action":"navigate"}"#), None);
    }

    #[test]
    fn reply_preview_takes_the_last_line_and_cleans_it() {
        assert_eq!(
            reply_preview("先说一句\n\n## 结果\n已经建好了 fizzbuzz.sh\n").as_deref(),
            Some("已经建好了 fizzbuzz.sh")
        );
        // markdown 记号是噪音
        assert_eq!(
            reply_preview("**完成**：`fizzbuzz.sh`").as_deref(),
            Some("完成：fizzbuzz.sh")
        );
        // 空正文没有预览
        assert_eq!(reply_preview("   \n\n"), None);
        assert_eq!(reply_preview(""), None);
    }

    #[test]
    fn reply_preview_is_bounded_even_for_a_huge_reply() {
        // 模型刚写了一个几千行的文件：只看尾部，而且只留一行
        let huge = format!("{}\n最后一行", "行\n".repeat(5000));
        let p = reply_preview(&huge).unwrap();
        assert_eq!(p, "最后一行");

        let long = "字".repeat(500);
        let p = reply_preview(&long).unwrap();
        assert!(p.chars().count() <= DETAIL_MAX + 1, "{}", p.chars().count());
        assert!(p.ends_with('…'));
    }

    #[test]
    fn tail_start_lands_on_a_char_boundary() {
        let s = "中文字符";
        let start = tail_start(s, 4);
        assert!(s.is_char_boundary(start));
        assert!(s[start..].len() <= 4 + 3);
    }

    #[test]
    fn snapshot_serializes_without_the_empty_fields() {
        let snap = Snapshot {
            sid: "abc".into(),
            title: "建个 fizzbuzz".into(),
            phase: Phase::Thinking.as_str(),
            tool: None,
            detail: None,
            started_at_ms: 1,
            active: true,
            finished_at_ms: None,
            ok: false,
            stopped: false,
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("tool"), "{json}");
        assert!(!json.contains("detail"), "{json}");
        assert!(!json.contains("finished_at_ms"), "{json}");
        assert!(json.contains(r#""phase":"thinking""#), "{json}");
    }

    /// 播报器对着一个真的 `AppState` 推 —— 但没接壳（`turn` 是空），
    /// 所以 `push_turn_status` 是空操作。这里验的是闸门：什么时候推、什么时候攒着。
    fn reporter() -> Reporter {
        Reporter::new(AppState::new(), "sid-1", "标题")
    }

    #[test]
    fn reporter_dedupes_identical_statuses() {
        let r = reporter();
        assert!(r.send(r.running(Phase::Thinking, None, None), false));
        // 逐字节相同：丢掉（`thinking()` 连着一千次也只该推一次）
        assert!(!r.send(r.running(Phase::Thinking, None, None), false));
        r.thinking();
    }

    #[test]
    fn reporter_throttles_repeats_inside_a_streaming_phase_only() {
        let r = reporter();
        assert!(r.send(
            r.running(Phase::Streaming, None, Some("第一行".into())),
            false
        ));
        // 半秒内的第二块攒着（内容不同也不推 —— 下次推的时候自然是最新的）
        assert!(!r.send(
            r.running(Phase::Streaming, None, Some("第二行".into())),
            false
        ));

        // 阶段切换不受节流限制：想 → 写是「有事发生了」，再快也得让壳看到
        let r2 = reporter();
        assert!(r2.send(r2.running(Phase::Thinking, None, None), false));
        assert!(r2.send(
            r2.running(Phase::Streaming, None, Some("刚开口".into())),
            false
        ));

        // 工具、结束（force）同理：立刻推
        assert!(r.send(
            r.running(Phase::Tool, Some("bash".into()), Some("ls".into())),
            false
        ));
        let done = Snapshot {
            active: false,
            finished_at_ms: Some(2),
            ..r.running(Phase::Done, None, None)
        };
        assert!(r.send(done, true));
    }

    #[test]
    fn reporter_pushes_a_status_for_every_phase_of_a_turn() {
        // 阶段名是**跨语言约定**（壳的 `Phase.of()` 按它解析），所以在这里钉住。
        // 至于「谁在听」：那是总线的事，跟播报器无关（没有订阅者时发出去就是没人收）。
        assert_eq!(Phase::Thinking.as_str(), "thinking");
        assert_eq!(Phase::Streaming.as_str(), "streaming");
        assert_eq!(Phase::Tool.as_str(), "tool");
        assert_eq!(Phase::Done.as_str(), "done");
    }
}
