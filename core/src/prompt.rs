//! 提示词：系统提示词 + 工具描述（schema 里给模型看的参数说明也在这儿）。
//!
//! ⚠️ **系统提示词（`system_prompt(平台)`）必须是常量，任何时候都不能变。**
//! 它按平台由 `'static` 片段拼一次就冻结（见下面 `system_prompt`）——「常量」说的是
//! 同一个部署里字节不变，不是「源码里只能是一个字面量」。
//!
//! 原因：它是消息数组的第一个元素，而 prompt cache 是**按前缀**命中的。
//! 只要第一个字节变了，整个对话的缓存全部失效。
//! 按 OpenCode Go 的定价，DeepSeek V4.1 Flash 的输入 $0.30/M，
//! 缓存读取 $0.006/M —— 差 50 倍。把界面快照塞进 system prompt
//! 意味着屏幕每次一变就重算整个上下文。
//!
//! 所以：
//! - 系统提示词：每个平台一份冻结的字符串，零运行时数据
//! - 动态内容（界面快照、设备信息）：作为一个**独立消息**插在
//!   用户输入前面，只生成一次然后冻结进历史
//! - 工具表：顺序固定，序列化字节完全确定
//! - 会话 ID：开一次就不变（provider 靠它做缓存路由）
//!
//! 最终消息形状（前缀单调增长，每轮都能命中上一轮的前缀）：
//!
//! [system: 系统提示词]              ← 永远不变
//! [system: 上下文#1]                ← 第一轮生成后冻结
//! [user:   U1]
//! [assistant: ...]
//! [tool: ...]
//! [system: 上下文#2]                ← 第二轮才追加
//! [user:   U2]
//! ...

//! 单独一个文件：改文案是高频操作，别跟循环 / 工具执行 / 历史拼装混在一起（那些在
//! `agent.rs`）。这里只有字符串、拼装，和三个工具的 schema。

use std::sync::OnceLock;

use serde_json::{json, Value};

use crate::linux;
use crate::platform::Platform;

/// 这个平台的系统提示词（每个平台拼一次，之后一直是同一个 `&'static str`）。
///
/// 为什么拼而不是写三个字面量：三份里绝大多数句子一字不差，只有「在哪跑、
/// 有哪些门」那几处不同 —— 抄三遍的话，改一句共用规则得记着改三处，漏一处
/// 几个平台干活的方式就开始漂。拼的输入全是 `'static` 片段，**没有运行时
/// 数据**，所以同一个平台每次拼出来都是同一串字节，缓存前缀不受影响。
pub fn system_prompt(p: Platform) -> &'static str {
    system_prompt_with(p, linux::has_phone_capabilities())
}

/// 平台 + **这台机器上有没有设备能力** → 提示词。
///
/// 为什么要显式传 `phone` 而不是在里面 `cfg!`：那一段（`agent device|clipboard|…`）
/// 只有**真手机**上才有 —— 桌面上的 proot 沙箱也是 `Platform::Android` 形态，
/// 但它没有闹钟和短信。判据本身是编译目标（`linux::has_phone_capabilities`），
/// 但如果把它埋在函数里，`dump_prompt` 在电脑上就渲染不出手机那份 ——
/// **渲染工具渲染不出真实产物，"看提示词"这件事就不成立了**。
pub fn system_prompt_with(p: Platform, phone: bool) -> &'static str {
    // 3 个平台 × 有没有设备能力
    static PROMPTS: [OnceLock<String>; 6] = [
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
        OnceLock::new(),
    ];
    PROMPTS[p as usize * 2 + usize::from(phone)]
        .get_or_init(|| compose_prompt(p, phone))
}

