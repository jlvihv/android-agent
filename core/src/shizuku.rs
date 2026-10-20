//! 沙箱里的 `agent shizuku`：以 **adb shell（uid 2000）/ root** 的身份跑 Android 系统命令。
//!
//! 和 `phone` 模块（`agent sms` 这一批）是**两条不同的路**：
//!
//! - `phone`：App 自己的身份（uid = 应用），走 Android 公开 API + 运行时权限。
//! - `shizuku`：借 Shizuku（或 Sui / AXManager）拿到 adb 甚至 root 的身份，
//!   走的是 `pm` / `am` / `settings` / `dumpsys` / `input` 这些**命令行工具** ——
//!   和开发者插着数据线 `adb shell` 敲的是同一套。
//!
//! 所以能力是互补的：`agent contacts list`（App 权限）能读的联系人，
//! `agent shizuku exec content query --uri content://…` 也能读；
//! 但「停掉别的 App」「改系统设置」「模拟点击」只有这条路上有。
//!
//! ## 分层：表在这边，解析也在这边
//!
//! Kotlin 那边只做三件事：状态机（binder 通不通、授没授权）、
//! `run(argv)`（把 argv 变成一条特权 shell 命令、跑、回原始 stdout）、
//! `state()`（永远能答的状态快照）。
//!
//! **参数校验、dumpsys 的文本解析、排版全在这里**。理由：
//!
//! 1. 排版在核心侧只有一份（`plain::render`），喂给它的结构化数据也该在这边产出；
//! 2. `dumpsys` 的输出是一坨文本，解析器是这套东西里**最容易坏**的部分
//!    （换机型、换系统版本就可能对不上）。放在 Rust 里才能喂一段真文本、断言
//!    解析结果 —— 放在 Kotlin 里就只能靠真机手点。
//!
//! ## 不审批
//!
//! 这套命令直接执行，没有「挂起等用户点确认」那一层（和别的工具一致，
//! 见 docs/08-tools-vs-sandbox）。代价是 `package uninstall` / `settings set` /
//! `file rm` 一次误判就能改坏东西 —— 所以每组 help 里都写清「会改什么」，
//! 那是模型唯一看得见的风险提示。
//!
//! ## 只有手机上有
//!
//! 桌面（宿主模式、电脑上的沙箱）没有这一组：`cli.rs` 里 `on: PHONE`，
//! 那台机器上生成的 `agent` 脚本里**根本没有** `shizuku` 这个 case，
//! 模型也就看不到「这台机器上没有」这层区分。

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 命令表（help、提示词、校验都从这里取）
// ---------------------------------------------------------------------------

/// 一个子命令。
pub struct Sub {
    /// 子命令名（`service` 组的 `status`）
    pub name: &'static str,
    /// `agent help` / `agent shizuku help` 里的那一行（**含**开头的 `agent shizuku `）
    pub usage: &'static str,
}

/// 一组子命令。
pub struct Group {
    pub name: &'static str,
    /// help 里组名后面那句
    pub summary: &'static str,
    pub subs: &'static [Sub],
    /// 会改东西的那几组：help 里带一句「会改什么」。
    pub danger: Option<&'static str>,
}

/// 全部命令组。**顺序就是 help 里的顺序。**
///
/// P0 是 `service` / `exec`（先证明拿得到 adb 身份）；P1 补上这批**结构化**的：
/// 它们都能用 `exec` 做到，差别在于「模型得自己拼命令、自己解析 dumpsys 文本」。
/// 这里的价值就是把那两件事收进来 —— 顺带把 `dumpsys` 的几万行压成几个字段。
///
/// **解析失败一律退回 `exec` 原文**（见 `shizuku_parse` 的模块说明）：
/// 系统版本一换格式就变，与其给一份"看起来对但字段是空的"结果，不如直接给原文。
pub const GROUPS: &[Group] = &[
    Group {
        name: "service",
        summary: "服务状态（不带子命令 = status）",
        subs: &[
            Sub { name: "status", usage: "agent shizuku service status          服务状态：版本 / uid / 是 adb 还是 root" },
            Sub { name: "ping", usage: "agent shizuku service ping            就绪回 OK，否则非 0 退出（脚本里用来判断）" },
        ],
        danger: None,
    },
    // ---- 只读 ----
    Group {
        name: "device",
        summary: "设备信息（不带子命令 = info）",
        subs: &[
            Sub { name: "info", usage: "agent shizuku device info            品牌 / 型号 / 系统版本" },
            Sub { name: "battery", usage: "agent shizuku device battery         电量 / 温度 / 状态 / 供电" },
            Sub { name: "top", usage: "agent shizuku device top             现在前台是哪个 App 的哪个页面" },
        ],
        danger: None,
    },
    Group {
        name: "package",
        summary: "看/装/删 App（不带子命令 = list）",
        subs: &[
            Sub { name: "list", usage: "agent shizuku package list [--third-party|-3|--system]   已装的包名（排序）" },
            Sub { name: "info", usage: "agent shizuku package info <包名>    版本 / SDK / 首装 / 更新 / 数据目录" },
            Sub { name: "permissions", usage: "agent shizuku package permissions <包名>   运行时权限 + 授没授" },
            Sub { name: "path", usage: "agent shizuku package path <包名>    APK 装在哪" },
            Sub { name: "install", usage: "agent shizuku package install <apk 路径> [-g]   装（-g 装完直接给全权限）" },
            Sub { name: "uninstall", usage: "agent shizuku package uninstall <包名> [-k]   卸载（-k 保留数据）" },
            Sub { name: "enable", usage: "agent shizuku package enable <包名>  启用" },
            Sub { name: "disable", usage: "agent shizuku package disable <包名> 停用（App 还在，但跑不起来了）" },
            Sub { name: "clear", usage: "agent shizuku package clear <包名>   清空数据（等于重装后第一次打开）" },
        ],
        danger: Some("install/uninstall/clear/disable 会改这台机器上的 App 和数据"),
    },
    Group {
        name: "settings",
        summary: "系统设置库（global / system / secure）",
        subs: &[
            Sub { name: "get", usage: "agent shizuku settings get <global|system|secure> <键>" },
            Sub { name: "set", usage: "agent shizuku settings set <命名空间> <键> <值>" },
            Sub { name: "delete", usage: "agent shizuku settings delete <命名空间> <键>   删掉这个键（回到系统默认）" },
            Sub { name: "list", usage: "agent shizuku settings list <命名空间> [--filter 关键词]" },
        ],
        danger: Some("set/delete 改系统设置（会影响整台机器）"),
    },
    Group {
        name: "activity",
        summary: "启动 / 停止 App",
        subs: &[
            Sub { name: "start", usage: "agent shizuku activity start -n <包名/页面>   或 -a <action> / -d <uri>" },
            Sub { name: "force-stop", usage: "agent shizuku activity force-stop <包名>   强杀（比 kill 彻底）" },
            Sub { name: "kill", usage: "agent shizuku activity kill <包名>" },
            Sub { name: "broadcast", usage: "agent shizuku activity broadcast <action> [-p 包名]" },
        ],
        danger: Some("force-stop/kill 会打断用户正在用的 App"),
    },
    Group {
        name: "input",
        summary: "模拟点按 / 滑动 / 按键 / 打字",
        subs: &[
            Sub { name: "tap", usage: "agent shizuku input tap <x> <y>" },
            Sub { name: "swipe", usage: "agent shizuku input swipe <x1> <y1> <x2> <y2> [毫秒]" },
            Sub { name: "key", usage: "agent shizuku input key <KEYCODE 或名字>   比如 BACK / HOME / ENTER" },
            Sub { name: "text", usage: "agent shizuku input text <文本>     往当前焦点里打字（空格自动转义）" },
        ],
        danger: Some("模拟输入：会真的点到 / 打到用户屏幕上"),
    },
    Group {
        name: "display",
        summary: "屏幕",
        subs: &[
            Sub { name: "list", usage: "agent shizuku display list           分辨率 / 密度 / 刷新率" },
            Sub { name: "size", usage: "agent shizuku display size [<宽>x<高>|reset]   改分辨率（reset 恢复）" },
            Sub { name: "density", usage: "agent shizuku display density [<dpi>|reset]   改密度（reset 恢复）" },
        ],
        danger: Some("size/density 改完整个 UI 会重排（改错了用 reset 恢复）"),
    },
    Group {
        name: "user",
        summary: "多用户（不带子命令 = list）",
        subs: &[
            Sub { name: "list", usage: "agent shizuku user list" },
            Sub { name: "switch", usage: "agent shizuku user switch <id>      切到另一个用户" },
            Sub { name: "start", usage: "agent shizuku user start <id>" },
            Sub { name: "stop", usage: "agent shizuku user stop <id>" },
            Sub { name: "create", usage: "agent shizuku user create <名字> [--guest]" },
            Sub { name: "remove", usage: "agent shizuku user remove <id>      删掉一个用户（连带它的数据）" },
        ],
        danger: Some("switch/remove 会切走当前用户、删掉整个用户的数据"),
    },
    Group {
        name: "notification",
        summary: "通知栏（不带子命令 = list）",
        subs: &[
            Sub { name: "list", usage: "agent shizuku notification list [--package 包名]   现在通知栏里有什么" },
            Sub { name: "dismiss", usage: "agent shizuku notification dismiss [--all|--package 包名]   划掉" },
        ],
        danger: Some("dismiss 会把用户的通知清掉"),
    },
    Group {
        name: "file",
        summary: "特权文件操作（能碰别的 App 的目录）",
        subs: &[
            Sub { name: "ls", usage: "agent shizuku file ls <路径> [-l]" },
            Sub { name: "pull", usage: "agent shizuku file pull <源> <目标>   复制出来" },
            Sub { name: "push", usage: "agent shizuku file push <源> <目标>   复制进去" },
            Sub { name: "rm", usage: "agent shizuku file rm <路径> [-r]    删" },
        ],
        danger: Some("rm 删东西不回收；pull/push 能读写 App 私有目录"),
    },
    Group {
        name: "exec",
        summary: "任意命令交给特权 shell（sh -c）跑",
        subs: &[Sub { name: "exec", usage: "agent shizuku exec <命令…> [--timeout-ms N]   跑一条命令，stdout 原样回来" }],
        // **要进 help**。这不是给维护者看的注释：「不审批」的代价就是靠这句话
        // 让模型知道自己在动什么（docs/15 §5）。删了它，"不审批"就只剩风险没有对冲。
        danger: Some("等于 adb shell，能改这台机器上任何东西"),
    },
];

