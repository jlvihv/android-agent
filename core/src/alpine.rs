//! 沙箱的 Alpine 原地升级（设置页里点一下）。
//!
//! 为什么是**原地**升级（改 `repositories` + `apk upgrade --available`，Alpine
//! 官方的升级方式），而不是换一个新的 minirootfs 重新解压：rootfs 里有用户的
//! 东西 —— `/root`（`gh` 的登录态、`.gitconfig`、模型建的项目和 venv）、后来
//! `apk add` 的包。重解等于全丢；原地升级一样不丢，还能跨好几个版本直接跳。
//!
//! 流程（`run`）：
//!
//! 1. 前置检查：proot 沙箱才有（宿主模式没有 Alpine）、没有回合在跑、剩余空间
//!    够放一份快照 + 下载；
//! 2. **快照**：在沙箱**外面**把整个 rootfs 复制一份（`<rootfs>.upgrade-bak`）。
//!    不能在沙箱里 tar —— 升级要是把 musl 弄坏了，沙箱里什么都跑不了，那份
//!    tar 也就解不回来；目录换回去不依赖沙箱还能不能用；
//! 3. 把 `repositories` 里的 `/v3.xx/` 换成目标版本（镜像域名不动）；
//! 4. `apk update && apk upgrade --available`，输出实时进进度日志；
//! 5. **验收不看 apk 的退出码**（proot 下 trigger 常常乱报非零，见 docs/02），
//!    看 `/etc/alpine-release` 是不是目标版本、`sh` / `bash` / `apk` 能不能真跑；
//! 6. 过了就删快照；没过就把快照换回去。
//!
//! 升级期间沙箱拒绝别的命令（`LinuxSandbox::set_maintenance`）：一边换 musl
//! 一边让模型跑命令，两边都可能拿到半新半旧的库。App 在升级中途被杀的话，
//! 下次启动 `recover_interrupted` 看见快照还在，就整个换回去。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::linux::LinuxSandbox;
use crate::state::AppState;

/// 快照目录的后缀（和 rootfs 同级，同一个文件系统 —— 换回去是 rename）
const BAK_SUFFIX: &str = ".upgrade-bak";
/// 失败时把升坏的那份先挪开（再删），不直接删：删 600MB 的树要好几秒，
/// 挪开是瞬间的，用户的沙箱能马上回来
const FAILED_SUFFIX: &str = ".upgrade-failed";
/// 快照复制**完**之后才写的标记。没有它的快照是半份（备份阶段被杀），不能拿来换
const SNAPSHOT_OK: &str = ".upgrade-snapshot-ok";
/// 快照不复制的目录：临时文件（重启就该没）、我们自己的状态文件
const SNAPSHOT_SKIP: &[&str] = &["tmp", ".agent-exit"];
/// `apk upgrade` 给多久。手机上实测 3.21 → 3.24（178 个包、~300MB）3 分 20 秒
const APK_TIMEOUT: Duration = Duration::from_secs(40 * 60);
/// 进度日志只留最后这么多行（界面上看的是尾巴）
const LOG_KEEP: usize = 200;
/// 除了快照，还要给下载 + 解包留的余量
const SPACE_MARGIN: u64 = 600 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    #[default]
    Idle,
    Running,
    Done,
    Failed,
}

/// 一次升级的进度（`GET /api/alpine` 里的 `job`）。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub state: JobState,
    pub from: String,
    pub to: String,
    /// 现在在做哪一步（给人看的一句话）
    pub step: String,
    /// apk 的输出等，最后 `LOG_KEEP` 行
    pub log: Vec<String>,
    pub error: Option<String>,
}

/// 挂在 `AppState` 上：升级任务的状态 + 「最新版本」的缓存。
#[derive(Default)]
pub struct Upgrader {
    job: Mutex<Job>,
    latest: Mutex<Option<String>>,
}

impl Upgrader {
    pub fn job(&self) -> Job {
        self.job.lock().map(|j| j.clone()).unwrap_or_default()
    }

