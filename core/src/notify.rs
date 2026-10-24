//! 本机系统通知：**「你面前这台机器」跑完一个回合时提醒你**。
//!
//! 为什么不是浏览器通知：`Notification` API 只在安全上下文里存在，而
//! `http://192.168.x.x:8787` 不是 —— 浏览器在那里连权限都不给（`permission` 恒为
//! `denied`），这是浏览器的规矩，改不了。更要紧的是**页面关掉就什么都没有了**，
//! 而这件事的用处恰恰是「人已经切走、去干别的了」。
//!
//! 所以通知由**你面前那台机器的核心**发成系统通知：
//!
//! ```text
//! Mac 的浏览器 ──页面──▶ Mac 核心（经隧道连着 Linux）
//!                             │  ▲ Turn 事件（隧道订阅，见 tunnel::watch_turns）
//!                             ▼  │
//!                        ┌─ macOS 通知中心 ─┐
//!                        └──────────────────┘
//! ```
//!
//! 关键的一点：**发通知的那台核心，不一定就是跑任务的那台**。你在这台 Linux 上连
//! 着一台 Mac 的核心里跑任务、人却坐在 Mac 前 —— 该弹通知的是 Mac。这件事靠
//! `machines` 那套隧道：本机核心连着某台机器时，它自己在后台订阅那台机器的
//! `/api/turns/stream`（隧道替它带上凭据），收到「完事」就发一条本机通知
//! （见 `machines::Machines::watch_turns`）。
//!
//! 手机不在这里：那边是壳的前台服务在发（见 docs/06 和 `AgentNotifications.showDone`）。
//! 所以这个模块在 Android 上也编译（`machines` 那套在手机上同样要跑），但
//! `available()` 为假、`send()` 永远返回「这台机器不发」—— 一个字节的系统调用都不做。

#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::{Command as Proc, Stdio};

use crate::status::Snapshot;

/// 一条要发出去的通知（拼好的文案）。
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// 标题 / 正文最多多少个字符。
///
/// 标题是「完成 · 会话名」：会话名可能是一整段（自动起标题拿的是第一句话），
/// 通知中心会把它折成两三行、把正文挤没。按**字符**算，汉字不算两个。
const TITLE_MAX: usize = 48;
const BODY_MAX: usize = 140;

/// 一个回合的结束状态 → 该发的那条通知。**不该发**（还在跑、用户自己停的、
/// 没有结果）就是 `None`。
///
/// 纯函数，测试钉在这里（`tests` 模块）：文案规则是界面的一部分，散在系统调用
/// 旁边就只能靠手测。
pub fn compose(s: &Snapshot) -> Option<Notice> {
    if s.active {
        return None;
    }
    // 用户自己按的停止：他看着这一刻发生的，不用再提醒一次
    if s.stopped {
        return None;
    }
    let title = format!(
        "{} · {}",
        if s.ok { "完成" } else { "出错了" },
        name_or(&s.title, "新对话")
    );
    // 正文 = 回复预览（`Snapshot::detail`，核心里是「模型最后说的那行」）
    let body = match s.detail.as_deref().map(one_line) {
        Some(d) if !d.is_empty() => clip(&d, BODY_MAX),
        _ => if s.ok {
            "任务跑完了"
        } else {
            "任务失败了"
        }
        .to_string(),
    };
    Some(Notice {
        title: clip(&title, TITLE_MAX),
        body,
    })
}

