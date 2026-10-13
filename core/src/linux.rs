//! Linux 沙箱：在设备上跑一个真正的 Alpine Linux userland。
//!
//! 参考实现见 `docs/02-linux-sandbox.md`（OpenMinis 的做法）。
//!
//! ## 核心约束
//!
//! Android 10+ 对 `untrusted_app` 强制 W^X：`app_data_file` 标签下的一切
//! （包括 `filesDir/alpine-rootfs/` 里的整个 rootfs）**不能被执行**。
//! `chmod +x` 会成功，`execve` 仍然 EACCES。
//!
//! 所以：
//! - **proot 二进制本身**放在 `nativeLibraryDir`（由 APK 的 `lib/**/*.so`
//!   解出来，标签 `apk_data_file`，可执行）。文件名必须是 `lib*.so`，
//!   否则 APK 安装器根本不会解压它。
//! - **rootfs 里的二进制**由 proot 的 ELF loader 自己 mmap 进内存，
//!   不走内核 `execve`。loader 同样在 `nativeLibraryDir`。
//!
//! ## 为什么这段逻辑在 Rust 而不是 Kotlin
//!
//! 因为 proot 是个普通 Linux 程序 —— 桌面上装个 x86_64 版就能把
//! 命令行构造、超时、输出截断这些逻辑全部测通，不必等真机。
//! Kotlin 只需要在 boot 时把两个路径交进来。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::editor::{self, Edit, EditOutcome};
use crate::reader::{self, ReadView};

/// 单次命令的默认超时。
///
/// 只给 `bash` 工具之外的入口用（`/api/linux` 的终端/CLI）—— 那边没人替我们
/// 估时间，得给个默认值。**`bash` 工具不走这个默认值**：模型必须自己传 `timeout`
///（见 `agent.rs`），免得它不假思索地依赖一个偏短的默认值，把长时间下载/编译
/// 在半路杀掉。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// 单次命令的超时上限：24 小时。
///
/// 基本等于「不设限」，但总得有个兜底 —— 一条真正挂死的命令不能把沙箱占住到
/// 天荒地老。下载、编译、`apk add` 这类活儿几十分钟到几小时都正常，6000 秒
/// 那种量级完全该允许。
pub const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);

/// `read` 工具允许整份读进内存的文本文件上限。
///
/// 读取要按行定位 `offset`，所以是整份读进来再切 —— 一个 200MB 的日志会把
/// 手机内存吃爆（OOM kill 不是 panic，但一样是崩溃）。超过就回一条错误，
/// 让模型改用 `sed -n` / `head` / `rg` 局部读。
const MAX_TEXT_BYTES: u64 = 32 * 1024 * 1024;

/// 回给模型的输出上限：**超过 `MAX_OUTPUT_LINES` 行或 `MAX_OUTPUT_BYTES` 字节**
/// 才算截断。
///
/// 截断后的呈现是首尾各留 `HEAD_LINES` / `TAIL_LINES` 行、中间用
/// `[省略 N 行]` 顶掉：头是命令的开场（版本号、表头、第一条结果），尾是结论
/// 和报错 —— 两头都有用，所以不像 `read` 那样只看头（bash 的输出没地方续读，
/// 只能靠 `/tmp` 里那份完整输出）。字节预算由两侧平分，保证两边都能露脸。
///
/// 触发线故意留得很宽（2000 行 / 50KB）：1999 行的文件应该一次就看全，
/// 否则模型得反复 offset 续读，比多给几千行贵得多。
pub const MAX_OUTPUT_LINES: usize = 2000;
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// 截断时首/尾各留多少行
pub const HEAD_LINES: usize = 100;
pub const TAIL_LINES: usize = 100;
/// 截断时分给首/尾各一侧的字节预算（给 `[省略 N 行]` 和换行留点余量）
const SIDE_BYTES: usize = (MAX_OUTPUT_BYTES - 256) / 2;

// ---------------------------------------------------------------------------
// 一条命令的进程模型（实现在 `run_live`，理由见 docs/02 的「进程模型」）
// ---------------------------------------------------------------------------

/// 每条命令的状态文件放在 rootfs 里哪个目录下（沙箱内就是 `/.agent-exit/`）。
///
/// rootfs 对 guest 就是 `/`，所以两边看的是同一个文件，不用额外挂载。
const EXIT_DIR: &str = ".agent-exit";

/// Alpine 里 `setsid` 的两个可能位置（3.21 是 usrmerge 之后的 `/usr/bin`）。
const SETSID_PATHS: [&str; 2] = ["/usr/bin/setsid", "/bin/setsid"];

/// 状态文件的轮询间隔。
const STATUS_POLL: Duration = Duration::from_millis(25);

/// 命令退出后，输出安静这么久就算读完了（见 `wait_for_quiet`）。
const OUTPUT_QUIET: Duration = Duration::from_millis(150);
/// 上面那条的硬上限：裸 `&` 起的子孙没脱离，可以一直往同一根管道写。
const OUTPUT_DRAIN_CAP: Duration = Duration::from_millis(2_000);

/// 超时后先 SIGTERM，等这么久还不走再 SIGKILL。
const KILL_GRACE: Duration = Duration::from_millis(1_000);
const KILL_POLL: Duration = Duration::from_millis(50);

/// 兜底等 proot 退出码的时间（它可能在给常驻进程当 tracer，不能无限等）。
const REAP_GRACE: Duration = Duration::from_millis(2_000);

/// 被我们杀掉之后，给读端收尾的时间。
const DRAIN_AFTER_KILL: Duration = Duration::from_millis(200);

/// 超过这么久的状态文件算上次崩溃留下的，清掉。
///
/// 必须 ≥ `MAX_TIMEOUT`：否则一条还在跑的长命令，它的 pid 文件会被**另一条**
/// 命令的清理删掉，那条命令超时时就只能拿 proot 的组当靶子（proot 一死，它
/// 起的常驻服务就失去 rootfs 翻译）。这些文件每个几十字节，多留一天没成本。
const STALE_STATUS_SECS: u64 = MAX_TIMEOUT.as_secs() + 3_600;

/// 超时时 `ExecResult::code` 报多少：跟 `timeout(1)` 一致，模型认得。
const TIMEOUT_EXIT: i32 = 124;

/// Alpine 的 PATH。
///
/// **必须显式设置。** 宿主（无论 Arch 还是 Android）的 PATH 里通常没有 `/bin`，
/// 而 Alpine minirootfs 的 busybox applet 全在 `/bin` 下 —— 不设的话
/// `ls`、`cat`、`uname` 全部 "not found"，只有 `/bin/sh` 能跑。
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/opt/bin";

/// 沙箱里固定的工作目录。命令的 cwd、相对路径的基准都是它。
///
/// 安卓上挂的是手机内部存储的 `Agent/` 文件夹 —— 用户在文件管理器里能
/// 直接看到 AI 的产出，App 卸载也不会丢。没有存储权限时（或桌面上）
/// 退回 rootfs 里的同名目录。
pub const WORKSPACE: &str = "/workspace";

/// 手机内部存储整个挂在这里（`/storage/emulated/0`）
pub const SHARED: &str = "/sdcard";

/// `read` 工具看到的东西：图片（缩放后内联）或文本（含截断提示）。
pub enum ReadResult {
    Text(ReadView),
    Image(crate::imaging::Encoded),
}

/// `edit` 工具的 create 模式建出来的文件。
///
/// 只回「建好了、多少行 / 字节」：内容是模型自己刚写进来的，原样回显纯属浪费
/// 上下文（`edit` 回 diff 是因为它没写过改动后的全文）。
#[derive(Debug)]
pub struct CreatedFile {
    pub lines: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// proot 可执行文件。Android 上是 `<nativeLibraryDir>/libproot.so`
    pub proot: PathBuf,
    /// rootfs 根目录
    pub rootfs: PathBuf,
    /// **宿主模式**：不套 proot，直接在宿主机上跑（桌面原生用，见 `LinuxSandbox::host`）。
    ///
    /// 打开后：`rootfs` 是 `/`（沙箱内的绝对路径就是本机路径），`proot` 不参与；
    /// 命令由 `/bin/bash -c` 直接跑，cwd 是挂到 `/workspace` 的那个宿主目录。
    /// `/sdcard`、loader、DNS 这些安卓专属项都不参与。
    #[serde(default)]
    pub host: bool,
    /// 额外的绑定挂载，`(宿主路径, 沙箱内路径)`
    #[serde(default)]
    pub binds: Vec<(String, String)>,
    /// proot 自己的临时目录（**宿主路径**）。
    ///
    /// 走 `PROOT_TMP_DIR` 环境变量，**不是** `TMPDIR` ——
    /// proot 只在 `PROOT_TMP_DIR` 里找，找不到就退回 `P_tmpdir`（`/tmp`），
    /// 而 Android 上 App 写不了 `/tmp`。
    #[serde(default)]
    pub tmp: Option<PathBuf>,
    /// 64 位 ELF loader，走 `PROOT_LOADER`。
    ///
    /// **安卓上必须设。** 不设的话 proot 会调 `extract_loader()` 把内嵌的
    /// loader 解到临时目录，而那是个 `app_data_file` —— W^X 直接拒绝执行，
    /// 最后报 `'/bin/sh' is not executable`。
    /// 只有 `nativeLibraryDir` 里的文件才允许执行。
    #[serde(default)]
    pub loader: Option<PathBuf>,
    /// 32 位 ELF loader，走 `PROOT_LOADER_32`
    #[serde(default)]
    pub loader32: Option<PathBuf>,
    /// 写进 rootfs 的 `/etc/resolv.conf`。
    ///
    /// **Alpine minirootfs 里根本没有这个文件**，所以 `apk add` 会因为
    /// DNS 解析失败而报 “no such package” —— 看起来像软件源挂了，
    /// 实际是名字都解析不出来。桌面上从宿主机拷；安卓上由 Kotlin
    /// 从 ConnectivityManager 拿系统 DNS 传进来。
    #[serde(default)]
    pub dns: Vec<String>,
    /// 时区偏移（分钟）。安卓上由 Kotlin 从系统时区传进来 ——
    /// 沙箱里读不到宿主时区，只能靠外部注入。
    #[serde(default)]
    pub tz_offset_minutes: Option<i32>,
    /// 额外环境变量
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 挂到 `/workspace` 的宿主目录。None = 用 rootfs 里的 `/workspace`
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// 挂到 `/sdcard` 的宿主目录（手机内部存储）。None = 不挂
    #[serde(default)]
    pub shared: Option<PathBuf>,
}

impl SandboxConfig {
    /// 接上（或断开）手机存储：整个存储挂 `/sdcard`，其中的 `Agent/` 挂
    /// `/workspace`。建不了 `Agent/`（没权限）就都不挂，退回 rootfs。
    pub fn with_shared_storage(mut self, storage: Option<&Path>) -> Self {
        self.shared = None;
        self.workspace = None;
        if let Some(root) = storage {
            let ws = root.join("Agent");
            match std::fs::create_dir_all(&ws) {
                Ok(()) => {
                    self.shared = Some(root.to_path_buf());
                    self.workspace = Some(ws);
                }
                Err(e) => tracing::warn!("建不了 {}（没有存储权限？）: {e}", ws.display()),
            }
        }
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,
    pub timed_out: bool,
    /// 输出被截断时，完整输出落在沙箱内的这个路径上，模型可以再去读/ grep
    pub full_output_path: Option<String>,
    /// 这次用的是不是真 bash。false = 没装 bash，退回了 `/bin/sh`（busybox
    /// ash），此时 `to_text` 会明说一句并让模型去装 —— 否则它写 bash 语法
    /// 只会拿到一句莫名其妙的 `syntax error`。
    pub using_bash: bool,
    /// 被信号杀掉时的信号号（正常退出是 `None`）。没有它的话，`exit -1`
    /// 分不清「被 OOM 杀了」和「proot 自己崩了」。
    pub signal: Option<i32>,
}

impl ExecResult {
    /// 给模型看的紧凑文本
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        if !self.stdout.is_empty() {
            s.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str("--- stderr ---\n");
            s.push_str(&self.stderr);
        }
        if self.truncated {
            s.push_str("\n[输出已截断]");
            if let Some(p) = &self.full_output_path {
                s.push_str(&format!("\n[完整输出在 {p}]"));
            }
        }
        if self.timed_out {
            // 说清「下面是半截的」，否则模型会当成完整输出，再按它推理
            s.push_str("\n[超时，已终止：下面是到被终止为止的输出，可能不全]");
        } else if let Some(sig) = self.signal {
            s.push_str(&format!("\n[被信号 {sig} 终止：{}]", signal_hint(sig)));
        }
        // 没 bash 就直说，并给出装它的命令。藏在提示词里没用 —— 模型只会在
        // 碰到 `syntax error` 时一头雾水。
        if !self.using_bash {
            s.push_str(
                "\n[沙箱里还没有 bash：这条是 /bin/sh（busybox ash）跑的，[[ ]] / 数组 / \
                 <<< / 进程替换这类 bash 语法会报 syntax error。装上：agent-setup --force]",
            );
        }
        if s.is_empty() {
            s.push_str("(无输出)");
        }
        s.push_str(&format!("\n[exit {}]", self.code));
        s
    }
}

/// 信号号只是个数，模型要的是「它为什么死了」。
fn signal_hint(sig: i32) -> &'static str {
    match sig {
        6 => "abort，多半是断言/内存越界",
        9 => "SIGKILL，多半是被 OOM 杀了",
        11 => "SIGSEGV，段错误",
        13 => "SIGPIPE，管道对面先退出了（比如 head 已读完）",
        15 => "SIGTERM，被要求退出",
        _ => "未知信号",
    }
}

/// 这次调用用的状态文件：宿主侧路径 + 沙箱内路径。
///
/// runner 在命令退出后把**命令的**退出码写进来（见 `runner_script`）。为什么不
/// 看 proot 的退出码：`--kill-on-exit` 去掉之后，只要还有一条常驻进程是 proot
/// 的 tracee，proot 就得活着，它那根 stdout 管道也就一直不 EOF ——「命令跑完了」
/// 这件事只能由命令自己报出来。
struct StatusFile {
    /// 沙箱内的路径（runner 的 `$2`）
    guest: String,
    /// 宿主侧对应文件
    host: PathBuf,
    /// 宿主侧 `<host>.pid`：runner 把自己的 pid（= 进程组号）写在这里
    pid_host: PathBuf,
}

impl StatusFile {
    /// `dir` 是**宿主侧**放状态文件的目录；`guest_dir` 是同一个目录在命令行里
    /// 该写成什么（沙箱里是 `/root/.agent-exit…` 那种 guest 路径，宿主模式下就是
    /// 宿主路径本身）。两件事分开传，是因为宿主模式**不能**往 `/` 里写东西。
    fn new(dir: &Path, guest_dir: &str) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            // 建不出来不致命：拿不到状态文件时会退回命令自己的退出码
            tracing::warn!("建不了 {}: {e}", dir.display());
        }
        prune_stale_status(dir);
        let name = uuid::Uuid::now_v7().to_string();
        Self {
            pid_host: dir.join(format!("{name}.pid")),
            host: dir.join(&name),
            guest: format!("{guest_dir}/{name}"),
        }
    }

    fn clean_up(&self) {
        let _ = std::fs::remove_file(&self.host);
        let _ = std::fs::remove_file(&self.pid_host);
    }
}

impl Drop for StatusFile {
    /// 正常结束和**被取消**（用户按停止，future 被丢掉）都得清。只靠 `run_live`
    /// 末尾调一次的话，取消那条路会把 `<uuid>.pid` 留在 `.agent-exit/` 里
    /// （`spawn` 失败、提前 `?` 返回同理）—— 真机上实测就是这么攒起来的，
    /// 而 `prune_stale_status` 要等一天才收。
    fn drop(&mut self) {
        self.clean_up();
    }
}

/// runner 写下的退出码。文件还不存在（命令没跑完）或者正好落在两次 write
/// 之间（`printf` 不是原子的）都返回 None —— 调用方下一拍再读。
fn read_status_code(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// runner 写下的进程组号。`<= 1` 一律当没拿到：`0` 是「我自己这个组」，
/// `1` 是 init —— 信号发错地方是灾难。
fn read_pgid(path: &Path) -> Option<i32> {
    let v: i32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    (v > 1).then_some(v)
}

/// 清掉上次崩溃 / 被杀留下的状态文件。它们很小，但无界。
fn prune_stale_status(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age.as_secs() > STALE_STATUS_SECS);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 给**整个进程组**发信号。
///
/// 组里包括命令自己、它的子孙、以及它没有刻意脱离的后台作业；用 `setsid` /
/// `nohup` 刻意脱离的不在里面，所以杀不杀得到由内核决定，不是对命令文本做
/// 正则判断（`&` 在 `curl 'a?x=1&y=2'`、`grep 'a & b'`、heredoc 里都是普通文本）。
fn kill_group(pgid: i32, sig: i32) {
    if pgid <= 1 {
        return;
    }
    // SAFETY: kill(2) 只读参数；失败返回 -1（ESRCH / EPERM），没有内存安全影响
    unsafe { libc::kill(-pgid, sig) };
}

/// 组里还有活着的进程吗。`kill(-pgid, 0)` 是存在性检查，`EPERM` 也算存在。
fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    let r = unsafe { libc::kill(-pgid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// 沙箱里真正执行命令的那层壳（命令行里的**一个 argv**，不是拼出来的字符串）。
///
/// 为什么不干脆 `sh -c "<script>"`：`--kill-on-exit` 去掉之后，「命令结束」和
/// 「proot 退出」不再是同一件事（proot 可能在给一条常驻进程当 tracer）。所以
/// runner 在命令退出后把**命令的**退出码写进状态文件，宿主以它为准；顺手把
/// 自己的 pid 写下来（`setsid` 让它同时是进程组号），超时 / 停止时宿主按这个
/// 组杀。
///
/// `$1` 是模型给的脚本、`$2` 是沙箱内的状态文件路径 —— 两个都是独立 argv，
/// 谁都不会被 shell 再解析一次。
fn runner_script(shell: &str, report_pgid: bool) -> String {
    // `$$` 在双引号里展开成 pid；printf 的格式串用单引号，别让外层先展开
    let pid_part = if report_pgid {
        "printf '%s' \"$$\" > \"$2.pid\"; "
    } else {
        ""
    };
    // 内层 `<shell> -c "$1"` 让脚本里的 `exit N` 只结束脚本本身，状态照样写得下；
    // 被信号打死时 `$?` 是 128+N，跟 shell 的约定一致
    format!("{pid_part}{shell} -c \"$1\"; rc=$?; printf '%s' \"$rc\" > \"$2\"; exit $rc")
}

/// 输出「最后一次读到数据」是多久以前 —— 命令退出之后靠它判断输出安静了。
#[derive(Debug)]
struct OutputTap {
    started: Instant,
    last_read_ms: AtomicU64,
}

impl OutputTap {
    fn new(started: Instant) -> Self {
        Self {
            started,
            last_read_ms: AtomicU64::new(0),
        }
    }

    /// 读端每收到一块数据就记一笔。
    fn note(&self) {
        self.last_read_ms
            .store(self.elapsed_ms(), Ordering::Relaxed);
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn quiet_for(&self, quiet: Duration) -> bool {
        self.elapsed_ms()
            .saturating_sub(self.last_read_ms.load(Ordering::Relaxed))
            >= quiet.as_millis() as u64
    }
}

/// 这次调用是怎么结束的。
enum Done {
    /// runner 报出了命令的退出码 —— 权威值
    Status(i32),
    /// 两根管道都 EOF：没有任何人还拿着写端（proot 也退了）
    PipesClosed,
    /// 用完 timeout
    TimedOut,
}

/// 一条命令的收尾责任。
///
/// 正常跑完会 `disarm()`；future 被丢掉（用户按停止 / 回合取消）时由 `Drop`
/// 杀命令组。为什么不靠 `Command::kill_on_drop`：它只 SIGKILL proot 本身，而
/// 被 proot trace 的 guest 进程是**独立的**宿主进程 —— proot 的
/// `--kill-on-exit` 是 userspace 的 `atexit` 钩子，SIGKILL 根本不跑它。原来那条
/// 路会留下一整棵孤儿树继续吃 CPU 和电。
struct CommandGuard {
    pid_file: PathBuf,
    /// 拿不到命令组号时的兜底：proot 自己的组（spawn 时 `process_group(0)` 建的）
    proot_pgid: Option<i32>,
    armed: bool,
}

impl CommandGuard {
    fn new(pid_file: PathBuf, proot_pgid: Option<i32>) -> Self {
        Self {
            pid_file,
            proot_pgid,
            armed: true,
        }
    }

    /// 杀谁：优先命令自己的组 —— proot 留着，它可能还在给别的常驻进程当
    /// tracer，杀了那些进程就失去 rootfs 翻译、变成坏的。
    fn target(&self) -> Option<i32> {
        read_pgid(&self.pid_file).or(self.proot_pgid)
    }

    /// 超时：先 SIGTERM（`apk add` / 编译器至少有机会收尾，留个 SIGKILL 打断的
    /// 半成品更麻烦），限时还不走就 SIGKILL。
    async fn terminate(&self) {
        let Some(pgid) = self.target() else { return };
        kill_group(pgid, libc::SIGTERM);
        let steps = KILL_GRACE.as_millis() / KILL_POLL.as_millis().max(1);
        for _ in 0..steps {
            tokio::time::sleep(KILL_POLL).await;
            if !group_alive(pgid) {
                return;
            }
        }
        kill_group(pgid, libc::SIGKILL);
    }

    /// 停止：直接 SIGKILL —— 用户在按停止，不等（而且 `Drop` 里也不该睡）。
    fn kill_now(&self) {
        if let Some(pgid) = self.target() {
            kill_group(pgid, libc::SIGKILL);
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill_now();
        }
    }
}

/// 退出码 ≥128 按 shell 的约定解出信号号（`sh -c` 里被信号打死的命令 `$?` 就是
/// 128+N）。有这个才能给出「被 OOM 杀了」这类提示 —— 代价是 `exit 137` 这种
/// 写法会被当成 SIGKILL，本来就分不出来。
fn signal_of_exit(code: i32) -> Option<i32> {
    let sig = code - 128;
    (1..=31).contains(&sig).then_some(sig)
}

// ---------------------------------------------------------------------------
// `agent ps` / `agent kill`：沙箱里现在还有什么在跑
// ---------------------------------------------------------------------------
//
// 为什么需要：刻意脱离（`setsid` / `nohup`）的常驻进程**能活过工具调用**，
// 但它们的父进程早就变成 init 了 —— 模型事后只能靠 `ps` 找 pid，而安卓上
// /proc 按 uid 过滤、有些版本干脆看不见自己的后代（实测机型能看到，但这个
// 不能假设）。能可靠知道的只有核心：它记得每次运行起的是哪个 proot，而
// guest 进程的 `TracerPid` 就是那个 proot（见 `procs()`）。

/// `/proc/<pid>` 里我们要的那几个字段。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawProc {
    pid: i32,
    ppid: i32,
    /// 进程组号（`stat` 第 5 个字段）
    pgid: i32,
    /// `R` / `S` / `D` / `T` / `Z` …
    state: char,
    /// 谁在 ptrace 它（0 = 没人）。guest 进程的 tracer 就是 proot。
    tracer: i32,
    /// 截断过的命令行
    cmdline: String,
}

/// 扫一个 /proc（测试里喂假的）。
///
/// **只看得见自己 uid 的进程** —— 安卓的 /proc 按 uid 过滤，所以拿到的天然
/// 就是 App 自己起的那些。单条读失败就跳过（进程随时可能退）。
fn scan_procs_in(proc_dir: &Path) -> Vec<RawProc> {
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let dir = e.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            continue;
        };
        let Some((state, ppid, pgid)) = parse_stat(&stat) else {
            continue;
        };
        let tracer = std::fs::read_to_string(dir.join("status"))
            .ok()
            .and_then(|s| parse_tracer(&s))
            .unwrap_or(0);
        let cmdline = std::fs::read(dir.join("cmdline"))
            .map(|b| render_cmdline(&b))
            .unwrap_or_default();
        out.push(RawProc {
            pid,
            ppid,
            pgid,
            state,
            tracer,
            cmdline,
        });
    }
    out
}

fn scan_procs() -> Vec<RawProc> {
    scan_procs_in(Path::new("/proc"))
}

/// `/proc/<pid>/stat` 的形状：`pid (comm) state ppid pgrp session …`。
///
/// `comm` 里可能有空格和括号（`(WebView 子进程)`），所以**从最后一个 `)` 之后**
/// 再切 —— 逐 token 切第一个 `)` 会在这种名字上错位。
fn parse_stat(stat: &str) -> Option<(char, i32, i32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut it = rest.split_whitespace();
    let state = it.next()?.chars().next()?;
    let ppid = it.next()?.parse().ok()?;
    let pgid = it.next()?.parse().ok()?;
    Some((state, ppid, pgid))
}

/// `/proc/<pid>/status` 里的 `TracerPid:`（没有就 0）。
fn parse_tracer(status: &str) -> Option<i32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("TracerPid:"))
        .map(|v| v.trim().parse().unwrap_or(0))
}