/// 全部组名（按 [`GROUPS`] 的顺序）。
pub fn group_names() -> Vec<&'static str> {
    GROUPS.iter().map(|g| g.name).collect()
}

/// `agent shizuku help` 的正文（不含状态那一行）。
pub fn help_text() -> String {
    let mut lines = vec![
        "agent shizuku <组> <子命令> [参数…]".to_string(),
        "以 adb（uid 2000）或 root 身份跑 pm / am / settings / dumpsys / input…".to_string(),
        String::new(),
    ];
    for g in GROUPS {
        lines.push(format!("  {:<10}{}", g.name, g.summary));
        for s in g.subs {
            lines.push(format!("    {}", s.usage));
        }
        if let Some(d) = g.danger {
            lines.push(format!("    会改东西：{d}"));
        }
    }
    lines.join("\n")
}

/// 提示词里那一项（只给名字和一句「能干什么」，细节让模型自己 `--help`）。
pub fn prompt_clause() -> String {
    // 组名全列（模型得知道有什么），但不展开每个子命令的用法 —— 那是
    // `agent shizuku help` 的事，67 行不该每轮都在提示词里。
    // 唯一值得在提示词里点明的是「能处理 dumpsys」：模型对 adb 的直觉就是
    // "输出一大坨要自己 grep"，知道这条会省掉很多轮试错。
    format!(
        "`agent shizuku <组> <子命令>`（**以 adb/root 身份**跑系统命令，{}；\
         常用的有结构化输出，不用自己拼 dumpsys；`agent shizuku help` 看全部）",
        group_names().join(" / ")
    )
}

/// 这一组是不是核心自己答的（不走壳）。
pub fn is_local(group: &str) -> bool {
    group == "service"
}

/// 校验 argv。`Ok(())` 才往下走。
///
/// **参数给错了要报错**，不能静默忽略：模型写一条 `agent shizuku settings set x y`
/// 少一个参数，静默跑成一条别的命令，它会对着「成功」的输出继续往下错
/// （`docs/10` 里 `new_tab URL` 被吃掉那次就是这么来的）。
pub fn validate(argv: &[String]) -> Result<(), String> {
    let Some(group) = argv.first().map(|s| s.as_str()) else {
        return Ok(()); // 不带参数 = 显示 help + 状态
    };
    let rest = &argv[1..];
    match group {
        // `help` 和「不带参数」一个意思。**必须在这里认** ——
        // 我一开始只在文档和提示词里写了「`agent shizuku help` 看每一组」，
        // 却漏在这儿，于是模型照着敲会得到一句「没有 help 这一组」（真机上撞到）。
        "help" | "-h" | "--help" => {
            if rest.is_empty() {
                Ok(())
            } else {
                Err("用法：agent shizuku help（不带参数）".into())
            }
        }
        "service" => {
            if rest.len() > 1 {
                return Err(format!(
                    "`agent shizuku service` 最多一个子命令（status / ping），给了 {} 个",
                    rest.len()
                ));
            }
            match rest.first().map(|s| s.as_str()) {
                None | Some("status") | Some("ping") => Ok(()),
                Some(other) => Err(format!(
                    "`agent shizuku service` 没有「{other}」这个子命令（只有 status / ping）"
                )),
            }
        }
        // P1 的结构化组：校验就在 `plan()` 里（它本来就要拼命令，顺手把参数
        // 检查做掉）—— 但**这里也要调一遍**，因为 CLI 的报错得在发请求之前就出来。
        // ⚠️ `exec` 必须排除：它也是 GROUPS 里的一员，不排除会被这里先吃掉，
        // 于是 `agent shizuku exec id` 变成「plan 拼出一条 exec 命令」——
        // 参数对不上就报错（撞过：`exec id` 直接被拒）。
        other if other != "exec" && GROUPS.iter().any(|g| g.name == other) => {
            plan(argv).map(|_| ())
        }
        "exec" => {
            // `--timeout-ms N` 是**唯一的带值开关**，要连它后面那个数一起摘掉。
            // 只过滤以 `--` 开头的会把 `5000` 当成命令 —— 静默跑一条错的命令，
            // 比报错糟得多（这是 docs/10 里 `new_tab URL` 被吃掉那个坑的同类）。
            let mut i = 0;
            let mut has_cmd = false;
            while i < rest.len() {
                let a = &rest[i];
                if a == "--timeout-ms" {
                    i += 2; // 开关 + 它的值
                } else if a.starts_with("--") {
                    return Err(format!(
                        "`agent shizuku exec` 没有 {a} 这个开关（只有 --timeout-ms）"
                    ));
                } else {
                    has_cmd = true;
                    i += 1;
                }
            }
            if has_cmd {
                Ok(())
            } else {
                Err("用法：agent shizuku exec <命令…> [--timeout-ms N]".into())
            }
        }
        other => Err(format!(
            "`agent shizuku` 没有「{other}」这一组（有：{}）",
            group_names().join(" / ")
        )),
    }
}

