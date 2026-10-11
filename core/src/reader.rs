//! `read` 工具的核心：按 offset/limit 读文本 + 头部截断。
//!
//! 移植自 OpenMinis 的 `PiToolText`（Kotlin，GPL-3.0）里的 `read` / `head`。
//! 设计要点：
//!
//! 1. **offset 从 1 开始** —— 跟模型在文件里看到的行号一致，不用它自己 ±1。
//! 2. **上限按 UTF-8 字节数算**（2000 行 / 50KB，先到者为准）：一行中文
//!    就是 3 字节，按「字符数」算会跟真实 token 预算差出几倍。
//! 3. **只截整行，不从行中间劈开。** 宁可少给几十字节，也不要给半行 JSON
//!    或半个多字节字符 —— 模型会当成文件真的长那样。
//! 4. **截断了必须告诉模型怎么续读**（`offset=N`），否则它默认文件就这么多行，
//!    然后开始编造后面的内容。
//!
//! `read` 只看文件头，所以这里只实现 `head`；尾截断（bash 输出用）在
//! `linux.rs` 里另有一套，两者互不影响。

/// 一次最多给模型多少行
pub const MAX_LINES: usize = 2000;
/// 一次最多给模型多少字节（UTF-8）
pub const MAX_BYTES: usize = 50 * 1024;

/// 给模型看的读取结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadView {
    /// 已经格式化好的正文（截断时会自带续读提示）
    pub content: String,
    /// 是否被 2000 行/50KB 上限截断
    pub truncated: bool,
    /// 正文里实际显示了多少行
    pub shown_lines: usize,
    /// 文件总行数
    pub total_lines: usize,
}

struct Head {
    content: String,
    truncated: bool,
    output_lines: usize,
    first_line_exceeds_limit: bool,
}