/// `/proc/<pid>/cmdline` 是 NUL 分隔的 argv。截到 ~120 字（安全地按字符切）。
fn render_cmdline(raw: &[u8]) -> String {
    let joined = raw
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    clip_chars(&joined, 120)
}

/// 这条进程是不是一个 proot（我们要找的孤儿就是它）。
///
/// 比的是**文件名**而不是整条路径：`cmdline` 是按空格拼起来的，而路径里
/// 可能有空格（用户的家目录），比路径会漏；文件名在两边都稳定
/// （安卓上是 `libproot.so`，桌面上是 `proot`）。
fn is_proot(p: &RawProc, proot_name: &str) -> bool {
    p.cmdline
        .split_whitespace()
        .next()
        .map(|a| a.rsplit('/').next().unwrap_or(a) == proot_name)
        .unwrap_or(false)
}

/// 按**字符**截断（`String::truncate` 撞上多字节边界会 panic）。
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// 多行命令压成一行（登记表里只留一条给模型认领的线索）。
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 进程还活着吗。僵尸也算「活着」（它占着 pid），调用方按需滤掉。
fn pid_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    let r = unsafe { libc::kill(pid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 我们起过的一条命令 —— `procs()` 靠它把 /proc 里的进程认到命令上。
#[derive(Debug, Clone)]
struct RunInfo {
    /// 命令原文（截断过）
    command: String,
    /// 起于（epoch 毫秒）
    started_ms: u64,
    /// proot 自己的 pid：**它名下的 tracee 就是这条命令的进程**
    proot_pid: i32,
    /// runner 写 pid 的宿主路径（读出来是这条命令的进程组）
    pid_file: PathBuf,
    /// 命令结束了吗。结束了还有活着的进程 = 刻意脱离的常驻进程。
    finished: bool,
}

/// 命令登记表。
#[derive(Default)]
struct RunRegistry {
    runs: BTreeMap<i32, RunInfo>,
}

impl RunRegistry {
    /// 最多留这么多条。一个长命 App 里命令是无限的，而这个表只在 `procs()`
    /// 被调到的时候才清。
    const KEEP: usize = 64;

    fn add(&mut self, run: RunInfo) {
        self.runs.insert(run.proot_pid, run);
        while self.runs.len() > Self::KEEP {
            // 先扔「已结束且 proot 已经不在了」的；实在没有就扔最老的
            let victim = self
                .runs
                .iter()
                .find(|(_, r)| r.finished && !pid_alive(r.proot_pid))
                .map(|(p, _)| *p)
                .or_else(|| self.runs.keys().next().copied());
            match victim {
                Some(p) => {
                    self.runs.remove(&p);
                }
                None => break,
            }
        }
    }

    /// 命令结束（不代表它名下的进程都死了 —— 刻意脱离的还在）。
    fn finish(&mut self, proot_pid: i32) {
        if let Some(r) = self.runs.get_mut(&proot_pid) {
            r.finished = true;
        }
    }

    /// 清掉 proot 已经没了的登记 —— proot 退出就说明它名下不可能还有进程。
    ///
    /// 这也顺手挡住了 pid 复用：proot 死后的登记不会再被拿去归因。
    fn prune(&mut self) {
        self.runs.retain(|pid, _| pid_alive(*pid));
    }
}

/// `agent ps` 里的一条**进程**（命令结束后还活着的那些，就是可以 kill 的）。
#[derive(Debug, Clone, Serialize)]
pub struct ProcEntry {
    pub pid: i32,
    pub pgid: i32,
    pub state: String,
    pub age_secs: u64,
    /// 命令已经结束、它还活着 —— 也就是刻意脱离的常驻进程
    pub leftover: bool,
    /// 它属于哪条命令（原文截断）
    pub command: String,
    /// 该进程自己的命令行
    pub cmdline: String,
}

/// `agent ps` 里的一条**正在跑的命令**（按命令聚合，不逐个列进程）。
#[derive(Debug, Clone, Serialize)]
pub struct RunningRun {
    /// 这条命令的进程组（拿不到就是 proot 的 pid）
    pub pgid: i32,
    pub age_secs: u64,
    /// 它名下现在有几个进程
    pub procs: usize,
    pub command: String,
}

/// `agent ps` 的全貌。
#[derive(Debug, Default, Serialize)]
pub struct ProcsReport {
    /// 宿主模式（没有 proot 登记表）。`to_text` 先看它。
    #[serde(default)]
    pub host: bool,
    /// /proc 里看得见多少个进程：用来区分「没有」和「看不见」
    pub visible: usize,
    pub leftovers: Vec<ProcEntry>,
    pub running: Vec<RunningRun>,
    /// 看得见、但不属于任何我们登记过的运行。典型是**上一次 App 进程被杀**
    /// （不是 force-stop，那个会把整个 uid 都清掉）时留下的 proot 树：新进程的
    /// 登记表里当然没有它们，而没有列出来就等于永远没人能收。
    pub orphans: Vec<ProcEntry>,
}

/// 孤儿列表最多列这么多（模型看的是一段文字，不是日志）。
const MAX_ORPHANS: usize = 20;

impl ProcsReport {
    /// 给模型看的排版。**在核心侧排好**，CLI 直接打 —— 免得两处各写一份
    /// （`search.rs` 的 `format_hits` 就是这么搬过来的）。
    pub fn to_text(&self) -> String {
        if self.host {
            let p = crate::platform::Platform::host();
            return format!(
                "宿主模式：没有 proot 登记表，认不出「刻意脱离的常驻进程」。\
                 直接用这台机器上的 {} 看，{} 停。",
                p.proc_tools(),
                p.kill_tool()
            );
        }
        if self.visible == 0 {
            return "看不见任何进程：这台机器的 /proc 被限制了（hidepid），沙箱里的 `ps` \
                    也一样看不到。这种机器上只能让命令自己把 pid 记下来\
                    （`setsid … & echo $!`，或写进一个 pid 文件）。"
                .into();
        }
        if self.leftovers.is_empty() && self.running.is_empty() && self.orphans.is_empty() {
            return "沙箱里现在没有别的东西在跑。".into();
        }
        let mut s = String::new();
        if !self.leftovers.is_empty() {
            s.push_str(&format!(
                "命令结束后还活着的常驻进程（{} 个）—— `agent kill <pid>` 停掉：\n",
                self.leftovers.len()
            ));
            for e in &self.leftovers {
                s.push_str(&fmt_proc_line(e));
            }
        }
        if !self.running.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("正在跑的命令（{} 条）：\n", self.running.len()));
            for r in &self.running {
                s.push_str(&format!(
                    "  pgid {}  {}个进程  {}前\n      ← {}\n",
                    r.pgid,
                    r.procs,
                    fmt_age(r.age_secs),
                    r.command
                ));
            }
        }
        if !self.orphans.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!(
                "不属于任何已知命令的进程（{} 个）—— 通常是**上一次 App 进程被杀**时\
                 留下的（那次的登记表已经不在了），`agent kill <pid>` 可以停掉：\n",
                self.orphans.len()
            ));
            for e in self.orphans.iter().take(MAX_ORPHANS) {
                s.push_str(&fmt_proc_line(e));
            }
            if self.orphans.len() > MAX_ORPHANS {
                s.push_str(&format!(
                    "  … 还有 {} 个没列\n",
                    self.orphans.len() - MAX_ORPHANS
                ));
            }
        }
        s.push_str("\n（pid / pgid 都是宿主 pid，跟沙箱里 `ps` 看到的一致）");
        s
    }
}

/// 一条进程在文字里的样子（`leftovers` / `orphans` 共用）。
fn fmt_proc_line(e: &ProcEntry) -> String {
    let age = if e.age_secs > 0 {
        format!("  {}前", fmt_age(e.age_secs))
    } else {
        String::new()
    };
    let cmd = if e.cmdline.is_empty() {
        "（读不到命令行）"
    } else {
        &e.cmdline
    };
    let from = if e.command.is_empty() {
        String::new()
    } else {
        format!("      ← 来自：{}\n", e.command)
    };
    format!(
        "  pid {}  pgid {}  {}{}\n      {}\n{from}",
        e.pid, e.pgid, e.state, age, cmd
    )
}

fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} 秒"),
        60..=3599 => format!("{} 分钟", secs / 60),
        _ => format!("{} 小时", secs / 3600),
    }
}

pub struct LinuxSandbox {
    cfg: SandboxConfig,
    /// 核心 HTTP 服务的端口。`agent` 命令靠它找到「自己」（见 `AGENT_CLI`）。
    /// 0 = 还不知道（核心还没 bind），这时不注入环境变量。
    agent_port: AtomicU16,
    /// 核心接口的凭据（`AGENT_TOKEN`）。见 `state::Auth`。
    agent_token: std::sync::RwLock<Option<String>>,
    /// 起过的命令（`agent ps` 用，见 `RunRegistry`）。
    runs: Mutex<RunRegistry>,
}

/// 沙箱里的 `agent` 命令。
///
/// 为什么要有它：模型平时只看得见自己的上下文和历史，看不见「自己现在什么状态」。
/// 这个命令让它能查自己（状态 / 下一次会收到的完整上下文 / 会话），
/// 也能操作自己所在的那台机器（手机上是坐标点击、按键、启动应用；
/// 宿主模式下那些不适用，但它照样能查状态和联网搜索）。
///
/// 只用 busybox 的 wget 或者 curl，没有别的依赖 —— Alpine minirootfs 里
/// 自带 wget；宿主模式（桌面原生）上进的一般是 curl。
/// 端口由 App 通过 `AGENT_PORT` 注入（`guest_env`）。
const AGENT_CLI: &str = r##"#!/bin/sh
# agent —— 在沙箱里和**自己**说话。
#
# 这个 App 的核心是个只听 127.0.0.1 的 HTTP 服务，这个脚本只是它的薄包装：
# 让你能看见自己（状态、下一次会收到的上下文、会话），也能操作自己所处的
# 那台机器（手机上是这台手机，桌面上就是这台电脑）。输出基本都是原始 JSON。
#
#   agent help               看全部
#   agent state              我现在的状态
#   agent request            下一次会发给我自己的完整请求体（提示词 + 工具表 + 历史）
#   agent tools              我有哪些工具
#   agent sessions           会话列表
#   agent session            当前这条会话的消息（不会切走你正在看的那条）
#   agent doctor             各部件体检
#   agent setup [--force|--list|包…]   装/看常用工具（rg fd jq git python3…）
#   agent command <json>     操作这台手机（只有手机上有），例如 '{"type":"key","name":"back"}'
#   agent open <uri>         打开它。手机上：http(s) / App 深链（weixin://、
#                            androidamap://）/ intent:// / tel: …（沙箱里的
#                            $BROWSER / xdg-open 最后也走到它）；电脑上交给本机
#                            的 open / xdg-open
#   agent browser <动作> [参数…]  驱动浏览器（手机上是 WebView，电脑上是无头 Chrome）：
#       navigate <url> | get_backbone [深度] | click <选择器> | type <选择器> <文本>
#       get_text [选择器] | get_readable | scroll [up|down] [像素]
#       screenshot [true=整页] | press Enter | find_elements <选择器> | user_agent mobile|desktop
#       back | wait_for_dom_stable [秒] | js '<代码>'
#       new_tab [url] | list_tabs | switch_tab <编号> | close_tab [编号]
#     输出是核心给的原始 JSON。一条命令里可以连做多步（这是它比工具调用强的地方）：
#       agent browser navigate <url> && agent browser get_backbone 3 && agent browser click '#x'
#     截图路径在 screenshot 的返回里，要 read 它才看得见。
#     其他参数直接给 JSON：agent browser '{"action":"click","x":100,"y":200}'
#   agent search <检索意图> [条数]   联网搜索（Exa），拿标题 + 链接 + 片段
#     输出是**排好版的文字**（不是 JSON）：抬头一行「N 条 · exa-mcp」，
#     下面每条三行（标题 / 网址 / 片段）。
#   agent ps                 沙箱里还有什么在跑（包括**命令结束后仍活着的常驻进程**；电脑上用系统的 ps）
#   agent kill <pid>         停掉一个（pid 从 agent ps 来）
#   agent get <path>         直接 GET 自己的接口（逃生口）
#   agent post <path> <json> 直接 POST
#
# 端口从 $AGENT_PORT 来。看不到它就说明不是在 agent 的 bash 里。
set -eu

BASE="${AGENT_BASE:-http://127.0.0.1:${AGENT_PORT:-}}"
if [ -z "$BASE" ] || [ "$BASE" = "http://127.0.0.1:" ]; then
  echo "AGENT_PORT 没设 —— 不是在 agent 的 bash 里跑？" >&2
  exit 1
fi
# 核心的接口要凭据（手机上别的 App 也能连 127.0.0.1）。App 通过环境变量给。
AUTH="x-agent-token: ${AGENT_TOKEN:-}"

# 「改自己身份」的接口不从这儿走：别让网页里的一段话把 agent 自己改了。
guard() {
  case "$1" in
    /api/llm*|/api/debug/system-prompt*)
      echo "拒绝：$1 会改掉我自己的身份（模型 / key / 提示词）。" >&2
      echo "那两件事留给人做：在宿主机上跑 scripts/agentctl.py" >&2
      exit 3
      ;;
  esac
}

get() {
  guard "$1"
  if command -v curl >/dev/null 2>&1; then
    curl -sS -m 120 -H "$AUTH" "$BASE$1" || { echo "GET $1 失败" >&2; exit 1; }
  else
    wget -q -T 120 -O - --header "$AUTH" "$BASE$1" || { echo "GET $1 失败" >&2; exit 1; }
  fi
}

post() {
  guard "$1"
  if command -v curl >/dev/null 2>&1; then
    curl -sS -m 120 -H "$AUTH" -H 'content-type: application/json' -d "$2" "$BASE$1" \
      || { echo "POST $1 失败" >&2; exit 1; }
  else
    wget -q -T 120 -O - --header "$AUTH" --header 'content-type: application/json' --post-data "$2" "$BASE$1" \
      || { echo "POST $1 失败" >&2; exit 1; }
  fi
}

# 把一行 JSON 摊开，方便人看（没有 jq，纯 shell）
fmt() { tr ',' '\n' | tr -d '{}"' | sed 's/^[[:space:]]*//; /^[[:space:]]*$/d'; }

# 参数里的反斜杠和引号要先转义，否则拼出来的 JSON 是坏的（browser / search 都用）
esc() { printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'; }

cmd="${1:-help}"
case "$cmd" in
  state)    get /api/debug/state | fmt ;;
  request)  get /api/debug/request | fmt ;;
  tools)    get /api/debug/tools | tr ',' '\n' | sed -n 's/.*"name":"\([a-z_]*\)".*/\1/p' ;;
  sessions) get /api/sessions | fmt ;;
  session)  get /api/session | fmt ;;
  # 什么在跑：核心那边记着每条命令是哪个 proot，而 guest 进程的 TracerPid
  # 就是它 —— 于是连刻意 setsid / nohup 脱离的常驻进程也能认出来（它们的
  # 父进程早就是 init 了，靠 ps 自己认不出来）。
  # `self` 参数把自己的进程组报上去，免得每次都在「正在跑」里看到自己。
  ps|procs)
    self_pgid=$(awk '{print $5}' /proc/self/stat 2>/dev/null || echo 0)
    get "/api/procs?format=text&self=$self_pgid"
    ;;
  kill)
    shift || true
    case "${1:-}" in
      ''|*[!0-9]*)
        echo "用法：agent kill <pid>（pid 从 `agent ps` 来）" >&2
        exit 2
        ;;
    esac
    # 先好声好气（让它收尾），几秒不走再来硬的
    kill -TERM "$1" 2>/dev/null || true
    for _ in 1 2 3; do
      kill -0 "$1" 2>/dev/null || { echo "已停止 $1"; exit 0; }
      sleep 1
    done
    kill -KILL "$1" 2>/dev/null || true
    if kill -0 "$1" 2>/dev/null; then
      echo "停不掉 $1（不是这个 uid 的进程？）" >&2
      exit 1
    fi
    echo "已强杀 $1"
    ;;
  doctor)
    get /api/health | fmt
    echo '--- 跑一句 shell ---'
    post /api/linux '{"command":"echo ok"}'
    ;;
  setup)
    shift || true
    if [ -x /usr/local/bin/agent-setup ]; then
      exec /usr/local/bin/agent-setup "$@"
    fi
    # 宿主模式：没有沙箱，也没有 apk
    echo "宿主模式：没有沙箱，工具就是这台机器上的。装东西直接用系统的包管理器。" >&2
    exit 0
    ;;
  command)
    if [ $# -lt 2 ]; then
      cat >&2 <<'EOT'
用法：agent command '{"type":"key","name":"back"}'
可用的 type：
  {"type":"key","name":"back|home|recents|notifications"}
  {"type":"tap_xy","x":540,"y":1200}
  {"type":"swipe","x1":540,"y1":1800,"x2":540,"y2":600,"ms":300}
  {"type":"launch","package":"com.android.settings"}  只到 App 首页
  {"type":"open","url":"androidamap://navi?…"}         按 URI 到具体页面/动作
EOT
      exit 2
    fi
    post /api/command "{\"cmd\":$2}"
    ;;
  open)
    shift || true
    uri=""
    for a in "$@"; do
      # 任何 `scheme:` 开头都接（tel: 这种没有 //；--flags 以 - 开头不会被误认）
      case "$a" in [a-zA-Z]*:*) uri="$a"; break ;; esac
    done
    if [ -z "$uri" ]; then
      # 不报错：这个命令也是 $BROWSER / xdg-open 的落点，
      # 而它们常被链在脚本里（set -e 下报错会把调用方炸掉）
      echo "agent open: 参数里没有 URI，跳过: $*" >&2
      exit 0
    fi
    # 宿主模式（桌面）：设备通道是 mock，发过去只会拿回一句假的「已打开」——
    # 直接交给这台电脑自己的打开器（macOS `open`，Linux `xdg-open`）
    if [ "${AGENT_HOST:-}" = 1 ]; then
      if command -v open >/dev/null 2>&1 && [ "$(uname)" = Darwin ]; then exec open "$uri"; fi
      if command -v xdg-open >/dev/null 2>&1; then exec xdg-open "$uri"; fi
      echo "agent open: 这台电脑上没有 open / xdg-open：$uri" >&2
      exit 1
    fi
    out=$(post /api/command "{\"cmd\":{\"type\":\"open\",\"url\":\"$(esc "$uri")\"}}")
    case "$out" in
      *'"ok":true'*)
        echo "已让手机打开: $uri"
        ;;
      *)
        detail=$(printf '%s' "$out" | sed -n 's/.*"detail":"\([^"]*\)".*/\1/p')
        echo "没能打开（${detail:-没有响应}）：$uri" >&2
        ;;
    esac
    ;;
  get)  [ $# -ge 2 ] || { echo "用法：agent get /api/…" >&2; exit 2; }; get "$2" ;;
  post) [ $# -ge 3 ] || { echo "用法：agent post /api/… '{…}'" >&2; exit 2; }; post "$2" "$3" ;;
  browser)
    shift || true
    act="${1:-}"; shift || true
    usage() {
      cat >&2 <<'EOT'
用法：agent browser <动作> [参数…]
  navigate <url>                   打开网页
  get_backbone [深度]              读页面结构（含可点的 selector）
  click <选择器>                    点击（也可以用 JSON 给 x/y）
  type <选择器> <文本>              往输入框里填
  get_text [选择器]                 读整页/某元素的文本（带导航页脚；文章用 get_readable）
  get_readable                     抽正文（文章页用这个）
  scroll [up|down] [像素]           滚动
  screenshot [true]                截图（true = 整页）；路径在返回里，用 read 看
  press <键名>                      按键：Enter（会提交表单）/Tab/Escape/方向键…
  find_elements <选择器>            列出匹配到的元素（个数 + 坐标，坐标可直接给 click）
  new_tab [url]                    开一只新标签（不给 url 就是空白页）
  list_tabs                        标签列表（含当前是哪一只）
  switch_tab <编号>                 切到某只标签
  close_tab [编号]                  关掉某只（不给编号 = 关当前这只）
  user_agent mobile|desktop        切 UA + 视口（会重载页面）
  back                             后退
  wait_for_dom_stable [秒]         等动态内容安定（默认 10）
  js '<代码>'                      在页面里跑一段 JS（可以 await）

一条命令里可以连做多步（省轮次）：
  agent browser navigate <url> && agent browser get_backbone 3 && agent browser click '#x'
参数给不全 / 给多了都会报错，不会静默忽略 —— 少给一个 url 就等于开空白页。
EOT
    }
    # 纯数字参数（标签编号 / 超时秒数）：非数字直接报错，别拼出一段坏 JSON 给核心
    int_arg() { case "${1:-}" in ''|*[!0-9]*) echo "agent browser $2：要一个数字，给的是「${1:-}」" >&2; exit 2 ;; esac; }
    case "$act" in
      help|-h|--help) usage; exit 0 ;;
      "")             usage; exit 2 ;;
    esac

    # 每个动作最多收几个参数，多给就报错。**静默扔掉参数**是这套 CLI 最坑人的错法
    # （踩过：`agent browser new_tab URL` 把 URL 吃了、开出一只空白标签，模型以为
    # 页面开了，对着白屏截了三张图 —— 见 docs/08-browser.md）。
    max=1
    case "$act" in
      get_readable|back|list_tabs|tabs) max=0 ;;
      type|scroll)                      max=2 ;;
    esac
    [ $# -le "$max" ] || { echo "agent browser ${act}：最多 $max 个参数，给了 $# 个" >&2; usage; exit 2; }

    # 参数长得复杂（要 x/y、要多个字段）就直接给 JSON：agent browser '{"action":…}'
    case "$act" in
      navigate)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"navigate\",\"url\":\"$(esc "$1")\"}" ;;
      get_backbone)
        if [ -n "${1:-}" ]; then int_arg "$1" get_backbone; body="{\"action\":\"get_backbone\",\"max_depth\":$1}"; else body='{"action":"get_backbone"}'; fi ;;
      click)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"click\",\"selector\":\"$(esc "$1")\"}" ;;
      type)
        [ -n "${1:-}" ] && [ -n "${2:-}" ] || { usage; exit 2; }
        body="{\"action\":\"type\",\"selector\":\"$(esc "$1")\",\"text\":\"$(esc "$2")\"}" ;;
      get_text)
        if [ -n "${1:-}" ]; then body="{\"action\":\"get_text\",\"selector\":\"$(esc "$1")\"}"; else body='{"action":"get_text"}'; fi ;;
      scroll)
        if [ -n "${2:-}" ]; then int_arg "$2" scroll; body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\",\"amount\":$2}"; else body="{\"action\":\"scroll\",\"direction\":\"${1:-down}\"}"; fi ;;
      screenshot)
        case "${1:-}" in
          ''|false) body='{"action":"screenshot"}' ;;
          true)     body='{"action":"screenshot","full_page":true}' ;;
          *)        echo "用法：agent browser screenshot [true]（true = 整页）" >&2; exit 2 ;;
        esac ;;
      back)         body='{"action":"back"}' ;;
      get_readable) body='{"action":"get_readable"}' ;;
      press)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"press\",\"key\":\"$(esc "$1")\"}" ;;
      find_elements|find)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"find_elements\",\"selector\":\"$(esc "$1")\"}" ;;
      user_agent|ua)
        case "${1:-}" in mobile|desktop) ;; *) echo "用法：agent browser user_agent mobile|desktop" >&2; exit 2 ;; esac
        body="{\"action\":\"set_user_agent\",\"user_agent\":\"$(esc "$1")\"}" ;;
      wait_for_dom_stable)
        if [ -n "${1:-}" ]; then int_arg "$1" wait_for_dom_stable; body="{\"action\":\"wait_for_dom_stable\",\"timeout\":$1}"; else body='{"action":"wait_for_dom_stable"}'; fi ;;
      js)
        [ -n "${1:-}" ] || { usage; exit 2; }
        body="{\"action\":\"js\",\"script\":\"$(esc "$1")\"}" ;;
      new_tab)
        if [ -n "${1:-}" ]; then body="{\"action\":\"new_tab\",\"url\":\"$(esc "$1")\"}"; else body='{"action":"new_tab"}'; fi ;;
      list_tabs|tabs) body='{"action":"list_tabs"}' ;;
      switch_tab)
        int_arg "${1:-}" switch_tab
        body="{\"action\":\"switch_tab\",\"tab\":$1}" ;;
      close_tab)
        if [ -n "${1:-}" ]; then int_arg "$1" close_tab; body="{\"action\":\"close_tab\",\"tab\":$1}"; else body='{"action":"close_tab"}'; fi ;;
      # 第一眼像 JSON 就原样发（复杂参数走这条）
      \{*) [ $# -eq 0 ] || { echo "JSON 形式后面不该再跟参数：$*" >&2; exit 2; }; body="$act" ;;
      # 光杆动作名：核心自己会认（认不出它会回「未知动作」，比在这边猜好）
      *)   body="{\"action\":\"$(esc "$act")\"}" ;;
    esac
    post /api/browser "$body"
    ;;
  search)
    shift || true
    if [ -z "${1:-}" ]; then
      echo "用法：agent search <检索意图> [条数]" >&2
      exit 2
    fi
    q=$(esc "$1")
    # format=text：让核心直接回**排好版的文字**，不是 JSON。
    # 打 JSON 的话模型和界面上看到的是一串带 \n 转义的字符串（“网址后面怎么都有个
    # \n”），CLI 还得靠 jq 才能把它拆开。抬头那行（`3 条 · exa-mcp`）是唯一丢掉的
    # 元信息，排查走哪条路时够用；要原始 JSON 就直接 agent post /api/websearch。
    if [ -n "${2:-}" ]; then body="{\"query\":\"$q\",\"num_results\":$2,\"format\":\"text\"}"; else body="{\"query\":\"$q\",\"format\":\"text\"}"; fi
    post /api/websearch "$body"
    ;;
  help|-h|--help)
    # 从第 2 行打到「端口从…」那行为止（那行是表头的结尾）。用哨兵行而不是
    # 写死行号：以前是 '2,31p'，表头一改 help 就悄悄少印或多印几行。
    sed -n 's/^# \{0,1\}//p' "$0" | sed -n '2,/端口从/p'
    ;;
  *)
    # ⚠️ 变量后面紧跟中文标点时**必须**写 ${var}：沙箱里是 busybox ash，它不认
    # UTF-8，会把全角字节当成变量名的一部分 —— 于是那个变量变成了一个不存在的
    # 名字，set -u 下直接「unbound variable」退出 1，提示语永远打不出来。
    echo "不认识的子命令：${cmd}（agent help）" >&2
    exit 2
    ;;