// ---------------------------------------------------------------------------
// 组 → 特权命令（P1）
// ---------------------------------------------------------------------------

/// 一条要跑的特权命令，外加**怎么把它的输出变成结构化结果**。
#[derive(Debug)]
pub struct Plan {
    /// 交给 `sh -c` 的命令（已拼好、已转义）
    pub cmd: String,
    /// 拿到 stdout 后怎么解析。`None` = 原样给 stdout（`exec`、`file ls` 这类）
    pub parse: Option<fn(&str) -> Option<Value>>,
    /// 跑完之后的成功回执（`uninstall` / `settings set` 这类没有可解析的输出）。
    /// 空 = 用 stdout；非空 = 用这句话 + 没被吃的 stdout。
    pub done: Option<String>,
}

impl Plan {
    fn raw(cmd: impl Into<String>) -> Plan {
        Plan {
            cmd: cmd.into(),
            parse: None,
            done: None,
        }
    }

    fn parsed(cmd: impl Into<String>, parse: fn(&str) -> Option<Value>) -> Plan {
        Plan {
            cmd: cmd.into(),
            parse: Some(parse),
            done: None,
        }
    }

    fn done(cmd: impl Into<String>, word: impl Into<String>) -> Plan {
        Plan {
            cmd: cmd.into(),
            parse: None,
            done: Some(word.into()),
        }
    }
}

