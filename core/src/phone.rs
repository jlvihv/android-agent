//! 这台**手机**的系统能力：电池存储、剪贴板、闹钟、定位、联系人、日历、短信、通知、朗读。
//!
//! 为什么单独一个模块、单独一条 FFI 通路（`PhoneHost`）：这些功能**都不需要无障碍
//! 服务**，却需要普通 Android API 和运行时权限。和 `DeviceApi`（点按/滑动/按键，
//! 靠无障碍）的可用条件完全不同 —— 照 `BrowserApi` 的先例，各走各的门。
//!
//! 命令清单就在这里一份（[`COMMANDS`]）：`agent` 命令里那一段（`cli.rs` 按它生成
//! 分发和 `agent help`）、`/api/device` 的校验、提示词都从这里取。几处各写一份的话，
//! 加一个能力必然漏掉一处。
//!
//! 这些命令**只在手机上**：电脑上没有闹钟、短信、联系人。分割在生成 `agent` 脚本
//! 时就做完了（见 `cli.rs`）—— 电脑上的 `agent` 里根本没有这些子命令，模型也就看不到
//! 「这台机器上没有」这层区分。
//!
//! `agent weather` 不在这张表里：它只是查网络，哪台机器上都能用。

/// 一个手机专有命令。
pub struct Command {
    /// 子命令名（`agent <name>`）
    pub name: &'static str,
    /// `agent help` 里的用法（不含开头的 `agent `；续行自带缩进）
    pub usage: &'static str,
}

/// 手机专有的命令。**顺序就是 `agent help` 里的顺序。**
pub const COMMANDS: &[Command] = &[
    Command { name: "device", usage: "device [info|battery|storage]     设备信息（不带子命令 = info）" },
    Command { name: "clipboard", usage: "clipboard get | set --text T | clear | status" },
    Command {
        name: "volume",
        usage: "volume [get|set|up|down|max|mute|unmute] [--stream music|ring|alarm|call|notification|system]\n    set --level 7 或 --percent 50；调完会回实际值（勿扰模式会把一些流按下去）",
    },
    Command { name: "brightness", usage: "brightness [get|set --percent 50|--level 0..255] [--auto]   屏幕亮度" },
    Command { name: "torch", usage: "torch on|off|toggle               手电筒" },
    Command { name: "vibrate", usage: "vibrate [--duration 200] [--pattern tap|long]   振动" },
    Command { name: "screenshot", usage: "screenshot [--path 路径.png]      截屏（默认落到当前目录的 screenshots/）" },
    Command {
        name: "radios",
        usage: "radios [wifi|bluetooth|airplane]  网络状态；给参数就是打开对应的设置页\n    （Android 10+ 不允许 App 自己开关 WiFi/蓝牙，只能把人送到设置页）",
    },
    Command {
        name: "alarm",
        usage: "alarm set --time HH:MM|ISO [--label L] [--repeat once|daily|weekdays]\n    | timer --duration 5m | list | cancel --id X | cancel --all",
    },
    Command {
        name: "location",
        usage: "location [current] [--timeout 10] [--no-address] | geocode --lat L --lon N | forward --query Q\n    current 默认带上反查的地址（place：省/市/区/街道）" },
    Command { name: "contacts", usage: "contacts list [--max N] | search <词> | get <id>" },
    Command {
        name: "calendar",
        usage: "calendar list [--today|--days N] | create --title T --start ISO | update --id ID …\n    | delete --id ID | calendars",
    },
    Command { name: "sms", usage: "sms list [--limit N] | search <词> | get <id> | stats" },
    Command { name: "notify", usage: "notify --title T [--body B]       发一条系统通知" },
    Command {
        name: "notifications",
        usage: "notifications [list|recent] [--limit N] [--app 包名]   通知栏里的 / 最近收到过的通知",
    },
    Command { name: "speak", usage: "speak <文本> [--voice zh-CN]      朗读" },
];