esac
"##;

/// `$BROWSER` / `xdg-open` 的落点：转给 `agent open`（逻辑都在那边）。
///
/// 为什么留这个小壳而不是直接把 `BROWSER` 指到 `agent`：`$BROWSER` 和那几个
/// 名字都得是**一个可执行文件**，写不了 `agent open`（不同调用方拆参数的方式
/// 还不一样；python 的 webbrowser 会拿 `%s` 去替换）。
const AGENT_OPEN: &str = r##"#!/bin/sh
# agent-open —— 见 `agent open`。也会以 xdg-open / sensible-browser /
# www-browser / x-www-browser / gnome-open / kde-open 的身份被调用。
exec /usr/local/bin/agent open "$@"
"##;

/// 这六个名字都是“开浏览器”的常见入口（OpenMinis 垫的是同一组），全转发给
/// `agent-open`。
const BROWSER_SHIM: &str =
    "#!/bin/sh\n# 见 agent-open；这些名字都是“开浏览器”的常见入口。\nexec /usr/local/bin/agent-open \"$@\"\n";

/// 预装的工具。**改这里就够了** —— `agent-setup` 脚本是从它生成的。
///
/// 为什么预装：Alpine minirootfs 里只有 busybox（没有 python、没有 curl、
/// 没有 jq、`grep` 不认 `-P`），而模型干活几乎每次都碰它们。装一次，
/// 后面每个回合就不用自己 `apk add`，也不会因为工具的小脾气卡住。
///
/// 增删请顺手想一下体积：手机上这是要下载的（约 75MB），别把 ffmpeg 塞进来。
/// 清单里最大的一块是 `github-cli`（单它装完就 44 MiB）。
pub const TOOLSET: &[&str] = &[
    "bash",            // 提示词里让模型用 bash，沙箱里原来只有 ash
    "curl",            // 比 busybox wget 好用得多
    "ca-certificates", // 没有它 curl/git 走 https 会验证失败
    "grep",            // GNU grep：busybox 的不认 -P
    "findutils",       // GNU find
    "jq",              // JSON（模型处理 JSON 太频繁了）
    "git",
    "github-cli", // gh：不用手掰 API / 开网页就能看 issue/PR、克隆、下 release
    "diffutils",  // diff/cmp：改文件前后对比、自己生成 patch
    "patch",      // 应用 diff（模型写/接补丁时用得上）
    "ripgrep",    // rg
    "fd",
    "tree",
    "unzip", // busybox 没有
    "python3",
    "openssl", // 证书/小工具用得上
];

/// `agent-setup`：装上面那套（幂等）。
/// `agent-setup`：装上面那套（幂等）。
///
/// 注意：**不能拿 `apk add` 的退出码当准**。proot 下 busybox / ca-certificates
/// 的 trigger 常常返回非零（实测：装一个全新的小包也会报 “1 error; N packages”，
/// 但包确实装上了）。所以脚本以**那些命令找不找得到**为准。
const AGENT_SETUP_TEMPLATE: &str = r##"#!/bin/sh
# agent-setup —— 把干活要用的工具装上。
#
# Alpine minirootfs 里只有 busybox（没有 python、没有 curl、没有 jq，
# grep 也不认 -P），而模型几乎每个任务都要碰到它们。App 启动后会自动跑一次
# 这个脚本；也可以随时手动跑（幂等）。
#
#   agent-setup            装标准那一套（装过就秒退）
#   agent-setup --force    不管标记，重跑一遍
#   agent-setup --list     只看看装了哪些
#   agent-setup <包…>      在标准那套之外再加几个
set -eu

MARK=/etc/agent-tools
VERSION=__VERSION__
BASE="__PACKAGES__"
# 装完必须能找到这些命令（ca-certificates 没有命令，单独查证书包）
CHECK="bash curl grep find jq git gh rg fd tree unzip python3 openssl diff patch"
CA=/etc/ssl/certs/ca-certificates.crt

missing() {
  out=""
  for b in $CHECK; do
    command -v "$b" >/dev/null 2>&1 || out="$out $b"
  done
  [ -s "$CA" ] || out="$out ca-certificates"
  echo "$out"
}

if [ "${1:-}" = "--list" ]; then
  for b in $CHECK; do
    printf '%-12s %s\n' "$b" "$(command -v "$b" 2>/dev/null || echo '❌ 没有')"
  done
  printf '%-12s %s\n' "ca-bundle" "$([ -s "$CA" ] && echo "$CA" || echo '❌ 没有')"
  exit 0
fi

force=""
if [ "${1:-}" = "--force" ]; then force=1; shift; fi

if [ -z "$force" ] && [ -f "$MARK" ] && [ "$(cat "$MARK")" = "$VERSION" ] \
   && [ -z "$(missing)" ]; then
  echo "工具就绪（$MARK=$VERSION）。重装：agent-setup --force"
  exit 0
fi

echo "装工具：$BASE $*"
echo "（首次要下几十 MB，之后不再装）"
# apk 的退出码在这个沙箱里不可信：proot 下 busybox/ca-certificates 的 trigger
# 常报 “1 error”，但包确实装上了。所以这里不看它，后面用「命令找不找得到」判断。
apk add --no-cache $BASE "$@" || true

left=$(missing)
if [ -n "$left" ]; then
  echo "还缺：$left" >&2
  echo "多半是网络/DNS。查一下：curl -sI https://dl-cdn.alpinelinux.org" >&2
  echo "然后重跑：agent-setup --force" >&2
  exit 1
fi

mkdir -p /etc
echo "$VERSION" > "$MARK"
echo "好了（apk 那句 1 error 是 proot 下 trigger 的事，以这份清单为准）："
for b in $CHECK; do
  printf '  %-10s %s\n' "$b" "$(command -v "$b" 2>/dev/null || echo '?')"
done
"##;

/// 预装脚本的内容。清单变了这里就变，`write_if_changed` 会重写文件。
fn setup_script() -> String {
    AGENT_SETUP_TEMPLATE
        .replace("__VERSION__", &TOOLSET.len().to_string())
        .replace("__PACKAGES__", &TOOLSET.join(" "))
}

/// 只在内容变了才写，避免每次启动都碰文件（和 resolv.conf 一样的做法）。
///
/// `exec` = 写成 0755（可执行）。
fn write_if_changed(path: &Path, body: &str, exec: bool) -> anyhow::Result<()> {
    let same = std::fs::read_to_string(path)
        .map(|old| old == body)
        .unwrap_or(false);
    if same {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, body)?;
    #[cfg(unix)]
    if exec {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = exec;
    Ok(())
}

/// 宿主模式要吗（`AGENT_LINUX_HOST=1`）。
fn host_mode_requested() -> bool {
    match std::env::var("AGENT_LINUX_HOST") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// 宿主模式下我们自己的数据目录（`agent` 命令、状态文件都在这儿）。
///
/// 故意**不用** `~/.local/bin` / `~/.local/bin` 那一类用户的目录：这是我们自己的
/// 东西，不能往用户的 PATH 配置里塞。
pub(crate) fn host_data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("android-agent"));
    base.join("android-agent")
}

/// 宿主模式下 `agent` 命令装在哪。
///
/// 落点是 `$XDG_DATA_HOME/android-agent/bin`（默认
/// `~/.local/share/android-agent/bin`）—— 只有我们自己发出去的命令的 PATH
/// 前缀里有它，不动用户的 shell 配置。
fn host_cli_dir() -> PathBuf {
    host_data_dir().join("bin")
}

/// 宿主模式用哪个 bash：**PATH 里的第一个**，没有再退回 `/bin/bash`。
///
/// 不能写死 `/bin/bash`：macOS 上那是 2007 年的 bash 3.2（GPLv3 之前的最后
/// 一版，苹果就停在那儿了）—— 没有关联数组、`${x,,}`、`mapfile`、`|&`，
/// 模型写的现代 bash 会报一串莫名其妙的错。装了 Homebrew bash 的人 PATH 里
/// `/opt/homebrew/bin` 排在 `/bin` 前面，按 PATH 找正好拿到新的；Linux 上
/// PATH 里的 bash 一般就是 `/usr/bin/bash`（= `/bin/bash`），行为不变。
fn host_bash() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .filter(|d| d.is_absolute())
                .map(|d| d.join("bash"))
                .find(|p| p.is_file())
        })
        .or_else(|| Some(PathBuf::from("/bin/bash")).filter(|p| p.is_file()))
}

/// macOS 没有 `setsid(1)`（那是 util-linux 的），而提示词和工具描述里「放一个
/// 能活过这次调用的后台服务」的写法就是 `setsid <命令> &` —— 少了它模型只能
/// 退到 `nohup … &`，可 nohup **不换进程组**，超时 / 停止时会被连组一起杀
/// （见 `CommandGuard`），跟「刻意脱离」的语义对不上。
///
/// 所以宿主模式在我们自己的 bin 目录里垫一个：系统自带的 perl 调
/// `POSIX::setsid()`。跟 util-linux 一样，调用方自己是组长时先 fork 一次
/// （组长调 setsid 会 EPERM），父进程直接返回。只在 PATH 里**没有**真
/// setsid 的机器上写（`prepare_host`），Linux 上一行都不碰。
const SETSID_SHIM: &str = r##"#!/bin/sh
# setsid —— macOS 没有 util-linux 的 setsid(1)，这是 android-agent 垫的一个。
# 用法同 util-linux：setsid <命令> [参数…]（让命令跑在一个新会话 / 进程组里）
[ $# -gt 0 ] || { echo "用法：setsid <命令> [参数…]" >&2; exit 2; }
exec /usr/bin/perl -MPOSIX -e '
  if (getpgrp() == $$) { my $p = fork(); die "setsid: fork: $!
" unless defined $p; exit 0 if $p; }
  POSIX::setsid() or die "setsid: $!
";
  exec { $ARGV[0] } @ARGV or die "setsid: $ARGV[0]: $!
";
' -- "$@"
"##;

/// PATH 里（我们自己的 bin 目录以外）有没有真的 `setsid`。
fn host_has_setsid(ours: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|d| d != ours)
                .any(|d| d.join("setsid").is_file())
        })
        .unwrap_or(false)
}

