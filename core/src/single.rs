//! 单实例：这台机器（这个用户）上只跑一个核心。
//!
//! 为什么要管这件事：同时起两个核心**不会有任何报错**，但会静默地互相干扰 ——
//! 都是宿主模式、都叫「jl」时，mDNS 里同名服务没法共存（后起的赢，先起的那个广播
//! 进程悄悄死掉，手机只找得到其中一个）；两个都写会话库时，各自的内存态又会互相
//! 覆盖。这两个坑都实际踩过（见 docs/11 的「局域网」一节和我加在广播上的检查）。
//! 所以默认是：**抢不到就起不来，并说清是谁在跑**。
//!
//! 用什么做的：`flock` 一把文件锁（[`default_path`]）。不用 PID 文件 —— PID 会复用、
//! 进程被 SIGKILL 时文件还留着，得自己判断「陈没陈」；文件锁由内核在进程消失时释放
//! （SIGKILL 也一样），不存在陈旧锁。锁文件里写一份「谁在跑」（pid / 端口 / 命令行），
//! 好在拒绝的时候直接把话说明白，而不是只丢一句「已经有一个了」。
//!
//! 确实要同时跑两个（DOM 审计、基准测试）：`AGENT_MULTI=1`，见 [`MULTI_ENV`]。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// 绕过单实例检查：`AGENT_MULTI=1`（审计 / 基准测试要自己起第二个核心）。
pub const MULTI_ENV: &str = "AGENT_MULTI";

/// 锁文件：`$XDG_DATA_HOME/android-agent/core.lock`（和库、`agent` 命令同一个根）。
///
/// **故意不跟 `AGENT_DATA_DIR` 走**：那个是会话库的位置，本来就允许分开；而单实例
/// 说的是「这台机器上一个」—— 各写各的库照样会互相干扰（mDNS 同名挤掉、两边的
/// 内存态各说各话）。放在数据目录之外，换库也挡得住。
pub fn default_path() -> PathBuf {
    crate::desktop::data_root().join("core.lock")
}

/// 现在拿着锁的是谁（从锁文件里读回来的）。读不懂就是全 `None`。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub cmd: Option<String>,
}

impl Holder {
    /// 一句话说清「谁在跑」，给被拒的那个核心打给人看。
    pub fn describe(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        if let Some(pid) = self.pid {
            bits.push(format!("pid {pid}"));
        }
        if let Some(port) = self.port {
            bits.push(format!("端口 {port}"));
        }
        if let Some(cmd) = &self.cmd {
            bits.push(cmd.clone());
        }
        if bits.is_empty() {
            return "（锁文件里没写细节，可能是别的工具占着）".into();
        }
        bits.join("，")
    }

    /// 停掉它要敲的那句（知道 pid 才给得出）。
    pub fn kill_hint(&self) -> Option<String> {
        self.pid.map(|p| format!("kill {p}"))
    }
}

/// 拿着锁。`Drop` 就放（进程结束内核也会放，包括 SIGKILL）。
#[derive(Debug)]
pub struct Guard {
    /// 打不开锁文件时是 `None`（见 `acquire`：锁不住也不能不让核心起）。
    /// 这个字段本身不重要，**它的存活**才是（文件关掉 = 锁放掉）。
    _file: Option<File>,
}

/// 抢这把锁。抢到 = `Ok`（顺手写下「我是谁」，下一个被拒的就能看见）；
/// 已经有人在跑 = `Err(那个人的信息)`。
///
/// `AGENT_MULTI=1` 时直接 `Ok` 且**完全不碰锁**：审计 / 基准要同时跑第二个核心，
/// 而它既不该被挡，也不该反过来挡住用户自己那个核心起来。
pub fn acquire(path: &Path, port: u16) -> Result<Guard, Holder> {
    if multi_allowed() {
        tracing::info!("{MULTI_ENV} 已设：跳过单实例检查（这台机器上会有不止一个核心）");
        return Ok(Guard { _file: None });
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            // 锁不上 ≠ 不能起：宁可让人用，也别因为一个文件的权限把核心挡在门外
            tracing::warn!(
                "打不开锁文件 {}（{e}）—— 这次跳过单实例检查",
                path.display()
            );
            return Ok(Guard { _file: None });
        }
    };
    // SAFETY: flock 只碰这个 fd；失败返回非 0，不写任何内存
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        // 有人在跑：把「谁」读回来（读不到就是空的 Holder，照样拦）
        return Err(read_holder(&mut file).unwrap_or_default());
    }
    // 抢到了：写下自己，好让下一个被拒的核心有话可说
    let cmd: String = std::env::args().collect::<Vec<_>>().join(" ");
    let info = serde_json::json!({
        "pid": std::process::id(),
        "port": port,
        "cmd": cmd.chars().take(200).collect::<String>(),
    });
    let _ = file.set_len(0);
    let _ = writeln!(file, "{info}");
    let _ = file.flush();
    Ok(Guard { _file: Some(file) })
}

/// `AGENT_MULTI` 算开着吗（`1`/`true`/`yes`/`on`；`0`/`false`/`no`/空当没开）。
fn multi_allowed() -> bool {
    std::env::var(MULTI_ENV).is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// 把锁文件里那份「谁在跑」读回来。读不懂就当没有（老版本写的、或者被写坏了）。
fn read_holder(file: &mut File) -> Option<Holder> {
    let mut raw = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut raw).ok()?;
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    Some(Holder {
        pid: v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
        port: v.get("port").and_then(|p| p.as_u64()).map(|p| p as u16),
        cmd: v.get("cmd").and_then(|c| c.as_str()).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个自己的锁文件（测试是并行的，别互相抢）
    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agent-single-{name}-{}.lock", std::process::id()))
    }

    #[test]
    fn the_second_core_is_turned_away_and_learns_who_is_running() {
        let path = tmp("second");
        let _ = std::fs::remove_file(&path);

        let first = acquire(&path, 8787).expect("第一个应该拿到锁");
        let who = acquire(&path, 8789).unwrap_err();
        assert_eq!(who.pid, Some(std::process::id()), "报的是先起那个的 pid");
        assert_eq!(who.port, Some(8787));
        assert!(who.kill_hint().unwrap().starts_with("kill "));
        assert!(who.describe().contains("8787"), "{}", who.describe());

        // 锁一放（进程还活着也算），下一个就能起
        drop(first);
        assert!(acquire(&path, 8789).is_ok());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_leftover_file_from_a_killed_core_blocks_nothing() {
        // SIGKILL 之后锁文件还躺在那里，但锁已经被内核放掉了：不该挡路。
        // 「陈不陈」这件事我们不用管，这正是选 flock 而不是 PID 文件的原因。
        let path = tmp("stale");
        std::fs::write(&path, "{\"pid\": 1, \"port\": 1, \"cmd\": \"早就没了\"}\n").unwrap();
        let g = acquire(&path, 8787).expect("文件在、但没人拿锁，应该能起");
        drop(g);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_garbled_lock_file_still_blocks_and_still_says_something() {
        // 读不懂也得拦住（宁可多问一句），而且不能打一串空话
        let path = tmp("garbled");
        let _ = std::fs::remove_file(&path);
        let first = acquire(&path, 8787).expect("先起一个");
        std::fs::write(&path, "不是 JSON").unwrap();
        let who = acquire(&path, 8789).unwrap_err();
        assert_eq!(who, Holder::default());
        assert!(who.describe().contains("没写细节"), "{}", who.describe());
        assert!(who.kill_hint().is_none());
        drop(first);
        let _ = std::fs::remove_file(&path);
    }
}
