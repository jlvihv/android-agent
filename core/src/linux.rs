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
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::editor::{self, Edit, EditOutcome};

/// 单次命令的默认超时
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// 回给模型的输出上限。超过就只留**尾部**。
/// 命令的错误和结论通常在末尾，砍尾部等于砍掉最有用的信息。
pub const MAX_OUTPUT_LINES: usize = 2000;
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// proot 可执行文件。Android 上是 `<nativeLibraryDir>/libproot.so`
    pub proot: PathBuf,
    /// rootfs 根目录
    pub rootfs: PathBuf,
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
            s.push_str("\n[输出已截断，只保留尾部]");
            if let Some(p) = &self.full_output_path {
                s.push_str(&format!("\n[完整输出在 {p}]"));
            }
        }
        if self.timed_out {
            s.push_str("\n[超时，已终止]");
        }
        if s.is_empty() {
            s.push_str("(无输出)");
        }
        s.push_str(&format!("\n[exit {}]", self.code));
        s
    }
}

pub struct LinuxSandbox {
    cfg: SandboxConfig,
}

impl LinuxSandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }

    /// 从环境变量构造（桌面开发用）。
    ///
    /// `AGENT_LINUX_ROOTFS` 指向 rootfs；`AGENT_LINUX_PROOT` 可选，默认从 PATH 找 `proot`。
    pub fn from_env() -> Option<Self> {
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
    pub fn prepare_rootfs(&self) -> anyhow::Result<()> {
        let rootfs = &self.cfg.rootfs;

        for d in ["root", "tmp", "var/tmp", "workspace", "sdcard"] {
            std::fs::create_dir_all(rootfs.join(d))?;
        }

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

    /// 构造完整命令行。抽出来单独测试，不需要真的跑 proot。
    ///
    /// 对应 OpenMinis `PRootKernel.buildProotCommand()`：
    /// ```text
    /// <proot> -0 -l -r <rootfs> -b /dev -b /proc -b /sys -w /workspace
    ///         [-b <host>:<linux> ...] /bin/sh -c "<script>"
    /// ```
    pub fn command_line(&self, script: &str) -> Vec<String> {
        let mut cmd: Vec<String> = Vec::with_capacity(20);

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

        // 子进程随 proot 一起死，避免留下孤儿
        arg!("--kill-on-exit");

        arg!("-w", WORKSPACE);

        arg!("/bin/sh", "-c", script);

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
        env.insert("TERM".into(), "dumb".into()); // 非交互，别输出 ANSI
        env.insert("NO_COLOR".into(), "1".into());
        env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
        env.insert("GOMAXPROCS".into(), "2".into()); // 手机上限
        // proot 的 link2symlink 会留 .l2s.* 哨兵文件，uv 默认 hardlink
        // 模式会二次 link 它们并拿到 EPERM
        env.insert("UV_LINK_MODE".into(), "symlink".into());
        // TZ 必须是 POSIX 格式（符号与 UTC 偏移相反）：musl 的 TZ 解析器
        // 认不出 "GMT+8" 这种带 +/- 的缩写
        env.insert("TZ".into(), posix_tz(self.cfg.tz_offset_minutes));

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
        let argv = self.command_line(script);
        let (bin, args) = argv.split_first().expect("command_line 至少有一个元素");

        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args);
        cmd.env_clear();
        for (k, v) in self.guest_env() {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // 让 proot 自己成为进程组组长，超时好整组杀
        cmd.kill_on_drop(true);

        if let Some(tmp) = &self.cfg.tmp {
            let _ = std::fs::create_dir_all(tmp);
        }

        let started = Instant::now();
        let mut child = cmd.spawn()?;

        // 边跑边读，内存里只留尾巴（见 `Capture`）。
        //
        // 原来是 wait_with_output() —— 整段输出全攒在内存里。`yes`、
        // `cat /dev/urandom | base64` 这类命令 60 秒能吐出几个 GB，
        // 手机上直接 OOM 带走整个 App。
        let out_task = tokio::spawn(Capture::read(child.stdout.take(), live.clone()));
        let err_task = tokio::spawn(Capture::read(child.stderr.take(), live));

        let (status, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(r) => (Some(r?), false),
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                (None, true)
            }
        };

        // 管道可能被沙箱里的孙进程继承着不关 —— 别为它无限等
        let grace = Duration::from_secs(2);
        let stdout = tokio::time::timeout(grace, out_task).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let stderr = tokio::time::timeout(grace, err_task).await.ok().and_then(|r| r.ok()).unwrap_or_default();
        let dropped = stdout.dropped + stderr.dropped;
        let out = std::process::Output {
            status: status.unwrap_or_default(),
            stdout: stdout.buf,
            stderr: stderr.buf,
        };

        let (stdout, t1) = tail_clamp(&out.stdout);
        let (stderr, t2) = tail_clamp(&out.stderr);
        let truncated = t1 || t2 || dropped > 0;

        // 截断了就把完整输出落进沙箱的 /tmp，并告诉模型路径 ——
        // 它可以再用 grep/sed 去翻，不用把几十 MB 塞回上下文
        let full_output_path = if truncated {
            self.spill_full_output(&out.stdout, &out.stderr)
        } else {
            None
        };

        Ok(ExecResult {
            code: if timed_out { -1 } else { out.status.code().unwrap_or(-1) },
            stdout,
            stderr,
            duration_ms: started.elapsed().as_millis() as u64,
            truncated,
            // 超时也把已经产出的输出带回去 —— 原来直接丢空，模型只知道
            // "超时了"，看不到卡在哪一步
            timed_out,
            full_output_path,
        })
    }

    /// 把完整输出写进 rootfs 的 `/tmp`，返回沙箱内的路径。
    fn spill_full_output(&self, stdout: &[u8], stderr: &[u8]) -> Option<String> {
        let name = format!("agent-out-{}.log", uuid::Uuid::now_v7());
        let host = self.cfg.rootfs.join("tmp").join(&name);
        std::fs::create_dir_all(host.parent()?).ok()?;

        let mut body = Vec::with_capacity(stdout.len() + stderr.len() + 32);
        body.extend_from_slice(stdout);
        if !stderr.is_empty() {
            body.extend_from_slice(b"\n--- stderr ---\n");
            body.extend_from_slice(stderr);
        }
        std::fs::write(&host, body).ok()?;
        Some(format!("/tmp/{name}"))
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
        anyhow::ensure!(!guest.trim().is_empty(), "路径为空");

        // 先在沙箱的视角里归一化，再决定落到哪个宿主目录
        let abs = if guest.starts_with('/') {
            PathBuf::from(guest)
        } else {
            Path::new(WORKSPACE).join(guest)
        };
        let g = lexical_normalize(&abs);

        let mounts = [(SHARED, &self.cfg.shared), (WORKSPACE, &self.cfg.workspace)];
        for (at, host) in mounts {
            let Some(host) = host else { continue };
            if let Ok(rest) = g.strip_prefix(at) {
                let root = lexical_normalize(host);
                return confine(&root, &root.join(rest), guest);
            }
        }

        let root = lexical_normalize(&self.cfg.rootfs);
        let rel = g.strip_prefix("/").unwrap_or(&g);
        confine(&root, &root.join(rel), guest)
    }

    /// `edit` 工具：读文件 → 按原文替换 → 写回。
    ///
    /// 整个过程读一次写一次，中间不落盘（见 `editor` 模块的设计说明）。
    pub fn edit_file(&self, guest_path: &str, edits: &[Edit]) -> anyhow::Result<EditOutcome> {
        let host = self.resolve_path(guest_path)?;
        anyhow::ensure!(
            host.is_file(),
            "不是文件或不存在: {guest_path}（要新建文件请用 bash 的 cat/tee）"
        );

        let raw = std::fs::read_to_string(&host)
            .map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}（只能编辑 UTF-8 文本）"))?;

        let outcome = editor::apply(&raw, edits, guest_path)?;
        std::fs::write(&host, &outcome.content)
            .map_err(|e| anyhow::anyhow!("写入失败 {guest_path}: {e}"))?;

        Ok(outcome)
    }

    /// 写文件（`bash` 之外的小工具，也供测试用）
    pub fn read_file(&self, guest_path: &str) -> anyhow::Result<String> {
        let host = self.resolve_path(guest_path)?;
        std::fs::read_to_string(&host)
            .map_err(|e| anyhow::anyhow!("读取失败 {guest_path}: {e}"))
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

/// 一路输出的尾巴。超过 `CAPTURE_LIMIT` 就把前一半扔掉，只记扔了多少。
///
/// 回给模型的最多 50 KB，落盘的完整输出也只需要"足够翻查"，所以 4 MB
/// 的尾巴足够；关键是内存有上界。
#[derive(Default)]
struct Capture {
    buf: Vec<u8>,
    dropped: u64,
}

const CAPTURE_LIMIT: usize = 4 << 20;

impl Capture {
    async fn read<R: tokio::io::AsyncRead + Unpin>(
        src: Option<R>,
        live: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    ) -> Capture {
        use tokio::io::AsyncReadExt;
        let mut cap = Capture::default();
        let Some(mut src) = src else { return cap };
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match src.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(tx) = &live {
                        let _ = tx.send(chunk[..n].to_vec());
                    }
                    cap.push(&chunk[..n]);
                }
            }
        }
        cap
    }

    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if self.buf.len() > CAPTURE_LIMIT {
            let cut = self.buf.len() - CAPTURE_LIMIT / 2;
            self.buf.drain(..cut);
            self.dropped += cut as u64;
        }
    }
}