impl LinuxSandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self {
            cfg,
            agent_port: AtomicU16::new(0),
            agent_token: std::sync::RwLock::new(None),
            runs: Mutex::new(RunRegistry::default()),
        }
    }

    /// 登记一条刚起的命令（`agent ps` 靠它把进程认到命令上）。
    fn register_run(&self, proot_pid: i32, script: &str, pid_file: PathBuf) {
        if let Ok(mut r) = self.runs.lock() {
            r.add(RunInfo {
                command: clip_chars(&one_line(script), 160),
                started_ms: now_ms(),
                proot_pid,
                pid_file,
                finished: false,
            });
        }
    }

    /// 命令跑完了。**不代表它名下没进程了** —— 刻意脱离的还在（见 `procs`）。
    fn finish_run(&self, proot_pid: i32) {
        if let Ok(mut r) = self.runs.lock() {
            r.finish(proot_pid);
        }
    }

    /// 沙箱里现在还有什么在跑（`agent ps`）。
    ///
    /// 归因靠 `TracerPid`：guest 进程的 tracer 就是那条命令的 proot，所以
    /// 「tracer == 某个我们起的 proot」= 这个进程是我们的。这是**唯一**能认出
    /// 刻意脱离的常驻进程的办法 —— 它们早就被 reparent 到 init 了，按父子关系
    /// 无论如何找不回来。
    ///
    /// `self_pgid` = 调用方自己所在的进程组（`agent ps` 从 `/proc/self/stat`
    /// 取的），用来把它自己那条命令从「正在跑」里去掉 —— 否则每次 `agent ps`
    /// 都会看到自己。
    pub fn procs(&self, self_pgid: Option<i32>) -> ProcsReport {
        // 宿主模式没有 proot，那套归因（`TracerPid == 我们的 proot`）无从谈起，
        // 而**不能**就这么扫一遍 /proc：那会把这台机器上所有 ppid==1 的进程
        // （用户桌面、系统服务）都列进「可以 kill」里。直接用系统自带的 ps。
        if self.cfg.host {
            return ProcsReport {
                host: true,
                ..ProcsReport::default()
            };
        }
        self.procs_with(scan_procs(), self_pgid)
    }

    /// `procs()` 的实现（扫描结果从外面给，好测）。
    fn procs_with(&self, all: Vec<RawProc>, self_pgid: Option<i32>) -> ProcsReport {
        let Ok(mut runs) = self.runs.lock() else {
            return ProcsReport::default();
        };
        runs.prune();
        let now = now_ms();
        let mut leftovers = Vec::new();
        let mut running = Vec::new();
        for run in runs.runs.values() {
            // 命令自己那棵树的进程（正在跑的）
            let mine: Vec<&RawProc> = all
                .iter()
                .filter(|p| p.tracer == run.proot_pid && p.state != 'Z')
                .collect();
            if mine.is_empty() {
                continue;
            }
            // 进程组：runner 写下的那个（拿不到就退回 proot 的 pid）
            let pgid = read_pgid(&run.pid_file).unwrap_or(run.proot_pid);
            let age = now.saturating_sub(run.started_ms) / 1000;
            if run.finished {
                for p in mine {
                    leftovers.push(ProcEntry {
                        pid: p.pid,
                        pgid: p.pgid,
                        state: p.state.to_string(),
                        age_secs: age,
                        leftover: true,
                        command: run.command.clone(),
                        cmdline: p.cmdline.clone(),
                    });
                }
            } else if Some(pgid) != self_pgid {
                running.push(RunningRun {
                    pgid,
                    age_secs: age,
                    procs: mine.len(),
                    command: run.command.clone(),
                });
            }
        }
        leftovers.sort_by_key(|e| e.pid);
        running.sort_by_key(|r| std::cmp::Reverse(r.age_secs));

        // 看得见、但我们不认识的那些。
        //
        // 判据卡两道，少一道都会出事：
        //
        // 1. **父进程已经没了**（`ppid == 1`）—— 那正是「App 进程被杀、它的孩子
        //    被 reparent 到 init」的样子。不卡这一条的话，任何同一个 uid、不归我们
        //    管的进程都会被列进来（单元测试里 cargo / rustc 就是）。
        // 2. **得真的是 proot**（名字对得上）—— 光看 `ppid == 1` 在**桌面上**是灾难：
        //    systemd 的服务、用户会话的 daemon 全都是 ppid 1，实测一次扫出 40 个
        //    （journald / NetworkManager / sshd…），把它们列成「`agent kill` 可以
        //    停掉」比漏掉糟得多。安卓上 /proc 按 uid 过滤，这个坑看不见 —— 所以
        //    这一版以前只在真机上验过。孤儿 proot 名下的 tracee 也要收（它们自己的
        //    ppid 还指着 guest 里的父进程）。
        let known: std::collections::BTreeSet<i32> = runs.runs.keys().copied().collect();
        let proot_name = self
            .cfg
            .proot
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "proot".to_string());
        let orphan_proots: std::collections::BTreeSet<i32> = all
            .iter()
            .filter(|p| {
                p.ppid == 1 && p.tracer == 0 && !known.contains(&p.pid) && is_proot(p, &proot_name)
            })
            .map(|p| p.pid)
            .collect();
        let me = std::process::id() as i32;
        let mut orphans = Vec::new();
        for p in all.iter() {
            // 僵尸不列；核心/App 自己那个进程无论如何不能出现在「可以 kill」的列表里
            if p.state == 'Z' || p.pid == me {
                continue;
            }
            if known.contains(&p.pid) || known.contains(&p.tracer) {
                continue;
            }
            // 只有孤儿 proot 自己、以及它名下的 tracee
            if !orphan_proots.contains(&p.pid) && !orphan_proots.contains(&p.tracer) {
                continue;
            }
            orphans.push(ProcEntry {
                pid: p.pid,
                pgid: p.pgid,
                state: p.state.to_string(),
                age_secs: 0,
                // 不是「刚才那条命令留下的」——它是「上一次 App 生命」留下的
                leftover: false,
                command: String::new(),
                cmdline: p.cmdline.clone(),
            });
        }
        orphans.sort_by_key(|e| e.pid);
        ProcsReport {
            host: false,
            visible: all.len(),
            leftovers,
            running,
            orphans,
        }
    }

    /// 宿主模式：核心直接在这台 Linux 上跑，没有沙箱。
    ///
    /// `workspace` 是命令的 cwd，同时也以 `/workspace` 这个名字出现在
    /// 路径解析里（`resolve_path` 的挂载表）。`rootfs` 给 `/` —— 沙箱里的
    /// 绝对路径于是就是本机路径，`read` / `edit` 一路不用改。
    pub fn host(workspace: PathBuf) -> Self {
        Self::new(SandboxConfig {
            proot: PathBuf::new(),
            rootfs: PathBuf::from("/"),
            host: true,
            binds: Vec::new(),
            tmp: None,
            loader: None,
            loader32: None,
            dns: Vec::new(),
            tz_offset_minutes: None,
            env: BTreeMap::new(),
            workspace: Some(workspace),
            shared: None,
        })
    }

    /// 是不是宿主模式（桌面原生）。系统提示词、预装工具都看它。
    pub fn is_host(&self) -> bool {
        self.cfg.host
    }

    /// 核心 bind 成功后告诉沙箱自己的端口 —— 沙箱里的 `agent` 命令要靠它。
    pub fn set_agent_port(&self, port: u16) {
        self.agent_port.store(port, Ordering::Relaxed);
    }

    /// 沙箱里的 `agent` 命令调核心接口要带的凭据。
    pub fn set_agent_token(&self, token: Option<String>) {
        if let Ok(mut g) = self.agent_token.write() {
            *g = token;
        }
    }

    /// rootfs 齐了吗（安卓上解压是后台做的，之前只是个空壳）。
    pub fn is_ready(&self) -> bool {
        if self.cfg.host {
            return true;
        }
        is_alpine_rootfs(&self.cfg.rootfs)
    }

    /// 常用工具装过了吗。
    ///
    /// 标记文件里存的是**清单版本**（工具数）。只判文件在不在的话，以后往
    /// `TOOLSET` 里加工具，老设备永远装不上 —— 它会一直以为自己装过了。
    pub fn is_provisioned(&self) -> bool {
        // 宿主模式：工具就是这台机器上本来就有的那些，没有 `apk add` 这一步
        if self.cfg.host {
            return true;
        }
        std::fs::read_to_string(self.cfg.rootfs.join("etc/agent-tools"))
            .map(|s| s.trim() == TOOLSET.len().to_string())
            .unwrap_or(false)
    }

    /// 把常用工具装上（`agent-setup`）。幂等，装过就秒退。
    ///
    /// 超时给得很大：手机上首次要下 ~75MB。
    pub async fn provision(&self) -> anyhow::Result<ExecResult> {
        self.run("/usr/local/bin/agent-setup", Duration::from_secs(900))
            .await
    }

    /// 从环境变量构造（桌面开发用）。
    ///
    /// 两条路：
    /// - `AGENT_LINUX_HOST=1` → **宿主模式**，直接在宿主机上跑（cwd 看
    ///   `AGENT_HOST_WORKSPACE`，默认当前目录），不需要 rootfs / proot。
    /// - 否则 `AGENT_LINUX_ROOTFS` 指向 Alpine rootfs；`AGENT_LINUX_PROOT`
    ///   可选，默认从 PATH 找 `proot`。
    pub fn from_env() -> Option<Self> {
        if host_mode_requested() {
            let ws = std::env::var("AGENT_HOST_WORKSPACE")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("/"));
            let sb = Self::host(ws);
            tracing::info!(
                "宿主模式：直接在 {} 上跑（没有沙箱）",
                sb.host_cwd().display()
            );
            if let Err(e) = sb.prepare_rootfs() {
                tracing::warn!("宿主模式准备失败: {e:#}");
            }
            return Some(sb);
        }

        let rootfs = std::env::var("AGENT_LINUX_ROOTFS").ok()?;
        let rootfs = PathBuf::from(rootfs);

        // 注意：**不能用 `rootfs/bin/sh` 判断**。Alpine 里它是指向
        // 绝对路径 `/bin/busybox` 的符号链接，`Path::exists()` 会拿
        // 宿主机根的 `/bin/busybox` 去解析 —— 在宿主上没有，返回 false。
        // 所以查真实存在的普通文件。
        if !is_alpine_rootfs(&rootfs) {
            tracing::warn!(
                "AGENT_LINUX_ROOTFS={} 看起来不是 Alpine rootfs（找不到 bin/busybox）",
                rootfs.display()
            );
            return None;
        }

        let proot = std::env::var("AGENT_LINUX_PROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("proot"));

        let sb = Self::new(SandboxConfig {
            proot,
            rootfs,
            host: false,
            binds: Vec::new(),
            tmp: None,
            loader: None,
            loader32: None,
            dns: Vec::new(),
            tz_offset_minutes: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        if let Err(e) = sb.prepare_rootfs() {
            tracing::warn!("准备 rootfs 失败: {e:#}");
        }
        Some(sb)
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.cfg
    }

    pub fn rootfs(&self) -> &Path {
        &self.cfg.rootfs
    }

    /// 启动前把 rootfs 里缺的基础文件补上。幂等。
    ///
    /// 目前做两件事：
    /// 1. `/etc/resolv.conf` —— 不写的话 `apk add` 直接挂
    /// 2. `/root`、`/tmp`、挂载点 `/workspace` 和 `/sdcard`
    ///    —— `-w /workspace` 需要它存在；挂载点事先建好比让 proot 虚拟出来稳
    ///
    /// 宿主模式另走一条（见 `prepare_host`）：**绝不往 `/` 里写东西**。
    pub fn prepare_rootfs(&self) -> anyhow::Result<()> {
        if self.cfg.host {
            return self.prepare_host();
        }
        let rootfs = &self.cfg.rootfs;

        for d in ["root", "tmp", "var/tmp", "workspace", "sdcard"] {
            std::fs::create_dir_all(rootfs.join(d))?;
        }

        // 沙箱里的 `agent` 命令：让模型能查看 / 操作自己
        write_if_changed(&rootfs.join("usr/local/bin/agent"), AGENT_CLI, true)?;
        // 开浏览器：$BROWSER / xdg-open 都指向它（见 AGENT_OPEN）
        write_if_changed(&rootfs.join("usr/local/bin/agent-open"), AGENT_OPEN, true)?;
        for alias in [
            "xdg-open",
            "sensible-browser",
            "www-browser",
            "x-www-browser",
            "gnome-open",
            "kde-open",
        ] {
            write_if_changed(
                &rootfs.join("usr/local/bin").join(alias),
                BROWSER_SHIM,
                true,
            )?;
        }
        // 预装工具的那个脚本（App 启动后会自动跑一次，也可以手动跑）
        write_if_changed(
            &rootfs.join("usr/local/bin/agent-setup"),
            &setup_script(),
            true,
        )?;

        let resolv = rootfs.join("etc/resolv.conf");
        let servers = self.resolved_dns();
        if !servers.is_empty() {
            let body: String = servers
                .iter()
                .map(|s| format!("nameserver {s}\n"))
                .collect();
            // 只在内容变了才写，避免每次启动都碰文件
            let same = std::fs::read_to_string(&resolv)
                .map(|old| old == body)
                .unwrap_or(false);
            if !same {
                std::fs::create_dir_all(rootfs.join("etc"))?;
                std::fs::write(&resolv, body)?;
                tracing::info!("写入 resolv.conf: {servers:?}");
            }
        } else {
            tracing::warn!("没有可用的 DNS 服务器 —— 沙箱里的 apk/curl 会解析不了域名");
        }

        Ok(())
    }

    /// 宿主模式要准备的东西：一个能跟核心说话的 `agent` 命令。
    ///
    /// 落点是 `$XDG_DATA_HOME/android-agent/bin`（默认
    /// `~/.local/share/android-agent/bin`）—— **不进用户的 PATH 配置**，
    /// 只在我们这个进程发出去的命令的 PATH 前缀里（见 `host_env`）。
    /// 宿主模式下**不装** `agent-open` / `xdg-open` 那套壳：桌面上的
    /// `xdg-open` 本来就该开用户自己的浏览器，不能把它抢到手机那条路上。
    fn prepare_host(&self) -> anyhow::Result<()> {
        // cwd 得真的在：`AGENT_HOST_WORKSPACE` 指到一个还不存在的目录时，
        // 每条命令都会在 spawn 那一步失败（模型看到的只是一句 ENOENT）
        let ws = self.host_cwd();
        if !ws.is_dir() {
            std::fs::create_dir_all(&ws)?;
            tracing::info!("宿主模式：建了工作目录 {}", ws.display());
        }
        let dir = host_cli_dir();
        write_if_changed(&dir.join("agent"), AGENT_CLI, true)?;
        // 没有真 setsid 的机器（macOS）垫一个；有的话把以前垫的拿掉，别挡着真的
        let shim = dir.join("setsid");
        if host_has_setsid(&dir) {
            let _ = std::fs::remove_file(&shim);
        } else if Path::new("/usr/bin/perl").is_file() {
            write_if_changed(&shim, SETSID_SHIM, true)?;
        } else {
            tracing::warn!("这台机器没有 setsid 也没有 /usr/bin/perl：`setsid … &` 会是 not found");
        }
        tracing::debug!("宿主模式 agent 命令: {}", dir.join("agent").display());
        Ok(())
    }

    /// 挂到 `/workspace` 的那个宿主目录（配了才有；宿主模式下就是命令的 cwd）。
    pub fn workspace(&self) -> Option<&Path> {
        self.cfg.workspace.as_deref()
    }

    /// 宿主模式下的 cwd：挂到 `/workspace` 的那个宿主目录。
    fn host_cwd(&self) -> PathBuf {
        self.cfg
            .workspace
            .clone()
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// 状态文件（`runner_script` 的 `$2`）放哪：返回 `(宿主目录, 命令行里的写法)`。
    ///
    /// 沙箱里是 rootfs 的 `<EXIT_DIR>`（guest 路径就是 `/<EXIT_DIR>`）；
    /// 宿主模式放我们自己的数据目录 —— **绝不往 `/` 里写**（那是用户的根目录）。
    fn exit_dir(&self) -> (PathBuf, String) {
        if self.cfg.host {
            let dir = host_data_dir().join("exit");
            let guest = dir.display().to_string();
            (dir, guest)
        } else {
            (self.cfg.rootfs.join(EXIT_DIR), format!("/{EXIT_DIR}"))
        }
    }

    /// 浏览器截图落哪儿（宿主目录 + 历史里存的那个路径前缀）。
    ///
    /// 沙箱里和 `BrowserController.SHOT_GUEST_DIR` 一致（`/root/.browser`）；
    /// 宿主模式下 `/root` 是**用户的根目录**（写不进去，也不该写），
    /// 换成我们自己的数据目录 —— 前缀就是宿主路径本身，`read` 直接解得开。
    pub fn shot_dir(&self) -> crate::gc::ShotDir {
        if self.cfg.host {
            let host = host_data_dir().join("browser");
            let guest = host.display().to_string();
            crate::gc::ShotDir { host, guest }
        } else {
            crate::gc::ShotDir {
                host: self.cfg.rootfs.join(crate::gc::SHOT_DIR),
                guest: format!("/{}", crate::gc::SHOT_DIR),
            }
        }
    }

    /// 我们自己的副产物落在哪（清扫用，见 `gc::SideDirs`）。
    pub fn side_dirs(&self) -> crate::gc::SideDirs {
        let spills = if self.cfg.host {
            // 和 `spill_captured` 一致：跟着 TMPDIR 走
            std::env::temp_dir()
        } else {
            self.cfg.rootfs.join("tmp")
        };
        crate::gc::SideDirs {
            shots: Some(self.shot_dir()),
            spills: Some(spills),
        }
    }

    /// 宿主模式的环境变量：**继承宿主环境**（PATH 就是用户自己那一份），
    /// 只覆盖几个会碍事的，再加上 `agent` 命令要的那两三个。
    ///
    /// 和沙箱那条路（`guest_env`：干净白名单）正好相反 —— 在用户的机器上，
    /// node / cargo / python / 代理变量都在他的环境里，重造一份根本用不了。
    fn host_env(&self) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = std::env::vars().collect();

        // 非交互：分页器会挂住（没人按键），颜色会把输出塞满转义符
        env.insert("TERM".into(), "dumb".into());
        env.insert("PAGER".into(), "cat".into());
        env.insert("GIT_PAGER".into(), "cat".into());
        env.insert("NO_COLOR".into(), "1".into());

        // 我们自己的 `agent` 命令排在最前面
        let cli = host_cli_dir();
        if cli.is_dir() {
            let old = env.get("PATH").cloned().unwrap_or_default();
            env.insert("PATH".into(), format!("{}:{old}", cli.display()));
        }

        // `agent` 命令据此分岔（比如 `agent open` 交给本机的打开器）
        env.insert("AGENT_HOST".into(), "1".into());

        let port = self.agent_port.load(Ordering::Relaxed);
        if port != 0 {
            env.insert("AGENT_PORT".into(), port.to_string());
            env.insert("AGENT_BASE".into(), format!("http://127.0.0.1:{port}"));
        }
        if let Some(t) = self.agent_token.read().ok().and_then(|g| g.clone()) {
            env.insert("AGENT_TOKEN".into(), t);
        }

        env.into_iter().collect()
    }

    fn resolved_dns(&self) -> Vec<String> {
        if !self.cfg.dns.is_empty() {
            return self.cfg.dns.clone();
        }
        // 环境变量覆盖
        if let Ok(v) = std::env::var("AGENT_LINUX_DNS") {
            let list: Vec<String> = v
                .split([',', ' '])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
        // 桌面兜底：抄宿主机的
        if let Ok(text) = std::fs::read_to_string("/etc/resolv.conf") {
            let list: Vec<String> = text
                .lines()
                .filter_map(|l| l.trim().strip_prefix("nameserver "))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && !s.starts_with("127."))
                .take(3)
                .collect();
            if !list.is_empty() {
                return list;
            }
        }
        vec!["1.1.1.1".into(), "8.8.8.8".into()]
    }

    /// 沙箱里有没有真 bash（没装就只能退回 ash，见 `command_line`）。
    pub fn has_bash(&self) -> bool {
        if self.cfg.host {
            return host_bash().is_some();
        }
        self.cfg.rootfs.join("bin/bash").is_file()
    }

    /// 沙箱里 `setsid` 的路径（rootfs 里没有就 None）。
    ///
    /// 它给每条命令开一个**自己的进程组** —— 超时 / 停止时只杀这个组，proot
    /// 留着（见 `CommandGuard`）。Alpine 3.21 是 usrmerge 之后的
    /// `/usr/bin/setsid`，老一点的在 `/bin`。
    ///
    /// **判存在不能用 `is_file()`。** Alpine 里 `/usr/bin/setsid` 是指向
    /// `/bin/busybox` 的**绝对**符号链接，在宿主机上解析会指向宿主机的
    /// `/bin/busybox`（不存在）—— `is_file()` 永远是 false。真机上踩过：命令行里
    /// 没有 setsid，超时那一杀就落到 proot 自己的组上（`process_group(0)` 那个
    /// 兜底不能当主路径用），proot 被 SIGKILL，它 trace 的常驻进程全成了没翻译
    /// 的孤儿。`is_alpine_rootfs()` 上有同样的教训。
    fn setsid_guest(&self) -> Option<&'static str> {
        SETSID_PATHS.into_iter().find(|p| {
            std::fs::symlink_metadata(self.cfg.rootfs.join(p.trim_start_matches('/'))).is_ok()
        })
    }

    /// 构造完整命令行。抽出来单独测试，不需要真的跑 proot。
    ///
    /// ```text
    /// <proot> -0 --link2symlink -r <rootfs> -b /dev -b /proc -b /sys
    ///         [-b <host>:<linux> ...] -w /workspace
    ///         [/usr/bin/setsid] /bin/sh -c <runner> sh <script> <status>
    /// ```
    /// `status` 是沙箱内的状态文件路径（宿主侧在 `<rootfs>/<EXIT_DIR>/`）。
    pub fn command_line(&self, script: &str, status: &str) -> Vec<String> {
        // 宿主模式：命令行里没有 proot，**但进程模型是同一套** —— 照样跑
        // runner、照样写状态文件、照样把「命令自己的进程组」当靶子
        // （见 `CommandGuard`）。差异只有：状态文件落在我们自己的数据目录里。
        if self.cfg.host {
            let shell = host_bash()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "/bin/sh".to_string());
            // 宿主模式不需要 guest 的 setsid：spawn 时那个 `process_group(0)`
            // 已经把 runner 变成组长，所以 `$$` 就是组号（`report_pgid = true`）。
            let runner = runner_script(&shell, true);
            return vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                runner,
                "sh".to_string(),
                script.to_string(),
                status.to_string(),
            ];
        }

        let mut cmd: Vec<String> = Vec::with_capacity(24);

        macro_rules! arg {
            ($($x:expr),+ $(,)?) => {{ $( cmd.push($x.to_string()); )+ }};
        }

        arg!(self.cfg.proot.display().to_string());

        // 假装 uid 0。Alpine 的 apk 装包会因为不是 root 而失败。
        arg!("-0");

        // --link2symlink：Android 的 /data 拒绝 app uid 跨目录 link()，
        // 而 apk 装 busybox-applet 包（binutils/gcc 依赖）全靠硬链接
        // ar/ld/nm/strip -> busybox。不加则每次装包都 EPERM。
        // 桌面上无害，所以两边用同一套参数，避免行为分叉。
        arg!("--link2symlink");

        arg!("-r", self.cfg.rootfs.display().to_string());

        for fs in ["/dev", "/proc", "/sys"] {
            arg!("-b", fs);
        }

        for (host, guest) in &self.cfg.binds {
            arg!("-b", format!("{host}:{guest}"));
        }
        if let Some(h) = &self.cfg.shared {
            arg!("-b", format!("{}:{SHARED}", h.display()));
        }
        if let Some(h) = &self.cfg.workspace {
            arg!("-b", format!("{}:{WORKSPACE}", h.display()));
        }

        arg!("-w", WORKSPACE);

        // 装过 bash 就用 bash：模型写的命令常带 bash 语法（`[[ ]]`、数组、
        // 进程替换、`<<<`），拿 ash 跑会报 "syntax error"。首次预装之前
        // （或者手动清了 rootfs）没有 bash，退回 /bin/sh（这时结果里会
        // 提醒模型去装，见 `ExecResult::to_text`）。
        let shell = if self.has_bash() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };

        // 没有 setsid 就退回「不分组」：那时 pid 文件不能信（`$$` 不是组号），
        // 超时 / 停止只能拿 proot 自己的组当靶子（见 `CommandGuard::target`）。
        let setsid = self.setsid_guest();
        let runner = runner_script(shell, setsid.is_some());
        if let Some(s) = setsid {
            arg!(s);
        }
        // runner 本身跑在 /bin/sh 里（busybox 一定在，`$$` / printf / `$?` 都是
        // POSIX）；模型那段脚本由 runner 交给上面选出的 shell 执行。
        arg!("/bin/sh", "-c", runner, "sh", script, status);

        cmd
    }

    /// 沙箱内的环境变量。
    ///
    /// 注意这里**不用 `putIfAbsent` 式的"继承宿主环境"** —— 宿主环境
    /// 带进来反而有害（PATH 就是典型）。显式给一份干净的白名单。
    fn guest_env(&self) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();

        env.insert("PATH".into(), GUEST_PATH.into());
        env.insert("HOME".into(), "/root".into());
        // ash 非登录 shell 也会读 ENV 指向的文件，这样 profile 里的
        // 别名/HISTFILE 才生效（我们用 /bin/sh -c，属于非登录 shell）
        env.insert("ENV".into(), "/etc/profile".into());
        env.insert("CHARSET".into(), "UTF-8".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        // 非交互，别输出 ANSI
        env.insert("TERM".into(), "dumb".into());
        // 沙箱里没有浏览器，但手机上有：把 BROWSER 指到 agent-open，它让手机
        // 去打开（`agent open` → 设备通道）。`gh auth login --web` / python
        // webbrowser / xdg-open 都会走这里。强制覆盖（不 putIfAbsent）：用户
        // dotfile 里的 BROWSER= 不该抢走它。
        env.insert("BROWSER".into(), "/usr/local/bin/agent-open".into());
        // 分页器会**挂住**（非交互环境没人按键），一次工具调用就废了
        env.insert("PAGER".into(), "cat".into());
        env.insert("GIT_PAGER".into(), "cat".into());
        env.insert("NO_COLOR".into(), "1".into());
        env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
        env.insert("GOMAXPROCS".into(), "2".into()); // 手机上限
                                                     // proot 的 link2symlink 会留 .l2s.* 哨兵文件，uv 默认 hardlink
                                                     // 模式会二次 link 它们并拿到 EPERM
        env.insert("UV_LINK_MODE".into(), "symlink".into());
        // TZ 必须是 POSIX 格式（符号与 UTC 偏移相反）：musl 的 TZ 解析器
        // 认不出 "GMT+8" 这种带 +/- 的缩写
        env.insert("TZ".into(), posix_tz(self.cfg.tz_offset_minutes));

        // 沙箱里的 `agent` 命令靠它找到核心（App 在 bind 成功后注入）
        let port = self.agent_port.load(Ordering::Relaxed);
        if port != 0 {
            env.insert("AGENT_PORT".into(), port.to_string());
            env.insert("AGENT_BASE".into(), format!("http://127.0.0.1:{port}"));
        }
        if let Some(t) = self.agent_token.read().ok().and_then(|g| g.clone()) {
            env.insert("AGENT_TOKEN".into(), t);
        }

        // 给 guest 程序用的临时目录
        if let Some(tmp) = &self.cfg.tmp {
            env.insert("TMPDIR".into(), tmp.display().to_string());
            // proot 自己用的（宿主路径）—— 名字和 TMPDIR 不一样，别搞混
            env.insert("PROOT_TMP_DIR".into(), tmp.display().to_string());
        }
        // 安卓必需：指向 nativeLibraryDir 里的 loader。
        // 不设的话 proot 会走内嵌 loader 解压路径，撞上 W^X。
        if let Some(l) = &self.cfg.loader {
            env.insert("PROOT_LOADER".into(), l.display().to_string());
        }
        if let Some(l) = &self.cfg.loader32 {
            env.insert("PROOT_LOADER_32".into(), l.display().to_string());
        }
        for (k, v) in &self.cfg.env {
            env.insert(k.clone(), v.clone());
        }

        env.into_iter().collect()
    }

    /// 在沙箱里跑一段 shell 脚本。
    pub async fn run(&self, script: &str, timeout: Duration) -> anyhow::Result<ExecResult> {
        self.run_live(script, timeout, None).await
    }

    /// 同 `run`，但把输出**边跑边**交给 `live`（stdout/stderr 按到达顺序混在一起）。
    /// 给前端实时显示用；最终结果照样完整返回。
    pub async fn run_live(
        &self,
        script: &str,
        timeout: Duration,
        live: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> anyhow::Result<ExecResult> {
        let started = Instant::now();
        let (exit_dir, exit_guest) = self.exit_dir();
        let status = StatusFile::new(&exit_dir, &exit_guest);
        let argv = self.command_line(script, &status.guest);
        let using_bash = self.has_bash();
        let (bin, args) = argv.split_first().expect("command_line 至少有一个元素");

        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args);
        if self.cfg.host {
            // cwd 是 workspace，环境是**宿主的**（见 `host_env`）
            cmd.current_dir(self.host_cwd());
            for (k, v) in self.host_env() {
                cmd.env(k, v);
            }
        } else {
            cmd.env_clear();
            for (k, v) in self.guest_env() {
                cmd.env(k, v);
            }
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // 自己的进程组：
        //  - 沙箱里的进程不再落在核心自己的组里（给核心发组信号不会顺手打进沙箱）；
        //  - 拿不到命令组号时（rootfs 里没有 setsid、runner 还没写下 pid 就被杀），
        //    `-<proot_pid>` 是个安全的兜底杀法 —— 宿主模式下这个 pid 就是 runner
        //    （没有 proot），同一个位置、同一种兜底。
        // 这里**不能**用 `kill_on_drop`：它只杀 proot，而 guest 进程是独立的
        // 宿主进程，会变成孤儿（见 `CommandGuard`）。
        cmd.process_group(0);

        if let Some(tmp) = &self.cfg.tmp {
            let _ = std::fs::create_dir_all(tmp);
        }

        let mut child = cmd.spawn()?;
        let child_pgid = child.id().map(|p| p as i32);
        // 登记：`agent ps` 把 /proc 里的进程认到命令上，就靠这条记录。
        // **宿主模式不登记** —— 那套归因靠 `TracerPid == 这条命令的 proot`，
        // 而宿主模式根本没有 proot（`procs()` 也因此直接回一句「用 ps/pgrep」）。
        if let Some(pid) = child_pgid {
            if !self.cfg.host {
                self.register_run(pid, script, status.pid_host.clone());
            }
        }

        // 退出状态 + 收尸走后台任务：proot 可能在给一条常驻进程当 tracer，活
        // 多久不知道，**不能在这里等它**（原来的 `child.wait()` 就是完成信号，
        // 现在不是了）。但也不能把 `Child` 丢了不管 —— 它是我们的子进程，
        // 没人 `wait` 就会变僵尸。
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let (exit_tx, mut exit_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = exit_tx.send(child.wait().await);
        });

        // 边跑边读，内存里只留**首尾两端**（见 `Capture`）。
        //
        // 原来是 wait_with_output() —— 整段输出全攒在内存里。`yes`、
        // `cat /dev/urandom | base64` 这类命令 60 秒能吐出几个 GB，
        // 手机上直接 OOM 带走整个 App。
        // 边跑边读；结果塞进共享的 `Capture`（为什么要共享见 `drain`）。
        let tap = Arc::new(OutputTap::new(started));
        let out_cap = Arc::new(Mutex::new(Capture::default()));
        let err_cap = Arc::new(Mutex::new(Capture::default()));
        let out_task = tokio::spawn(Capture::read_into(
            stdout_pipe,
            live.clone(),
            out_cap.clone(),
            Some(tap.clone()),
        ));
        let err_task = tokio::spawn(Capture::read_into(
            stderr_pipe,
            live,
            err_cap.clone(),
            Some(tap.clone()),
        ));

        // future 被丢掉（用户按停止 / 回合取消）时靠它收场
        let mut guard = CommandGuard::new(status.pid_host.clone(), child_pgid);

        // 等这次调用结束。两条独立的路，先到先算：
        //  - runner 写下的状态文件 = 命令真的退出了（proot 可能还在）
        //  - 两根管道都 EOF = 没人再拿着写端（proot 也退了）
        // 都没有就等 timeout。
        let done = wait_for_done(started, timeout, &status.host, &out_task, &err_task).await;

        let timed_out = matches!(done, Done::TimedOut);
        if timed_out {
            guard.terminate().await;
        } else if matches!(done, Done::Status(_)) {
            // 命令已经退出，但管道里可能还有没读完的字节（裸 `&` 起的子孙还在写）
            wait_for_quiet(&tap, &out_task, &err_task).await;
        }

        // 管道可能还被沙箱里某条**刻意脱离**的进程占着（`setsid … &`）—— 别为
        // 它无限等，但也**别把已经读到的丢了**（见 `drain`）。
        let grace = if timed_out {
            DRAIN_AFTER_KILL
        } else {
            Duration::ZERO
        };
        let stdout = drain(out_task, &out_cap, grace).await;
        let stderr = drain(err_task, &err_cap, grace).await;
        let (stdout_text, t1) = stdout.render();
        let (stderr_text, t2) = stderr.render();
        // 中间被扔掉（输出超过内存上限）也算截断
        let dropped = stdout.dropped_bytes + stderr.dropped_bytes;
        let truncated = t1 || t2 || dropped > 0;

        // 退出码以 runner 写下的为准（那是**命令**的退出码）。管道先 EOF 的
        // 那条路上可能还差一拍没看到文件，再读一次。
        let status_code = match done {
            Done::Status(c) => Some(c),
            _ => read_status_code(&status.host),
        };
        // 拿不到状态文件（runner 没起来、被我们杀了）才退回 proot 的退出码；
        // 这时它多半马上就退，但也可能还在当 tracer，所以是有界的等。
        //
        // 超时不在此列：code / signal 都是我们自己定的，用不着 proot 的退出码 ——
        // 而它那会儿很可能正给一条刻意脱离的常驻进程当 tracer，白等 REAP_GRACE
        // （真机上实测：3 秒的超时变成 5.5 秒返回）。
        let proot_exit = if timed_out || status_code.is_some() {
            None
        } else {
            let early = exit_rx.try_recv().ok().and_then(|r| r.ok());
            match early {
                Some(st) => Some(st),
                None => tokio::time::timeout(REAP_GRACE, exit_rx)
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .and_then(|r| r.ok()),
            }
        };
        guard.disarm();
        if let Some(pid) = child_pgid {
            if !self.cfg.host {
                self.finish_run(pid);
            }
        }

        let code = if timed_out {
            TIMEOUT_EXIT
        } else {
            status_code
                .or_else(|| proot_exit.and_then(|s| s.code()))
                .unwrap_or(-1)
        };

        // 截断了就把捕获到的输出落进沙箱的 /tmp，并告诉模型路径 ——
        // 它可以再用 grep/sed 去翻，不用把几十 MB 塞回上下文
        let full_output_path = if truncated {
            self.spill_captured(&stdout, &stderr)
        } else {
            None
        };

        // 信号：超时那次是我们自己杀的，报了反而混淆。其余情况下优先按 shell
        // 的 128+N 约定解（状态文件里就是那个值），实在没有才看 proot 自己
        // 是怎么死的。
        use std::os::unix::process::ExitStatusExt;
        let signal = if timed_out {
            None
        } else {
            signal_of_exit(code).or_else(|| proot_exit.and_then(|s| s.signal()))
        };

        Ok(ExecResult {
            code,
            signal,
            stdout: stdout_text,
            stderr: stderr_text,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            // 超时也把已经产出的输出带回去 —— 原来直接丢空，模型只知道
            // "超时了"，看不到卡在哪一步
            timed_out,
            full_output_path,
            using_bash,
        })
    }

    /// 把捕获到的输出写进 rootfs 的 `/tmp`，返回沙箱内的路径。
    ///
    /// 中间被扔掉过的话（输出超过内存上限）在文件里也标出来，免得模型
    /// grep 的时候以为前后是连着的。
    fn spill_captured(&self, stdout: &Capture, stderr: &Capture) -> Option<String> {
        let name = format!("agent-out-{}.log", uuid::Uuid::now_v7());
        // 宿主模式：写系统临时目录（跟着 TMPDIR 走），回报的也就是本机路径
        // （`read` 那边 rootfs 是 `/`，直接解得开）；沙箱里是 rootfs 的 /tmp。
        let (host, guest) = if self.cfg.host {
            let p = std::env::temp_dir().join(&name);
            let g = p.display().to_string();
            (p, g)
        } else {
            (
                self.cfg.rootfs.join("tmp").join(&name),
                format!("/tmp/{name}"),
            )
        };
        std::fs::create_dir_all(host.parent()?).ok()?;

        let mut body = Vec::new();
        stdout.write_to(&mut body);
        if !stderr.is_empty() {
            body.extend_from_slice(b"\n--- stderr ---\n");
            stderr.write_to(&mut body);
        }
        std::fs::write(&host, body).ok()?;
        Some(guest)
    }

    // -----------------------------------------------------------------------
    // 文件访问
    // -----------------------------------------------------------------------

    /// 是否挂了手机存储
    pub fn has_shared_storage(&self) -> bool {
        self.cfg.shared.is_some()
    }

    /// 沙箱内路径 → 宿主机路径。
    ///
    /// - `/root/a.txt` → `<rootfs>/root/a.txt`
    /// - `a.txt`（相对） → 按 cwd `/workspace` 算
    /// - `/workspace/…`、`/sdcard/…` → 挂载的宿主目录（有挂的话）
    ///
    /// 两道越界检查：先做**词法归一化**（处理 `.` 和 `..`），
    /// 文件存在时再 `canonicalize` 一次，堵住符号链接逃逸 ——
    /// Alpine 里大量条目是指向绝对路径的符号链接。
    pub fn resolve_path(&self, guest: &str) -> anyhow::Result<PathBuf> {
        self.resolve_path_kind(guest, false)
    }

    /// 目标还**不存在**（`edit` 的 create 模式）时的路径解析。
    ///
    /// `confine` 的真实路径检查靠 `canonicalize`，新文件查不了 —— 得往上找
    /// 最近一个存在的祖先（见 `confine_new`）。
    pub fn resolve_new_path(&self, guest: &str) -> anyhow::Result<PathBuf> {
        self.resolve_path_kind(guest, true)
    }

    fn resolve_path_kind(&self, guest: &str, for_new: bool) -> anyhow::Result<PathBuf> {
        anyhow::ensure!(!guest.trim().is_empty(), "路径为空");

        // 先在沙箱的视角里归一化，再决定落到哪个宿主目录
        let abs = if guest.starts_with('/') {
            PathBuf::from(guest)
        } else {
            Path::new(WORKSPACE).join(guest)
        };
        let g = lexical_normalize(&abs);

        let check = |root: &Path, target: PathBuf| -> anyhow::Result<PathBuf> {
            if for_new {
                confine_new(root, &target, guest)
            } else {
                confine(root, &target, guest)
            }
        };

        let mounts = [(SHARED, &self.cfg.shared), (WORKSPACE, &self.cfg.workspace)];
        for (at, host) in mounts {
            let Some(host) = host else { continue };
            if let Ok(rest) = g.strip_prefix(at) {
                let root = lexical_normalize(host);
                return check(&root, root.join(rest));
            }
        }

        let root = lexical_normalize(&self.cfg.rootfs);
        let rel = g.strip_prefix("/").unwrap_or(&g);
        check(&root, root.join(rel))
    }

    /// `edit` 工具：读文件 → 按原文替换 → 写回。
    ///
    /// 整个过程读一次写一次，中间不落盘（见 `editor` 模块的设计说明）。
    pub fn edit_file(&self, guest_path: &str, edits: &[Edit]) -> anyhow::Result<EditOutcome> {
        let host = self.resolve_path(guest_path)?;
        anyhow::ensure!(
            host.is_file(),
            "不是文件或不存在: {guest_path}（要新建文件就给 content，不要用 edits）"
        );

        let raw = std::fs::read_to_string(&host)
            .map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}（只能编辑 UTF-8 文本）"))?;

        let outcome = editor::apply(&raw, edits, guest_path)?;
        atomic_write(&host, outcome.content.as_bytes())
            .map_err(|e| anyhow::anyhow!("写入失败 {guest_path}: {e}"))?;

        Ok(outcome)
    }

    /// `edit` 工具的 create 模式：新建一个文件（父目录按需建）。
    ///
    /// 已存在就报错，**不静默覆盖** —— 想改已有文件应该先 `read` 再用 `edits`，
    /// 那样改错了还能在 diff 里看出来；直接盖掉等于把原文扔了。
    pub fn create_file(&self, guest_path: &str, content: &str) -> anyhow::Result<CreatedFile> {
        let host = self.resolve_new_path(guest_path)?;
        anyhow::ensure!(
            !host.exists(),
            "已存在: {guest_path}（content 只用于新建；要改它请先 read，再用 edits）"
        );

        if let Some(dir) = host.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("建目录失败 {}: {e}", dir.display()))?;
        }
        atomic_write(&host, content.as_bytes())
            .map_err(|e| anyhow::anyhow!("写入失败 {guest_path}: {e}"))?;

        Ok(CreatedFile {
            lines: content.lines().count(),
            bytes: content.len(),
        })
    }

    /// `read` 工具：先嗅魔数，是图片就走图片那条路（缩放 + 内联），
    /// 否则当 UTF-8 文本读（offset/limit/头截断，见 `reader` 模块）。
    pub fn read_any(
        &self,
        guest_path: &str,
        offset: usize,
        limit: Option<usize>,
        numbered: bool,
    ) -> anyhow::Result<ReadResult> {
        let host = self.resolve_readable(guest_path)?;
        // 先嗅头（只读 12 字节）再决定走哪条路：不能为了判断“是不是图片”
        // 就把一个可能上百 MB 的文本文件整份读进来。
        if crate::imaging::is_image_file(&host)? {
            if let Some(img) = crate::imaging::load_if_image(&host)? {
                return Ok(ReadResult::Image(img));
            }
        }
        let text = read_utf8(&host, guest_path)?;
        Ok(ReadResult::Text(reader::read(
            &text, guest_path, offset, limit, numbered,
        )?))
    }

    /// 纯文本那条路（测试与内部用）。
    pub fn read_text(
        &self,
        guest_path: &str,
        offset: usize,
        limit: Option<usize>,
        numbered: bool,
    ) -> anyhow::Result<ReadView> {
        let host = self.resolve_readable(guest_path)?;
        let text = read_utf8(&host, guest_path)?;
        reader::read(&text, guest_path, offset, limit, numbered)
    }

    fn resolve_readable(&self, guest_path: &str) -> anyhow::Result<PathBuf> {
        let host = self.resolve_path(guest_path)?;
        anyhow::ensure!(
            host.is_file(),
            "不是文件或不存在: {guest_path}（目录内容用 bash 的 ls）"
        );
        Ok(host)
    }

    /// 写文件（`bash` 之外的小工具，也供测试用）
    pub fn read_file(&self, guest_path: &str) -> anyhow::Result<String> {
        let host = self.resolve_path(guest_path)?;
        std::fs::read_to_string(&host).map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}"))
    }
}