    pub fn cached_latest(&self) -> Option<String> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }

    /// 占住「正在升级」（检查和置位在同一把锁里，两个请求不会都进来）。
    pub fn try_begin(&self, from: &str, to: &str) -> bool {
        let Ok(mut j) = self.job.lock() else {
            return false;
        };
        if j.state == JobState::Running {
            return false;
        }
        *j = Job {
            state: JobState::Running,
            from: from.into(),
            to: to.into(),
            ..Job::default()
        };
        true
    }

    fn update(&self, f: impl FnOnce(&mut Job)) {
        if let Ok(mut j) = self.job.lock() {
            f(&mut j);
        }
    }

    fn step(&self, s: impl Into<String>) {
        let s = s.into();
        tracing::info!("Alpine 升级：{s}");
        self.update(|j| {
            j.log.push(format!("▸ {s}"));
            j.step = s;
            trim_log(&mut j.log);
        });
    }

    fn log_line(&self, line: String) {
        self.update(|j| {
            j.log.push(line);
            trim_log(&mut j.log);
        });
    }
}

fn trim_log(log: &mut Vec<String>) {
    if log.len() > LOG_KEEP {
        let cut = log.len() - LOG_KEEP;
        log.drain(..cut);
    }
}

// ---------------------------------------------------------------------------
// 版本
// ---------------------------------------------------------------------------