/// 只留**尾部**：先砍行数，再砍字节。
///
/// 与头截断相比，尾巴里有报错信息和最终结论 —— 那些才是模型需要的。
///
/// 保留末尾换行（用 `lines()` + `join()` 会把它吃掉）。
fn tail_clamp(bytes: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut truncated = false;

    // 行数：'\n' 的个数，再加上末尾那段没有换行的
    let newlines = text.matches('\n').count();
    let lines = newlines + usize::from(!text.is_empty() && !text.ends_with('\n'));

    let mut s: String = if lines > MAX_OUTPUT_LINES {
        truncated = true;
        let drop = lines - MAX_OUTPUT_LINES;
        // 第 drop 个 '\n' 之后就是保留区的起点
        let mut seen = 0usize;
        let mut cut = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == drop {
                    cut = i + 1;
                    break;
                }
            }
        }
        text[cut..].to_string()
    } else {
        text.into_owned()
    };

    if s.len() > MAX_OUTPUT_BYTES {
        truncated = true;
        let mut start = s.len() - MAX_OUTPUT_BYTES;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        s = s[start..].to_string();
    }

    (s, truncated)
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

    #[tokio::test]
    async fn capture_keeps_memory_bounded_and_keeps_the_tail() {
        // 模拟一个狂吐输出的命令：远超上限
        let total = CAPTURE_LIMIT * 3 + 123;
        let mut data = vec![b'x'; total - 4];
        data.extend_from_slice(b"END\n");
        let cap = Capture::read(Some(&data[..]), None).await;
        assert!(cap.buf.len() <= CAPTURE_LIMIT, "内存要有上界");
        assert!(cap.buf.ends_with(b"END\n"), "尾巴不能丢");
        assert_eq!(cap.buf.len() as u64 + cap.dropped, total as u64);
    }
    use super::*;

    fn sandbox() -> LinuxSandbox {
        LinuxSandbox::new(SandboxConfig {
            proot: PathBuf::from("/native/libproot.so"),
            rootfs: PathBuf::from("/data/files/alpine-rootfs"),
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
        let argv = sandbox().command_line("echo hi");
        assert_eq!(argv[0], "/native/libproot.so");
        assert_eq!(argv[1], "-0");
        assert_eq!(argv[2], "--link2symlink");
        assert_eq!(&argv[3..5], &["-r", "/data/files/alpine-rootfs"]);
        assert_eq!(&argv[5..11], &["-b", "/dev", "-b", "/proc", "-b", "/sys"]);
        assert_eq!(&argv[11..13], &["-b", "/host/share:/mnt/share"]);
        assert_eq!(argv[13], "--kill-on-exit");
        assert_eq!(&argv[14..16], &["-w", "/workspace"]);
        assert_eq!(&argv[16..], &["/bin/sh", "-c", "echo hi"]);
    }

    #[test]
    fn script_is_passed_as_one_argv_element_not_shell_escaped() {
        // 关键：脚本原样作为单个参数传，不做任何转义 —— 因为我们是
        // 直接 execve，没有中间 shell 会二次解析它
        let nasty = "echo \"a b\"; ls | grep x; echo '$(whoami)'";
        let argv = sandbox().command_line(nasty);
        assert_eq!(argv.last().unwrap(), nasty);
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
        assert_eq!(env.get("TMPDIR").map(String::as_str), Some("/cache/proot-tmp"));
    }

    #[test]
    fn tz_uses_posix_sign_convention() {
        // UTC+8 在 POSIX 里写成 LCL-8（符号相反），否则 musl 解析不出来
        assert_eq!(parse_posix_tz("LCL-8"), Some(480));
        assert_eq!(parse_posix_tz("UTC+5:30"), Some(-330));
        assert_eq!(parse_posix_tz("CST-8"), Some(480));
    }

    #[test]
    fn tail_clamp_keeps_the_end_not_the_start() {
        // 报错和结论在尾巴里，所以砍头不砍尾
        let mut body = String::new();
        for i in 0..(MAX_OUTPUT_LINES + 50) {
            body.push_str(&format!("line {i}\n"));
        }
        let (out, truncated) = tail_clamp(body.as_bytes());
        assert!(truncated);
        assert!(out.contains(&format!("line {}\n", MAX_OUTPUT_LINES + 49)), "最后一行丢了");
        assert!(!out.contains("line 0\n"), "开头没被砍掉");
        assert!(out.lines().count() <= MAX_OUTPUT_LINES);
    }

    #[test]
    fn tail_clamp_respects_utf8_boundaries() {
        let big = "中".repeat(MAX_OUTPUT_BYTES); // 每个 3 字节，必超
        let (out, truncated) = tail_clamp(big.as_bytes());
        assert!(truncated);
        assert!(out.len() <= MAX_OUTPUT_BYTES);
        assert!(out.chars().all(|c| c == '中'), "切出了半个字符");
    }

    #[test]
    fn tail_clamp_leaves_small_output_alone() {
        let (out, truncated) = tail_clamp(b"hello\nworld\n");
        assert_eq!(out, "hello\nworld\n");
        assert!(!truncated);
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

        assert_eq!(sb.resolve_path("/root/a.txt").unwrap(), t.dir.join("root/a.txt"));
        // 相对路径以 /workspace 为 cwd（没挂手机存储时就是 rootfs 里的目录）
        assert_eq!(sb.resolve_path("a.txt").unwrap(), t.dir.join("workspace/a.txt"));
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
                assert!(p.starts_with(&t.dir), "{bad} 解析到了 rootfs 外面: {}", p.display());
            }
        }
    }

    #[test]
    fn workspace_and_phone_storage_are_mounted_and_confined() {
        let t = TempRoot::new("mounts");
        let phone = t.dir.with_extension("phone");
        let _ = std::fs::remove_dir_all(&phone);
        std::fs::create_dir_all(&phone).unwrap();

        let cfg = t.sandbox().config().clone().with_shared_storage(Some(&phone));
        assert!(phone.join("Agent").is_dir(), "应该在手机存储里建好 Agent/");
        let sb = LinuxSandbox::new(cfg);
        assert!(sb.has_shared_storage());

        let ws = phone.join("Agent");
        assert_eq!(sb.resolve_path("/workspace/a/b.html").unwrap(), ws.join("a/b.html"));
        assert_eq!(sb.resolve_path("b.html").unwrap(), ws.join("b.html"), "相对路径落在工作目录");
        assert_eq!(sb.resolve_path("/sdcard/DCIM/x.jpg").unwrap(), phone.join("DCIM/x.jpg"));
        // 想从挂载点 .. 出去：在沙箱视角里归一化后回到 rootfs，到不了宿主机
        assert_eq!(sb.resolve_path("/sdcard/../etc/hosts").unwrap(), t.dir.join("etc/hosts"));
        assert_eq!(sb.resolve_path("/workspace/../../x").unwrap(), t.dir.join("x"));

        let argv = sb.command_line("true");
        let joined = argv.join(" ");
        assert!(joined.contains(&format!("-b {}:/sdcard", phone.display())));
        assert!(joined.contains(&format!("-b {}:/workspace", ws.display())));

        // 断开（权限被收回）：回到 rootfs 里的 /workspace
        let off = LinuxSandbox::new(sb.config().clone().with_shared_storage(None));
        assert!(!off.has_shared_storage());
        assert_eq!(off.resolve_path("b.html").unwrap(), t.dir.join("workspace/b.html"));

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
                &[Edit::new("host=old", "host=new"), Edit::new("port=80", "port=443")],
            )
            .unwrap();

        assert_eq!(sb.read_file("/root/conf.txt").unwrap(), "host=new\nport=443\n");
        assert_eq!(out.first_changed_line, 1);
        assert!(out.patch.contains("-host=old"));
        assert!(out.patch.contains("+host=new"));
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
        assert!(err.contains("bash"), "错误信息应该告诉模型该怎么办: {err}");
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