/// `path` 必须落在 `root` 里面：词法上一次，存在的话再按真实路径查一次
fn confine(root: &Path, path: &Path, guest: &str) -> anyhow::Result<PathBuf> {
    let norm = lexical_normalize(path);
    anyhow::ensure!(norm.starts_with(root), "路径越界，不允许走出沙箱: {guest}");
    if let Ok(real) = norm.canonicalize() {
        let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        anyhow::ensure!(real.starts_with(&real_root), "符号链接指向沙箱外: {guest}");
    }
    Ok(norm)
}

/// 给**还不存在**的目标做 confinement。
///
/// 目标自己 `canonicalize` 不了，就往上找最近一个存在的祖先，看它的真实路径
/// 在不在沙箱里。这步不能省：模型能在沙箱里 `ln -s /etc /workspace/link`，而
/// 我们是在**宿主**上按路径写文件的 —— 不查就写到沙箱外面去了。
///
/// （`read` / `edit` 那条路不用它：文件已存在，`confine` 的 `canonicalize`
/// 已经兜住了。所以它是给 create 专用。）
fn confine_new(root: &Path, path: &Path, guest: &str) -> anyhow::Result<PathBuf> {
    let norm = lexical_normalize(path);
    anyhow::ensure!(norm.starts_with(root), "路径越界，不允许走出沙箱: {guest}");
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let mut probe = norm.clone();
    while probe.starts_with(root) {
        match probe.canonicalize() {
            Ok(real) => {
                anyhow::ensure!(real.starts_with(&real_root), "符号链接指向沙箱外: {guest}");
                return Ok(norm);
            }
            // 连 root 都不存在（沙箱还没起）就无从检查，交给后面的 IO 报错
            Err(_) if probe == root => break,
            Err(_) => {
                if !probe.pop() {
                    break;
                }
            }
        }
    }
    Ok(norm)
}

/// 整份写入一个文件，**要么全是新的、要么还是旧的**。
///
/// `std::fs::write` 是先清空再写：写到一半进程被杀（系统回收、用户划掉 App），
/// 文件就只剩半截甚至是空的 —— 而 `/workspace` 是用户内部存储里的文件夹，坏了
/// 就是真坏了。所以先写同目录下的临时文件、刷盘，再 rename 盖过去（同一个文件
/// 系统里 rename 是原子的）。
///
/// - 目标是**符号链接**：写到它指向的那个文件，不要把链接本身换成普通文件
///   （原来 `fs::write` 也是跟着链接走的）。越界检查调用方已经做过（`confine`）。
/// - 改已有文件时保留原来的权限位（不然可执行脚本改一下就不能执行了）。
fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let target = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(_) => path.to_path_buf(), // 新文件
    };
    let dir = target.parent().unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(
        ".{name}.agent-{}.tmp",
        uuid::Uuid::now_v7().simple()
    ));
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(&target) {
            // 共享存储（FUSE）上改权限可能不被支持：失败不算错
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// 读成 UTF-8 文本。二进制给一条能操作的错误，而不是一句 "invalid utf-8"。
fn read_utf8(host: &Path, guest_path: &str) -> anyhow::Result<String> {
    let size = std::fs::metadata(host).map(|m| m.len()).unwrap_or(0);
    check_text_size(size, guest_path)?;
    let raw = std::fs::read(host).map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}"))?;
    String::from_utf8(raw).map_err(|_| {
        anyhow::anyhow!(
            "不是 UTF-8 文本（二进制文件？）: {guest_path}；要查看内容用 bash 的 file / xxd"
        )
    })
}

/// 文本大小闸门。单独抽出来是为了能测（不用真写一个 32MB 的文件）。
fn check_text_size(size: u64, guest_path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        size <= MAX_TEXT_BYTES,
        "文件 {}MB，超过 {}MB：不能整份读进内存（会 OOM）。\
         用 bash 局部读，例如 `sed -n '1,2000p' {guest_path}` 或 `head -c 50000 {guest_path}`",
        size / 1024 / 1024,
        MAX_TEXT_BYTES / 1024 / 1024
    );
    Ok(())
}

/// 词法归一化：处理 `.` / `..` / 重复分隔符，**不碰文件系统**。
///
/// 不能用 `canonicalize` 代替：目标文件可能还不存在，而且 rootfs 里
/// 大量绝对符号链接会被解到宿主机上去。
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            // 在根目录上 `..` 是空操作（PathBuf::pop 对 "/" 返回 false）
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// rootfs 看起来是不是真的 Alpine。
///
/// 只看**普通文件**，因为 Alpine 里大量条目是指向绝对路径的符号链接，
/// 在宿主机上解析会失败。
fn is_alpine_rootfs(rootfs: &Path) -> bool {
    rootfs.join("bin/busybox").is_file() || rootfs.join("etc/alpine-release").is_file()
}

/// 一路输出里保留的两端：**头部**和**尾部**。
///
/// 回给模型的是「首 100 行 + `[省略 N 行]` + 尾 100 行」（见 `render`），
/// 所以两端都得留着。超过内存上限时扔的是**中间**：头部攒满 `CAPTURE_HEAD`
/// 就封口（切在行边界上），后面的进尾部；尾部超过 `CAPTURE_TAIL` 就从最旧的
/// 一端挤掉，挤掉多少记在 `dropped_*` 里。
///
/// 内存上界 = `CAPTURE_HEAD + CAPTURE_TAIL`，跟原来一个 4 MB 的缓冲一样。
#[derive(Default)]
struct Capture {
    head: Vec<u8>,
    tail: Vec<u8>,
    /// 中间扔掉的字节数
    dropped_bytes: u64,
    /// 中间扔掉的整行数；单行超长、只能从行中间扔时算不进来
    dropped_lines: u64,
}

const CAPTURE_HEAD: usize = 2 << 20;
const CAPTURE_TAIL: usize = 2 << 20;

impl Capture {
    /// 边读边往里塞；`live` 是给前端的实时副本，`tap` 记「最后读到数据」的时刻
    /// （命令退出后判断输出安静下来用的，见 `wait_for_quiet`）。
    ///
    /// 结果放在共享的 `Arc<Mutex<Capture>>` 里、而不是当返回值：管道可能被
    /// 刻意脱离的进程占着不关（`setsid … &`），读任务会一直卡在 `read()` 上 ——
    /// 父任务等不到它，但**已经读到的输出必须拿得走**（见 `drain`）。
    async fn read_into<R: tokio::io::AsyncRead + Unpin>(
        src: Option<R>,
        live: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
        cap: Arc<Mutex<Capture>>,
        tap: Option<Arc<OutputTap>>,
    ) {
        use tokio::io::AsyncReadExt;
        let Some(mut src) = src else { return };
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match src.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(t) = &tap {
                        t.note();
                    }
                    if let Some(tx) = &live {
                        let _ = tx.send(chunk[..n].to_vec());
                    }
                    lock(&cap).push(&chunk[..n]);
                }
            }
        }
    }

    fn push(&mut self, data: &[u8]) {
        let mut rest = data;
        if self.head.len() < CAPTURE_HEAD {
            let take = (CAPTURE_HEAD - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            // 头刚封口：把最后那半行挪进尾部，让头停在行边界上
            if self.head.len() >= CAPTURE_HEAD {
                if let Some(nl) = self.head.iter().rposition(|&b| b == b'\n') {
                    let leftover = self.head.split_off(nl + 1);
                    self.tail.extend_from_slice(&leftover);
                }
            }
        }
        if !rest.is_empty() {
            self.push_tail(rest);
        }
    }

    /// 往尾部追加；超过 `CAPTURE_TAIL` 就从最旧的一端挤掉。
    fn push_tail(&mut self, data: &[u8]) {
        self.tail.extend_from_slice(data);
        if self.tail.len() <= CAPTURE_TAIL {
            return;
        }
        // 挤到行边界（整行扔）；单行超长找不到换行，就按字节扔 ——
        // 但别整段都丢，那样尾部就永远攒不起来了
        let excess = self.tail.len() - CAPTURE_TAIL;
        let cut = match self.tail[excess..].iter().position(|&b| b == b'\n') {
            Some(i) => excess + i + 1,
            None => excess,
        };
        self.dropped_lines += self.tail[..cut].iter().filter(|&&b| b == b'\n').count() as u64;
        self.dropped_bytes += cut as u64;
        self.tail.drain(..cut);
    }

    fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty()
    }

    /// 渲染成给模型看的样子：首 `HEAD_LINES` 行 + `[省略 N 行]` + 尾 `TAIL_LINES` 行。
    ///
    /// 没超上限就原样返回。第二个返回值是「有没有截断」。
    fn render(&self) -> (String, bool) {
        if self.is_empty() {
            return (String::new(), false);
        }
        let head = String::from_utf8_lossy(&self.head);
        let head_lines = real_lines(&head);
        // 尾部为空 = 没扔过中间：整份输出都在 `head` 里，首尾取自同一份
        let split = !self.tail.is_empty();
        if !split && head_lines.len() <= MAX_OUTPUT_LINES && head.len() <= MAX_OUTPUT_BYTES {
            return (head.into_owned(), false);
        }

        let tail_text = String::from_utf8_lossy(&self.tail);
        let tail_lines = if split {
            real_lines(&tail_text)
        } else {
            Vec::new()
        };

        let (head_out, head_shown) = head_part(&head_lines, HEAD_LINES, SIDE_BYTES);
        let (tail_out, tail_shown) = if split {
            tail_part(&tail_lines, TAIL_LINES, SIDE_BYTES)
        } else {
            // 同一份行：尾部只能取头没拿走的那些
            tail_part(&head_lines[head_shown..], TAIL_LINES, SIDE_BYTES)
        };

        // 省略了多少行：头尾没展示的 + 中间扔掉的
        // （没分家时首尾取自同一份行，尾巴拿走的那些不能重复计入“省略”）
        let omitted_lines = if split {
            (head_lines.len() - head_shown)
                + self.dropped_lines as usize
                + (tail_lines.len() - tail_shown)
        } else {
            head_lines.len() - head_shown - tail_shown
        };
        // 省略了多少字节：留下的里没展示的 + 中间扔掉的
        let kept = (self.head.len() + self.tail.len()) as u64;
        let shown = (head_out.len() + tail_out.len()) as u64;
        let omitted_bytes = kept.saturating_sub(shown) + self.dropped_bytes;

        // 正常报「省略 N 行」；单行超长、整行数不出来时退回报字节
        let marker = if omitted_lines > 0 {
            format!("[省略 {omitted_lines} 行]")
        } else {
            format!("[省略 {}]", crate::reader::fmt_size(omitted_bytes as usize))
        };

        let mut out = String::with_capacity(head_out.len() + tail_out.len() + marker.len() + 4);
        if !head_out.is_empty() {
            out.push_str(&head_out);
            out.push('\n');
        }
        out.push_str(&marker);
        out.push('\n');
        out.push_str(&tail_out);
        // 原文结尾有换行就保留
        let ended_nl = if split {
            tail_text.ends_with('\n')
        } else {
            head.ends_with('\n')
        };
        if ended_nl && !tail_out.is_empty() {
            out.push('\n');
        }
        (out, true)
    }

    /// 落盘用：头 +（中间扔掉的提示）+ 尾。
    ///
    /// 中间扔过东西就必须标出来，不然模型 grep 这份文件会以为前后是连着的。
    fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.head);
        if self.tail.is_empty() {
            return;
        }
        if self.dropped_bytes > 0 {
            let note = if self.dropped_lines > 0 {
                format!(
                    "\n[... 中间省略 {} 行 / {} ...]\n",
                    self.dropped_lines,
                    crate::reader::fmt_size(self.dropped_bytes as usize)
                )
            } else {
                format!(
                    "\n[... 中间省略 {} ...]\n",
                    crate::reader::fmt_size(self.dropped_bytes as usize)
                )
            };
            out.extend_from_slice(note.as_bytes());
        }
        out.extend_from_slice(&self.tail);
    }
}

