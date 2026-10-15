//! 这台**手机**的系统能力：电池存储、剪贴板、闹钟、定位、联系人、日历、短信、通知、朗读。
//!
//! 为什么单独一个模块、单独一条 FFI 通路（`PhoneHost`）：这些功能**都不需要无障碍
//! 服务**，却需要普通 Android API 和运行时权限。和 `DeviceApi`（点按/滑动/按键，
//! 靠无障碍）的可用条件完全不同 —— 照 `BrowserApi` 的先例，各走各的门。
//!
//! 命令清单就在这里一份：沙箱 CLI（`agent help` 里那一段）、`/api/device` 的校验、
//! 提示词都从 [`COMMANDS`] 取。三处各写一份的话，加一个能力必然漏掉一处。
//!
//! ## 为什么这些命令**只在手机上**
//!
//! 桌面上没有闹钟、没有短信、没有联系人 —— 硬做只会得到一个 `unknown command`。
//! 所以：
//!
//! - 沙箱里 `agent <设备命令>` 在电脑上直接报「这台机器上没有」，**连核心都不问**
//!   （`linux.rs` 的 `AGENT_CLI`：`$AGENT_DEVICE` 由核心按平台注入）；
//! - `agent help` 里那一段在电脑上不打印（同一个开关）；
//! - 提示词里（`prompt.rs` 的 `cli_phone`）也只有手机上才写。
//!
//! `agent weather` 不在这张表里：它只是查网络，电脑上同样能用。

/// 手机专有的命令名。**顺序就是 `agent help` 里的顺序。**
pub const COMMANDS: &[&str] = &[
    "device",
    "clipboard",
    "volume",
    "brightness",
    "torch",
    "vibrate",
    "screenshot",
    "radios",
    "alarm",
    "location",
    "contacts",
    "calendar",
    "sms",
    "notify",
    "speak",
];

/// 这是不是一个手机专有命令。
pub fn is_command(name: &str) -> bool {
    COMMANDS.contains(&name)
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
        for name in COMMANDS {
            assert!(seen.insert(*name), "{name} 出现了两次");
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
