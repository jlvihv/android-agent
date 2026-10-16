//! 常驻 shell：沙箱里一直跑着一个 proot + bash，每条命令在它里面 fork 一个子 shell 跑。
//!
//! ## 为什么
//!
//! 手机上每条命令起一次 proot：proot 自己启动、再在它底下 exec `setsid` → `sh` →
//! `bash`（proot 底下每 exec 一次约 19ms —— 每个系统调用都要被 ptrace 拦一道），实测
//! 一条 `true` 要 **~170ms**（不算 HTTP）。而在一个已经跑着的 bash 里 fork 一个子 shell
//! 只要 **~6ms**。agent 一个长回合动辄两百多次 bash 调用，这就是几十秒的纯开销。
//!
//! ## 怎么跑一条命令
//!
//! 核心往三个文件里写东西，然后往常驻 bash 的 stdin 写一行 `<id>`：
//!
//! - `<id>.sh`：模型给的脚本原文
//! - `<id>.run`：`set +m`、`cd`、`export` 这条命令自己的环境，最后 `. <id>.sh`
//! - `<id>.out` / `<id>.err`：两个命名管道（FIFO），命令的 stdout / stderr
//!
//! 常驻 bash（[`DISPATCHER`]）读到 `<id>` 就起一个**后台作业**：
//!
//! ```text
//! ( ( . <id>.run ) </dev/null >"<id>.out" 2>"<id>.err"; printf '%s' "$?" > "<id>" ) &
//! printf '%s' "$!" > "<id>.pid"
//! ```
//!
//! - `set -m`（作业控制）让每个作业**自成一个进程组**，组号就是 `$!`：超时 / 停止时宿主
//!   照旧按组杀（`CommandGuard`），杀不到常驻 bash 自己
//! - 作业里 `set +m`：模型脚本里的 `cmd &` 照旧留在**命令自己的组**里（跟以前一样会被
//!   一起杀）；刻意 `setsid` 脱离的照旧脱离
//! - 两层子 shell：脚本里的 `exit N` 只结束里面那层，外面那层照样把退出码写进状态文件
//!   （不用 `trap EXIT`：模型自己的脚本常常也设 `trap … EXIT`，会把我们的盖掉）
//! - 每个作业是常驻 bash 的 fork：`cd`、`export` 都只活在这一条命令里，下一条命令拿到
//!   的还是干净的状态 —— 隔离程度和以前每条命令一个新 bash 一样
//!
//! 状态文件、pid 文件的名字和以前一模一样，所以等结束、超时、按组杀、收输出全走原来
//! 那套（`linux.rs` 的 `exec`）。差别只在「怎么起」和「输出从哪来」（FIFO 而不是子进程
//! 的管道 —— EOF 语义一样：命令和它没脱离的子孙都关了写端才 EOF）。
//!
//! 常驻的那个挂了（被杀、proot 崩了）下一条命令会重起一个；起不来就退回每条命令一个
//! proot 的老路（`linux.rs`）。

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

/// 常驻 bash 跑的那段脚本（`$1` = 放命令文件的目录，沙箱内路径；`$2` = 它起来之后
/// 写自己 pid 的文件名 —— 每个常驻 shell 一个，见 [`Resident::marker`]）。
///
/// - 先 `. /etc/profile`：以前每条命令的 runner 是 `/bin/sh`，`ENV=/etc/profile` 让它
///   把 profile 读一遍再交给 bash（PATH、LANG 这些都从这儿来）。这里读一次，后面所有
///   作业都是它的 fork，拿到的是同一份
/// - `jobs >/dev/null`：让 bash 把已经结束的作业从作业表里清掉（不然一个长命的常驻
///   bash 跑几千条命令，作业表只增不减）
pub const DISPATCHER: &str = r#"d="$1"
. /etc/profile >/dev/null 2>&1
set -m
printf '%s' "$$" > "$d/$2"
while IFS= read -r id; do
  [ -n "$id" ] || continue
  (
    ( . "$d/$id.run" ) </dev/null >"$d/$id.out" 2>"$d/$id.err"
    printf '%s' "$?" > "$d/$id"
  ) &
  printf '%s' "$!" > "$d/$id.pid"
  jobs >/dev/null 2>&1
done
"#;

/// 常驻的那个进程（宿主上看是 proot；宿主模式的测试里直接是 bash）。
pub struct Resident {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    /// 宿主上那个进程的 pid（`spawn` 时 `process_group(0)`，所以也是组号）
    pid: i32,
    /// 子进程退出了（后台任务 `wait` 到就置上）
    dead: Arc<AtomicBool>,
    /// 它起来之后写自己 pid 的文件（宿主路径）
    marker: PathBuf,
}

impl Resident {
    /// 它起来之后写自己 pid 的文件名（放在命令文件那个目录里）。每个一份 —— 同一个目录
    /// 可能同时有别的常驻 shell（测试并行）、上一次 App 生命留下的。
    pub fn marker_name() -> String {
        format!("resident-{}.pid", uuid::Uuid::now_v7())
    }

