//! `agent files` —— 看一眼工作目录（`/workspace`）现在成什么样了。
//!
//! 为什么要有这个命令：模型**只看得见自己敲过的路径**，看不见「这个目录整体是不是
//! 乱了」。真机上撞到过：三个截图 png 混在十几个项目目录中间，谁都看不出来 ——
//! 每次都是「我要在根目录建个文件」，几十次之后就成了杂物堆。
//!
//! 所以这里把根目录摊开，并把**散在根目录的文件**单独列出来（成品该收进子目录、
//! 临时的该放 `/tmp`）。它不整理任何东西 —— 只把「该收一收了」这件事变得看得见。
//! 整理是模型的活（它会 `mv`），也是用户的决定（他的文件，他不点头我们不动）。

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 递归算大小时最多看这么多条目（大目录不至于让命令卡住）。
const SIZE_BUDGET: u32 = 20_000;

/// 一个顶层条目。
struct Entry {
    name: String,
    /// 目录里有多少个条目（只数一层）；文件是 None
    items: Option<usize>,
    /// 目录是递归求和；文件是自身大小
    bytes: u64,
    /// 这个目录**里面**有没有可能是散着的临时文件？现在只用来标注「不认识的」
    modified: Option<SystemTime>,
}

/// 生成给人（和模型）看的文字。
///
/// `root_guest` 是它在沙箱里的路径（显示用），`host` 是真正的宿主目录。
pub fn render(root_guest: &str, host: &Path) -> anyhow::Result<String> {
    let rd = std::fs::read_dir(host)
        .map_err(|e| anyhow::anyhow!("读不了 {root_guest}（{}）：{e}", host.display()))?;

    let mut dirs: Vec<Entry> = Vec::new();
    let mut files: Vec<Entry> = Vec::new();
    let mut total_bytes = 0u64;

    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        // 隐藏文件（`.gitignore` 之类）不列：它们通常是有意放的
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let modified = e.metadata().ok().and_then(|m| m.modified().ok());
        if ft.is_dir() {
            let mut budget = SIZE_BUDGET;
            let (bytes, items) = dir_stats(&e.path(), &mut budget);
            total_bytes += bytes;
            dirs.push(Entry {
                name,
                items: Some(items),
                bytes,
                modified,
            });
        } else if ft.is_file() {
            let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
            total_bytes += bytes;
            files.push(Entry {
                name,
                items: None,
                bytes,
                modified,
            });
        }
    }

    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::new();
    out.push_str(&format!(
        "{root_guest}  {} 个目录 / {} 个文件，共 {}\n",
        dirs.len(),
        files.len(),
        human_bytes(total_bytes)
    ));

    if dirs.is_empty() && files.is_empty() {
        out.push_str("\n（空的。一个任务一个子目录，成品放里面。）\n");
        return Ok(out);
    }

    if !dirs.is_empty() {
        out.push_str("\n目录\n");
        let width = dirs
            .iter()
            .map(|d| d.name.chars().count())
            .max()
            .unwrap_or(0);
        for d in &dirs {
            out.push_str(&format!(
                "  {:<width$}  {:>5} 项  {:>9}  {}\n",
                format!("{}/", d.name),
                d.items.unwrap_or(0),
                human_bytes(d.bytes),
                ago(d.modified),
                width = width + 1,
            ));
        }
    }

    if !files.is_empty() {
        out.push_str(&format!(
            "\n散在根目录的文件（{} 个）—— 成品收进子目录，临时的放 /tmp 或删掉\n",
            files.len()
        ));
        let width = files
            .iter()
            .map(|f| f.name.chars().count())
            .max()
            .unwrap_or(0);
        for f in &files {
            out.push_str(&format!(
                "  {:<width$}  {:>9}  {}\n",
                f.name,
                human_bytes(f.bytes),
                ago(f.modified),
                width = width,
            ));
        }
    }

    out.push_str(
        "\n（这是 /workspace 的全貌。每个会话在它自己那个 6 位小目录里干活，\n\
         截图在该目录的 screenshots/ 下；临时产物放 /tmp，依赖和解压的东西放 /root。\n\
         这个命令只报告，不动任何文件 —— 要归置也是模型自己 mv。）\n",
    );
    Ok(out)
}

