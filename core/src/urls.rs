//! 从工具输出里挑出 http(s) 网址，给前端做成"一键打开"。
//!
//! 沙箱里的 `agent-open`（也就是 `$BROWSER` / `xdg-open`）会把**明确要打开**的
//! URL 报给宿主（`POST /api/open`），但**只是打印**网址的命令也很多：
//! `gh auth login` 的设备码页是自己打出来的，`curl -I`、`git remote -v`、
//! 一堆 CLI 的 "see https://…" 更是家常便饭。扫一遍输出，用户就能直接点，
//! 不用手抄。

/// 一条输出里最多回几个 —— 多了没用，只把界面塞满
pub const MAX_URLS: usize = 5;

/// 短于这个长度的不像网址（`http://a.b` 是 10）
const MIN_LEN: usize = 10;
/// 长于这个长度的多半是粘贴错了或者被拼进了别的东西
const MAX_LEN: usize = 2048;

/// 挑出 http(s) 网址：去重、保持出现顺序、最多 [`MAX_URLS`] 个。
///
/// 前后被标点/括号裹着的会剪掉（`<https://x>`、`（https://x）`、句末的 `.`
/// 都常见）：不剪的话点开就是 404。
///
/// 只认 http(s)：`mailto:`、`file://`、`javascript:` 这些不是给人点的，也用不上。
pub fn extract(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        let Some(url) = clean(token) else { continue };
        if out.iter().any(|u| u == url) {
            continue;
        }
        out.push(url.to_string());
        if out.len() >= MAX_URLS {
            break;
        }
    }
    out
}

/// 剪掉包裹的标点并确认是 http(s)。不是就返回 `None`。
fn clean(token: &str) -> Option<&str> {
    // 前导：引号/括号/尖括号（`<https://…>` 是 Markdown 自动链接的写法）
    let t = token.trim_start_matches([
        '"', '\'', '`', '<', '(', '[', '{', '“', '‘', '（', '【', '《',
    ]);
    // 尾部：引号/括号/各种标点（中英文都算）
    let t = t.trim_end_matches([
        '"', '\'', '`', '>', ')', ']', '}', '”', '’', '）', '】', '》', ',', ';', ':', '!', '?',
        '。', '，', '、', '；', '：', '！', '？', '…',
    ]);
    // 句末的句号单独处理：`https://x.com.` 要剪，`.../cli.git` 不能动
    let t = t.trim_end_matches('.');
    if t.len() < MIN_LEN || t.len() > MAX_LEN {
        return None;
    }
    let head = t.get(..8).unwrap_or(t).to_ascii_lowercase();
    if head.starts_with("http://") || head.starts_with("https://") {
        Some(t)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_http_urls_and_keeps_order_without_duplicates() {
        let text = "看 https://a.example/1 ，再看 http://b.example 和 https://a.example/1 一次";
        assert_eq!(
            extract(text),
            vec!["https://a.example/1", "http://b.example"]
        );
    }

    #[test]
    fn trims_punctuation_that_would_404() {
        // 中文标点紧跟在网址后面是常态；Markdown 的 <…> 也一样
        assert_eq!(
            extract("见 <https://github.com/cli/cli>。"),
            vec!["https://github.com/cli/cli"]
        );
        assert_eq!(
            extract("(https://x.example/a),"),
            vec!["https://x.example/a"]
        );
        assert_eq!(
            extract("see https://x.example/a."),
            vec!["https://x.example/a"]
        );
        // ….git 的句号是网址的一部分，不能剪
        assert_eq!(
            extract("https://x.example/repo.git"),
            vec!["https://x.example/repo.git"]
        );
    }

    #[test]
    fn ignores_non_http_and_bare_hosts() {
        let text = "mailto:a@b.example file:///etc/passwd javascript:alert(1) x.example 以及 ftp://f.example";
        assert!(extract(text).is_empty(), "{:?}", extract(text));
    }

    #[test]
    fn caps_the_list() {
        let text: String = (0..20).map(|i| format!("https://x.example/{i} ")).collect();
        assert_eq!(extract(&text).len(), MAX_URLS);
    }

    #[test]
    fn finds_the_gh_device_page_in_a_realistic_result() {
        // gh auth login --web 的真实输出
        let text = "! First copy your one-time code: 19BC-184D\n\
                    Open this URL to continue in your web browser: https://github.com/login/device\n\
                    [exit 0]";
        assert_eq!(extract(text), vec!["https://github.com/login/device"]);
    }
}
