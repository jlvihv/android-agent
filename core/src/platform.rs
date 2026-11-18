//! 跑在哪种机器上 —— 系统提示词、工具表、`agent ps` 这些「给模型看的话」里
//! **随平台变的那几句**都从这里取。
//!
//! 平台是一个值：提示词和工具表都由「共用片段 + 这个平台的片段」拼出来，每个
//! 平台拼一次就冻结（见 `prompt::system_prompt` / `agent::tools`）—— 前缀缓存要的
//! 是**同一个部署里字节确定**，不是「源码里是一个字面量」。
//!
//! 不能只有一份工具表（写死「Alpine 沙箱 / 已预装 / apk add」）：那样桌面就得
//! 整句去**反驳**它，`pgrep` / `ss` 这类每个系统的事实也会散在三处。

/// 模型跑在哪。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// 安卓手机：proot + Alpine 沙箱。桌面上的 `just dev`（proot 沙箱）也是这个
    /// 形态 —— 那条路本来就是为了在电脑上复现手机。
    Android,
    /// 桌面 Linux，桌面（没有沙箱）
    Linux,
    /// macOS，桌面（没有沙箱）
    Macos,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Android, Platform::Linux, Platform::Macos];

    /// 「平台 × 二元档位」的槽位数（见 [`Platform::slot2`]）。
    ///
    /// 提示词（有没有设备能力）、工具表（worker / router）都是一份「按平台 × 两档」
    /// 的静态缓存，各在自己模块里声明一个 `[OnceLock<_>; 6]`。以前那个 6 和
    /// `p as usize * 2 + 槽位` 在四个地方各写一遍 —— 加一个平台就漏一个（数组长度
    /// 不会自己跟着变，而多出来的平台会静默落到越界 panic 上）。
    pub const fn slot2(self, second: usize) -> usize {
        self as usize * 2 + second
    }

    /// 这种「平台 × 两档」缓存的槽位总数。
    pub const SLOTS2: usize = Self::ALL.len() * 2;

    /// 这个二进制上的桌面是哪种（按编译目标，一个二进制只跑在一种系统上）。
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Linux
        }
    }

    /// 是不是桌面（没有沙箱，直接用这台电脑）。
    pub const fn is_host(self) -> bool {
        !matches!(self, Platform::Android)
    }

    /// 看「后台还有什么在跑 / 谁占着端口」用哪些命令。
    ///
    /// 安卓沙箱里是我们自己的 `agent ps`（靠 proot 的登记表认得出刻意脱离的
    /// 常驻进程）；桌面没有登记表，用系统的。macOS 没有 `ss`，BSD 的
    /// `pgrep` 没有 `-a`（那是 procps 的）。
    pub const fn proc_tools(self) -> &'static str {
        match self {
            Platform::Android => "`agent ps`",
            Platform::Linux => "`ps` / `pgrep -af` / `ss -ltnp`",
            Platform::Macos => "`ps` / `pgrep -fl` / `lsof -nP -iTCP -sTCP:LISTEN`",
        }
    }

    /// 停掉一个进程用什么。
    pub const fn kill_tool(self) -> &'static str {
        match self {
            Platform::Android => "`agent kill <pid>`",
            Platform::Linux | Platform::Macos => "`kill <pid>`",
        }
    }
}