    /// 起一个。`argv` 是完整命令行（最后两个元素是 [`DISPATCHER`] 的参数：目录的沙箱内
    /// 路径、[`Self::marker_name`]），`env` 是它的全部环境（会先 `env_clear`），`marker`
    /// 是那个文件的宿主路径。
    pub fn start(
        argv: &[String],
        env: &[(String, String)],
        cwd: Option<&Path>,
        marker: &Path,
    ) -> std::io::Result<Self> {
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (bin, args) = argv
            .split_first()
            .ok_or_else(|| std::io::Error::other("常驻 shell 的命令行是空的"))?;
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(args)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // 自己一个进程组：给核心发的组信号不会打进来，作业的组和它也分开
            .process_group(0);
        if let Some(c) = cwd {
            cmd.current_dir(c);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().map(|p| p as i32).unwrap_or(0);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("常驻 shell 没有 stdin"))?;
        let dead = Arc::new(AtomicBool::new(false));
        let flag = dead.clone();
        // 收尸 + 记下「它没了」：下一条命令看到就重起一个
        tokio::spawn(async move {
            let _ = child.wait().await;
            flag.store(true, Ordering::Relaxed);
        });
        tracing::info!("常驻 shell 起来了（pid {pid}）");
        Ok(Self {
            stdin: tokio::sync::Mutex::new(stdin),
            pid,
            dead,
            marker: marker.to_path_buf(),
        })
    }

    /// 起来了没有（调度脚本写下了自己的 pid）
    pub fn ready(&self) -> bool {
        self.marker.exists()
    }

    pub fn alive(&self) -> bool {
        !self.dead.load(Ordering::Relaxed)
    }

    /// 宿主上那个进程的 pid（`agent ps` 认 tracee 用）
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// 常驻 bash 自己的 pid（`agent ps` 里要把它排除掉）
    pub fn shell_pid(&self) -> Option<i32> {
        std::fs::read_to_string(&self.marker)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// 交给它一条命令（文件已经写好、FIFO 已经建好）。
    pub async fn submit(&self, id: &str) -> std::io::Result<()> {
        if !self.alive() {
            return Err(std::io::Error::other("常驻 shell 已经退出"));
        }
        let mut w = self.stdin.lock().await;
        w.write_all(format!("{id}\n").as_bytes()).await?;
        w.flush().await
    }

    /// 关掉它（升级 Alpine 前、换 rootfs 前）。作业各有各的进程组，不在这里杀。
    pub fn stop(&self) {
        if self.pid > 1 {
            // SAFETY: kill(2) 只读参数
            unsafe { libc::kill(-self.pid, libc::SIGKILL) };
        }
        self.dead.store(true, Ordering::Relaxed);
    }
}

impl Drop for Resident {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.marker);
    }
}

/// 建一个命名管道。
pub fn mkfifo(path: &Path) -> std::io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("路径里有 NUL"))?;
    // SAFETY: c 是合法的 NUL 结尾字符串
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// 打开命名管道的读端。**会等到写端出现**（作业里的重定向打开它）—— 所以放在阻塞
/// 线程里；打开之后转成异步的、非阻塞的读端（写端都关了就 EOF，跟子进程的管道一样）。
pub async fn open_fifo_reader(path: PathBuf) -> Option<tokio::net::unix::pipe::Receiver> {
    let file = tokio::task::spawn_blocking(move || std::fs::File::open(path))
        .await
        .ok()?
        .ok()?;
    tokio::net::unix::pipe::Receiver::from_file(file).ok()
}

/// 有读端正卡在 `open` 上等写端（作业根本没起来）：以写方式开一下再关，把它放出来。
/// 没人在等的话 `ENXIO`，什么都不做。
pub fn unblock_fifo(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path);
}

/// shell 单引号转义：`it's` → `'it'\''s'`
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// 脚本要不要单独起一个 bash 跑（而不是在常驻 shell 的 fork 里直接 `.` 进来）。
///
/// 只有一种情况：脚本里用了 `$$`。子 shell 里的 `$$` 是**常驻 bash 自己的** pid（bash
/// 规定的，改不了）—— `kill $$` 会把常驻 shell 整个杀掉，`/tmp/x.$$` 这种临时文件名在
/// 两条同时跑的命令之间会撞上。单独 `exec` 一个 bash，`$$` 就是这条命令自己的了；代价是
/// 多一次 bash 启动（手机上 ~45ms，还是比每条命令一个 proot 快得多）。
pub fn needs_own_shell(script: &str) -> bool {
    script.contains("$$") || script.contains("${$}")
}

/// 一条命令的 `<id>.run`：关掉作业控制、进 cwd、导出这条命令自己的环境，再执行脚本
/// （`own_shell` 见 [`needs_own_shell`]）。
pub fn run_file(
    cwd: &str,
    env: &[(String, String)],
    script_guest: &str,
    own_shell: bool,
) -> String {
    let mut out = String::from(
        // 作业控制只给常驻 bash 自己用（每个作业一个进程组）；脚本里的 `cmd &` 要留在
        // 命令自己的组里，跟以前一样被一起杀
        "set +m\n",
    );
    out.push_str(&format!("cd -- {} 2>/dev/null || cd /\n", sh_quote(cwd)));
    for (k, v) in env {
        out.push_str(&format!("export {k}={}\n", sh_quote(v)));
    }
    if own_shell {
        // `$(< 文件)` 是 bash 的内建读文件（不 exec 一个 cat）
        out.push_str(&format!(
            "exec \"$BASH\" -c \"$(< {q})\" \"$BASH\"\n",
            q = sh_quote(script_guest)
        ));
    } else {
        out.push_str(&format!(". {}\n", sh_quote(script_guest)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_single_quotes_and_newlines() {
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        let f = run_file("/w/it's", &[("K".into(), "v\n2".into())], "/x/1.sh", false);
        assert!(f.starts_with("set +m\n"));
        assert!(f.contains(r"cd -- '/w/it'\''s'"));
        assert!(f.contains("export K='v\n2'"));
        assert!(f.ends_with(". '/x/1.sh'\n"));
        assert!(run_file("/", &[], "/x/1.sh", true).contains("exec \"$BASH\" -c"));
        assert!(needs_own_shell("kill $$") && needs_own_shell("echo ${$}"));
        assert!(!needs_own_shell("echo $HOME $1 $?"));
    }
}