/// shell 单引号转义。
///
/// 拼 `sh -c "…"` 时，任何来自模型/用户的值都可能带空格、引号、`;`、反引号 ——
/// 不转义就是命令注入（而且是以 adb 身份注入）。这里一律用单引号包起来，
/// 内部的单引号按 shell 的规矩写成 `'\''`。
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 组 + argv → 要跑的命令。
///
/// 返回 `Err` 表示**参数不对**（缺参数、给多了、不认识的子命令）。这类错误必须在
/// 发出去之前拦住：静默拼出一条别的命令，模型会对着"成功"的输出继续往下错。
pub fn plan(argv: &[String]) -> Result<Plan, String> {
    let group = argv.first().map(|s| s.as_str()).unwrap_or("");
    let rest: Vec<&str> = argv[1..].iter().map(|s| s.as_str()).collect();
    // 第一项是子命令；不带子命令时用这一组的默认值（help 里写了"不带 = xxx"）
    let (sub, a) = split_sub(group, &rest);

    match (group, sub) {
        // ---- device ----
        ("device", "info") => Ok(Plan::parsed(
            "getprop | grep -E '\\[ro\\.(product|build)\\.(model|brand|manufacturer|device)|\\[ro\\.build\\.version\\.(release|sdk)'",
            crate::shizuku_parse::parse_device_info,
        )),
        ("device", "battery") => Ok(Plan::parsed("dumpsys battery", crate::shizuku_parse::parse_battery)),
        ("device", "top") => Ok(Plan::parsed(
            "dumpsys activity activities",
            crate::shizuku_parse::parse_top_activity,
        )),

        // ---- package ----
        ("package", "list") => {
            let mut flag = "";
            for x in &a {
                flag = match *x {
                    "--third-party" | "-3" => "-3",
                    "--system" => "-s",
                    "--disabled" => "-d",
                    other => return Err(format!("`package list` 不认识「{other}」（只有 --third-party/-3/--system/--disabled）")),
                };
            }
            Ok(Plan::parsed(
                format!("pm list packages {flag}"),
                crate::shizuku_parse::parse_packages,
            ))
        }
        ("package", "info") => {
            let pkg = need(&a, 0, "package info <包名>")?;
            Ok(Plan::parsed(
                format!("dumpsys package {}", shq(pkg)),
                crate::shizuku_parse::parse_package_info,
            ))
        }
        ("package", "permissions") => {
            let pkg = need(&a, 0, "package permissions <包名>")?;
            Ok(Plan::parsed(
                format!("dumpsys package {}", shq(pkg)),
                crate::shizuku_parse::parse_permissions,
            ))
        }
        ("package", "path") => {
            let pkg = need(&a, 0, "package path <包名>")?;
            Ok(Plan::raw(format!("pm path {}", shq(pkg))))
        }
        ("package", "install") => {
            let apk = need(&a, 0, "package install <apk 路径> [-g]")?;
            let g = if a.contains(&"-g") { "-g " } else { "" };
            Ok(Plan::done(
                format!("pm install -r {g}{}", shq(apk)),
                format!("已装 {apk}"),
            ))
        }
        ("package", "uninstall") => {
            let pkg = need(&a, 0, "package uninstall <包名> [-k]")?;
            let k = if a.contains(&"-k") { "-k " } else { "" };
            Ok(Plan::done(
                format!("pm uninstall {k}{}", shq(pkg)),
                format!("已卸载 {pkg}"),
            ))
        }
        ("package", "enable") => {
            let pkg = need(&a, 0, "package enable <包名>")?;
            Ok(Plan::done(
                format!("pm enable {}", shq(pkg)),
                format!("已启用 {pkg}"),
            ))
        }
        ("package", "disable") => {
            let pkg = need(&a, 0, "package disable <包名>")?;
            Ok(Plan::done(
                format!("pm disable-user --user 0 {}", shq(pkg)),
                format!("已停用 {pkg}"),
            ))
        }
        ("package", "clear") => {
            let pkg = need(&a, 0, "package clear <包名>")?;
            Ok(Plan::done(
                format!("pm clear {}", shq(pkg)),
                format!("已清空 {pkg} 的数据"),
            ))
        }

        // ---- settings ----
        ("settings", "get") => {
            let (ns, key) = need2(&a, "settings get <global|system|secure> <键>")?;
            check_ns(ns)?;
            // 只回一行值：不带 key 前缀，免得模型得自己剥
            Ok(Plan::done(
                format!("settings get {} {}", shq(ns), shq(key)),
                String::new(),
            ))
        }
        ("settings", "set") => {
            let (ns, key, val) = need3(&a, "settings set <命名空间> <键> <值>")?;
            check_ns(ns)?;
            Ok(Plan::done(
                format!("settings put {} {} {}", shq(ns), shq(key), shq(val)),
                format!("已设 {ns}/{key} = {val}"),
            ))
        }
        ("settings", "delete") => {
            let (ns, key) = need2(&a, "settings delete <命名空间> <键>")?;
            check_ns(ns)?;
            Ok(Plan::done(
                format!("settings delete {} {}", shq(ns), shq(key)),
                format!("已删 {ns}/{key}"),
            ))
        }
        ("settings", "list") => {
            let ns = need(&a, 0, "settings list <命名空间> [--filter 词]")?;
            check_ns(ns)?;
            let f = a
                .iter()
                .position(|x| *x == "--filter")
                .and_then(|i| a.get(i + 1))
                .map(|k| format!(" | grep -i -- {}", shq(k)))
                .unwrap_or_default();
            Ok(Plan::parsed(
                format!("settings list {}{f}", shq(ns)),
                crate::shizuku_parse::parse_settings_list,
            ))
        }

        // ---- activity ----
        ("activity", "start") => {
            // `-n pkg/cls` / `-a action` / `-d uri` 原样传：这是 adb 自己的语法，
            // 我们只做转义，不重新解释
            if a.is_empty() {
                return Err("用法：agent shizuku activity start -n <包名/页面>（或 -a <action> / -d <uri>）".into());
            }
            let args: Vec<String> = a.iter().map(|x| shq(x)).collect();
            Ok(Plan::done(
                format!("am start -W {}", args.join(" ")),
                "已启动".to_string(),
            ))
        }
        ("activity", "force-stop") => {
            let pkg = need(&a, 0, "activity force-stop <包名>")?;
            Ok(Plan::done(
                format!("am force-stop {}", shq(pkg)),
                format!("已强停 {pkg}"),
            ))
        }
        ("activity", "kill") => {
            let pkg = need(&a, 0, "activity kill <包名>")?;
            Ok(Plan::done(
                format!("am kill {}", shq(pkg)),
                format!("已杀 {pkg}"),
            ))
        }
        ("activity", "broadcast") => {
            let act = need(&a, 0, "activity broadcast <action> [-p 包名]")?;
            let mut cmd = format!("am broadcast -a {}", shq(act));
            if let Some(i) = a.iter().position(|x| *x == "-p") {
                let pkg = a.get(i + 1).ok_or("-p 后面要跟包名")?;
                cmd.push_str(&format!(" -p {}", shq(pkg)));
            }
            Ok(Plan::done(cmd, "已发广播".to_string()))
        }

        // ---- input ----
        ("input", "tap") => {
            let (x, y) = need2(&a, "input tap <x> <y>")?;
            let x = num(x, "input tap 的 x")?;
            let y = num(y, "input tap 的 y")?;
            Ok(Plan::done(
                format!("input tap {x} {y}"),
                format!("已点 ({x}, {y})"),
            ))
        }
        ("input", "swipe") => {
            let p = "input swipe <x1> <y1> <x2> <y2> [毫秒]";
            let (x1, y1) = need2(&a, p)?;
            let (x2, y2) = need2(&a[2..], p)?;
            let ms = a.get(4).copied().unwrap_or("300");
            let c = [
                num(x1, "swipe 的 x1")?,
                num(y1, "swipe 的 y1")?,
                num(x2, "swipe 的 x2")?,
                num(y2, "swipe 的 y2")?,
            ];
            let ms = num(ms, "swipe 的毫秒")?;
            Ok(Plan::done(
                format!("input swipe {} {} {} {} {ms}", c[0], c[1], c[2], c[3]),
                format!("已从 ({}, {}) 滑到 ({}, {})", c[0], c[1], c[2], c[3]),
            ))
        }
        ("input", "key") => {
            let k = need(&a, 0, "input key <KEYCODE 或名字>")?;
            // 名字（BACK/HOME）和数字（4/3）都认；转成大写的 KEYCODE 名字
            let name = k.to_ascii_uppercase();
            let known = matches!(
                name.as_str(),
                "BACK" | "HOME" | "MENU" | "APP_SWITCH" | "ENTER" | "DEL" | "TAB"
                    | "POWER" | "VOLUME_UP" | "VOLUME_DOWN" | "WAKEUP" | "SLEEP"
                    | "CAMERA" | "SEARCH" | "NOTIFICATION" | "MEDIA_PLAY_PAUSE"
            );
            if !known && k.parse::<i32>().is_err() {
                return Err(format!(
                    "input key 不认「{k}」：给数字 keycode，或这些名字之一：BACK HOME MENU \
                     APP_SWITCH ENTER DEL TAB POWER VOLUME_UP VOLUME_DOWN WAKEUP SLEEP CAMERA \
                     SEARCH NOTIFICATION MEDIA_PLAY_PAUSE"
                ));
            }
            Ok(Plan::done(
                format!("input keyevent {}", shq(&name)),
                format!("已按 {name}"),
            ))
        }
        ("input", "text") => {
            // 文本可能有很多段（`input text hello world`）—— 拼起来
            if a.is_empty() {
                return Err("用法：agent shizuku input text <文本>".into());
            }
            let text = a.join(" ");
            // `input text` 不认空格：要换成 %s；`'` 直接去掉（它会截断参数）
            let safe = text.replace(' ', "%s").replace('\'', "");
            Ok(Plan::done(
                format!("input text {}", shq(&safe)),
                format!("已输入「{text}」"),
            ))
        }

        // ---- display ----
        ("display", "list") => Ok(Plan::parsed(
            "dumpsys display",
            crate::shizuku_parse::parse_display,
        )),
        ("display", "size") => match a.first().copied() {
            None => Err("用法：agent shizuku display size <宽>x<高>|reset".into()),
            Some("reset") => Ok(Plan::done("wm size reset", "已恢复默认分辨率")),
            Some(v) => {
                if !v.contains('x') {
                    return Err(format!("display size 要 `<宽>x<高>`（比如 1080x2400）或 reset，给的是「{v}」"));
                }
                Ok(Plan::done(
                    format!("wm size {}", shq(v)),
                    format!("已改分辨率 {v}"),
                ))
            }
        },
        ("display", "density") => match a.first().copied() {
            None => Err("用法：agent shizuku display density <dpi>|reset".into()),
            Some("reset") => Ok(Plan::done("wm density reset", "已恢复默认密度")),
            Some(v) => {
                let d = num(v, "display density 的 dpi")?;
                Ok(Plan::done(
                    format!("wm density {d}"),
                    format!("已改密度 {d}"),
                ))
            }
        },

        // ---- user ----
        ("user", "list") => Ok(Plan::parsed(
            "pm list users",
            crate::shizuku_parse::parse_users,
        )),
        ("user", "switch") => {
            let id = need(&a, 0, "user switch <id>")?;
            let id = num(id, "user id")?;
            Ok(Plan::done(
                format!("am switch-user {id}"),
                format!("已切到用户 {id}"),
            ))
        }
        ("user", "start") => {
            let id = need(&a, 0, "user start <id>")?;
            let id = num(id, "user id")?;
            Ok(Plan::done(
                format!("am start-user {id}"),
                format!("已启动用户 {id}"),
            ))
        }
        ("user", "stop") => {
            let id = need(&a, 0, "user stop <id>")?;
            let id = num(id, "user id")?;
            Ok(Plan::done(
                format!("am stop-user {id}"),
                format!("已停止用户 {id}"),
            ))
        }
        ("user", "create") => {
            let name = need(&a, 0, "user create <名字> [--guest]")?;
            let guest = if a.contains(&"--guest") { "--guest " } else { "" };
            Ok(Plan::done(
                format!("pm create-user {guest}{}", shq(name)),
                format!("已建用户 {name}"),
            ))
        }
        ("user", "remove") => {
            let id = need(&a, 0, "user remove <id>")?;
            let id = num(id, "user id")?;
            Ok(Plan::done(
                format!("pm remove-user {id}"),
                format!("已删用户 {id}"),
            ))
        }

        // ---- notification ----
        ("notification", "list") => {
            let f = a
                .iter()
                .position(|x| *x == "--package")
                .and_then(|i| a.get(i + 1))
                .map(|p| format!(" | grep -F -- {}", shq(&format!("pkg={p} "))))
                .unwrap_or_default();
            Ok(Plan::parsed(
                format!("dumpsys notification --noredact{f}"),
                crate::shizuku_parse::parse_notifications,
            ))
        }
        ("notification", "dismiss") => {
            if a.contains(&"--all") {
                return Ok(Plan::done("cmd notification cancel_all", "已清空通知栏"));
            }
            match a.iter().position(|x| *x == "--package").and_then(|i| a.get(i + 1)) {
                Some(pkg) => Ok(Plan::done(
                    format!("cmd notification cancel {}", shq(pkg)),
                    format!("已清掉 {pkg} 的通知"),
                )),
                None => Err("用法：agent shizuku notification dismiss --all | --package <包名>".into()),
            }
        }

        // ---- file ----
        ("file", "ls") => {
            let p = need(&a, 0, "file ls <路径> [-l]")?;
            let l = if a.contains(&"-l") { "-l " } else { "" };
            Ok(Plan::raw(format!("ls {l}{}", shq(p))))
        }
        ("file", "pull") | ("file", "push") => {
            let (src, dst) = need2(&a, "file pull|push <源> <目标>")?;
            Ok(Plan::done(
                format!("cp -r {} {}", shq(src), shq(dst)),
                format!("已复制 {src} → {dst}"),
            ))
        }
        ("file", "rm") => {
            let p = need(&a, 0, "file rm <路径> [-r]")?;
            let r = if a.contains(&"-r") { "-rf " } else { "-f " };
            Ok(Plan::done(format!("rm {r}{}", shq(p)), format!("已删 {p}")))
        }

        _ => Err(format!(
            "`agent shizuku {group}` 没有「{sub}」这个子命令（`agent shizuku help` 看全部）"
        )),
    }
}