/// 提示词和工具描述里**随平台变**的那几句。共用的句子只在 `compose_prompt` /
/// `bash_description` 里写一份。
struct Phrasing {
    /// 第一句：你是谁、跑在哪
    who: &'static str,
    /// cwd / 文件放哪 / 能动用户的什么
    files: &'static str,
    /// 那句里示例用的路径（沙箱里就是 /workspace，宿主模式给一个真实的绝对路径）
    link_example: &'static str,
    /// 紧跟那一句后面的**独立一行**（手机上：网址 / App 深链不加前缀；宿主模式空着
    /// —— 空串等于没这一行，不会留出空行）
    link_bullet: &'static str,
    /// bash 工具描述的开头：在哪跑
    bash_where: &'static str,
    /// bash 工具描述：这台机器上有什么、怎么装东西
    bash_env: &'static str,
}
/// Linux / macOS 宿主模式共用的部分（各自再盖掉不同的那几句）。
const HOST: Phrasing = Phrasing {
    who: "",
    // 只留规则：工具和 shell 的相对路径都以 cwd 为基准，没有沙箱那层映射。
    files: "相对路径基于 cwd，用户不指定目录时，你就尽量在 /tmp 下工作，不污染用户家目录，动用户的东西之前先说明，删除或覆盖先确认。",
    // 不说这句它会以为只能链 /workspace 里的，先把截图拷进来再给链接
    link_example: "/tmp/报告/index.html",
    link_bullet: "",
    bash_where: "跑 bash 命令（cwd=启动时的目录，`pwd` 就是它；每次新进程，shell 状态不持久）。",
    bash_env: "优先用已预装的 curl/git/gh/jq/rg/fd 等高性能工具，效率更高。",
};

const ANDROID: Phrasing = Phrasing {
    who: "你是运行在安卓手机上的 Agent。",
    files: "你在 Alpine Linux 沙盒的当前目录里工作（可用 `pwd` 查看），它在 /workspace 里面 —— 那对应手机内部存储的 Agent 文件夹（用户能在文件管理器看到，卸载不丢）；临时产物放 /tmp，依赖和解压的东西放 /root（重装会清空）。整个内部存储挂在 /sdcard，可读写用户文件 —— 动它之前先说明，删除或覆盖先确认；不确定就先 `ls /sdcard`（可能没挂上）。",
    link_example: "/workspace/报告/index.html",
    link_bullet: "\n- 网址和 App 深链（`https://…`、`weixin://…`、`androidamap://…`）直接写地址，不要加 `preview:` —— 它只给文件用。",
    bash_where: "在 Alpine Linux 沙箱里跑 bash 命令（每次新进程，文件系统持久、shell 状态不持久；`pwd` 看当前在哪）。",
    bash_env: "优先用已预装的 curl/git/gh/jq/rg/fd/tree/unzip/python3 等高性能工具，效率更高。安装 Python 包时优先 `apk add py3-*`。",
};

const LINUX: Phrasing = Phrasing {
    who: "你是运行在 Linux 上的 Agent。",
    ..HOST
};

/// `setsid` 在 macOS 上不是系统自带的 —— 是宿主模式垫在 PATH 里的
/// （`linux::SETSID_SHIM`），所以后台服务的写法跟别处一样，不用另说。
const MACOS: Phrasing = Phrasing {
    who: "你是运行在 Mac（macOS）上的 Agent。",
    // 不点这一句的话，模型几乎一定按 GNU 写，白白报错一轮
    bash_env: "优先用已预装的 curl/git/gh/jq/rg/fd 等高性能工具，效率更高。这是 BSD 用户态：`sed -i ''`、`date`、`stat`、`readlink -f`、`xargs` 的参数跟 GNU 不一样，拿不准先看 `man` 或用 python3；装东西用 brew 或 uv。",
    ..HOST
};

impl Phrasing {
    const fn of(p: Platform) -> &'static Phrasing {
        match p {
            Platform::Android => &ANDROID,
            Platform::Linux => &LINUX,
            Platform::Macos => &MACOS,
        }
    }
}

