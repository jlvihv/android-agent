//! 磁盘上的副产物清理。
//!
//! 会话会在磁盘上留下几类文件，都是**只有我们知道它们什么时候没用了**的：
//!
//! - `<data>/images/`：内联给模型的图片副本（`read` 读图、浏览器截图缩好的那份）
//! - `<data>/attachments/`：用户在输入卡片里附的图片 / PDF
//! - `<rootfs>/root/.browser/shot-*.jpg`：浏览器截图的原图（Kotlin 写的）
//! - `<rootfs>/tmp/agent-out-*.log`：bash 输出被截断时溢出的完整输出
//!
//! 不清的话只增不减，手机存储被慢慢吃掉。两个时机清：
//!
//! 1. **删会话 / 清空会话**时，清它引用的那些（`owned_files`）；
//! 2. **启动时**扫一遍，清没有任何会话引用的（`sweep`）—— 兜住进程被杀、
//!    上传了没发、`agent browser screenshot` 这类不进历史的。

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::agent::ChatMessage;

/// 浏览器截图在**沙箱**里的目录（和 `BrowserController.SHOT_GUEST_DIR` 一致）
pub const SHOT_DIR: &str = "root/.browser";

/// 浏览器截图落在哪：宿主目录 + 它在沙箱视角里的路径前缀。
///
/// 历史里存的是**后者**（`ImagePart::source`，模型 `read` 它），磁盘上是前者。
/// 两处不同源（沙箱里是 `<rootfs>/root/.browser` ↔ `/root/.browser`；宿主模式
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
/// 照搬沙箱那两个位置在宿主模式下会去动用户的 `/root` 和 `/tmp`。
#[derive(Debug, Clone, Default)]
pub struct SideDirs {
    pub shots: Option<ShotDir>,
    /// bash 输出被截断时溢出的完整输出（`agent-out-*.log`）
    pub spills: Option<PathBuf>,
}

/// 历史里没引用、但模型可能还在用的文件（bash 里直接处理截图、grep 溢出的
/// 输出），启动时只清这么久以前的。
const LOOSE_TTL: Duration = Duration::from_secs(24 * 3600);