/// 把 argv 拆成 (子命令, 其余参数)。不带子命令时用这一组的默认值。
fn split_sub<'a>(group: &str, rest: &'a [&'a str]) -> (&'a str, Vec<&'a str>) {
    // 有默认子命令的组（help 里的「不带子命令 = xxx」就是这几个）
    let default = match group {
        "device" => Some("info"),
        "package" => Some("list"),
        "user" => Some("list"),
        "notification" => Some("list"),
        "display" => Some("list"),
        _ => None,
    };
    match rest.split_first() {
        // 第一个是已知子命令
        Some((first, tail)) if is_sub(group, first) => (first, tail.to_vec()),
        // 空：用默认值
        None => (default.unwrap_or(""), Vec::new()),
        // 不是子命令。这里要分清**两种**情况：
        //
        // - 以 `-` 开头 → 是给默认子命令的开关（`package list -3` 的 `-3`）
        // - 别的东西 → **打错的子命令**，不能当成默认子命令的参数
        //
        // 后者是关键：`agent shizuku package explode` 静默变成
        // `pm list packages explode`，回一堆包名 —— 模型会以为"explode 执行成功了"
        // （这正是 docs/10 里 `new_tab URL` 被吃掉那个坑的同一类）。
        Some((first, _)) if !first.starts_with('-') => (first, rest.to_vec()),
        Some(_) => (default.unwrap_or(""), rest.to_vec()),
    }
}

fn is_sub(group: &str, name: &str) -> bool {
    GROUPS
        .iter()
        .find(|g| g.name == group)
        .map(|g| g.subs.iter().any(|s| s.name == name))
        .unwrap_or(false)
}

fn need<'a>(a: &[&'a str], i: usize, usage: &str) -> Result<&'a str, String> {
    a.get(i)
        .copied()
        .ok_or_else(|| format!("用法：agent shizuku {usage}"))
}

fn need2<'a>(a: &[&'a str], usage: &str) -> Result<(&'a str, &'a str), String> {
    if a.len() < 2 {
        return Err(format!("用法：agent shizuku {usage}"));
    }
    Ok((a[0], a[1]))
}

fn need3<'a>(a: &[&'a str], usage: &str) -> Result<(&'a str, &'a str, &'a str), String> {
    if a.len() < 3 {
        return Err(format!("用法：agent shizuku {usage}"));
    }
    Ok((a[0], a[1], a[2]))
}

/// 数字参数：非数字直接报错，别拼出一条坏命令。
fn num(s: &str, what: &str) -> Result<i64, String> {
    s.parse()
        .map_err(|_| format!("{what} 要一个数字，给的是「{s}」"))
}

fn check_ns(ns: &str) -> Result<(), String> {
    if matches!(ns, "global" | "system" | "secure") {
        Ok(())
    } else {
        Err(format!(
            "命名空间只有 global / system / secure，给的是「{ns}」"
        ))
    }
}

// ---------------------------------------------------------------------------
// 边界：核心 → 壳
// ---------------------------------------------------------------------------

/// 由 Kotlin 实现：拿特权身份跑一条命令。
///
/// **实现方只做三件事**，别的都在核心这边：
/// 1. [`state`](ShizukuApi::state) —— 永远答得出来的状态快照（binder 通不通、授没授权）；
/// 2. [`run`](ShizukuApi::run) —— 把 argv 变成一条特权 shell 命令、跑、回原始输出；
/// 3. [`request_permission`](ShizukuApi::request_permission) —— 拉一次授权界面。
///
/// 参数校验、dumpsys 解析、排版都不在壳里：那些是**判断**，判断留在 Rust 里才能测
/// （喂一段真 dumpsys 文本断言解析结果）；壳里只能靠真机手点。
///
/// 实现方**会阻塞**（等 binder、等授权），调用方走 `spawn_blocking`，见 `Core::shizuku`。
pub trait ShizukuApi: Send + Sync + 'static {
    /// 状态快照，JSON 字符串：
    /// `{"state":"READY","version":13,"uid":2000,"sui":false}`。
    ///
    /// **不许抛**：拿不到就回 `{"state":"NOT_INSTALLED"}`。
    fn state(&self) -> String;

    /// 跑一条命令，`argv` 不含组名之外的任何加工（原样来自 shell）。
    ///
    /// 返回 JSON 字符串，三种形状（见 [`reply_text`](reply_text)）：
    ///
    /// ```json
    /// {"ok":true, "text":"…"}                         原样打（exec / file ls 这类）
    /// {"ok":true, "data":{…}}                          核心用 plain::render 排版
    /// {"ok":false,"error":{"code":"…","message":"…"}}  错误：…（code）
    /// ```
    fn run(&self, argv: Vec<String>) -> String;

    /// 拉一次授权界面。人在旁边才点得动 —— 返回「请求发出去了没有」，
    /// 不是「授没授权」（结果得之后再问 [`state`](ShizukuApi::state)）。
    fn request_permission(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// 壳的回包 → 纯文本
// ---------------------------------------------------------------------------

/// 把壳回的那三种形状收敛成**一行给模型看的纯文本**。
///
/// 排版只有一份（`plain::render`）：结构化数据喂给它，天生就是文字的直接原样打。
/// 失败以 `错误：` 开头 —— 沙箱里的 `agent` 脚本据此**非 0 退出**，
/// 模型写的 `&&` 链才会断（不然它会把失败当成功继续往下跑）。
pub fn reply_text(raw: &str) -> String {
    let v: Value = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        // 壳是另一个语言写的：回了看不懂的东西时，把内容如实带出来而不是丢掉
        Err(_) => {
            let t = raw.trim();
            return if t.is_empty() {
                "错误：壳没有回任何内容".to_string()
            } else {
                t.to_string()
            };
        }
    };
    envelope_text(&v)
}

