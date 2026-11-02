//! 到处都要用的一点点字符串处理。
//!
//! 以前 `cap` / `one_line` / `host_of` 各在五六个模块里各写一份（`status.rs` 的
//! 叫 `cap`、`linux.rs` 叫 `clip_chars`、`notify.rs` 和 `cdp.rs` 叫 `clip`），
//! 改一处忘一处就会漂 —— 同一条通知和同一条状态行的截断规则不一样，看着像 bug。
//! 语义只有这几种，收在这里一份。

/// 按**字符**截断并加省略号。
///
/// 不按字节：中文一个字三字节，按字节切会切出半个字（界面上是个方块）。
pub fn cap(s: &str, max: usize) -> String {
    let Some((cut, _)) = s.char_indices().nth(max) else {
        return s.to_string();
    };
    let mut out = String::with_capacity(cut + '…'.len_utf8());
    out.push_str(&s[..cut]);
    out.push('…');
    out
}

/// 压成一行：连续空白折成一个空格，首尾不留空白。
///
/// 通知只有一段（多个换行会被系统折成一堆空白），登记表里也只需要一条线索。
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 去掉协议头之后那一截（到 `/`、`?`、`#` 为止），**是 `url` 的一段** ——
/// 调用方可以直接拿它的长度回原文上切片。
pub fn authority(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))
        .unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

/// 域名：小写，去掉 `user:pass@` 和端口。认不出时给空串。
pub fn host_name(url: &str) -> String {
    let auth = authority(url);
    let host = auth.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("");
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_counts_characters_not_bytes() {
        assert_eq!(cap("报告", 5), "报告", "没超长就原样");
        assert_eq!(cap("报告文件", 2), "报告…", "按字符切，不留半个汉字");
        assert_eq!(cap("abcdef", 3), "abc…");
    }

    #[test]
    fn one_line_folds_whitespace() {
        assert_eq!(one_line("  a\n\n b\tc  "), "a b c");
        assert_eq!(one_line("   "), "");
    }

    #[test]
    fn host_name_is_the_bare_domain() {
        assert_eq!(
            host_name("https://API.OpenAI.com/v1/models"),
            "api.openai.com"
        );
        assert_eq!(
            host_name("http://user:pw@host.docker:8080/x"),
            "host.docker"
        );
        assert_eq!(host_name("//cdn.example.com/a.js"), "cdn.example.com");
        assert_eq!(host_name(""), "");
    }

    #[test]
    fn authority_is_a_slice_of_the_input() {
        let url = "https://example.com:443/a/b?q=1#frag";
        let a = authority(url);
        assert_eq!(a, "example.com:443");
        assert!(url.ends_with(&url[url.find(a).unwrap() + a.len()..]));
    }
}