/// 一段历史引用的、**归我们管**的文件（绝对路径）。
///
/// 只认我们自己写出来的：图片副本、附件、浏览器截图原图。`read` 读图时的
/// 原图是用户自己的文件，**绝不能**算进来。
pub fn owned_files(msgs: &[ChatMessage], dirs: &SideDirs) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            ChatMessage::UserFiles { attachments, .. } => {
                out.extend(attachments.iter().map(|a| PathBuf::from(&a.path)));
            }
            ChatMessage::Tool { images, .. } => {
                for img in images {
                    out.push(PathBuf::from(&img.path));
                    if img.tool == "browser" {
                        if let Some(p) = dirs
                            .shots
                            .as_ref()
                            .and_then(|s| browser_shot(s, &img.source))
                        {
                            out.push(p);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 截图的历史路径（`<shots.guest>/<文件名>`）→ 磁盘上的路径。别的形状一律不认
/// （不让一条历史记录指挥我们去删机器上随便哪个文件）。
fn browser_shot(shots: &ShotDir, guest: &str) -> Option<PathBuf> {
    let rest = Path::new(guest).strip_prefix(&shots.guest).ok()?;
    let mut parts = rest.components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(name)), None) => Some(shots.host.join(name)),
        _ => None,
    }
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
        n += sweep_dir(
            spills,
            |name| name.starts_with("agent-out-") && name.ends_with(".log"),
            referenced,
            loose,
        );
    }
    n
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Attachment, AttachmentKind, ImagePart};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-gc-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tool_with(path: &str, source: &str, tool: &str) -> ChatMessage {
        ChatMessage::Tool {
            tool_call_id: "c".into(),
            content: "{}".into(),
            images: vec![ImagePart {
                path: path.into(),
                mime: "image/jpeg".into(),
                source: source.into(),
                tool: tool.into(),
            }],
        }
    }

    /// 我们写出来的算，用户自己的原图不算；浏览器截图的路径只认 `/root/.browser/<名字>`。
    #[test]
    fn only_files_we_wrote_are_owned() {
        let root = Path::new("/rootfs");
        let msgs = vec![
            ChatMessage::UserFiles {
                text: String::new(),
                attachments: vec![Attachment {
                    id: "a.png".into(),
                    path: "/data/attachments/a.png".into(),
                    mime: "image/png".into(),
                    name: "a.png".into(),
                    kind: AttachmentKind::Image,
                }],
            },
            tool_with("/data/images/1.jpg", "/workspace/我的照片.jpg", "read"),
            tool_with("/data/images/2.jpg", "/root/.browser/shot-1.jpg", "browser"),
            tool_with(
                "/data/images/3.jpg",
                "/root/.browser/../../etc/passwd",
                "browser",
            ),
            tool_with("/data/images/4.jpg", "/etc/passwd", "browser"),
        ];
        let got = owned_files(&msgs, &side_dirs_for(root));
        let want: Vec<PathBuf> = [
            "/data/attachments/a.png",
            "/data/images/1.jpg",
            "/data/images/2.jpg",
            "/rootfs/root/.browser/shot-1.jpg",
            "/data/images/3.jpg",
            "/data/images/4.jpg",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(got, want);
    }

    /// 沙箱形态的副产物目录（截图 / 溢出输出都在 rootfs 里）。
    fn side_dirs_for(root: &Path) -> SideDirs {
        SideDirs {
            shots: Some(ShotDir {
                host: root.join(SHOT_DIR),
                guest: format!("/{SHOT_DIR}"),
            }),
            spills: Some(root.join("tmp")),
        }
    }

    /// 启动清扫：没被引用、又比起点早的才删；被引用的、起点之后写的、
    /// 不归我们管的文件名都不碰。沙箱里那两类再多宽限一天。
    #[test]
    fn sweep_removes_only_old_orphans() {
        let data = tmp("data");
        let root = tmp("root");
        for d in ["images", "attachments"] {
            std::fs::create_dir_all(data.join(d)).unwrap();
        }
        std::fs::create_dir_all(root.join(SHOT_DIR)).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();

        let orphan = data.join("images/orphan.jpg");
        let kept = data.join("images/kept.jpg");
        let att = data.join("attachments/old.pdf");
        let shot = root.join(SHOT_DIR).join("shot-1.jpg");
        let spill = root.join("tmp/agent-out-1.log");
        let users = root.join("tmp/user-file.log");
        for f in [&orphan, &kept, &att, &shot, &spill, &users] {
            std::fs::write(f, b"x").unwrap();
        }
        let referenced: HashSet<PathBuf> = [kept.clone()].into_iter().collect();

        // 起点在这些文件之后、但不到一天：images/attachments 的孤儿删，沙箱里的留
        let later = SystemTime::now() + Duration::from_secs(60);
        assert_eq!(
            sweep(Some(&data), &side_dirs_for(&root), &referenced, later),
            2
        );
        assert!(!orphan.exists() && !att.exists());
        assert!(kept.exists() && shot.exists() && spill.exists() && users.exists());

        // 起点在一天以后：沙箱里的过期了；不归我们管的文件名照旧不碰
        let much_later = SystemTime::now() + LOOSE_TTL + Duration::from_secs(60);
        assert_eq!(
            sweep(Some(&data), &side_dirs_for(&root), &referenced, much_later),
            2
        );
        assert!(!shot.exists() && !spill.exists());
        assert!(kept.exists() && users.exists());

        // 起点之前写的才删：起点在文件之前 → 什么都不动
        std::fs::write(&orphan, b"x").unwrap();
        let earlier = SystemTime::now() - Duration::from_secs(60);
        assert_eq!(
            sweep(Some(&data), &side_dirs_for(&root), &referenced, earlier),
            0
        );
        assert!(orphan.exists());

        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_dir_all(&root);
    }
}