/// 结构化组的结果：**解析失败就退回原文**。
///
/// 这条规矩比"结构化"本身更重要。解析器依赖 `dumpsys` 的文本格式，系统版本一换
/// 就可能对不上；那时如果给一份"字段都是空的"结果，模型会当成事实（`电量: -1`
/// 读作真没电）。给原文的话，它自己就能读出来，而且**立刻知道解析器没跟上**。
///
/// 四种情况：
///
/// 1. 失败（非 OK 回包）→ 和 `exec` 一样收敛成 `错误：…`
/// 2. `done` 有词（`uninstall` / `input tap` 这类没有可解析输出）→ 那句话
/// 3. 有解析器且解析成功 → 结构化数据，`plain::render` 排成表/键值
/// 4. 有解析器但解析失败 → **原文**（截断过），并说明一句
pub fn structured_text(raw: &str, plan: &Plan) -> String {
    let v: Value = match serde_json::from_str(raw.trim()) {
        Ok(v) => v,
        Err(_) => {
            let t = raw.trim();
            return if t.is_empty() {
                "错误：壳没有回任何内容".to_string()
            } else {
                t.to_string()
            };
        }
    };
    // 失败照旧走统一的错误那条路（带 TIMEOUT / PERMISSION_DENIED 等有意义的码）
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        return envelope_text(&v);
    }
    let out = v
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_end();

    // 2. 没有可解析的东西：说一句做完了
    if let Some(word) = &plan.done {
        if !word.is_empty() {
            return word.clone();
        }
        // `settings get` 这类：done 是空串 = stdout 就是答案
        return crate::linux::truncate_for_model(out).0;
    }

    // 3/4. 有解析器
    match plan.parse {
        Some(f) => match f(out) {
            Some(data) => crate::plain::render(&data),
            None => {
                let (text, truncated) = crate::linux::truncate_for_model(out);
                let note = if truncated {
                    "\n（原文太长已截断，加过滤重跑）"
                } else {
                    ""
                };
                format!("（这台机器上的输出格式认不出来，下面是原文）\n{text}{note}")
            }
        },
        None => {
            let (text, truncated) = crate::linux::truncate_for_model(out);
            if truncated {
                format!(
                    "{text}\n（输出太长已截断 —— 自己加过滤重跑，比如 `| head`、`| grep 关键词`）"
                )
            } else {
                text
            }
        }
    }
}

/// 已经解析好的回包 → 纯文本（[`reply_text`] 的另一半，`service` 组也走这里）。
pub fn envelope_text(v: &Value) -> String {
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        let e = &v["error"];
        let msg = e
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| e.as_str())
            .unwrap_or("失败了（没说原因）");
        // 只在**码本身有信息**时带上它。`OPERATION_FAILED` 是「命令非 0 退出」的
        // 统称，而 stdout/stderr 已经把话说清了 —— 再补个括号纯属重复
        // （实测 `ls: /nope: No such file or directory（OPERATION_FAILED）`）。
        // 真正需要区分的三种：超时、被拒、被杀 —— 它们意味着「换参数也没用」。
        return match e.get("code").and_then(|c| c.as_str()) {
            Some(code @ ("TIMEOUT" | "PERMISSION_DENIED" | "KILLED")) => {
                format!("错误：{msg}（{code}）")
            }
            _ => format!("错误：{msg}"),
        };
    }
    // 天生就是文本的（exec 的 stdout、file ls…）：原样打，别过 render。
    // 但**必须截断**：`dumpsys activity` 一次 2MB，不拦就直接吃满上下文。
    // 走 `bash` 同一套规则（2000 行 / 50KB，首尾各留），不另立一套。
    if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
        let (text, truncated) = crate::linux::truncate_for_model(t.trim_end());
        return if truncated {
            format!("{text}\n（输出太长已截断 —— 自己加过滤重跑，比如 `| head`、`| grep 关键词`）")
        } else {
            text
        };
    }
    match v.get("data") {
        None | Some(Value::Null) => "完成".to_string(),
        Some(d) => crate::plain::render(d),
    }
}

// ---------------------------------------------------------------------------
// 状态：壳回的 `state()` → 一行给人看的话
// ---------------------------------------------------------------------------

/// 壳回的状态快照（`{"state":"READY","version":13,"uid":2000,"sui":false}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// NOT_INSTALLED / NOT_RUNNING / NEED_PERMISSION / READY
    pub state: String,
    pub version: i64,
    pub uid: i64,
    /// binder 是 Sui（root 模块）给的吗 —— 只影响文案
    pub sui: bool,
}

impl State {
    /// 从一个 JSON 字符串里读出来。壳是另一个语言写的，读不出来就当「不可用」。
    pub fn parse(raw: &str) -> State {
        let v: Value = serde_json::from_str(raw.trim()).unwrap_or(Value::Null);
        let known = ["READY", "NEED_PERMISSION", "NOT_RUNNING"];
        let state = v
            .get("state")
            .and_then(|s| s.as_str())
            .filter(|s| known.contains(s))
            .unwrap_or("NOT_INSTALLED")
            .to_string();
        State {
            state,
            version: v.get("version").and_then(|x| x.as_i64()).unwrap_or(-1),
            uid: v.get("uid").and_then(|x| x.as_i64()).unwrap_or(-1),
            sui: v.get("sui").and_then(|x| x.as_bool()).unwrap_or(false),
        }
    }

    pub fn ready(&self) -> bool {
        self.state == "READY"
    }

    /// 身份：uid 2000 = adb，0 = root。
    fn identity(&self) -> &'static str {
        match self.uid {
            0 => "root",
            2000 => "adb",
            _ => "未知",
        }
    }

    /// 来源（只影响文案）。
    fn backend(&self) -> &'static str {
        if self.sui {
            "Sui"
        } else {
            "Shizuku"
        }
    }

    /// 一行状态。`service status` 和「不带参数」的 banner 都用它 ——
    /// **同一个状态只有一种写法**（以前这里两处各拼一套，一个 5 行一个 1 行）。
    pub fn line(&self) -> String {
        if self.ready() {
            format!(
                "{} {}，{} uid={}",
                self.backend(),
                self.version,
                self.identity(),
                self.uid
            )
        } else {
            format!("{}（{}）", self.state, self.hint())
        }
    }

    /// 没就绪时该怎么办。**得说清是哪一步**：这三种情况的修法完全不同，
    /// 一句「不可用」会让模型换着参数反复重试。
    pub fn hint(&self) -> &'static str {
        match self.state.as_str() {
            "READY" => "",
            "NEED_PERMISSION" => {
                "Shizuku 在跑，但这个 App 还没被授权：设置 → 手机权限 → Shizuku → 去授权"
            }
            "NOT_RUNNING" => "Shizuku 装了但没在跑：用无线调试或 root 启动一次",
            _ => "这台机器上没有 Shizuku：装 Shizuku / AXManager，或用 root 刷 Sui 模块",
        }
    }
}

/// `service` 组的回答（核心自己拼，不走壳）。
pub fn service_reply(argv: &[String], raw_state: &str) -> Value {
    let st = State::parse(raw_state);
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("status");
    match sub {
        // ping 是给脚本判断用的：成功就一个 OK，别的什么都不说
        "ping" => {
            if st.ready() {
                json!({ "ok": true, "text": "OK" })
            } else {
                json!({ "ok": false, "error": { "code": st.state, "message": st.hint() } })
            }
        }
        // 没就绪也照实把状态说出来（不是错误，是「现在是这样」）
        _ => json!({ "ok": true, "text": st.line() }),
    }
}

