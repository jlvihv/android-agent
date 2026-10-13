//! 跑在哪种机器上 —— 系统提示词、工具表、`agent ps` 这些「给模型看的话」里
//! **随平台变的那几句**都从这里取。
//!
//! 为什么要有它：以前工具表只有一份（写死了「Alpine 沙箱 / 已预装 / apk add」），
//! 宿主模式的提示词只好整句去**反驳**工具表，`pgrep` / `ss` 这类每个系统的事实
//! 散在三处。现在平台是一个值，提示词和工具表都由「共用片段 + 这个平台的片段」
//! 拼出来，每个平台拼一次就冻结（见 `agent::system_prompt` / `agent::tools`）——
//! 前缀缓存要的是**同一个部署里字节确定**，不是「源码里是一个字面量」。

/// 模型跑在哪。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// 安卓手机：proot + Alpine 沙箱。桌面上的 `just dev`（proot 沙箱）也是这个
    /// 形态 —— 那条路本来就是为了在电脑上复现手机。
    Android,
    /// 桌面 Linux，宿主模式（没有沙箱）
    Linux,
    /// macOS，宿主模式（没有沙箱）
    Macos,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Android, Platform::Linux, Platform::Macos];

    /// 这个二进制上的宿主模式是哪种（按编译目标，一个二进制只跑在一种系统上）。
    pub const fn host() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Macos
        } else {
            Platform::Linux
        }
    }

    /// 是不是宿主模式（没有沙箱，直接用这台电脑）。
    pub const fn is_host(self) -> bool {
        !matches!(self, Platform::Android)
    }

    /// 看「后台还有什么在跑 / 谁占着端口」用哪些命令。
    ///
    /// 安卓沙箱里是我们自己的 `agent ps`（靠 proot 的登记表认得出刻意脱离的
    /// 常驻进程）；宿主模式没有登记表，用系统的。macOS 没有 `ss`，BSD 的
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_flavour_follows_the_compile_target() {
        assert!(Platform::host().is_host());
        assert!(!Platform::Android.is_host());
        assert_eq!(
            Platform::host() == Platform::Macos,
            cfg!(target_os = "macos")
        );
    }

    #[test]
    fn nobody_is_told_to_use_a_command_their_system_lacks() {
        let mac = Platform::Macos.proc_tools();
        assert!(!mac.contains("ss -"), "macOS 没有 ss");
        assert!(!mac.contains("pgrep -af"), "BSD pgrep 没有 -a");
        assert!(!Platform::Linux.proc_tools().contains("lsof"));
    }
}