/// 这是不是一个手机专有命令。
pub fn is_command(name: &str) -> bool {
    COMMANDS.iter().any(|c| c.name == name)
}

/// 全部命令名（按 [`COMMANDS`] 的顺序）。
pub fn names() -> Vec<&'static str> {
    COMMANDS.iter().map(|c| c.name).collect()
}

/// 由 Kotlin 实现：跑一条手机命令。
///
/// **边界用 `(名字, argv)` 而不是 JSON**：参数本来就来自 shell（`agent sms list --limit 5`），
/// 原样传过去最省事 —— 核心不需要懂每个命令的参数，加子命令不用动 Rust 这一侧。
///
/// 实现方**会阻塞**（权限弹窗要等人点、定位要等定位到），所以调用方必须走
/// `spawn_blocking`，见 `Core::phone`。一次调用可能是几十秒（定位）。
pub trait PhoneApi: Send + Sync + 'static {
    /// `argv` 是子命令 + 参数，**不含**命令名本身（`["list", "--limit", "5"]`）。
    ///
    /// 返回一个 JSON 字符串，约定的形状：
    ///
    /// ```json
    /// {"ok": true,  "data": { … }}
    /// {"ok": false, "error": {"code": "permission_denied", "message": "…"}}
    /// ```
    ///
    /// `ok:false` 时沙箱里的 CLI 会以非 0 退出（`&&` 链因此会断），而 `error.code`
    /// 是给模型看的：它得能区分「用户拒绝了权限」（别重试，去解释）和「参数错了」。
    fn call(&self, name: &str, argv: &[String]) -> Result<String, String>;

    /// 一句话说明是哪个后端（`/api/health`、体检用）。
    fn describe(&self) -> String {
        "Android".to_string()
    }
}

/// 把 Kotlin 回来的东西收敛成 CLI 该打的那一行 JSON。
///
/// 壳那边理论上会回合法 JSON，但它是**另一个语言写的**：回来的东西不能直接相信，
/// 不然一段意外的文本会让整条 `agent` 命令吐出一串谁也解析不了的东西。
pub fn normalize_reply(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v @ serde_json::Value::Object(_)) => v,
        // 不是对象（是数组、字符串、或者纯文本）：包一层，别丢内容
        Ok(v) => serde_json::json!({ "ok": true, "data": v }),
        Err(_) if trimmed.is_empty() => {
            serde_json::json!({ "ok": true, "data": serde_json::Value::Null })
        }
        Err(_) => serde_json::json!({ "ok": true, "data": trimmed }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_commands_are_the_only_ones() {
        assert!(is_command("sms"));
        assert!(is_command("calendar"));
        assert!(!is_command("weather"), "weather 两端都有，不属于这里");
        assert!(!is_command("browser"));
        assert!(!is_command(""));
    }

    /// 名字不能重复，也不能有空格/大写 —— 它们是 shell 里的子命令，也是
    /// `agent help` 里的第一列。
    #[test]
    fn names_are_distinct_and_shell_safe() {
        let mut seen = std::collections::HashSet::new();
        for name in names() {
            assert!(seen.insert(name), "{name} 出现了两次");
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase()),
                "{name} 得是全小写字母"
            );
        }
    }

    #[test]
    fn replies_are_always_json_objects() {
        let object = normalize_reply(r#"{"ok":true,"data":{"battery":42}}"#);
        assert_eq!(object["data"]["battery"], 42);

        // 壳回了一句人话：不能丢，但也不能让它污染 CLI 的输出形状
        let text = normalize_reply("定位超时");
        assert_eq!(text["ok"], true);
        assert_eq!(text["data"], "定位超时");

        // 数组 / 空串
        assert_eq!(normalize_reply("[1,2]")["data"][1], 2);
        assert_eq!(normalize_reply("  ")["data"], serde_json::Value::Null);

        // 已经是 JSON 但带前后空白
        assert_eq!(normalize_reply(" \n{\"ok\":false}\n")["ok"], false);
    }
}