fn name_or(s: &str, fallback: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// 压成一行：通知只有一段，多个换行会被系统折成一堆空白。
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按**字符**截断（`String::len` 数的是字节，汉字会被截成半个）。
fn clip(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------------------
// 开关
// ---------------------------------------------------------------------------

/// 库里 `meta` 表里的键：`"off"` = 不弹系统通知（默认开）。
///
/// 跟着机器走（不是某个浏览器），因为它描述的是**这台电脑**的偏好 —— 而且发通知
/// 的是核心自己，页面可能压根没开着。
const PREF_KEY: &str = "notify_done";

/// 这台机器要不要在回合跑完时弹系统通知。
///
/// **默认开**：手机上的完成通知也是总发（见 `AgentNotifications.showDone`），
/// 人放下电脑去干别的时那就该响。关掉它是给「跑批量脚本」这种场景的 —— 一晚上
/// 几十个回合，屏幕上会一直跳。
///
/// 读一次就是一次 SQLite 查询，而这条判断在**每个回合结束**时都要问一遍 ——
/// 一次几微秒，相对于一个回合（几十秒到几分钟）不重要。不做内存缓存：缓存要
/// 跟着 `set_enabled` 失效，多一处可能忘的地方，换回来的那点时间不值。
pub fn enabled(db: Option<&crate::db::Db>) -> bool {
    !matches!(
        db.and_then(|d| d.get_meta(PREF_KEY).ok().flatten())
            .as_deref(),
        Some("off")
    )
}

/// 开 / 关（设置页那个开关）。`db` 拿不到就是记账失败 —— 开关是这台机器的事，
/// 没地方记就只能明说。
pub fn set_enabled(db: Option<&crate::db::Db>, on: bool) -> anyhow::Result<()> {
    let db = db.ok_or_else(|| anyhow::anyhow!("这台机器没有会话库，开关没地方记"))?;
    if on {
        db.delete_meta(PREF_KEY)
    } else {
        db.set_meta(PREF_KEY, "off")
    }
}

// ---------------------------------------------------------------------------
// 交给系统
// ---------------------------------------------------------------------------

/// 这台机器上有没有「发通知」这个能力。
///
/// 手机上**没有**：那边是壳的前台服务在发（`AgentNotifications.showDone`），核心
/// 再发一次就是同一个任务响两下。`machines::watch_own_turns` 也据此直接不启动。
pub fn available() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// 真发一条。返回 `Err` 是给人看的原因（设置页的「试一下」要显示它）。
///
/// 都是系统自带的东西，没有额外依赖：
///
/// - macOS：`osascript -e 'display notification …'`（`terminal-notifier` 不保证有）
/// - Linux：`notify-send`（libnotify；桌面发行版基本都有）
///
/// **不阻塞调用方**：这是收尾路径（回合刚结束），一条通知卡住不该拖住别的。但
/// `Command::output()` 会等 —— 所以调用方用 `spawn`（见 `machines::watch_turns`）。
pub fn send(n: &Notice) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // 用 osascript 拼 AppleScript 字符串：引号和反斜杠得转义，否则一句会话标题
        // 里带个双引号就能把脚本拆成两句（`display notification` 之后跟任意命令）。
        let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            esc(&n.body),
            esc(&n.title)
        );
        run(Proc::new("osascript").arg("-e").arg(script))
    }
    #[cfg(target_os = "linux")]
    {
        run(Proc::new("notify-send")
            .arg("--app-name=agent")
            .arg("--urgency=normal")
            .arg(&n.title)
            .arg(&n.body))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = n;
        Err("这台机器上不发通知（只有电脑版核心会）".into())
    }
}

/// 跑一下，非零退出把 stderr 当原因带回去。
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run(cmd: &mut Proc) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("起不来 {}：{e}", cmd.get_program().to_string_lossy()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!(
            "{} 失败（{}）",
            cmd.get_program().to_string_lossy(),
            out.status
        )
    } else {
        err
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Phase;

    /// 一条「跑完了」的快照（形状照抄 `status::Snapshot`）
    fn done() -> Snapshot {
        Snapshot {
            sid: "s1".into(),
            title: "FizzBuzz".into(),
            phase: Phase::Done,
            tool: None,
            detail: Some("跑完了，输出 1 到 100".into()),
            started_at_ms: 1,
            active: false,
            finished_at_ms: Some(2),
            ok: true,
            stopped: false,
        }
    }

    #[test]
    fn a_finished_turn_becomes_one_line_with_the_reply_as_the_body() {
        let n = compose(&done()).unwrap();
        assert_eq!(n.title, "完成 · FizzBuzz");
        assert_eq!(n.body, "跑完了，输出 1 到 100");
    }

    #[test]
    fn failure_and_empty_still_say_something() {
        let mut s = done();
        s.ok = false;
        s.detail = None;
        let n = compose(&s).unwrap();
        assert_eq!(n.title, "出错了 · FizzBuzz");
        assert_eq!(n.body, "任务失败了");

        // 还没起标题（刚建会话）：兜底「新对话」，不要出现「完成 · 」
        let mut s = done();
        s.title = String::new();
        s.detail = None;
        let n = compose(&s).unwrap();
        assert_eq!(n.title, "完成 · 新对话");
        assert_eq!(n.body, "任务跑完了");
    }

    #[test]
    fn running_or_user_stopped_turns_do_not_notify() {
        let mut s = done();
        s.active = true;
        assert!(compose(&s).is_none(), "还在跑");
        let mut s = done();
        s.stopped = true;
        assert!(compose(&s).is_none(), "用户自己停的");
    }

    #[test]
    fn the_body_is_one_line_and_both_ends_are_bounded_by_characters() {
        let mut s = done();
        s.detail = Some("第一行\n\n  第二行  ".into());
        assert_eq!(compose(&s).unwrap().body, "第一行 第二行");

        // 汉字按**字符**算：140 个汉字截成 140 字 + 省略号（按字节会只剩 46 个）
        let mut s = done();
        s.detail = Some("汉".repeat(300));
        let body = compose(&s).unwrap().body;
        assert_eq!(body.chars().count(), BODY_MAX + 1);
        assert!(body.ends_with('…'));

        // 标题也一样（自动起标题拿的是第一句话，可能很长）
        let mut s = done();
        s.title = "题".repeat(200);
        let title = compose(&s).unwrap().title;
        assert!(title.chars().count() <= TITLE_MAX + 1);
    }
}