fn compose_prompt(p: Platform, phone: bool) -> String {
    let t = Phrasing::of(p);
    format!(
        "{who}\n\
         \n\
         - {files}\n\
         - 想让用户看链接时：写 `preview:` 链接，如 `[打开](preview:{link_example})`，图片/音频/视频都用图片语法，能渲染 `![说明](preview:…)`。{link_bullet}\n\
         - `agent` 命令（已在 PATH 里）：{cli}，`agent help` 看全部。\n\
         - 一次回复里可以带多个工具调用：互不依赖的操作（比如一次新建好几个文件、跑几条互不相干的命令）合并到同一条回复里一起发，有依赖关系的才分成多轮。\n\
         \n\
         回答要言简意赅。",
        who = t.who,
        files = t.files,
        link_example = t.link_example,
        link_bullet = t.link_bullet,
        // 这台机器上有的子命令（和沙箱里那份 `agent` 同一张表，见 `cli.rs`）
        cli = crate::cli::prompt_clause(crate::cli::Machine::of(p, phone)),
    )
}

/// `read` 工具的描述：截断规则 + 图片那条管线。
pub fn read_description() -> String {
    // 2000 行 / 50KB 从 `linux.rs` 的常量来，别抄死 —— 两处各写一遍，改常量不会报错，
    // 句子就悄悄跟实际行为不一致了。
    format!(
        "读取文本或图片。读取文本时，输出会截断到 {lines} 行或 {kb}KB，没读完时末尾会写明整个文件多少行、多大、用哪个 offset 续读，据此决定继续读还是改用 bash 的 rg/sed 局部读。读取图片时（png/jpg/gif/webp）可能会缩放（offset/limit 对图片无效），结果里写明原图尺寸和文件大小。",
        lines = linux::MAX_OUTPUT_LINES,
        kb = linux::MAX_OUTPUT_BYTES / 1024,
    )
}

/// `read` 的 schema（参数说明也是给模型看的话，一并放这儿）。
pub fn read_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 cwd）" },
            "offset": { "type": "integer", "description": "起始行号，从 1 开始（可选，默认 1）" },
            "limit": { "type": "integer", "description": "最多读取的行数（可选，默认读到截断上限）" },
            "numbered": { "type": "boolean", "description": "给每行加 cat -n 同款行号（可选，默认关）" }
        },
        "required": ["path"]
    })
}

/// `bash` 工具的描述：在哪跑 + 共用的输出规则 + 这台机器上有什么 +
/// 后台服务怎么放、怎么收。
pub fn bash_description(p: Platform) -> String {
    let t = Phrasing::of(p);
    // 截断规则的数字从常量取：以前两处文案各抄一遍 2000 / 50KB / 100，
    // 改常量不会报错，句子就悄悄跟实际行为不一致了（见 `linux.rs`）。
    format!(
        "{where_}命令输出超过 {lines} 行或 {kb}KB 时，给**首 {head} 行 + [省略 N 行] + 尾 {tail} 行**，完整输出存临时文件并返回路径，要看中间那段就去 read 它。{env}后台常驻服务用 `setsid <命令> > /tmp/x.log 2>&1 &`（能活过这次调用；裸 `&` 起的会在超时/停止时被一起杀掉），之后 {procs} 看还剩什么、{kill} 停掉。",
        lines = linux::MAX_OUTPUT_LINES,
        kb = linux::MAX_OUTPUT_BYTES / 1024,
        head = linux::HEAD_LINES,
        tail = linux::TAIL_LINES,
        where_ = t.bash_where,
        env = t.bash_env,
        procs = p.proc_tools(),
        kill = p.kill_tool(),
    )
}

/// `bash` 的 schema。
pub fn bash_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "要执行的 shell 命令" },
            "timeout": { "type": "number", "description": "超时秒数（可选，默认 60，上限 86400）。" }
        },
        "required": ["command"]
    })
}