/// 取文件头：最多 2000 行 / 50KB。
fn head(text: &str) -> Head {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= MAX_LINES && text.len() <= MAX_BYTES {
        return Head {
            content: text.to_string(),
            truncated: false,
            output_lines: lines.len(),
            first_line_exceeds_limit: false,
        };
    }
    // 第一行自己就超了：一行都放不下，别硬塞半行
    if lines[0].len() > MAX_BYTES {
        return Head {
            content: String::new(),
            truncated: true,
            output_lines: 0,
            first_line_exceeds_limit: true,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in lines.iter().take(MAX_LINES) {
        // 行与行之间的 `\n` 也算进预算（第一行没有前导换行）
        let size = line.len() + if kept.is_empty() { 0 } else { 1 };
        if used + size > MAX_BYTES {
            break;
        }
        kept.push(line);
        used += size;
    }
    Head {
        content: kept.join("\n"),
        truncated: true,
        output_lines: kept.len(),
        first_line_exceeds_limit: false,
    }
}

/// 读取 `text` 的第 `offset` 行（1-indexed）起的 `limit` 行。
///
/// `limit` 为 `None` 表示读到截断上限。`path` 只用于提示信息
/// （续读/超长行会告诉模型该跑哪条 bash）。
pub fn read(
    text: &str,
    path: &str,
    offset: usize,
    limit: Option<usize>,
) -> anyhow::Result<ReadView> {
    let lines: Vec<&str> = text.split('\n').collect();
    // offset 从 1 开始；0 或负数会落到第 1 行（跟参考实现一致）
    let start = offset.saturating_sub(1);
    anyhow::ensure!(
        start < lines.len(),
        "offset {offset} 超出文件末尾（共 {} 行）",
        lines.len()
    );

    let selected: Vec<&str> = match limit {
        Some(l) => lines[start..].iter().take(l).copied().collect(),
        None => lines[start..].to_vec(),
    };
    let t = head(&selected.join("\n"));

    if t.first_line_exceeds_limit {
        // 单行超 50KB：给模型一条能直接跑的 bash 命令
        let kb = lines[start].len() as f64 / 1024.0;
        let quoted = path.replace('\'', "'\\''");
        return Ok(ReadView {
            content: format!(
                "[第 {} 行有 {kb:.1}KB，超过 50.0KB 上限。用 bash：sed -n '{}p' '{quoted}' | head -c {MAX_BYTES}]",
                start + 1,
                start + 1,
            ),
            truncated: true,
            shown_lines: 0,
            total_lines: lines.len(),
        });
    }

    // 后续还有没有内容：给了 limit 就看 limit，没给就看实际截断到哪
    let consumed = match limit {
        Some(l) => start + l.min(lines.len() - start),
        None => start + t.output_lines,
    };

    let content = if t.truncated {
        let reason = if t.output_lines >= MAX_LINES {
            String::new()
        } else {
            "（50.0KB 上限）".to_string()
        };
        format!(
            "{}\n\n[已显示第 {}-{} 行，共 {} 行{reason}。用 offset={} 继续。]",
            t.content,
            start + 1,
            start + t.output_lines,
            lines.len(),
            start + t.output_lines + 1,
        )
    } else if consumed < lines.len() {
        format!(
            "{}\n\n[文件还有 {} 行。用 offset={} 继续。]",
            t.content,
            lines.len() - consumed,
            consumed + 1,
        )
    } else {
        t.content
    };

    Ok(ReadView {
        content,
        truncated: t.truncated,
        shown_lines: t.output_lines,
        total_lines: lines.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(n: usize, body: &str) -> String {
        (0..n)
            .map(|i| format!("{body}{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn small_file_is_returned_verbatim() {
        let v = read("a\nb\nc", "/root/x.txt", 1, None).unwrap();
        assert_eq!(v.content, "a\nb\nc");
        assert!(!v.truncated);
        assert_eq!(v.total_lines, 3);
    }

    #[test]
    fn offset_is_one_indexed() {
        let v = read("a\nb\nc", "/root/x.txt", 2, None).unwrap();
        assert_eq!(v.content, "b\nc");
    }

    #[test]
    fn offset_zero_clamps_to_first_line() {
        let v = read("a\nb", "/root/x.txt", 0, None).unwrap();
        assert_eq!(v.content, "a\nb");
    }

    #[test]
    fn empty_file_reads_as_one_empty_line() {
        let v = read("", "/root/x.txt", 1, None).unwrap();
        assert_eq!(v.content, "");
        assert_eq!(v.total_lines, 1);
    }

    #[test]
    fn offset_past_eof_is_an_error() {
        let err = read("a\nb", "/root/x.txt", 9, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("超出文件末尾"), "{err}");
        assert!(err.contains("共 2 行"), "{err}");
    }

    #[test]
    fn limit_stops_early_and_hints_continue() {
        let v = read(&lines(10, "l"), "/root/x.txt", 1, Some(3)).unwrap();
        assert!(v.content.starts_with("l0\nl1\nl2"), "{}", v.content);
        assert!(v.content.contains("文件还有 7 行"), "{}", v.content);
        assert!(v.content.contains("offset=4"), "{}", v.content);
        assert!(!v.truncated);
    }

    #[test]
    fn offset_and_limit_number_lines_globally() {
        let v = read(&lines(10, "l"), "/root/x.txt", 4, Some(2)).unwrap();
        assert!(v.content.starts_with("l3\nl4"), "{}", v.content);
        assert!(v.content.contains("文件还有 5 行"), "{}", v.content);
        assert!(v.content.contains("offset=6"), "{}", v.content);
    }

    #[test]
    fn line_limit_hits_at_2000() {
        let v = read(&lines(2500, "l"), "/root/x.txt", 1, None).unwrap();
        assert!(v.truncated);
        assert_eq!(v.shown_lines, MAX_LINES);
        assert_eq!(v.total_lines, 2500);
        assert!(
            v.content.contains("已显示第 1-2000 行，共 2500 行"),
            "{}",
            v.content
        );
        // 行数先到，提示里不该说 50KB 上限
        assert!(!v.content.contains("50.0KB 上限"), "{}", v.content);
        assert!(v.content.contains("offset=2001"), "{}", v.content);
    }

    #[test]
    fn byte_limit_hits_and_lines_stay_intact() {
        // 每行约 100 字节 × 2000 行 ≈ 200KB > 50KB，先撞字节上限
        let text = lines(2000, &"x".repeat(98));
        let source: Vec<&str> = text.split('\n').collect();
        let v = read(&text, "/root/x.txt", 1, None).unwrap();
        assert!(v.truncated);
        assert!(v.shown_lines < MAX_LINES, "{}", v.shown_lines);
        let body = v.content.split("\n\n[").next().unwrap();
        assert!(body.len() <= MAX_BYTES, "正文 {} 字节，超了", body.len());
        assert!(v.content.contains("（50.0KB 上限）"), "{}", v.content);
        // 只允许在行边界截断：给出的每一行必须和原文逐字相同
        assert_eq!(
            body.split('\n').collect::<Vec<_>>(),
            &source[..v.shown_lines]
        );
    }

    #[test]
    fn offset_survives_byte_truncation() {
        let text = lines(2000, &"x".repeat(98));
        let v = read(&text, "/root/x.txt", 100, None).unwrap();
        assert!(v.truncated);
        assert!(v.content.contains("已显示第 100-"), "{}", v.content);
    }

    #[test]
    fn first_line_over_limit_suggests_bash() {
        let text = "z".repeat(60 * 1024);
        let v = read(&text, "/root/big.txt", 1, None).unwrap();
        assert!(v.truncated);
        assert_eq!(v.shown_lines, 0);
        assert!(v.content.contains("超过 50.0KB 上限"), "{}", v.content);
        assert!(
            v.content.contains("sed -n '1p' '/root/big.txt'"),
            "{}",
            v.content
        );
    }

    #[test]
    fn path_with_quote_is_shell_escaped() {
        let v = read(&"z".repeat(60 * 1024), "/root/a'b.txt", 1, None).unwrap();
        assert!(v.content.contains("'/root/a'\\''b.txt'"), "{}", v.content);
    }
}