/// 拿 `Capture` 的锁；被上个任务的 panic 毒到了也照样用（里面只有字节，
/// 没有需要维持的不变式）。
fn lock(cap: &Mutex<Capture>) -> MutexGuard<'_, Capture> {
    cap.lock().unwrap_or_else(|e| e.into_inner())
}

/// 等这次调用结束：状态文件 / 两根管道 EOF / 超时，三条路先到先算。
async fn wait_for_done(
    started: Instant,
    timeout: Duration,
    status_host: &Path,
    out: &tokio::task::JoinHandle<()>,
    err: &tokio::task::JoinHandle<()>,
) -> Done {
    loop {
        if let Some(code) = read_status_code(status_host) {
            break Done::Status(code);
        }
        if out.is_finished() && err.is_finished() {
            break Done::PipesClosed;
        }
        if started.elapsed() >= timeout {
            break Done::TimedOut;
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

/// 命令已经退出（状态文件出现）之后，把管道里最后那点输出读完。
///
/// 不能一见状态文件就关读端：字节可能还在管道缓冲区里，而且**裸 `&` 起的子孙
/// 没有脱离**，还在往同一根管道写。等到「一段时间没有新数据」为止 —— 但有个
/// 硬上限（真有进程一直在写，不能陪它到天荒地老）。
///
/// 两根管道都 EOF 就没什么可等的了，直接收。
async fn wait_for_quiet(
    tap: &OutputTap,
    out: &tokio::task::JoinHandle<()>,
    err: &tokio::task::JoinHandle<()>,
) {
    let seen = Instant::now();
    loop {
        if out.is_finished() && err.is_finished() {
            return;
        }
        let since = seen.elapsed();
        if since >= OUTPUT_DRAIN_CAP {
            return;
        }
        if since >= OUTPUT_QUIET && tap.quiet_for(OUTPUT_QUIET) {
            return;
        }
        tokio::time::sleep(STATUS_POLL).await;
    }
}

/// 等读任务收尾，把捕获到的输出取走。
///
/// 正常情况下管道会 EOF、任务自己结束。还被某条刻意脱离的进程占着的话最多
/// 等 `grace`，超时就 abort 掉任务 —— 要的是**已读到的输出**，不是干净的收尾。
///
/// 原来这里是「等 2 秒，超时就用 `unwrap_or_default()`」：那个默认值是**空的
/// `Capture`**，等于把 2 秒内已经读到的输出全扔了。一条后台进程就能让模型
/// 看到空输出，而且看不出来为什么。
///
/// 现在的调用方只有在**刚杀完命令组**时才给 grace（`DRAIN_AFTER_KILL`）；
/// 知道命令已经退出、输出也安静了的那条路给 0。
async fn drain(
    mut task: tokio::task::JoinHandle<()>,
    cap: &Mutex<Capture>,
    grace: Duration,
) -> Capture {
    match tokio::time::timeout(grace, &mut task).await {
        // 任务自己收尾了 —— 注意：这时 `task` **已经被 poll 完**，
        // 再来一个 `task.await` 会 panic（“polled after completion”），
        // 而 release 是 `panic = "abort"` —— 每次 bash 调用都会把 App 干掉。
        Ok(_) => {}
        Err(_) => {
            // 管道还被孙进程占着：不等了，但要把已读到的拿走。
            // abort 之后才允许再 await（这时它确实还没完成）。
            task.abort();
            let _ = task.await;
        }
    }
    std::mem::take(&mut *lock(cap))
}

/// 按 `reader` 的规则拆行：结尾 `\n` 切出来的空串不算一行。
fn real_lines(text: &str) -> Vec<&str> {
    match text.strip_suffix('\n') {
        Some(t) => t.split('\n').collect(),
        None => text.split('\n').collect(),
    }
}

/// 取头部 ≤`want` 行、≤`budget` 字节。返回 (正文, 算作「已展示」的行数)。
///
/// 第一行就超预算时给它的前半截 —— 宁可给半行，也别让模型什么都看不到
/// （省掉的那部分会进 `[省略 …]`）。
fn head_part(lines: &[&str], want: usize, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut shown = 0usize;
    for line in lines.iter().take(want) {
        let sep = usize::from(shown > 0);
        if out.len() + sep + line.len() > budget {
            if shown == 0 {
                out.push_str(clip_head(line, budget));
                return (out, 1);
            }
            break;
        }
        if shown > 0 {
            out.push('\n');
        }
        out.push_str(line);
        shown += 1;
    }
    (out, shown)
}

/// 取尾部 ≤`want` 行、≤`budget` 字节（从后往前凑，返回时顺序正常）。
fn tail_part(lines: &[&str], want: usize, budget: usize) -> (String, usize) {
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().rev().take(want) {
        let sep = usize::from(!kept.is_empty());
        if used + sep + line.len() > budget {
            if kept.is_empty() {
                return (clip_tail(line, budget).to_string(), 1);
            }
            break;
        }
        used += sep + line.len();
        kept.push(line);
    }
    kept.reverse();
    (kept.join("\n"), kept.len())
}

/// `s` 的前 `budget` 字节（不切开多字节字符）
fn clip_head(s: &str, budget: usize) -> &str {
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` 的后 `budget` 字节（不切开多字节字符）
fn clip_tail(s: &str, budget: usize) -> &str {
    let mut start = s.len().saturating_sub(budget);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// 设备当前时区的 POSIX TZ 串。
///
/// 与 UTC 偏移符号相反：UTC+8 → `LCL-8`。
fn posix_tz(override_minutes: Option<i32>) -> String {
    let offset_min = override_minutes
        .or_else(|| {
            std::env::var("AGENT_TZ_OFFSET_MIN")
                .ok()
                .and_then(|s| s.parse::<i32>().ok())
        })
        .or_else(host_offset_minutes)
        .unwrap_or(0);

    let sign = if offset_min >= 0 { '-' } else { '+' };
    let abs = offset_min.abs();
    let (h, m) = (abs / 60, abs % 60);
    if m == 0 {
        format!("LCL{sign}{h}")
    } else {
        format!("LCL{sign}{h}:{m:02}")
    }
}

/// 桌面兜底：从 `TZ` 环境变量读偏移。没有 chrono 依赖，手写解析。
fn host_offset_minutes() -> Option<i32> {
    // TZ 环境变量优先
    if let Ok(tz) = std::env::var("TZ") {
        if let Some(v) = parse_posix_tz(&tz) {
            return Some(v);
        }
    }
    None
}

/// 解析形如 `CST-8` / `UTC+5:30` 的 POSIX TZ 串，返回 UTC 偏移分钟数
fn parse_posix_tz(s: &str) -> Option<i32> {
    let bytes = s.as_bytes();
    let i = bytes.iter().position(|&b| b == b'-' || b == b'+')?;
    let sign = if bytes[i] == b'-' { 1 } else { -1 };
    let rest = &s[i + 1..];
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    Some(sign * (h * 60 + m))
}

#[cfg(test)]
mod tests {

    /// 测试用：正常读完一份内存里的字节（不关心管道被占着的情形）。
    async fn read_all(data: &[u8]) -> Capture {
        let cap = Arc::new(Mutex::new(Capture::default()));
        Capture::read_into(Some(data), None, cap.clone(), None).await;
        let out = std::mem::take(&mut *lock(&cap));
        out
    }

    #[tokio::test]
    async fn capture_keeps_both_ends_and_counts_the_middle() {
        // 模拟一个狂吐输出的命令：远超上限
        let total = CAPTURE_HEAD + CAPTURE_TAIL + 10_000;
        let mut data = b"START\n".to_vec();
        data.resize(total - 4, b'\n');
        data.extend_from_slice(b"END\n");
        let cap = read_all(&data).await;
        assert!(cap.head.starts_with(b"START\n"), "头部不能丢");
        assert!(cap.tail.ends_with(b"END\n"), "尾部不能丢");
        assert!(cap.head.len() <= CAPTURE_HEAD, "内存要有上界");
        assert!(cap.tail.len() <= CAPTURE_TAIL, "内存要有上界");
        assert!(cap.dropped_bytes > 0, "中间确实扔了");
        // 丢掉的部分就是中间那段，不多不少
        assert_eq!(
            cap.head.len() as u64 + cap.tail.len() as u64 + cap.dropped_bytes,
            total as u64
        );
    }
    use super::*;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("/native/libproot.so"),
            rootfs: PathBuf::from("/data/files/alpine-rootfs"),
            host: false,
            binds: vec![("/host/share".into(), "/mnt/share".into())],
            tmp: Some(PathBuf::from("/data/cache/tmp")),
            dns: vec!["10.0.0.1".into()],
            tz_offset_minutes: None,
            loader: None,
            loader32: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        })
    }

    #[test]
    fn command_line_matches_the_documented_shape() {
        // 这个假 rootfs 里没有 setsid，所以走的是「不分组」的退化形态
        let argv = sandbox().command_line("echo hi", "/.agent-exit/x");
        assert_eq!(argv[0], "/native/libproot.so");
        assert_eq!(argv[1], "-0");
        assert_eq!(argv[2], "--link2symlink");
        assert_eq!(&argv[3..5], &["-r", "/data/files/alpine-rootfs"]);
        assert_eq!(&argv[5..11], &["-b", "/dev", "-b", "/proc", "-b", "/sys"]);
        assert_eq!(&argv[11..13], &["-b", "/host/share:/mnt/share"]);
        assert_eq!(&argv[13..15], &["-w", "/workspace"]);
        assert_eq!(&argv[15..18], &["/bin/sh", "-c", argv[17].as_str()]);
        assert_eq!(&argv[18..], &["sh", "echo hi", "/.agent-exit/x"]);
        // 这个参数一加回来，`nohup` / `setsid` 起的服务就活不过这次调用
        assert!(
            !argv.iter().any(|a| a == "--kill-on-exit"),
            "不能再用 --kill-on-exit: {argv:?}"
        );
    }

    /// 有 setsid 就一定要用它：命令得跑在**自己的进程组**里，超时 / 停止时
    /// 才能只杀它（proot 要留着给常驻进程当 tracer）。
    #[test]
    fn command_line_runs_the_command_in_its_own_process_group() {
        let t = TempRoot::new("setsid");
        std::fs::write(t.dir.join("bin/setsid"), b"").unwrap();
        let argv = t.sandbox().command_line("echo hi", "/.agent-exit/x");
        let setsid_at = argv
            .iter()
            .position(|a| a == "/bin/setsid")
            .expect("setsid 在里面");
        assert_eq!(
            &argv[setsid_at..],
            &[
                "/bin/setsid",
                "/bin/sh",
                "-c",
                argv[setsid_at + 3].as_str(),
                "sh",
                "echo hi",
                "/.agent-exit/x"
            ],
            "setsid 得直接排在 runner 的 /bin/sh 前面: {argv:?}"
        );
        let runner = &argv[setsid_at + 3];
        assert!(
            runner.contains("$2.pid"),
            "有 setsid 时 runner 必须报进程组号: {runner}"
        );
    }

    /// 没有 setsid 时 runner 不能报 pid —— 那个 `$$` 不是组号，按它杀会杀错组。
    #[test]
    fn command_line_degrades_without_setsid() {
        let t = TempRoot::new("nosetsid");
        let argv = t.sandbox().command_line("true", "/.agent-exit/x");
        assert!(!argv.iter().any(|a| a.ends_with("setsid")), "{argv:?}");
        let runner = argv.iter().find(|a| a.contains("rc=$?")).unwrap();
        assert!(
            !runner.contains("$2.pid"),
            "没有 setsid 时不能报组号: {runner}"
        );
    }

    #[test]
    fn script_is_passed_as_one_argv_element_not_shell_escaped() {
        // 关键：脚本原样作为单个参数传，不做任何转义 —— 因为我们是
        // 直接 execve，没有中间 shell 会二次解析它（runner 拿它当 `$1`）
        let nasty = "echo \"a b\"; ls | grep x; echo '$(whoami)'";
        let argv = sandbox().command_line(nasty, "/.agent-exit/x");
        assert_eq!(argv[argv.len() - 2], nasty);
    }

    /// 没 bash 退回 ash、装了就用 bash —— 两条路都要盯住。
    #[test]
    fn command_line_uses_bash_once_it_is_installed() {
        let t = TempRoot::new("shellbash");
        let sb = t.sandbox();
        // 真正执行脚本的 shell 写在 runner 里，runner 是 `-c` 的下一个参数
        let runner_of = |argv: Vec<String>| {
            let i = argv.iter().position(|a| a == "-c").expect("-c 一定在");
            argv[i + 1].clone()
        };
        assert!(
            runner_of(sb.command_line("true", "/.agent-exit/x")).starts_with("/bin/sh -c"),
            "没装就退回 ash"
        );
        std::fs::write(t.dir.join("bin/bash"), b"").unwrap();
        assert!(
            runner_of(sb.command_line("true", "/.agent-exit/x")).starts_with("/bin/bash -c"),
            "装了就该用 bash，不然模型写的 bash 语法白写"
        );
    }

    /// runner 是纯 POSIX sh，所以拿**宿主的** /bin/sh 就能把它验到底：
    /// 退出码和组号真的写下来了，脚本也没被多解析一层。
    #[test]
    fn runner_writes_the_exit_code_and_the_group_id() {
        let t = TempRoot::new("runner");
        let status = t.dir.join("status");
        let path = status.display().to_string();
        let script = "echo \"a b\"; echo '$(whoami)'; exit 7";
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &runner_script("/bin/sh", true), "sh", script, &path])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7), "runner 要把命令的退出码传出来");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "a b\n$(whoami)\n",
            "脚本原样交给内层 sh，没被多解析一层"
        );
        assert_eq!(std::fs::read_to_string(&status).unwrap(), "7");
        let pid = read_pgid(&PathBuf::from(format!("{path}.pid"))).expect("pid 文件要能读");
        assert!(pid > 1, "pid 得是个真 pid: {pid}");
    }

    /// `kill_group` 得真的打到组里的**每个**进程，不只是组长。
    /// 这就是超时 / 停止时不再漏孤儿的那一步。
    #[tokio::test]
    async fn kill_group_takes_down_the_whole_group() {
        use std::os::unix::process::CommandExt;
        // 组里两条 sleep：组长（sh）和它自己起的子进程
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & sleep 30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(group_alive(pgid), "组该起来了");

        kill_group(pgid, libc::SIGKILL);
        let _ = child.wait(); // 给组长收尸，免得僵尸顶着组号
        let mut gone = false;
        for _ in 0..40 {
            if !group_alive(pgid) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !gone {
            // 别把 30 秒的 sleep 留给下一次测试
            kill_group(pgid, libc::SIGKILL);
        }
        assert!(gone, "组里还有活着的进程（孤儿漏出来了）");
    }

    /// 真机上「命令退出、但它起的进程还拿着 stdout」这条形状，在 macOS 上拿
    /// 宿主的 /bin/sh 就能复现（测的是**我们的**监控 / 收尾逻辑，不是 proot）：
    ///
    /// - 有状态文件就立刻收工，不陪那根永不 EOF 的管道；
    /// - 输出安静之后的 `drain(0)` 照样把已经读到的输出完整交出来；
    /// - 组杀真的能打到拿着管道的那条（读端因此等到 EOF）。
    #[tokio::test]
    async fn a_status_file_ends_the_call_without_waiting_for_the_pipe() {
        let t = TempRoot::new("heldpipe");
        let status = t.dir.join("status");
        let pid_file = t.dir.join("status.pid");
        let started = Instant::now();

        // 后台 sleep 继承 stdout（就是「常驻进程」那根写端），然后命令自己退出、
        // 写下状态文件 —— runner 干的就是这件事。
        //
        // 路径用双引号包住：测试的临时目录名里带 `ThreadId(128)` 这种括号，
        // 裸词里的 `(` 会被 sh 当语法错误（runner 那边是 `> "$2"`，本来就带引号）。
        let script = format!(
            "sleep 30 & echo hi; printf 0 > \"{}\"; exit 0",
            status.display()
        );
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap() as i32;
        std::fs::write(&pid_file, pgid.to_string()).unwrap();

        let tap = Arc::new(OutputTap::new(started));
        let cap = Arc::new(Mutex::new(Capture::default()));
        let out_task = tokio::spawn(Capture::read_into(
            child.stdout.take(),
            None,
            cap.clone(),
            Some(tap.clone()),
        ));
        let err_task = tokio::spawn(async {});

        let done = wait_for_done(
            started,
            Duration::from_secs(5),
            &status,
            &out_task,
            &err_task,
        )
        .await;
        assert!(
            matches!(done, Done::Status(0)),
            "得靠状态文件收工，不能等管道"
        );
        assert!(
            !out_task.is_finished(),
            "这会儿后台进程还拿着写端（形状才对）"
        );

        let before = Instant::now();
        wait_for_quiet(&tap, &out_task, &err_task).await;
        assert!(
            before.elapsed() < Duration::from_secs(1),
            "安静了就该立刻走，实际 {:?}",
            before.elapsed()
        );

        let got = drain(out_task, &cap, Duration::ZERO).await;
        assert_eq!(
            String::from_utf8_lossy(&got.head),
            "hi\n",
            "已经读到的输出不能丢"
        );

        // 收尾：把那条 sleep 干掉（平时这是 CommandGuard 的事）
        kill_group(pgid, libc::SIGKILL);
        let _ = child.wait().await;
    }

    /// 超时又没有状态文件时：组杀要真的打到拿着管道的那条，读端才会 EOF。
    /// 这就是「不再漏孤儿」那一步的可测形式。
    #[tokio::test]
    async fn a_timed_out_command_group_is_killed_and_releases_the_pipe() {
        let t = TempRoot::new("timeoutkill");
        let pid_file = t.dir.join("status.pid");

        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id().unwrap() as i32;
        std::fs::write(&pid_file, pgid.to_string()).unwrap();
        let guard = CommandGuard::new(pid_file, Some(pgid));

        let cap = Arc::new(Mutex::new(Capture::default()));
        let out_task = tokio::spawn(Capture::read_into(
            child.stdout.take(),
            None,
            cap.clone(),
            None,
        ));
        let err_task = tokio::spawn(async {});

        let started = Instant::now();
        let done = wait_for_done(
            started,
            Duration::from_millis(200),
            &t.dir.join("never-written"),
            &out_task,
            &err_task,
        )
        .await;
        assert!(matches!(done, Done::TimedOut));

        guard.terminate().await;
        let mut released = false;
        for _ in 0..40 {
            if out_task.is_finished() {
                released = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(released, "杀了组，管道还没 EOF —— 孤儿还在拿着它");
        // 先收尸再判组：被杀死的组长在 `wait` 之前是僵尸，而僵尸也占着组号
        let _ = child.wait().await;
        assert!(!group_alive(pgid), "组里还有活着的进程");
        let _ = drain(out_task, &cap, Duration::ZERO).await;
    }

    /// Alpine 里 `/usr/bin/setsid` 是指向 `/bin/busybox` 的**绝对**符号链接，
    /// 在宿主机上解析必然失败 —— 判存在不能 `is_file()`。
    ///
    /// 漏了这一步的后果（真机上实测）：命令行里没有 setsid，runner 就落在
    /// proot 自己的进程组里，超时那一杀打到 proot 头上（它忽略 SIGTERM，然后
    /// 吃 SIGKILL），被 trace 的常驻进程全部失去翻译。
    #[test]
    fn setsid_is_found_when_it_is_a_dangling_absolute_symlink() {
        let t = TempRoot::new("setsidlink");
        std::fs::create_dir_all(t.dir.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink("/bin/busybox", t.dir.join("usr/bin/setsid")).unwrap();

        let argv = t.sandbox().command_line("true", "/.agent-exit/x");
        assert!(
            argv.iter().any(|a| a == "/usr/bin/setsid"),
            "绝对符号链接也算在：{argv:?}"
        );
        let runner = argv.iter().find(|a| a.contains("rc=$?")).unwrap();
        assert!(runner.contains("$2.pid"), "有 setsid 就该报组号: {runner}");
    }

    /// `agent ps`：扫 /proc 的解析。假 /proc 直接造，不靠真实进程。
    #[test]
    fn proc_scan_parses_stat_status_and_cmdline() {
        // comm 里有空格和括号：必须从**最后一个** ')' 之后切
        assert_eq!(
            parse_stat("4211 (WebView 子进程) S 1 4211 4211 0 -1 4194560 1 0 0 0 0"),
            Some(('S', 1, 4211))
        );
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (sh) Z 2 3"), Some(('Z', 2, 3)));
        assert_eq!(
            parse_tracer("Name:\tsleep\nTracerPid:\t21707\nPPid:\t1\n"),
            Some(21707)
        );
        assert_eq!(parse_tracer("TracerPid:\t0\n"), Some(0));
        assert_eq!(parse_tracer("Name:\tsleep\n"), None);
        // cmdline 是 NUL 分隔的 argv；超长按字符切（不能撞多字节边界）
        assert_eq!(render_cmdline(b"sh\0-c\0echo hi\0"), "sh -c echo hi");
        assert_eq!(render_cmdline(b""), "");
        let long = format!("echo {}", "汉".repeat(200));
        let clipped = render_cmdline(long.as_bytes());
        assert!(clipped.ends_with('…'), "{clipped}");
        assert!(
            clipped.chars().count() <= 121,
            "{}",
            clipped.chars().count()
        );

        // 一个假 /proc：两条看得见、一条 stat 缺失（进程刚退的样子）
        let t = TempRoot::new("procscan");
        let p = t.dir.join("proc");
        for (pid, stat, tracer, cmdline) in [
            (
                "4211",
                "4211 (sh) S 1 4211 4211 0 -1",
                "21707",
                "sh\0-c\0sleep 1\0",
            ),
            (
                "4212",
                "4212 (sleep) S 4211 4211 4211 0 -1",
                "21707",
                "sleep\x001\x00",
            ),
            ("4213", "4213 (gone) Z 1 4213 4213 0 -1", "0", ""),
        ] {
            let d = p.join(pid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), stat).unwrap();
            std::fs::write(d.join("status"), format!("TracerPid:\t{tracer}\n")).unwrap();
            std::fs::write(d.join("cmdline"), cmdline).unwrap();
        }
        // 一个非数字目录 + 一个缺 stat 的目录
        std::fs::create_dir_all(p.join("net")).unwrap();
        std::fs::create_dir_all(p.join("4214")).unwrap();
        let mut got = scan_procs_in(&p);
        got.sort_by_key(|r| r.pid);
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].pid, 4211);
        assert_eq!(got[0].cmdline, "sh -c sleep 1");
        assert_eq!(got[0].tracer, 21707);
        assert_eq!(got[1].ppid, 4211);
        assert_eq!(got[2].state, 'Z');
    }

    /// 归因：`TracerPid` 指向我们起的 proot 的进程才算我们的；命令结束后还活着
    /// 的那些进 `leftovers`（这才是可以 `agent kill` 的）。
    #[test]
    fn procs_attributes_survivors_to_the_command_that_left_them() {
        let t = TempRoot::new("procsattr");
        let sb = t.sandbox();
        // 拿一个真活着的进程当 "proot"（归因只要求 pid 活着）
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        let proot = child.id() as i32;
        let pid_file = t.dir.join("nope.pid"); // 没有 pid 文件 = 退回 proot 自己的 pid
        sb.register_run(proot, "setsid sh -c 'while :; do date; done'", pid_file);

        let fake = vec![
            RawProc {
                pid: 9001,
                ppid: 1,
                pgid: 9001,
                state: 'S',
                tracer: proot,
                cmdline: "sh -c while :; do date; done".into(),
            },
            // 上个 App 生命留下的孤儿 proot（名字对得上）＋它名下的 tracee
            RawProc {
                pid: 4242,
                ppid: 1,
                pgid: 4242,
                state: 'S',
                tracer: 0,
                cmdline: "proot -0 -r /files/rootfs /bin/sh -c sleep 9".into(),
            },
            RawProc {
                pid: 9002,
                ppid: 1,
                pgid: 9002,
                state: 'S',
                tracer: 4242,
                cmdline: "别人的".into(),
            },
            // **不是** proot 的 ppid==1 进程（桌面上那是 systemd 的服务）：
            // 列进「可以 kill」比漏掉糟得多 —— 这条就是那个坑的回归测试
            RawProc {
                pid: 9100,
                ppid: 1,
                pgid: 9100,
                state: 'S',
                tracer: 0,
                cmdline: "/usr/lib/systemd/systemd-journald".into(),
            },
        ];
        // 命令还在跑：进 running，不逐个列进程
        let r = sb.procs_with(fake.clone(), None);
        assert_eq!(r.visible, 4);
        assert!(r.leftovers.is_empty());
        assert_eq!(r.running.len(), 1);
        assert_eq!(r.running[0].procs, 1);
        assert_eq!(r.running[0].pgid, proot);
        assert!(
            r.running[0].command.contains("while :; do date"),
            "{:?}",
            r.running[0]
        );
        let text = r.to_text();
        assert!(text.contains("正在跑的命令"), "{text}");
        // 那条 tracer 不认识、父进程也没了的：进孤儿，不进「正在跑」
        assert_eq!(
            r.orphans.len(),
            2,
            "孤儿 proot 和它名下的 tracee：{:?}",
            r.orphans
        );

        // 调用方报上自己的进程组：不该在「正在跑」里看到自己
        let me = sb.procs_with(fake.clone(), Some(proot));
        assert!(
            me.running.is_empty(),
            "自己那条命令得滤掉：{:?}",
            me.running
        );

        // 命令结束了，它还活着 = 刻意脱离的常驻进程：能拿到 pid 就能杀
        sb.finish_run(proot);
        let r = sb.procs_with(fake, None);
        assert!(r.running.is_empty());
        assert_eq!(r.leftovers.len(), 1);
        assert_eq!(r.leftovers[0].pid, 9001);
        assert!(r.leftovers[0].leftover);
        let text = r.to_text();
        assert!(text.contains("还活着的常驻进程"), "{text}");
        assert!(text.contains("agent kill <pid>"), "{text}");
        assert!(text.contains("pid 9001"), "{text}");
        // 孤儿 proot + 它名下的 tracee：看得见、不认识，而且**父进程已经没了** ——
        // 正是「上个 App 生命留下的」那种，要列出来（否则没人能收）
        assert_eq!(
            r.orphans.len(),
            2,
            "孤儿 proot 和它的 tracee：{:?}",
            r.orphans
        );
        assert!(r.orphans.iter().any(|o| o.pid == 4242), "{:?}", r.orphans);
        assert!(
            r.orphans.iter().any(|o| o.cmdline.contains("别人的")),
            "{:?}",
            r.orphans
        );
        assert!(
            !r.orphans.iter().any(|o| o.pid == 9100),
            "ppid==1 但不是 proot 的（systemd 服务）绝不能列进来：{:?}",
            r.orphans
        );
        assert!(
            text.contains("不属于任何已知命令的进程"),
            "孤儿也得说清楚是什么：{text}"
        );
        assert!(!text.contains("← 来自：\n"), "孤儿没有「来自」：{text}");

        // proot 没了 → 登记被清掉，不再误导模型
        child.kill().unwrap();
        child.wait().unwrap();
        let other = RawProc {
            pid: 9500,
            ppid: 1,
            pgid: 9500,
            state: 'S',
            tracer: 0,
            // 名字得对得上（`is_proot`）—— 只有 proot 才可能是「我们上一次
            // 生命留下的」，随便一个 ppid==1 的进程不行
            cmdline: "proot -0 -r /files/rootfs /bin/sh -c echo hi".into(),
        };
        // 父进程还在、又不归我们的 → 不是孤儿（单元测试里 cargo 自己就是这种）
        let alive_parent = RawProc {
            pid: 9501,
            ppid: 4242,
            pgid: 4242,
            state: 'S',
            tracer: 0,
            cmdline: "别人的孩子".into(),
        };
        let r = sb.procs_with(vec![other, alive_parent], None);
        assert!(r.leftovers.is_empty() && r.running.is_empty());
        assert_eq!(r.orphans.len(), 1, "只列真孤儿：{:?}", r.orphans);
        assert_eq!(r.orphans[0].pid, 9500);
        assert!(r.to_text().contains("不属于任何已知命令的进程"));
        // 什么都没有时（看得见 /proc，但没我们的进程）
        let empty = ProcsReport {
            visible: 3,
            ..Default::default()
        };
        assert!(
            empty.to_text().contains("没有别的东西在跑"),
            "{}",
            empty.to_text()
        );
    }

    /// /proc 看不见任何东西时得**明说**，不能假装「什么都没有」。
    #[test]
    fn procs_says_so_when_proc_is_hidden() {
        let t = TempRoot::new("procshidden");
        let r = t.sandbox().procs_with(vec![], None);
        assert_eq!(r.visible, 0);
        assert!(r.to_text().contains("hidepid"), "{}", r.to_text());
    }

    #[test]
    fn status_files_are_removed_when_the_run_ends_or_is_cancelled() {
        let t = TempRoot::new("statusclean");
        let paths = {
            let s = StatusFile::new(&t.dir, "/.agent-exit");
            std::fs::write(&s.host, b"0").unwrap();
            std::fs::write(&s.pid_host, b"4242").unwrap();
            assert!(s.host.exists() && s.pid_host.exists());
            (s.host.clone(), s.pid_host.clone())
        }; // 出了作用域 = 取消（future 被丢掉）也会走同一条路
        assert!(!paths.0.exists(), "状态文件得清掉");
        assert!(
            !paths.1.exists(),
            "pid 文件也得清掉，不然 .agent-exit 会越积越多"
        );
    }

    /// `pgid <= 1` 一个都不许发：`0` 是「我自己这个组」，`1` 是 init。
    /// 这道闸真的生效的话，这个测试进程能活到最后一行。
    #[test]
    fn kill_group_refuses_unsafe_targets() {
        assert!(!group_alive(0) && !group_alive(1) && !group_alive(-1));
        kill_group(0, libc::SIGKILL);
        kill_group(1, libc::SIGKILL);
    }

    #[test]
    fn status_and_pgid_files_are_read_only_when_they_parse() {
        let t = TempRoot::new("statusread");
        let f = t.dir.join("s");
        assert_eq!(read_status_code(&f), None, "文件不在就得是 None");
        std::fs::write(&f, b"").unwrap();
        assert_eq!(
            read_status_code(&f),
            None,
            "写了一半（空）也是 None，下一拍再读"
        );
        std::fs::write(&f, b"boom").unwrap();
        assert_eq!(read_status_code(&f), None);
        std::fs::write(&f, b"0\n").unwrap();
        assert_eq!(read_status_code(&f), Some(0));
        std::fs::write(&f, b" 7 ").unwrap();
        assert_eq!(read_status_code(&f), Some(7));

        assert_eq!(read_pgid(&f), Some(7));
        for bad in ["0", "1", "-3", ""] {
            std::fs::write(&f, bad.as_bytes()).unwrap();
            assert_eq!(read_pgid(&f), None, "{bad:?} 不能当组号");
        }
        std::fs::write(&f, b"4242").unwrap();
        assert_eq!(read_pgid(&f), Some(4242));
    }

    /// 「输出安静了」得真的按时间算，不然命令刚退出就把最后一块输出丢了。
    #[test]
    fn output_tap_reports_silence() {
        let tap = OutputTap::new(Instant::now());
        assert!(!tap.quiet_for(Duration::from_millis(150)), "刚起步不算安静");
        std::thread::sleep(Duration::from_millis(30));
        tap.note();
        assert!(!tap.quiet_for(Duration::from_millis(150)), "刚写过不算安静");
        std::thread::sleep(Duration::from_millis(180));
        assert!(
            tap.quiet_for(Duration::from_millis(150)),
            "180ms 没动静就安静了"
        );
    }

    // --- 宿主模式（桌面原生，不套 proot）-----------------------------------

    /// 宿主模式跑的是**本机的** shell：没有 proot、没有 `-r` / `-w`，
    /// 但**进程模型是同一套** —— runner + 状态文件 + 命令自己的进程组。
    #[test]
    fn host_mode_command_line_is_plain_local_bash() {
        let sb = LinuxSandbox::host(PathBuf::from("/tmp"));
        assert!(sb.is_host());
        // rootfs 是 `/`：沙箱里的绝对路径就是本机路径，所以 read/edit 那条路
        // 一行都不用改
        assert_eq!(sb.rootfs(), Path::new("/"));

        // 状态文件不能落在 `/`（那是用户的根目录）——宿主模式下它的「guest 路径」
        // 就是宿主路径本身
        let (exit_dir, exit_guest) = sb.exit_dir();
        assert_eq!(exit_dir.display().to_string(), exit_guest);
        assert!(!exit_dir.starts_with("/.agent-exit"), "别往 `/` 里写");

        let shell = if Path::new("/bin/bash").is_file() {
            "/bin/bash"
        } else {
            "/bin/sh"
        };
        let status = "/home/u/.local/share/android-agent/exit/x";
        let argv = sb.command_line("echo hi", status);
        assert_eq!(&argv[..2], &["/bin/sh", "-c"], "runner 跑在 POSIX sh 里");
        let runner = &argv[2];
        assert!(
            runner.starts_with("printf '%s' \"$$\" > \"$2.pid\""),
            "宿主模式要报组号（`process_group(0)` 保证了 $$ 就是组号）: {runner}"
        );
        assert!(
            runner.contains(&format!("{shell} -c \"$1\"")),
            "脚本交给上面选出的 shell 跑: {runner}"
        );
        assert_eq!(&argv[3..], &["sh", "echo hi", status]);
        assert!(
            !argv.iter().any(|a| a.contains("proot")),
            "宿主模式命令行里不该有 proot: {argv:?}"
        );

        assert_eq!(
            sb.resolve_path("/etc/hostname").unwrap(),
            Path::new("/etc/hostname")
        );
        assert_eq!(
            sb.resolve_path("/workspace/a.txt").unwrap(),
            Path::new("/tmp/a.txt"),
            "/workspace 就是那个宿主目录"
        );
    }

    /// 没有 rootfs 也算就绪，也**不需要**预装工具（`apk add` 那套不该跑）。
    #[test]
    fn host_mode_is_ready_and_needs_no_provisioning() {
        let sb = LinuxSandbox::host(PathBuf::from("/tmp"));
        assert!(sb.is_ready(), "宿主模式不靠 rootfs 判断");
        assert!(sb.is_provisioned(), "不然启动时会去 apk add");
    }

    /// `agent ps` 在宿主模式**不能**扫 /proc 那套：那会把用户桌面和系统服务
    /// 一起列成「可以 kill」。它只该回一句话，让人去用 ps/pgrep。
    #[test]
    fn host_mode_agent_ps_says_to_use_the_real_ps() {
        let sb = LinuxSandbox::host(PathBuf::from("/tmp"));
        let r = sb.procs(None);
        assert!(r.host);
        assert!(r.leftovers.is_empty() && r.running.is_empty() && r.orphans.is_empty());
        let text = r.to_text();
        assert!(
            text.contains("宿主模式") && text.contains("pgrep"),
            "{text}"
        );
    }

    /// 命令真的在本机跑，cwd 是 workspace，环境是宿主的（PATH 不被重写）。
    #[tokio::test]
    async fn host_mode_runs_on_this_machine_with_the_host_environment() {
        let t = TempRoot::new("hostrun");
        let sb = LinuxSandbox::host(t.dir.clone());
        let r = sb
            .run(
                "pwd; echo PATH_HAS_BIN=$([ -x /bin/sh ] && echo yes || echo no); touch made-by-host-mode",
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert!(
            r.stdout.contains(&t.dir.display().to_string()),
            "{}",
            r.stdout
        );
        assert!(r.using_bash, "宿主模式一定找得到 bash");
        assert!(t.dir.join("made-by-host-mode").is_file(), "改的是真文件");
    }

    /// 命令退出就返回，**不等**它刻意放后台的常驻进程（状态文件当完成信号，
    /// 和沙箱那条路同一个机制）—— 宿主模式没有 proot 会一直占着管道。
    #[tokio::test]
    async fn host_mode_returns_when_the_command_exits_not_when_the_daemon_does() {
        let t = TempRoot::new("hostdaemon");
        let sb = LinuxSandbox::host(t.dir.clone());
        let started = Instant::now();
        let r = sb
            .run(
                "setsid sleep 30 > /dev/null 2>&1 & echo started",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert_eq!(r.code, 0, "stderr: {}", r.stderr);
        assert!(r.stdout.contains("started"));
        assert!(!r.timed_out, "命令自己退出了，不该等到超时");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "等了 {:?} —— 完成信号又退回「管道 EOF」了",
            started.elapsed()
        );
    }

    /// 超时要把**子孙**一起杀掉：宿主模式一样跑 `CommandGuard`（组号从状态文件
    /// 里读），只杀最外层那层 shell 的话子进程会留在用户机器上。
    #[tokio::test]
    async fn host_mode_kills_the_whole_process_group_on_timeout() {
        let t = TempRoot::new("hostkill");
        let sb = LinuxSandbox::host(t.dir.clone());
        let r = sb
            // 子进程把 pid 写进文件，自己睡很久；`sh -c` 那层是孙进程
            .run(
                "sh -c 'echo $$ > alive.pid; sleep 120' & sleep 120",
                Duration::from_millis(600),
            )
            .await
            .unwrap();
        assert!(r.timed_out, "这个脚本就该被超时杀掉");
        // 等信号落地，再看那个 pid 还在不在
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pid: i32 = std::fs::read_to_string(t.dir.join("alive.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "pid {pid} 还活着 —— 进程组没杀干净");
    }

    /// 管道正常关闭（常见情形）：drain 不能多 await 一次 —— 那条路上
    /// `JoinHandle` 已经被 poll 完，再 poll 会 panic 掉整个 App。
    #[tokio::test]
    async fn drain_handles_a_task_that_finishes_in_time() {
        let cap = Arc::new(Mutex::new(Capture::default()));
        let task = tokio::spawn(Capture::read_into(
            Some(&b"hello\n"[..]),
            None,
            cap.clone(),
            None,
        ));
        let got = drain(task, &cap, Duration::from_secs(5)).await;
        assert_eq!(got.head, b"hello\n");
        assert_eq!(got.dropped_bytes, 0);
    }

    /// 管道被孙进程占着不关时：等到 grace 也得把**已经读到的**交出来。
    ///
    /// 原来这段是 `timeout(...).unwrap_or_default()` —— 默认值是空 `Capture`，
    /// 一条 `nohup x &` 就能让输出凭空消失。
    #[tokio::test]
    async fn captured_output_survives_a_pipe_that_never_closes() {
        use tokio::io::AsyncWriteExt;
        let (mut w, r) = tokio::io::duplex(4096);
        let cap = Arc::new(Mutex::new(Capture::default()));
        let task = tokio::spawn(Capture::read_into(Some(r), None, cap.clone(), None));
        w.write_all(b"partial output\n").await.unwrap();
        // 写端不关（就像还在跑的后台进程），读任务永远等不到 EOF
        tokio::time::sleep(Duration::from_millis(50)).await;

        let got = drain(task, &cap, Duration::from_millis(100)).await;
        assert!(
            got.head.starts_with(b"partial output\n"),
            "已经读到的输出不能扔：{:?}",
            String::from_utf8_lossy(&got.head)
        );
    }

    /// 失败必须响亮：超时要说明输出是半截的，被杀要说清是哪个信号。
    #[test]
    fn killed_or_timed_out_is_loud() {
        let mut r = ExecResult {
            code: -1,
            stdout: "half\n".into(),
            stderr: String::new(),
            duration_ms: 10,
            truncated: false,
            timed_out: true,
            full_output_path: None,
            using_bash: true,
            signal: Some(9),
        };
        let text = r.to_text();
        assert!(text.contains("超时"), "{text}");
        assert!(text.contains("可能不全"), "超时要说清输出是半截的：{text}");
        assert!(
            !text.contains("被信号"),
            "超时的信号是我们自己发的，别重复报：{text}"
        );
        assert!(text.contains("half"), "输出不能丢：{text}");

        // 不是超时、但被信号带走：要说清是哪个信号、大概为什么
        r.timed_out = false;
        let text = r.to_text();
        assert!(text.contains("被信号 9"), "{text}");
        assert!(text.contains("OOM"), "信号要翻译成人话：{text}");
    }

    /// 没 bash 时结果里必须明说，并给出装它的命令。
    #[test]
    fn missing_bash_is_announced_with_the_fix() {
        let mut r = ExecResult {
            code: 0,
            stdout: "hi\n".into(),
            stderr: String::new(),
            duration_ms: 0,
            truncated: false,
            timed_out: false,
            full_output_path: None,
            using_bash: false,
            signal: None,
        };
        let text = r.to_text();
        assert!(text.contains("/bin/sh"), "{text}");
        assert!(text.contains("agent-setup --force"), "{text}");
        assert!(text.contains("syntax error"), "{text}");

        // 有 bash 时别唠叨
        r.using_bash = true;
        let text = r.to_text();
        assert!(!text.contains("agent-setup"), "{text}");
    }

    #[test]
    fn path_is_explicit_because_the_host_path_is_wrong_inside_the_sandbox() {
        let env = sandbox().guest_env();
        let path = env.iter().find(|(k, _)| k == "PATH").unwrap();
        assert_eq!(path.1, GUEST_PATH);
        assert!(path.1.contains("/bin"));
    }

    #[test]
    fn proot_env_vars_are_injected_because_android_needs_them() {
        // 这三条在安卓上是**必需**的，不是可选优化：
        //  - PROOT_LOADER / _32：不设则 proot 走内嵌 loader 解压路径，
        //    解到 app_data_file 上被 W^X 拒绝 → "'/bin/sh' is not executable"
        //  - PROOT_TMP_DIR：proot 只认这个名字，不认 TMPDIR；
        //    找不到就退回 /tmp，而 App 写不了 /tmp
        let sb = LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("/native/libproot.so"),
            rootfs: PathBuf::from("/files/rootfs"),
            host: false,
            binds: vec![],
            tmp: Some(PathBuf::from("/cache/proot-tmp")),
            loader: Some(PathBuf::from("/native/libproot-loader.so")),
            loader32: Some(PathBuf::from("/native/libproot-loader32.so")),
            dns: vec![],
            tz_offset_minutes: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        let env: BTreeMap<_, _> = sb.guest_env().into_iter().collect();

        assert_eq!(
            env.get("PROOT_LOADER").map(String::as_str),
            Some("/native/libproot-loader.so")
        );
        assert_eq!(
            env.get("PROOT_LOADER_32").map(String::as_str),
            Some("/native/libproot-loader32.so")
        );
        assert_eq!(
            env.get("PROOT_TMP_DIR").map(String::as_str),
            Some("/cache/proot-tmp")
        );
        // 两者是不同的东西，都得有
        assert_eq!(
            env.get("TMPDIR").map(String::as_str),
            Some("/cache/proot-tmp")
        );

        // 沙箱里没有浏览器：$BROWSER 指向 agent-open，沙箱里任何“开浏览器”
        // 的调用（gh auth login / python webbrowser / xdg-open）都会变成
        // “请宿主打开”。
        assert_eq!(
            env.get("BROWSER").map(String::as_str),
            Some("/usr/local/bin/agent-open")
        );
    }

    #[test]
    fn tz_uses_posix_sign_convention() {
        // UTC+8 在 POSIX 里写成 LCL-8（符号相反），否则 musl 解析不出来
        assert_eq!(parse_posix_tz("LCL-8"), Some(480));
        assert_eq!(parse_posix_tz("UTC+5:30"), Some(-330));
        assert_eq!(parse_posix_tz("CST-8"), Some(480));
    }

    fn render_of(n: usize, body: impl Fn(usize) -> String) -> (String, bool) {
        let text: String = (0..n).map(body).collect();
        let mut cap = Capture::default();
        cap.push(text.as_bytes());
        cap.render()
    }

    #[test]
    fn render_keeps_head_and_tail_with_a_marker() {
        // 跨过 2000 行才截断；截断后首尾各 100 行
        let n = MAX_OUTPUT_LINES + 500;
        let (out, truncated) = render_of(n, |i| format!("line {i}\n"));
        assert!(truncated);
        assert!(out.starts_with("line 0\n"), "{out}");
        assert!(out.contains("line 99\n"), "头要留 100 行：{out}");
        assert!(!out.contains("line 100\n"), "第 101 行不该在：{out}");
        assert!(
            out.contains(&format!("[省略 {} 行]", n - HEAD_LINES - TAIL_LINES)),
            "{out}"
        );
        assert!(
            out.contains(&format!("line {}\n", n - 1)),
            "尾要留到最后一行：{out}"
        );
        assert!(!out.contains(&format!("line {}\n", n - 101)), "{out}");
        // 首 100 + 标记 + 尾 100
        assert_eq!(out.lines().count(), HEAD_LINES + TAIL_LINES + 1);
    }

    #[test]
    fn render_of_1999_lines_is_left_alone() {
        // 触发线是 2000 行：差一行不该被省成 200 行，否则模型得反复续读
        let (out, truncated) = render_of(MAX_OUTPUT_LINES - 1, |i| format!("line {i}\n"));
        assert!(!truncated);
        assert!(!out.contains("[省略"), "{out}");
        assert!(out.contains(&format!("line {}\n", MAX_OUTPUT_LINES - 2)));
        assert_eq!(out.lines().count(), MAX_OUTPUT_LINES - 1);
    }

    #[test]
    fn render_of_a_split_capture_shows_both_ends() {
        // 中间被扔掉过（超过内存上限）：头尾分别来自两块缓冲
        let mut cap = Capture::default();
        cap.head.extend_from_slice(b"head-line\n");
        cap.tail.extend_from_slice(b"tail-line\n");
        cap.dropped_lines = 42;
        cap.dropped_bytes = 1234;
        let (out, truncated) = cap.render();
        assert!(truncated);
        assert!(out.starts_with("head-line\n"), "{out}");
        assert!(out.ends_with("tail-line\n"), "{out}");
        assert!(out.contains("[省略 42 行]"), "{out}");
    }

    #[test]
    fn render_shares_the_byte_budget_between_both_ends() {
        // 每行 ~1KB × 300 行：行数没到、字节先超，两侧各分一半预算
        let (out, truncated) = render_of(300, |i| format!("line{i}{}\n", "x".repeat(990)));
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT_BYTES, "{} 字节", out.len());
        assert!(!out.starts_with("[省略"), "第一行就得留下：{out}");
        assert!(out.contains("line0"), "{out}");
        assert!(out.contains("line299"), "{out}");
        assert!(out.contains("[省略 "));
    }

    #[test]
    fn render_never_splits_a_multibyte_char() {
        // 一整行 150KB 的中文：两侧只能按字节给半行，不能切出半个字
        let (out, truncated) = render_of(1, |_| "中".repeat(MAX_OUTPUT_BYTES));
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT_BYTES, "{} 字节", out.len());
        assert!(!out.contains('\u{fffd}'), "切出了半个字符");
        // 一行都放不下时退回报字节，而不是「省略 0 行」
        assert!(
            out.contains("[省略 ") && !out.contains("[省略 0 行]"),
            "{out}"
        );
    }

    #[test]
    fn render_of_a_few_lines_stays_verbatim() {
        let (out, truncated) = render_of(3, |i| format!("line {i}\n"));
        assert_eq!(out, "line 0\nline 1\nline 2\n");
        assert!(!truncated);
        // 空输出还是空
        assert_eq!(Capture::default().render(), (String::new(), false));
    }

    #[test]
    fn spilled_file_marks_the_gap_it_could_not_keep() {
        // 中间扔过东西 → 文件里也要断开，不能看着像连着的
        let mut cap = Capture::default();
        cap.head.extend_from_slice(b"head\n");
        cap.tail.extend_from_slice(b"tail\n");
        cap.dropped_bytes = 4 << 20;
        let mut body = Vec::new();
        cap.write_to(&mut body);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("中间省略 4.0MB"), "{body}");

        // 没扔过东西（总量不到上限）就不该凭空出现省略提示
        let mut cap = Capture::default();
        cap.push(b"hello\nworld\n");
        let mut body = Vec::new();
        cap.write_to(&mut body);
        assert_eq!(String::from_utf8(body).unwrap(), "hello\nworld\n");
    }

    // --- 路径解析 ---------------------------------------------------------

    struct TempRoot {
        dir: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agent-linux-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            std::fs::create_dir_all(dir.join("root/sub")).unwrap();
            std::fs::create_dir_all(dir.join("etc")).unwrap();
            std::fs::create_dir_all(dir.join("tmp")).unwrap();
            // 必须是普通文件，否则 is_alpine_rootfs 认不出来
            std::fs::write(dir.join("bin/busybox"), b"").unwrap();
            Self { dir }
        }

        fn sandbox(&self) -> LinuxSandbox {
            LinuxSandbox::new(SandboxConfig {
                proot: PathBuf::from("proot"),
                rootfs: self.dir.clone(),
                host: false,
                binds: vec![],
                tmp: None,
                dns: vec!["1.1.1.1".into()],
                tz_offset_minutes: None,
                loader: None,
                loader32: None,
                env: BTreeMap::new(),
                workspace: None,
                shared: None,
            })
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn absolute_and_relative_paths_both_land_inside_the_rootfs() {
        let t = TempRoot::new("paths");
        let sb = t.sandbox();

        assert_eq!(
            sb.resolve_path("/root/a.txt").unwrap(),
            t.dir.join("root/a.txt")
        );
        // 相对路径以 /workspace 为 cwd（没挂手机存储时就是 rootfs 里的目录）
        assert_eq!(
            sb.resolve_path("a.txt").unwrap(),
            t.dir.join("workspace/a.txt")
        );
        assert_eq!(
            sb.resolve_path("/etc/resolv.conf").unwrap(),
            t.dir.join("etc/resolv.conf")
        );
    }

    #[test]
    fn path_traversal_never_leaves_the_rootfs() {
        let t = TempRoot::new("escape");
        let sb = t.sandbox();

        // 先在沙箱视角里归一化：和真实 Linux 一样，根目录的 `..` 还是根目录。
        // 所以这些路径要么被拒，要么落在 rootfs 里 —— 绝不会到宿主机上去
        for bad in [
            "/../outside.txt",
            "/root/../../outside.txt",
            "../../outside.txt",
            "/root/../..",
        ] {
            if let Ok(p) = sb.resolve_path(bad) {
                assert!(
                    p.starts_with(&t.dir),
                    "{bad} 解析到了 rootfs 外面: {}",
                    p.display()
                );
            }
        }
    }

    #[test]
    fn workspace_and_phone_storage_are_mounted_and_confined() {
        let t = TempRoot::new("mounts");
        let phone = t.dir.with_extension("phone");
        let _ = std::fs::remove_dir_all(&phone);
        std::fs::create_dir_all(&phone).unwrap();

        let cfg = t
            .sandbox()
            .config()
            .clone()
            .with_shared_storage(Some(&phone));
        assert!(phone.join("Agent").is_dir(), "应该在手机存储里建好 Agent/");
        let sb = LinuxSandbox::new(cfg);
        assert!(sb.has_shared_storage());

        let ws = phone.join("Agent");
        assert_eq!(
            sb.resolve_path("/workspace/a/b.html").unwrap(),
            ws.join("a/b.html")
        );
        assert_eq!(
            sb.resolve_path("b.html").unwrap(),
            ws.join("b.html"),
            "相对路径落在工作目录"
        );
        assert_eq!(
            sb.resolve_path("/sdcard/DCIM/x.jpg").unwrap(),
            phone.join("DCIM/x.jpg")
        );
        // 想从挂载点 .. 出去：在沙箱视角里归一化后回到 rootfs，到不了宿主机
        assert_eq!(
            sb.resolve_path("/sdcard/../etc/hosts").unwrap(),
            t.dir.join("etc/hosts")
        );
        assert_eq!(
            sb.resolve_path("/workspace/../../x").unwrap(),
            t.dir.join("x")
        );

        let argv = sb.command_line("true", "/.agent-exit/x");
        let joined = argv.join(" ");
        assert!(joined.contains(&format!("-b {}:/sdcard", phone.display())));
        assert!(joined.contains(&format!("-b {}:/workspace", ws.display())));

        // 断开（权限被收回）：回到 rootfs 里的 /workspace
        let off = LinuxSandbox::new(sb.config().clone().with_shared_storage(None));
        assert!(!off.has_shared_storage());
        assert_eq!(
            off.resolve_path("b.html").unwrap(),
            t.dir.join("workspace/b.html")
        );

        let _ = std::fs::remove_dir_all(&phone);
    }

    #[test]
    fn symlinks_pointing_outside_the_rootfs_are_rejected() {
        let t = TempRoot::new("symlink");
        let sb = t.sandbox();

        // 在 rootfs 里放一个指向宿主机 /etc/passwd 的链接
        std::os::unix::fs::symlink("/etc/passwd", t.dir.join("root/evil")).unwrap();
        assert!(
            sb.resolve_path("/root/evil").is_err(),
            "符号链接逃逸没被拦住"
        );
    }

    // --- 文件编辑 ---------------------------------------------------------

    #[test]
    fn edit_file_reads_applies_and_writes_back() {
        let t = TempRoot::new("edit");
        let sb = t.sandbox();
        std::fs::write(t.dir.join("root/conf.txt"), "host=old\nport=80\n").unwrap();

        let out = sb
            .edit_file(
                "/root/conf.txt",
                &[
                    Edit::new("host=old", "host=new"),
                    Edit::new("port=80", "port=443"),
                ],
            )
            .unwrap();

        assert_eq!(
            sb.read_file("/root/conf.txt").unwrap(),
            "host=new\nport=443\n"
        );
        assert_eq!(out.first_changed_line, 1);
        assert!(out.patch.contains("-host=old"));
        assert!(out.patch.contains("+host=new"));
    }

    /// 写文件是「临时文件 + rename」：改完不留临时文件、保留可执行位；
    /// 改一个符号链接时改的是它指向的文件，链接本身还是链接。
    #[test]
    fn edit_writes_atomically_keeping_mode_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempRoot::new("atomic");
        let sb = t.sandbox();
        let script = t.dir.join("root/run.sh");
        std::fs::write(&script, "echo old\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("run.sh", t.dir.join("root/link.sh")).unwrap();

        sb.edit_file("/root/link.sh", &[Edit::new("old", "new")])
            .unwrap();

        assert_eq!(std::fs::read_to_string(&script).unwrap(), "echo new\n");
        let link = std::fs::symlink_metadata(t.dir.join("root/link.sh")).unwrap();
        assert!(link.file_type().is_symlink(), "链接不能被换成普通文件");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "可执行位要保住");
        let leftovers: Vec<_> = std::fs::read_dir(t.dir.join("root"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件要么被 rename 掉，要么清掉");
    }

    #[test]
    fn edit_file_refuses_a_missing_file_with_a_useful_message() {
        let t = TempRoot::new("editmissing");
        let err = t
            .sandbox()
            .edit_file("/root/nope.txt", &[Edit::new("a", "b")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("不存在"), "{err}");
        assert!(
            err.contains("content"),
            "错误信息应该告诉模型怎么新建: {err}"
        );
    }

    #[test]
    fn edit_file_rejects_ambiguous_edits_without_touching_disk() {
        let t = TempRoot::new("editdup");
        let sb = t.sandbox();
        let original = "x\nx\n";
        std::fs::write(t.dir.join("root/f.txt"), original).unwrap();

        assert!(sb.edit_file("/root/f.txt", &[Edit::new("x", "y")]).is_err());
        // 失败时不能留下半成品
        assert_eq!(sb.read_file("/root/f.txt").unwrap(), original);
    }

    // --- 文件读取 ---------------------------------------------------------

    #[test]
    fn read_text_resolves_the_path_and_applies_offset_and_limit() {
        let t = TempRoot::new("read");
        let sb = t.sandbox();
        std::fs::create_dir_all(t.dir.join("workspace")).unwrap();
        std::fs::write(t.dir.join("workspace/a.txt"), "l0\nl1\nl2\nl3\n").unwrap();

        // 相对路径按 cwd `/workspace` 算
        let v = sb.read_text("a.txt", 2, Some(2), false).unwrap();
        assert!(v.content.starts_with("l1\nl2"), "{}", v.content);
        assert!(v.content.contains("offset=4"), "{}", v.content);
    }

    #[test]
    fn oversized_text_files_are_refused_before_reading() {
        assert!(check_text_size(1024, "/root/a.txt").is_ok());
        assert!(check_text_size(MAX_TEXT_BYTES, "/root/a.txt").is_ok());
        let err = check_text_size(MAX_TEXT_BYTES + 1, "/root/big.log")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OOM"), "{err}");
        assert!(err.contains("sed -n"), "要给出替代做法: {err}");
        assert!(err.contains("/root/big.log"), "{err}");
    }

    #[test]
    fn read_text_rejects_missing_files_and_binaries_with_a_useful_message() {
        let t = TempRoot::new("readbad");
        let sb = t.sandbox();

        let err = sb
            .read_text("/root/nope.txt", 1, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("不存在"), "{err}");
        assert!(err.contains("bash"), "应该告诉模型替代做法: {err}");

        std::fs::write(t.dir.join("root/blob.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let err = sb
            .read_text("/root/blob.bin", 1, None, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("UTF-8"), "{err}");
    }

    // --- edit 的 create 模式 ---------------------------------------------

    #[test]
    fn create_file_writes_new_files_and_makes_parent_dirs() {
        let t = TempRoot::new("create");
        let sb = t.sandbox();

        // 相对路径按 /workspace 算；父目录不存在也得能建（模型常写 a/b/c.py）
        let made = sb.create_file("sub/dir/a.py", "print('hi')\n").unwrap();
        assert_eq!((made.lines, made.bytes), (1, 12));
        assert_eq!(
            std::fs::read_to_string(t.dir.join("workspace/sub/dir/a.py")).unwrap(),
            "print('hi')\n"
        );

        // 空文件也算建出来
        let made = sb.create_file("/root/empty", "").unwrap();
        assert_eq!((made.lines, made.bytes), (0, 0));
        assert!(t.dir.join("root/empty").is_file());
    }

    #[test]
    fn create_file_refuses_to_overwrite_and_says_what_to_do() {
        let t = TempRoot::new("createexists");
        let sb = t.sandbox();
        std::fs::write(t.dir.join("root/a.txt"), "原文").unwrap();

        let err = sb
            .create_file("/root/a.txt", "盖掉")
            .unwrap_err()
            .to_string();
        assert!(err.contains("已存在"), "{err}");
        assert!(err.contains("edits"), "要告诉模型怎么改：{err}");
        // 原文一个字都不能动
        assert_eq!(
            std::fs::read_to_string(t.dir.join("root/a.txt")).unwrap(),
            "原文"
        );
    }

    #[test]
    fn create_file_refuses_to_escape_through_a_symlink_or_dotdot() {
        let t = TempRoot::new("createlink");
        let sb = t.sandbox();
        let outside = std::env::temp_dir().join(format!("agent-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();

        // 词法越界（`..`）到不了外面：沙箱视角里 `/root/../..` 就是 `/`
        sb.create_file("/root/../../escape.txt", "x").unwrap();
        assert!(
            t.dir.join("escape.txt").is_file(),
            "`..` 应该被夹在 rootfs 里，落到 <rootfs>/escape.txt"
        );

        // 符号链接越界：模型能在沙箱里 `ln -s <外面> /workspace/link`，
        // 而我们是在宿主上按路径写的 —— 必须往上找真实路径查一遍
        std::fs::create_dir_all(t.dir.join("workspace")).unwrap();
        std::os::unix::fs::symlink(&outside, t.dir.join("workspace/link")).unwrap();
        let err = sb
            .create_file("/workspace/link/evil.txt", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("沙箱外"), "{err}");
        assert!(!outside.join("evil.txt").exists(), "不能写到沙箱外面去");

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn prepare_rootfs_writes_resolv_conf_because_apk_needs_it() {
        let dir = std::env::temp_dir().join(format!("agent-linux-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        // 必须是**普通文件**才算 rootfs —— bin/sh 是绝对符号链接，宿主机解析不到
        std::fs::write(dir.join("bin/busybox"), b"").unwrap();

        let sb = LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("proot"),
            rootfs: dir.clone(),
            host: false,
            binds: vec![],
            tmp: None,
            dns: vec!["9.9.9.9".into(), "1.1.1.1".into()],
            tz_offset_minutes: None,
            loader: None,
            loader32: None,
            env: BTreeMap::new(),
            workspace: None,
            shared: None,
        });
        sb.prepare_rootfs().unwrap();

        let resolv = std::fs::read_to_string(dir.join("etc/resolv.conf")).unwrap();
        assert_eq!(resolv, "nameserver 9.9.9.9\nnameserver 1.1.1.1\n");
        assert!(dir.join("root").is_dir());
        assert!(dir.join("tmp").is_dir());

        // 幂等
        sb.prepare_rootfs().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 沙箱里那个 `agent` 命令是 `prepare_rootfs` 写进去的。
    ///
    /// 它是模型「查看 / 操作自己」的唯一入口（`agent state` / `agent request` /
    /// `agent command`），掉了就等于这个能力没了 —— 而且不会有任何报错，
    /// 只是模型发现 `agent: not found`。
    #[test]
    fn prepare_rootfs_installs_the_agent_cli() {
        let t = TempRoot::new("agentcli");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let cli = t.dir.join("usr/local/bin/agent");
        let body = std::fs::read_to_string(&cli).expect("agent 命令没写进去");
        assert!(body.starts_with("#!/bin/sh"), "要有 shebang");
        assert!(body.contains("/api/debug/request"), "得能看自己的上下文");
        assert!(body.contains("/api/command"), "得能操作手机");
        assert!(body.contains("AGENT_PORT"), "端口从环境变量来");
        // 浏览器现在是**唯一的门**（`browser` 工具删了），多标签那几个子命令
        // 得在 —— 它们曾经只加在工具那边，CLI 没跟上，于是 `new_tab URL`
        // 走进兜底分支把 URL 静默吃掉。
        for sub in [
            "navigate",
            "new_tab",
            "list_tabs",
            "switch_tab",
            "close_tab",
        ] {
            assert!(body.contains(sub), "浏览器子命令 {sub} 掉了");
        }
        // `agent ps` / `agent kill`：常驻服务能活过工具调用，就得能看/停它们
        assert!(body.contains("/api/procs"), "`agent ps` 的后端接口掉了");
        assert!(
            body.contains("agent ps") && body.contains("agent kill"),
            "help 里得列出来"
        );
        assert!(
            body.contains("kill -TERM") && body.contains("kill -KILL"),
            "`agent kill` 要先 TERM 再 KILL（给对 TERM 有反应的进程一个收尾机会）"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cli).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "必须可执行（现在是 {:o}）", mode);
        }

        // 语法交给 sh -n 过一遍（busybox ash 的语法是 POSIX 子集，够了）
        if let Ok(out) = std::process::Command::new("sh")
            .arg("-n")
            .arg(&cli)
            .output()
        {
            assert!(
                out.status.success(),
                "shell 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // 幂等：再跑一次不炸
        sb.prepare_rootfs().unwrap();
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// 沙箱 CLI 的浏览器 case 表和后端（`cdp::ACTIONS`）**必须对得上**。
    ///
    /// 漂了就是「CLI 发得出去、后端回未知动作」的静默失败 —— `new_tab` 那次
    /// 把 URL 吃掉就是这个类别（见 docs/08）。CLI 允许短别名
    /// （`user_agent` 就是 `set_user_agent`），所以别名要在这儿列出来。
    #[cfg(not(target_os = "android"))]
    #[test]
    fn agent_cli_and_cdp_agree_on_the_action_list() {
        let t = TempRoot::new("cdpactions");
        t.sandbox().prepare_rootfs().unwrap();
        let cli = std::fs::read_to_string(t.dir.join("usr/local/bin/agent")).unwrap();

        for action in crate::cdp::ACTIONS {
            // CLI 允许短别名（`user_agent` 就是 `set_user_agent`）
            let name = if *action == "set_user_agent" {
                "user_agent"
            } else {
                action
            };
            assert!(
                cli.contains(&format!("{name})")) || cli.contains(&format!("{name}|")),
                "CLI 的 case 表里没有 {action}（找的是 `{name})`）—— 两边漂了"
            );
        }
        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// `$var` 后面**不能**紧跟中文标点，必须写 `${var}`。
    ///
    /// 沙箱里是 busybox ash，它不认 UTF-8：把它后面的全角字节当成变量名的
    /// 一部分，于是 `$cmd（` 变成一个叫 `cmd（` 的变量、`set -u` 下
    /// 「unbound variable」退出 1 —— 那句提示语永远打不出来（`agent frobnicate`
    /// 就这么稢了很久，直到给 `agent browser` 加参数校验时才被碰出来）。
    #[test]
    fn agent_cli_braces_variables_before_multibyte() {
        let b = AGENT_CLI.as_bytes();
        let name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
        let mut i = 0usize;
        let mut bad: Vec<String> = Vec::new();
        while i < b.len() {
            let start_ok = b[i] == b'$'
                && i + 1 < b.len()
                && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_');
            if start_ok {
                let mut j = i + 1;
                while j < b.len() && name(b[j]) {
                    j += 1;
                }
                if j < b.len() && b[j] >= 0x80 {
                    bad.push(String::from_utf8_lossy(&b[i..j]).into_owned());
                }
                i = j;
                continue;
            }
            i += 1;
        }
        assert!(
            bad.is_empty(),
            "这些变量后面紧跟多字节字符，沙箱的 ash 会把它当成变量名的一部分：{bad:?}"
        );
    }

    /// 预装脚本也得写进去、可执行，而且**工具清单不能是空的**。
    ///
    /// 这个清单掉了不会有任何报错 —— 只是模型又开始每个回合自己 `apk add`。
    #[test]
    fn prepare_rootfs_installs_the_toolset_script() {
        let t = TempRoot::new("agentsetup");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let sh = t.dir.join("usr/local/bin/agent-setup");
        let body = std::fs::read_to_string(&sh).expect("agent-setup 没写进去");
        assert!(body.starts_with("#!/bin/sh"));
        for tool in TOOLSET {
            assert!(body.contains(tool), "清单里的 {tool} 没出现在脚本里");
        }
        assert!(!body.contains("__PACKAGES__"), "模板占位没替换");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sh).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "必须可执行");
        }
        if let Ok(out) = std::process::Command::new("sh").arg("-n").arg(&sh).output() {
            assert!(
                out.status.success(),
                "shell 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // 清单得真管用：模型最常碰的几样必须在
        for want in [
            "ripgrep",
            "fd",
            "jq",
            "python3",
            "curl",
            "bash",
            "github-cli",
        ] {
            assert!(TOOLSET.contains(&want), "预装清单里少了 {want}");
        }

        // 没装过 → false；标记文件版本对上 → true；**版本对不上要重新装**
        assert!(!sb.is_provisioned());
        std::fs::write(t.dir.join("etc/agent-tools"), "1").unwrap();
        assert!(!sb.is_provisioned(), "老版本的标记不该算装过");
        std::fs::write(t.dir.join("etc/agent-tools"), TOOLSET.len().to_string()).unwrap();
        assert!(sb.is_provisioned());

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// 沙箱里没有浏览器：`$BROWSER` 和 xdg-open 这些名字都指向 `agent-open`，
    /// 它只把网址打回输出 —— 由 agent 在回复里转告用户（没有“自动打开”这回事）。
    /// 另外这六个都是“开浏览器”的常见名字，至少别让调用方拿到 not found。
    #[test]
    fn rootfs_gets_the_browser_shims() {
        let t = TempRoot::new("open");
        let sb = t.sandbox();
        sb.prepare_rootfs().unwrap();

        let open = t.dir.join("usr/local/bin/agent-open");
        let body = std::fs::read_to_string(&open).expect("agent-open 没写进去");
        assert!(body.starts_with("#!/bin/sh"));
        // 只剩一个薄壳：真正的逻辑在 `agent open` 里
        assert!(body.contains("exec /usr/local/bin/agent open"), "{body}");

        for alias in [
            "xdg-open",
            "sensible-browser",
            "www-browser",
            "x-www-browser",
            "gnome-open",
            "kde-open",
        ] {
            let p = t.dir.join("usr/local/bin").join(alias);
            let shim =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{alias} 没写进去: {e}"));
            assert!(
                shim.contains("exec /usr/local/bin/agent-open"),
                "{alias}: {shim}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "{alias} 必须可执行");
            }
        }

        if let Ok(out) = std::process::Command::new("sh")
            .arg("-n")
            .arg(&open)
            .output()
        {
            assert!(
                out.status.success(),
                "agent-open 语法不过：{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    /// `agent` 命令靠 `AGENT_PORT` 找到核心；核心还没 bind 时不该注入。
    #[test]
    fn guest_env_carries_the_agent_port_once_known() {
        let t = TempRoot::new("agentport");
        let sb = t.sandbox();

        let has = |k: &str| sb.guest_env().iter().any(|(n, _)| n == k);
        assert!(!has("AGENT_PORT"), "还没 bind 就不该有端口");

        sb.set_agent_port(41234);
        let env: BTreeMap<String, String> = sb.guest_env().into_iter().collect();
        assert_eq!(env.get("AGENT_PORT").map(String::as_str), Some("41234"));
        assert_eq!(
            env.get("AGENT_BASE").map(String::as_str),
            Some("http://127.0.0.1:41234")
        );

        let _ = std::fs::remove_dir_all(&t.dir);
    }

    #[test]
    fn alpine_detection_ignores_symlinks_to_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("agent-linux-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();

        // 模拟 Alpine：bin/sh 是指向绝对路径的符号链接。
        // 用 Path::exists() 判断的话，它会拿宿主机的 /bin/busybox 去解析，
        // 在宿主上没有 → 误判成"不是 rootfs"。
        std::os::unix::fs::symlink("/bin/busybox", dir.join("bin/sh")).unwrap();
        assert!(!is_alpine_rootfs(&dir), "只靠 bin/sh 会误判");

        std::fs::write(dir.join("bin/busybox"), b"").unwrap();
        assert!(is_alpine_rootfs(&dir));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
