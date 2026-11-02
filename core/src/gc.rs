//! 磁盘上的副产物清理。
//!
//! 会话会在磁盘上留下几类文件，都是**只有我们知道它们什么时候没用了**的：
//!
//! - `<data>/images/`：内联给模型的图片副本（`read` 读图缩好的那份）
//! - `<data>/attachments/`：用户在输入卡片里附的图片 / PDF
//! - `<rootfs>/root/.browser/shot-*.jpg`：浏览器截图的原图（Kotlin 写的）
//! - `<rootfs>/tmp/`：我们自己往临时目录里放的东西 —— bash 输出被截断时溢出的
//!   完整输出（`agent-out-*.log`），以及 `edit` 建文件撞上已存在时留下的
//!   `<名字>.new`（见 `LinuxSandbox::spill_new_file`）。两种都靠 `spills` 清
//!
//! 不清的话只增不减，手机存储被慢慢吃掉。两个时机清：
//!
//! 1. **删会话 / 清空会话**时，清它引用的那些（`owned_files`）；
//! 2. **启动时**扫一遍，清没有任何会话引用的（`sweep`）—— 兜住进程被杀、
//!    上传了没发、`agent browser screenshot` 这类不进历史的。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::ChatMessage;

/// 浏览器截图在**沙箱**里的目录（和 `BrowserController.SHOT_GUEST_DIR` 一致）
pub const SHOT_DIR: &str = "root/.browser";

/// 浏览器截图落在哪：宿主目录 + 它在沙箱视角里的路径前缀。
///
/// 历史里存的是**后者**（`ImagePart::source`，模型 `read` 它），磁盘上是前者。
/// 两处不同源（沙箱里是 `<rootfs>/root/.browser` ↔ `/root/.browser`；桌面
/// 下 rootfs 是 `/`，`/root` 是**用户的根目录** —— 既写不进去也不该写，
/// 所以换成我们自己的数据目录，那时前缀就是宿主路径本身）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotDir {
    pub host: PathBuf,
    pub guest: String,
}

/// 我们自己的副产物落在哪（都是宿主路径）。
///
/// 只有我们知道它们什么时候没用了，所以清扫也要知道它们在哪儿 ——
/// 照搬沙箱那两个位置在桌面下会去动用户的 `/root` 和 `/tmp`。
#[derive(Debug, Clone, Default)]
pub struct SideDirs {
    pub shots: Option<ShotDir>,
    /// bash 输出被截断时溢出的完整输出（`agent-out-*.log`）
    pub spills: Option<PathBuf>,
}

/// 历史里没引用、但模型可能还在用的文件（bash 里直接处理截图、grep 溢出的
/// 输出），启动时只清这么久以前的。
const LOOSE_TTL: Duration = Duration::from_hours(24);

/// 一段历史引用的、**归我们管**的文件（绝对路径）：附件、内联给模型的图片副本。
/// `read` 读图时的原图（`ImagePart::source`）是用户自己的文件，**绝不能**算进来。
pub fn owned_files(msgs: &[ChatMessage]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            ChatMessage::User { attachments, .. } => {
                out.extend(attachments.iter().map(|a| PathBuf::from(&a.path)));
            }
            ChatMessage::Tool { images, .. } => {
                out.extend(images.iter().map(|img| PathBuf::from(&img.path)));
            }
            _ => {}
        }
    }
    out
}

/// 删掉这些文件，返回删了几个。不存在的不算错。
pub fn remove_all(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// 启动时的清扫。
///
/// - `referenced`：库里所有会话引用的文件（`owned_files` 的并集）
/// - `before`：只删比这个时刻**更早**改过的文件。传「读会话之前」的时刻：
///   之后新写的（回合刚存的图片、刚上传的附件）一律不碰，免得和正在跑的
///   回合抢。
///
/// 返回删了几个。
pub fn sweep(
    data_dir: Option<&Path>,
    dirs: &SideDirs,
    referenced: &HashSet<PathBuf>,
    before: SystemTime,
) -> usize {
    let mut n = 0;
    if let Some(d) = data_dir {
        // 只有历史引用它们才有用：没被引用就是孤儿
        for dir in ["images", "attachments"] {
            n += sweep_dir(&d.join(dir), |_| true, referenced, before);
        }
    }
    // 这两类在沙箱里，宽限一天（模型可能在 bash 里直接处理它们）
    let loose = before
        .checked_sub(LOOSE_TTL)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some(shots) = &dirs.shots {
        n += sweep_dir(
            &shots.host,
            |name| name.starts_with("shot-"),
            referenced,
            loose,
        );
    }
    if let Some(spills) = &dirs.spills {
        n += sweep_dir(spills, is_spill_name, referenced, loose);
    }
    n
}

/// 临时目录里哪些文件名是**我们**留的（别的都别碰，哪怕过期了）。
///
/// - `agent-out-*.log`：bash 输出溢出（`LinuxSandbox::spill_captured`）
/// - `<名字>.new` / `.new.2`：`edit` 的 create 模式撞上已存在文件时把内容放这儿了，
///   等模型 `mv` 过去（`LinuxSandbox::spill_new_file`）
fn is_spill_name(name: &str) -> bool {
    if name.starts_with("agent-out-") {
        return true;
    }
    let Some((_, rest)) = name.rsplit_once(".new") else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('.')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn sweep_dir(
    dir: &Path,
    matches: impl Fn(&str) -> bool,
    referenced: &HashSet<PathBuf>,
    before: SystemTime,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !matches(name) || referenced.contains(&path) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        if !meta.is_file() || meta.modified().map_or(true, |t| t >= before) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            n += 1;
        }
    }
    n
}