/// 递归求和：目录里有多少条目、一共多大。`budget` 用完就停（大目录不卡死）。
fn dir_stats(dir: &Path, budget: &mut u32) -> (u64, usize) {
    let mut bytes = 0u64;
    let mut items = 0usize;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    for e in rd.flatten() {
        if *budget == 0 {
            break;
        }
        *budget -= 1;
        items += 1;
        match e.file_type() {
            Ok(t) if t.is_dir() => bytes += dir_stats(&e.path(), budget).0,
            Ok(t) if t.is_file() => bytes += e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => {}
        }
    }
    (bytes, items)
}

fn human_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    let f = n as f64;
    if f < KB {
        format!("{n} B")
    } else if f < KB * KB {
        format!("{:.1} KB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.2} GB", f / (KB * KB * KB))
    }
}

/// 相对时间。比绝对时间戳好读，也不用引一个日期库。
fn ago(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".into() };
    let Ok(d) = SystemTime::now().duration_since(t) else {
        return "刚刚".into();
    };
    let secs = d.as_secs();
    if secs < 60 {
        "刚刚".into()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86_400 {
        format!("{} 小时前", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{} 天前", secs / 86_400)
    } else {
        format!("{} 个月前", secs / (86_400 * 30))
    }
}

/// 把 `/workspace`（或它的子目录）解析成宿主路径的工具见 `LinuxSandbox::resolve_path`；
/// 这个模块只管画表。
pub fn entry_path(root: &Path, sub: Option<&str>) -> PathBuf {
    match sub {
        Some(s) if !s.is_empty() => root.join(s),
        _ => root.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-files-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn lists_dirs_and_calls_out_loose_files() {
        let d = tmp("loose");
        std::fs::create_dir_all(d.join("项目A")).unwrap();
        std::fs::write(d.join("项目A/a.txt"), b"hello").unwrap();
        std::fs::create_dir_all(d.join("项目B")).unwrap();
        std::fs::write(d.join("散文件.png"), vec![0u8; 2048]).unwrap();

        let out = render("/workspace", &d).unwrap();
        assert!(out.contains("/workspace  2 个目录 / 1 个文件"), "{out}");
        assert!(out.contains("项目A/"), "{out}");
        assert!(out.contains("5 B"), "算出来的大小不对：{out}");
        assert!(out.contains("散在根目录的文件（1 个）"), "{out}");
        assert!(out.contains("散文件.png"), "{out}");
        assert!(out.contains("2.0 KB"), "{out}");

        let _ = std::fs::remove_dir_all(&d);
    }

    /// 隐藏文件（`.gitignore` 这类）是有意放的，不算「散落」。
    #[test]
    fn hidden_entries_are_ignored() {
        let d = tmp("hidden");
        std::fs::write(d.join(".gitignore"), b"x").unwrap();
        std::fs::write(d.join("x.txt"), b"x").unwrap();
        let out = render("/workspace", &d).unwrap();
        assert!(!out.contains(".gitignore"), "{out}");
        assert!(out.contains("x.txt"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_dir_says_so() {
        let d = tmp("empty");
        let out = render("/workspace", &d).unwrap();
        assert!(out.contains("0 个目录 / 0 个文件"), "{out}");
        assert!(out.contains("空的"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unreadable_root_is_an_error_not_a_panic() {
        let e = render("/workspace", Path::new("/definitely/not/here")).unwrap_err();
        assert!(e.to_string().contains("/workspace"), "{e}");
    }

    #[test]
    fn sizes_and_ages_read_like_a_human_wrote_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(ago(None), "—");
        assert_eq!(ago(Some(SystemTime::now())), "刚刚");
    }

    /// 嵌套目录要递归算进总大小（不然「看着 0 项」的目录其实塞了几个 G）。
    #[test]
    fn nested_dirs_count_toward_size_and_items() {
        let d = tmp("nested");
        std::fs::create_dir_all(d.join("a/b/c")).unwrap();
        std::fs::write(d.join("a/b/c/big.bin"), vec![0u8; 4096]).unwrap();
        let out = render("/workspace", &d).unwrap();
        assert!(out.contains("4.0 KB"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