/// rootfs 里装的是哪一版（`/etc/alpine-release`，如 `3.24.2`）。
pub fn installed_version(rootfs: &Path) -> Option<String> {
    std::fs::read_to_string(rootfs.join("etc/alpine-release"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// rootfs 的架构（`/etc/apk/arch`）。读不到就按核心自己的架构猜 ——
/// proot 不做跨架构，两者一定一样。
pub fn arch(rootfs: &Path) -> String {
    std::fs::read_to_string(rootfs.join("etc/apk/arch"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

/// `3.24.2` → `(3, 24, 2)`；带 `_rc1` 之类后缀的不认（不往测试版上升）。
fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next().map(|c| c.parse().ok()).unwrap_or(Some(0))?;
    it.next().is_none().then_some((a, b, c))
}

/// `to` 比 `from` 新吗。认不出的一律当「不新」（宁可不升）。
pub fn is_newer(to: &str, from: &str) -> bool {
    match (parse_version(to), parse_version(from)) {
        (Some(t), Some(f)) => t > f,
        _ => false,
    }
}

/// `3.24.2` → `v3.24`（仓库路径里的那一段）。
fn branch(v: &str) -> Option<String> {
    let (a, b, _) = parse_version(v)?;
    Some(format!("v{a}.{b}"))
}

/// 从 `latest-releases.yaml` 里取版本号（第一个 `version:`）。
fn parse_latest(yaml: &str) -> Option<String> {
    yaml.lines()
        .filter_map(|l| l.trim().strip_prefix("version:"))
        .map(|v| v.trim().trim_matches('"').to_string())
        .find(|v| parse_version(v).is_some())
}

/// 问 Alpine 官方最新的稳定版是哪个（结果顺手缓存）。
pub async fn fetch_latest(up: &Upgrader, arch: &str) -> anyhow::Result<String> {
    let url = format!(
        "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/{arch}/latest-releases.yaml"
    );
    let text = crate::llm::shared_http()?
        .get(&url)
        .timeout(Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let v = parse_latest(&text).ok_or_else(|| anyhow::anyhow!("{url} 里没找到版本号"))?;
    if let Ok(mut g) = up.latest.lock() {
        *g = Some(v.clone());
    }
    Ok(v)
}

/// 把 `repositories` 里每一行的 `/v3.xx/` 换成 `/<branch>/`（镜像域名、注释不动）。
///
/// 一行都没换到就返回 None —— 那是 `edge` 或者自定义的源，不知道该怎么升，
/// 交给人。
fn retarget_repositories(text: &str, branch: &str) -> Option<String> {
    let mut hit = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('#') {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(i) = rest.find("/v") {
            out.push_str(&rest[..i]);
            let tail = &rest[i + 1..];
            // `/v<数字>.<数字>/`
            let end = tail.find('/').unwrap_or(tail.len());
            let seg = &tail[..end];
            let looks = seg
                .strip_prefix('v')
                .and_then(|n| n.split_once('.'))
                .is_some_and(|(a, b)| {
                    !a.is_empty()
                        && !b.is_empty()
                        && a.bytes().all(|c| c.is_ascii_digit())
                        && b.bytes().all(|c| c.is_ascii_digit())
                });
            if looks && end < tail.len() {
                out.push('/');
                out.push_str(branch);
                hit = true;
            } else {
                out.push('/');
                out.push_str(seg);
            }
            rest = &tail[end..];
        }
        out.push_str(rest);
    }
    hit.then_some(out)
}

// ---------------------------------------------------------------------------
// 快照
// ---------------------------------------------------------------------------

fn sibling(rootfs: &Path, suffix: &str) -> PathBuf {
    let name = rootfs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rootfs".into());
    rootfs.with_file_name(format!("{name}{suffix}"))
}

/// 递归复制一棵树：保留权限位和符号链接（Alpine 里一大半是指向绝对路径的
/// 链接，**不能**跟着解析，那会解到宿主机上去）。硬链接各复制一份（proot 的
/// link2symlink 下本来就没几个真硬链接）；设备 / FIFO / socket 跳过。
fn copy_tree(src: &Path, dst: &Path, skip_top: &[&str]) -> std::io::Result<u64> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if skip_top.iter().any(|s| name == *s) {
            continue;
        }
        let from = e.path();
        let to = dst.join(&name);
        let ft = e.file_type()?;
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if ft.is_dir() {
            n += copy_tree(&from, &to, &[])?;
            let mode = std::fs::metadata(&from)?.permissions().mode();
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
        } else if ft.is_file() {
            // `fs::copy` 连权限位一起带过去
            std::fs::copy(&from, &to)?;
        }
        n += 1;
    }
    Ok(n)
}

/// 一棵树占多少字节（不跟链接）。
fn tree_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => tree_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// 这个路径所在的文件系统还剩多少字节。
fn free_bytes(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs 只写我们给的那块内存
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

/// 把快照换回去：升坏的那份先挪开（瞬间），再删。
fn restore(rootfs: &Path, bak: &Path) -> std::io::Result<()> {
    let failed = sibling(rootfs, FAILED_SUFFIX);
    if failed.exists() {
        std::fs::remove_dir_all(&failed)?;
    }
    if rootfs.exists() {
        std::fs::rename(rootfs, &failed)?;
    }
    std::fs::rename(bak, rootfs)?;
    let _ = std::fs::remove_file(rootfs.join(SNAPSHOT_OK));
    let _ = std::fs::remove_dir_all(&failed);
    Ok(())
}

/// 启动时调：上次升级没走完（App 在中途被杀）就把快照换回去。
///
/// 快照还在 = 升级没有走到「验收通过、删快照」那一步。哪怕其实已经升完、只差
/// 删快照就被杀了，换回去的代价也只是「还是旧版本」，可以再点一次。
pub fn recover_interrupted(rootfs: &Path) -> Option<String> {
    let bak = sibling(rootfs, BAK_SUFFIX);
    if !bak.is_dir() {
        // 升坏那份没删干净的话顺手清掉
        let _ = std::fs::remove_dir_all(sibling(rootfs, FAILED_SUFFIX));
        return None;
    }
    // 快照本身要是没复制完（升级还在第一步就被杀），它就不能用
    if !bak.join(SNAPSHOT_OK).is_file() {
        let _ = std::fs::remove_dir_all(&bak);
        return Some("上次的 Alpine 升级在备份阶段被打断，沙箱没动过，已清掉半份备份".into());
    }
    Some(match restore(rootfs, &bak) {
        Ok(()) => "上次的 Alpine 升级没走完（App 中途被关），已经换回升级前的沙箱".into(),
        Err(e) => format!("上次的 Alpine 升级没走完，换回备份失败：{e}"),
    })
}

// ---------------------------------------------------------------------------
// 升级
// ---------------------------------------------------------------------------

/// 能不能开始：返回 `(rootfs, 当前版本)`，或者一句为什么不行。
pub fn precheck(st: &AppState, sb: &LinuxSandbox) -> Result<(PathBuf, String), String> {
    let Some(cfg) = sb.proot_config() else {
        return Err("宿主模式没有 Alpine 沙箱".into());
    };
    if !sb.is_ready() {
        return Err("沙箱还没准备好".into());
    }
    if st.alpine.job().state == JobState::Running {
        return Err("已经在升级了".into());
    }
    if !st.busy_sessions().is_empty() {
        return Err("有对话正在进行，等它结束再升级".into());
    }
    let rootfs = cfg.rootfs.clone();
    let current = installed_version(&rootfs).ok_or("读不到当前版本（/etc/alpine-release）")?;
    Ok((rootfs, current))
}

/// 升到 `to`（如 `3.24.2`）。在后台跑，进度在 `st.alpine.job()`。
///
/// 调之前先过 `precheck`，并用 `Upgrader::try_begin` 占住。
pub async fn run(
    st: Arc<AppState>,
    sb: Arc<LinuxSandbox>,
    rootfs: PathBuf,
    from: String,
    to: String,
) {
    let up = &st.alpine;
    sb.set_maintenance(true);
    let res = upgrade(&st, &sb, &rootfs, &to).await;
    sb.set_maintenance(false);
    match res {
        Ok(()) => {
            up.step(format!("升级完成：{from} → {to}"));
            up.update(|j| j.state = JobState::Done);
            st.log("info", format!("Alpine 已升级：{from} → {to}"));
        }
        Err(e) => {
            let msg = format!("{e:#}");
            up.log_line(format!("✗ {msg}"));
            up.update(|j| {
                j.state = JobState::Failed;
                j.error = Some(msg.clone());
            });
            st.log("warn", format!("Alpine 升级失败：{msg}"));
        }
    }
}

async fn upgrade(st: &AppState, sb: &LinuxSandbox, rootfs: &Path, to: &str) -> anyhow::Result<()> {
    let up = &st.alpine;
    let branch = branch(to).ok_or_else(|| anyhow::anyhow!("认不出目标版本 {to}"))?;
    let repos_path = rootfs.join("etc/apk/repositories");
    let repos = std::fs::read_to_string(&repos_path)?;
    let new_repos = retarget_repositories(&repos, &branch).ok_or_else(|| {
        anyhow::anyhow!("/etc/apk/repositories 里没有 /v3.xx/ 这样的版本段（edge 或自定义源？），不知道该怎么改")
    })?;

    // 1. 空间
    up.step("检查剩余空间");
    let (need, free) = {
        let r = rootfs.to_path_buf();
        tokio::task::spawn_blocking(move || (tree_size(&r), free_bytes(&r))).await?
    };
    if let Some(free) = free {
        let want = need + SPACE_MARGIN;
        anyhow::ensure!(
            free >= want,
            "空间不够：备份 + 下载要 {} MB，只剩 {} MB",
            want / (1 << 20),
            free / (1 << 20)
        );
    }

    // 2. 快照
    up.step(format!("备份当前沙箱（{} MB）", need / (1 << 20)));
    let bak = sibling(rootfs, BAK_SUFFIX);
    {
        let (r, b) = (rootfs.to_path_buf(), bak.clone());
        tokio::task::spawn_blocking(move || {
            if b.exists() {
                std::fs::remove_dir_all(&b)?;
            }
            copy_tree(&r, &b, SNAPSHOT_SKIP)?;
            std::fs::write(b.join(SNAPSHOT_OK), "")
        })
        .await?
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&bak);
            anyhow::anyhow!("备份失败（沙箱没动过）：{e}")
        })?;
    }

    // 3–5. 改源、升级、验收；任何一步不过就换回快照
    let outcome = apply(st, sb, rootfs, &repos_path, &new_repos, to).await;
    match outcome {
        Ok(()) => {
            up.step("清理备份");
            let b = bak.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(b)).await;
            Ok(())
        }
        Err(e) => {
            up.step("没成功，换回升级前的沙箱");
            let (r, b) = (rootfs.to_path_buf(), bak.clone());
            match tokio::task::spawn_blocking(move || restore(&r, &b)).await? {
                Ok(()) => Err(e.context("已换回升级前的沙箱，什么都没丢")),
                Err(re) => Err(e.context(format!(
                    "换回备份也失败了（{re}）—— 备份还在 {}",
                    bak.display()
                ))),
            }
        }
    }
}

async fn apply(
    st: &AppState,
    sb: &LinuxSandbox,
    rootfs: &Path,
    repos_path: &Path,
    new_repos: &str,
    to: &str,
) -> anyhow::Result<()> {
    let up = &st.alpine;
    up.step(format!("软件源换到 {}", branch(to).unwrap_or_default()));
    std::fs::write(repos_path, new_repos)?;

    up.step("下载并安装新版本的包（几百 MB，几分钟）");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // 输出是一块一块来的，按行拼好再进日志
    let pump = async {
        let mut pending = String::new();
        while let Some(chunk) = rx.recv().await {
            pending.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(i) = pending.find('\n') {
                let line: String = pending.drain(..=i).collect();
                let line = line.trim_end();
                if !line.is_empty() {
                    up.log_line(line.to_string());
                }
            }
        }
        if !pending.trim().is_empty() {
            up.log_line(pending.trim_end().to_string());
        }
    };
    let script = "apk update && apk upgrade --available";
    let (res, ()) = tokio::join!(sb.run_maintenance(script, APK_TIMEOUT, Some(tx)), pump);
    let res = res?;
    anyhow::ensure!(
        !res.timed_out,
        "apk 跑了 {} 分钟还没完",
        APK_TIMEOUT.as_secs() / 60
    );
    // 退出码只记下来，不当判据（见模块说明）
    if res.code != 0 {
        up.log_line(format!("（apk 退出码 {}，以下面的验收为准）", res.code));
    }

    up.step("验收");
    let got = installed_version(rootfs).unwrap_or_default();
    // 同一个分支上**不比目标旧**就算到了（升级途中仓库可能刚好又出了个补丁版）
    anyhow::ensure!(
        branch(&got) == branch(to) && !is_newer(to, &got),
        "升级后 /etc/alpine-release 是 {got:?}，不是 {to}"
    );
    let check = "/bin/sh -c 'echo sh-ok' && apk --version >/dev/null && echo apk-ok \
                 && { [ ! -e /bin/bash ] || bash -c 'echo bash-ok'; }";
    let r = sb
        .run_maintenance(check, Duration::from_secs(60), None)
        .await?;
    anyhow::ensure!(
        r.code == 0 && r.stdout.contains("sh-ok") && r.stdout.contains("apk-ok"),
        "升级后跑不起来基本命令：{}",
        r.to_text()
    );
    up.log_line(format!(
        "✓ {}",
        r.stdout.split_whitespace().collect::<Vec<_>>().join(" ")
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("3.24.2", "3.21.0"));
        assert!(is_newer("3.24.2", "3.24.1"));
        assert!(is_newer("3.10.0", "3.9.6"), "按数字比，不按字符串");
        assert!(!is_newer("3.21.0", "3.24.2"));
        assert!(!is_newer("3.24.2", "3.24.2"));
        assert!(!is_newer("3.25.0_rc1", "3.24.2"), "测试版不认");
        assert!(!is_newer("edge", "3.24.2"));
        assert_eq!(branch("3.24.2").as_deref(), Some("v3.24"));
    }

    #[test]
    fn the_latest_version_comes_from_the_release_yaml() {
        let yaml = "-\n  title: \"Mini root filesystem\"\n  branch: v3.24\n  version: 3.24.2\n  flavor: alpine-minirootfs\n";
        assert_eq!(parse_latest(yaml).as_deref(), Some("3.24.2"));
        assert_eq!(parse_latest("nothing here"), None);
    }

    #[test]
    fn repositories_keep_their_mirror_and_comments() {
        let text = "https://mirrors.ustc.edu.cn/alpine/v3.21/main\n\
                    #https://dl-cdn.alpinelinux.org/alpine/v3.21/testing\n\
                    https://dl-cdn.alpinelinux.org/alpine/v3.21/community\n";
        let out = retarget_repositories(text, "v3.24").unwrap();
        assert_eq!(
            out,
            "https://mirrors.ustc.edu.cn/alpine/v3.24/main\n\
             #https://dl-cdn.alpinelinux.org/alpine/v3.21/testing\n\
             https://dl-cdn.alpinelinux.org/alpine/v3.24/community\n"
        );
        // edge / 自定义：不知道怎么升，交给人
        assert_eq!(
            retarget_repositories("https://dl-cdn.alpinelinux.org/alpine/edge/main\n", "v3.24"),
            None
        );
        // 路径里别的 `/v…` 不能误伤
        let odd = "https://example.com/vendor/alpine/v3.21/main\n";
        assert_eq!(
            retarget_repositories(odd, "v3.24").unwrap(),
            "https://example.com/vendor/alpine/v3.24/main\n"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-alpine-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_snapshot_keeps_links_and_modes_and_skips_tmp() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("copy");
        let src = root.join("alpine-rootfs");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::create_dir_all(src.join("tmp")).unwrap();
        std::fs::write(src.join("bin/busybox"), "elf").unwrap();
        std::fs::set_permissions(
            src.join("bin/busybox"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        // Alpine 式的绝对链接：不能被解析到宿主机上
        std::os::unix::fs::symlink("/bin/busybox", src.join("bin/sh")).unwrap();
        std::fs::write(src.join("tmp/junk"), "x").unwrap();

        let dst = sibling(&src, BAK_SUFFIX);
        copy_tree(&src, &dst, SNAPSHOT_SKIP).unwrap();
        assert_eq!(
            std::fs::read_link(dst.join("bin/sh")).unwrap(),
            Path::new("/bin/busybox")
        );
        let mode = std::fs::metadata(dst.join("bin/busybox"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert!(!dst.join("tmp").exists(), "tmp 不进快照");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_interrupted_upgrade_is_rolled_back_on_start() {
        let root = scratch("recover");
        let rootfs = root.join("alpine-rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/alpine-release"), "3.24.2\n").unwrap();
        // 没有快照：什么都不做
        assert_eq!(recover_interrupted(&rootfs), None);

        // 快照完整：换回去
        let bak = sibling(&rootfs, BAK_SUFFIX);
        std::fs::create_dir_all(bak.join("etc")).unwrap();
        std::fs::write(bak.join("etc/alpine-release"), "3.21.0\n").unwrap();
        std::fs::write(bak.join(SNAPSHOT_OK), "").unwrap();
        assert!(recover_interrupted(&rootfs).unwrap().contains("换回"));
        assert!(!rootfs.join(SNAPSHOT_OK).exists(), "标记不能留在沙箱里");
        assert_eq!(installed_version(&rootfs).as_deref(), Some("3.21.0"));
        assert!(!bak.exists() && !sibling(&rootfs, FAILED_SUFFIX).exists());

        // 快照是半份（备份阶段被杀，`etc` 可能已经复制过去了）：不碰沙箱，清掉半份
        std::fs::create_dir_all(bak.join("etc")).unwrap();
        std::fs::write(bak.join("etc/alpine-release"), "3.21.0\n").unwrap();
        assert!(recover_interrupted(&rootfs).unwrap().contains("没动过"));
        assert_eq!(installed_version(&rootfs).as_deref(), Some("3.21.0"));
        assert!(!bak.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