/// 不带参数时的正文：help + 状态那一行。
pub fn banner(raw_state: &str) -> String {
    format!("{}\n\n{}", help_text(), State::parse(raw_state).line())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn empty_argv_is_help_not_an_error() {
        assert!(validate(&[]).is_ok());
    }

    #[test]
    fn service_defaults_to_status_and_rejects_extras() {
        assert!(validate(&v(&["service"])).is_ok());
        assert!(validate(&v(&["service", "status"])).is_ok());
        assert!(validate(&v(&["service", "ping"])).is_ok());
        let e = validate(&v(&["service", "stop"])).unwrap_err();
        assert!(e.contains("stop"), "{e}");
        assert!(validate(&v(&["service", "status", "x"])).is_err());
    }

    /// help 得真的能敲 —— 提示词里写了「`agent shizuku help` 看每一组」，
    /// 那就必须能跑（漏过一次：真机上照敲得到「没有 help 这一组」）。
    #[test]
    fn help_is_a_real_subcommand() {
        for a in ["help", "-h", "--help"] {
            assert!(validate(&v(&[a])).is_ok(), "agent shizuku {a} 该能跑");
        }
        assert!(validate(&v(&["help", "多余"])).is_err());
    }

    #[test]
    fn every_usage_mentioned_in_docs_is_runnable() {
        // 提示词/help 里承诺的入口，validate 都得放行
        for argv in [
            vec!["help"],
            vec!["service"],
            vec!["service", "status"],
            vec!["service", "ping"],
            vec!["exec", "id"],
        ] {
            assert!(validate(&v(&argv)).is_ok(), "{argv:?} 该能跑");
        }
    }

    #[test]
    fn exec_needs_a_command_but_tolerates_flags() {
        assert!(validate(&v(&["exec", "id"])).is_ok());
        assert!(validate(&v(&["exec", "--timeout-ms", "5000", "id"])).is_ok());
        assert!(validate(&v(&["exec"])).is_err());
        assert!(validate(&v(&["exec", "--timeout-ms", "5000"])).is_err());
    }

    #[test]
    fn unknown_group_names_the_ones_that_exist() {
        let e = validate(&v(&["camera", "shoot"])).unwrap_err();
        assert!(e.contains("camera"), "{e}");
        for name in group_names() {
            assert!(e.contains(name), "错误信息里该列出 {name}：{e}");
        }
        // 试过 `package list` 当"不存在的组"—— 它现在是真的了。
        // 这条提醒：加组的时候，拿某个组当反例的测试要一起看一眼。
        assert!(validate(&v(&["package", "list"])).is_ok());
    }

    #[test]
    fn state_parses_and_degrades() {
        let st = State::parse(r#"{"state":"READY","version":13,"uid":2000,"sui":false}"#);
        assert!(st.ready());
        assert_eq!(st.identity(), "adb");
        assert!(st.line().contains("adb"), "{}", st.line());
        assert!(st.line().contains("13"), "{}", st.line());
        // 同一个状态只有一种写法：status 和 banner 都走 `line()`
        assert!(st.line().contains("13"), "{}", st.line());

        // 壳回了一段看不懂的东西：当成没装，而不是 panic
        let bad = State::parse("这不是 JSON");
        assert_eq!(bad.state, "NOT_INSTALLED");
        assert!(!bad.ready());
        assert!(bad.line().contains("没有 Shizuku"));
    }

    #[test]
    fn root_backend_is_named() {
        let st = State::parse(r#"{"state":"READY","version":13,"uid":0,"sui":true}"#);
        assert_eq!(st.identity(), "root");
        assert!(st.line().contains("Sui"), "{}", st.line());
        assert!(st.line().contains("root"), "{}", st.line());
    }

    #[test]
    fn every_state_has_a_distinct_hint() {
        let hints: Vec<&str> = ["NOT_INSTALLED", "NOT_RUNNING", "NEED_PERMISSION"]
            .iter()
            .map(|s| State::parse(&format!(r#"{{"state":"{s}"}}"#)).hint())
            .collect();
        for h in &hints {
            assert!(!h.is_empty());
        }
        assert_ne!(hints[0], hints[1]);
        assert_ne!(hints[1], hints[2]);
    }

    #[test]
    fn ping_fails_loudly_when_not_ready() {
        let r = service_reply(&v(&["service", "ping"]), r#"{"state":"NOT_RUNNING"}"#);
        assert_eq!(r["ok"], json!(false));
        assert_eq!(r["error"]["code"], json!("NOT_RUNNING"));
    }

    #[test]
    fn status_is_ok_even_when_not_ready() {
        // 「现在没就绪」是一个事实，不是错误 —— 不该让 `agent shizuku service status` 非 0 退出
        let r = service_reply(&v(&["service", "status"]), r#"{"state":"NEED_PERMISSION"}"#);
        assert_eq!(r["ok"], json!(true));
        let text = r["text"].as_str().unwrap();
        assert!(text.contains("NEED_PERMISSION"), "{text}");
        assert!(text.contains("授权"), "没就绪时要给出下一步：{text}");
        assert_eq!(text.lines().count(), 1, "状态就该是一行：{text}");
    }

    #[test]
    fn error_codes_only_when_they_carry_information() {
        // 命令自己已经把话说清了：别再补一个 OPERATION_FAILED
        let t = reply_text(
            r#"{"ok":false,"error":{"code":"OPERATION_FAILED","message":"ls: /nope: No such file"}}"#,
        );
        assert_eq!(t, "错误：ls: /nope: No such file");

        // 这三种要留着：它们意味着「换参数也没用，得改条件」
        for code in ["TIMEOUT", "PERMISSION_DENIED", "KILLED"] {
            let t = reply_text(&format!(
                r#"{{"ok":false,"error":{{"code":"{code}","message":"x"}}}}"#
            ));
            assert!(t.ends_with(&format!("（{code}）")), "{code}: {t}");
        }
    }

    /// 大输出必须被拦住 —— `dumpsys activity` 实测 2MB。
    #[test]
    fn huge_output_is_truncated_like_bash() {
        let big = "行
"
        .repeat(5_000);
        let v = serde_json::json!({ "ok": true, "text": big }).to_string();
        let t = reply_text(&v);
        assert!(
            t.len() < crate::linux::MAX_OUTPUT_BYTES,
            "没拦住：{} 字节",
            t.len()
        );
        assert!(t.contains("省略"), "{t}");
        assert!(t.contains("已截断"), "要告诉模型怎么重跑：{t}");
        // 首尾都得露脸（头是开场，尾是结论/报错）
        assert!(t.starts_with("行"), "{t}");
        assert!(t.trim_end().ends_with("）"), "{t}");
    }

    /// 没超限的一个字节都不许动。
    #[test]
    fn small_output_is_untouched() {
        let v = serde_json::json!({ "ok": true, "text": "uid=2000(shell)\n" }).to_string();
        assert_eq!(reply_text(&v), "uid=2000(shell)");
    }

    #[test]
    fn ping_says_exactly_ok() {
        let r = service_reply(
            &v(&["service", "ping"]),
            r#"{"state":"READY","version":13,"uid":2000}"#,
        );
        assert_eq!(r["text"], json!("OK"), "ping 是给脚本判断用的，别解释：{r}");
    }

    #[test]
    fn status_is_one_line() {
        for raw in [
            r#"{"state":"READY","version":13,"uid":2000,"sui":false}"#,
            r#"{"state":"READY","version":13,"uid":0,"sui":true}"#,
            r#"{"state":"NOT_RUNNING"}"#,
            r#"{"state":"NEED_PERMISSION"}"#,
            r#"{"state":"NOT_INSTALLED"}"#,
        ] {
            let r = service_reply(&v(&["service", "status"]), raw);
            let text = r["text"].as_str().unwrap();
            assert_eq!(text.lines().count(), 1, "{raw} → {text}");
            // 就绪时：身份和 uid 说同一件事，只留一个
            if raw.contains("READY") {
                assert_eq!(text.matches("uid=").count(), 1, "uid 只该出现一次：{text}");
            }
        }
    }

    // ---- P1：组 → 命令（plan）----

    fn cmd(argv: &[&str]) -> String {
        plan(&v(argv)).expect("该能拼出命令").cmd
    }

    #[test]
    fn every_group_and_sub_is_plannable_or_says_why() {
        // 表里每一格要么能拼出命令，要么给一句带用法的错误（不能 panic、不能空）
        for g in GROUPS {
            for sub in g.subs {
                if g.name == "service" || g.name == "exec" {
                    continue; // 这两组不走 plan
                }
                let argv = v(&[g.name, sub.name]);
                match plan(&argv) {
                    Ok(p) => assert!(!p.cmd.is_empty(), "{} {} 拼出空命令", g.name, sub.name),
                    Err(e) => {
                        assert!(!e.is_empty(), "{} {} 报了个空错误", g.name, sub.name);
                        // 报错必须告诉人怎么用，否则模型只能瞎试
                        assert!(
                            e.contains("用法") || e.contains("不认识") || e.contains("要"),
                            "{} {} 的错误没指路：{e}",
                            g.name,
                            sub.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn shell_values_are_quoted_not_injected() {
        // 模型/用户给的值都可能带 shell 元字符。全部单引号包住、内部单引号转义
        let p = plan(&v(&["package", "info", "com.x; rm -rf /data"])).unwrap();
        assert_eq!(p.cmd, "dumpsys package 'com.x; rm -rf /data'");
        // 单引号本身
        let p = plan(&v(&["settings", "set", "global", "k", "a'b"])).unwrap();
        assert_eq!(p.cmd, "settings put 'global' 'k' 'a'\\''b'");
        // 反引号和 $() 也是普通字符
        let p = plan(&v(&["package", "info", "`id` $(whoami)"])).unwrap();
        assert_eq!(p.cmd, "dumpsys package '`id` $(whoami)'");
    }

    #[test]
    fn defaults_are_the_ones_help_promises() {
        // help 里写着「不带子命令 = list/info」，那就得真的是那个
        assert_eq!(cmd(&["package"]), "pm list packages ");
        assert_eq!(cmd(&["user"]), "pm list users");
        assert_eq!(cmd(&["notification"]), "dumpsys notification --noredact");
        assert_eq!(cmd(&["display"]), "dumpsys display");
        assert!(cmd(&["device"]).contains("getprop"));
    }

    #[test]
    fn numbers_are_checked_before_building_a_command() {
        // 非数字不能拼进命令（`input tap x 200` 会让 input 报一句看不懂的话）
        let e = plan(&v(&["input", "tap", "abc", "200"])).unwrap_err();
        assert!(e.contains("数字"), "{e}");
        assert!(plan(&v(&["input", "tap", "100", "200"])).is_ok());
        assert!(plan(&v(&["display", "density", "很密"])).is_err());
    }

    #[test]
    fn missing_arguments_are_refused_with_usage() {
        for (argv, want) in [
            (vec!["package", "info"], "包名"),
            (vec!["settings", "get"], "用法"),
            (vec!["settings", "set", "global", "k"], "用法"),
            (vec!["input", "tap", "1"], "用法"),
            (vec!["file", "rm"], "路径"),
        ] {
            let e = plan(&v(&argv)).unwrap_err();
            assert!(e.contains(want), "{argv:?} → {e}");
        }
    }

    #[test]
    fn settings_namespaces_are_validated() {
        assert!(plan(&v(&["settings", "get", "wrong", "k"])).is_err());
        for ns in ["global", "system", "secure"] {
            assert!(plan(&v(&["settings", "get", ns, "k"])).is_ok(), "{ns}");
        }
    }

    #[test]
    fn exact_commands_for_the_common_ones() {
        assert_eq!(cmd(&["device", "battery"]), "dumpsys battery");
        assert_eq!(cmd(&["package", "list", "-3"]), "pm list packages -3");
        assert_eq!(
            cmd(&["package", "uninstall", "com.x", "-k"]),
            "pm uninstall -k 'com.x'"
        );
        assert_eq!(cmd(&["input", "key", "back"]), "input keyevent 'BACK'");
        assert_eq!(
            cmd(&["input", "swipe", "1", "2", "3", "4"]),
            "input swipe 1 2 3 4 300"
        );
        assert_eq!(cmd(&["display", "size", "reset"]), "wm size reset");
        // `input text` 的空格要换成 %s（input 不认空格）
        assert_eq!(
            cmd(&["input", "text", "hello", "world"]),
            "input text 'hello%sworld'"
        );
    }

    #[test]
    fn unknown_subcommand_is_refused() {
        let e = plan(&v(&["package", "explode"])).unwrap_err();
        assert!(e.contains("explode"), "{e}");
        assert!(e.contains("help"), "{e}");
    }

    // ---- P1：解析失败退回原文 ----

    #[test]
    fn unparsable_output_falls_back_to_the_raw_text() {
        let p = Plan {
            cmd: "dumpsys battery".into(),
            parse: Some(crate::shizuku_parse::parse_battery),
            done: None,
        };
        let raw = r#"{"ok":true,"text":"这份 dumpsys 格式我没见过\n"}"#;
        let t = structured_text(raw, &p);
        assert!(t.contains("认不出来"), "要说清是解析失败：{t}");
        assert!(t.contains("我没见过"), "原文得给全：{t}");
    }

    #[test]
    fn parsed_output_is_short() {
        let p = Plan {
            cmd: "dumpsys battery".into(),
            parse: Some(crate::shizuku_parse::parse_battery),
            done: None,
        };
        let raw = r#"{"ok":true,"text":"  level: 53\n  temperature: 377\n  status: 2\n"}"#;
        let t = structured_text(raw, &p);
        // 三个字段三行，别把原文照抄
        assert!(t.lines().count() <= 3, "结构化之后要短：{t}");
        assert!(t.contains("53%"), "{t}");
    }

    #[test]
    fn done_plans_say_one_line() {
        let p = plan(&v(&["package", "uninstall", "com.x"])).unwrap();
        // 注意：这条 JSON 里必须是 \n 转义，不能是真换行（真换行会让整段 JSON 非法）
        let raw = r#"{"ok":true,"text":"Success\n"}"#;
        assert_eq!(structured_text(raw, &p), "已卸载 com.x");
    }

    #[test]
    fn failures_still_go_through_the_shared_error_path() {
        let p = plan(&v(&["package", "uninstall", "com.x"])).unwrap();
        let raw = r#"{"ok":false,"error":{"code":"TIMEOUT","message":"超了"}}"#;
        assert_eq!(structured_text(raw, &p), "错误：超了（TIMEOUT）");
    }

    #[test]
    fn help_lists_every_group() {
        let h = help_text();
        for g in GROUPS {
            assert!(h.contains(g.name), "help 里没列 {}：{h}", g.name);
        }
        assert!(
            h.contains("会改东西"),
            "「不审批」的代价靠这句话对冲，不能删：{h}"
        );
    }

    #[test]
    fn prompt_clause_lists_groups() {
        let c = prompt_clause();
        for g in group_names() {
            assert!(c.contains(g), "{c}");
        }
        assert!(c.contains("adb"), "{c}");
    }

    #[test]
    fn usages_start_with_the_command_name() {
        // help 的每一行都以 `agent shizuku ` 开头：抄进别的文档时不用再补前缀
        for g in GROUPS {
            for s in g.subs {
                assert!(s.usage.starts_with("agent shizuku "), "{}", s.usage);
                assert!(
                    s.usage.contains(g.name),
                    "{} 里没有组名 {}",
                    s.usage,
                    g.name
                );
            }
        }
    }
}