/// `edit` 工具的描述。
pub fn edit_description() -> String {
    "改文件或新建文件。新建文件时，path 会按需创建父目录，文件已存在会报错，content 中写新文件全文。改文件时，edits 做精确替换（一个或多个；相邻改动请合并成一条，但不要为了够到远处的改动而在 oldText 里塞大段没变的内容）。edits 与 content 互斥。".to_string()
}

/// `edit` 的 schema。
pub fn edit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "文件路径（相对或绝对；相对路径基于 cwd）" },
            "edits": {
                "type": "array",
                "description": "一条或多条替换（改已有文件时用）",
                "items": {
                    "type": "object",
                    "properties": {
                        "oldText": { "type": "string", "description": "要替换的原文；必须在原文件中唯一，且不能与其他 edits[].oldText 重叠" },
                        "newText": { "type": "string", "description": "替换成的内容" }
                    },
                    "required": ["oldText", "newText"]
                }
            },
            "content": { "type": "string", "description": "新文件的全文（新建文件时用）" }
        },
        "required": ["path"]
    })
}

/// 回合被打断时留下的工具调用结果。
///
/// 正常情况下停止会把没跑完的调用补上结果（见 `drive`），这里是兜底：
/// 进程在两步之间被杀留下的会话。
pub const INTERRUPTED: &str = "错误：没有结果，回合被中断（用户停止或 App 被关闭）";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux;

    /// 设备命令只在**真有**的那台机器上讲（清单从 `cli.rs` 那张表生成）。
    ///
    /// 提示词里写了而实际做不到是最贵的一种错：模型会拿它当确定的事实用，
    /// 撞墙之后反复换参数重试（它不怀疑命令存不存在）。
    #[test]
    fn device_commands_are_only_promised_where_they_exist() {
        let phone = system_prompt_with(Platform::Android, true);
        let want = format!("`agent {}`", crate::phone::names().join("|"));
        assert!(phone.contains(&want), "手机形态的清单不对，应该是 {want}：\n{phone}");

        // 三个平台的「没有设备能力」形态（手机之外的任何机器）：一个字都不该提
        for plat in Platform::ALL {
            let p = system_prompt_with(plat, false);
            assert!(
                !p.contains("agent sms") && !p.contains("agent tap"),
                "{plat:?} 上没有这些，提示词里不该提：\n{p}"
            );
        }

        // 真实那份跟着这台机器走
        let p = system_prompt(Platform::Android);
        assert_eq!(p.contains("agent sms"), linux::has_phone_capabilities());
        for s in ["agent browser", "agent search"] {
            assert!(p.contains(s), "提示词里掉了 {s}");
        }
    }

    /// **提示词里不能出现具体的工作目录名**。
    ///
    /// 每个会话的目录都不一样，写进提示词就等于给每个会话一份不同的
    /// system prompt —— 前缀缓存全废（见 docs/03）。这条只能靠测试守：
    /// 文案里手滑写一个例子路径，功能上完全看不出来，钱却在悄悄烧。
    #[test]
    fn prompt_never_mentions_a_concrete_working_dir() {
        let dir = crate::linux::session_guest_dir("0199e1f0-aaaa-7000-8000-000000000001");
        for p in Platform::ALL {
            for phone in [false, true] {
            let s = system_prompt_with(p, phone);
            assert!(!s.contains(&dir), "{p:?} 的提示词里出现了会话目录：{dir}");
            assert!(
                !s.contains("sessions/"),
                "{p:?} 的提示词里出现了 sessions/ 这种具体路径"
            );
            }
        }
    }

    /// 拼出来的句子不能有双顿号 / 空括号（`agent` 那一行是按机器拼的）。
    #[test]
    fn the_cli_sentence_stays_clean() {
        for p in Platform::ALL {
            for phone in [false, true] {
            let s = system_prompt_with(p, phone);
            assert!(!s.contains("、、"), "拼出双顿号了：\n{s}");
            assert!(!s.contains("（）"), "拼出空括号了：\n{s}");
            }
        }
    }
}
