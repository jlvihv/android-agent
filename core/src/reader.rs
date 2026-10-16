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
//! 5. **结果里带上整个文件的行数和字节数** —— 模型据此判断这份文件是「再读
//!    几轮能读完」还是「得换 bash 的 rg/sed 局部读」。
//! 6. **`numbered` 可选加行号**（`cat -n` 同款）：模型要引用「第几行」时
//!    不用自己数；号是全局行号，跟 `offset` / `sed -n` / `edit` 对得上。
//!
//! `read` 只看文件头，所以这里只实现 `head`；尾截断（bash 输出用）在
//! `linux.rs` 里另有一套，两者互不影响。

/// 一次最多给模型多少行
pub const MAX_LINES: usize = 2000;
/// 一次最多给模型多少字节（UTF-8）
pub const MAX_BYTES: usize = 50 * 1024;

/// 人类可读的字节数：不足 1KB 给 `B`，之后是一位小数的 `KB` / `MB` / `GB`。
///
/// 提示里要告诉模型「这个文件多大」：写 `1234567B` 它还得自己换算，
/// 直接给 `1.2MB` 它一眼就知道该不该整份读。
pub fn fmt_size(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = bytes as f64;
    if n < KB {
        format!("{bytes}B")
    } else if n < MB {
        format!("{:.1}KB", n / KB)
    } else if n < GB {
        format!("{:.1}MB", n / MB)
    } else {
        format!("{:.1}GB", n / GB)
    }
}

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
    /// 文件总字节数（UTF-8，即磁盘上的大小）
    pub total_bytes: usize,
}

struct Head {
    content: String,
    truncated: bool,
    output_lines: usize,
    first_line_exceeds_limit: bool,
}

/// 取文件头：最多 2000 行 / 50KB。
///
/// `lines` 是已经按 offset/limit 选好的片段，`first_number` 是它第一行的
/// 全局行号（1-indexed）。`numbered` 为真时每行前面加行号，**行号本身也占
/// 50KB 预算** —— 否则 2000 行会悄悄超出一大截。
///
/// `phantom`：`lines` 最后一个元素是「结尾换行切出来的空串」而不是真的一行
/// （空文件同理）。`cat -n` 不给它编号，这里也得跳过。
fn head(lines: &[&str], first_number: usize, numbered: bool, phantom: bool) -> Head {
    // 真行数：`cat -n` 只数这些行
    let real_len = lines.len() - usize::from(phantom);
    // 行号宽度是**逐行**按 `%6d` 算的：第 999999 行 6 列，第 1000000 行
    // 7 列，不是整份文件统一一个宽度（真机上对过）。
    let line_prefix = |i: usize| {
        if numbered && i < real_len {
            prefix_len(first_number + i)
        } else {
            0
        }
    };
    let text_len: usize = (0..lines.len())
        .map(|i| line_prefix(i) + lines[i].len())
        .sum::<usize>()
        + lines.len().saturating_sub(1);
    if lines.len() <= MAX_LINES && text_len <= MAX_BYTES {
        return Head {
            content: render(lines, first_number, numbered, phantom),
            truncated: false,
            output_lines: lines.len(),
            first_line_exceeds_limit: false,
        };
    }
    // 第一行自己就超了：一行都放不下，别硬塞半行
    if lines[0].len() + line_prefix(0) > MAX_BYTES {
        return Head {
            content: String::new(),
            truncated: true,
            output_lines: 0,
            first_line_exceeds_limit: true,
        };
    }

    let mut used = 0usize;
    let mut kept = 0usize;
    for (i, line) in lines.iter().take(MAX_LINES).enumerate() {
        // 行与行之间的 `\n` 也算进预算（第一行没有前导换行）
        let size = line_prefix(i) + line.len() + if i == 0 { 0 } else { 1 };
        if used + size > MAX_BYTES {
            break;
        }
        used += size;
        kept += 1;
    }
    Head {
        // 截断到中间时那个结尾空串根本没被保留；万一真保留了（正好切到
        // 结尾），`phantom && kept == lines.len()` 会把结尾 `\n` 补上
        content: render(
            &lines[..kept],
            first_number,
            numbered,
            phantom && kept == lines.len(),
        ),
        truncated: true,
        output_lines: kept,
        first_line_exceeds_limit: false,
    }
}

/// `cat -n` 的行号列数：`printf("%6d")` 里的 6。
const NUMBER_COLS: usize = 6;

/// `cat -n` 的行号前缀占多少字节：`%6d`（至少 6 列）+ 一个 TAB。
/// 位数超过 6 位时自然变宽，所以是逐行算。
fn prefix_len(n: usize) -> usize {
    n.to_string().len().max(NUMBER_COLS) + 1
}

/// 拼正文；`numbered` 为真时给每行加 `cat -n` 同款前缀（`%6d` 右对齐 + TAB）。
///
/// 行号用全局行号：从 `first_number` 开始，而不是从 1 重新数 ——
/// 这样截断提示里的 `offset=N`、`sed -n 'Np'` 跟屏幕上看到的号是同一个。
///
/// 跟 GNU `cat -n` 对齐的两个细节（真机上对过）：
/// - 文本以 `\n` 结尾时，切出来的最后一个空串**不是一行**（`phantom`），
///   不编号，但结尾那个 `\n` 要留下；
/// - 结尾没有 `\n` 时也不补 `\n`。
///
/// 第二点是 GNU 跟 busybox 的分叉：Alpine 的 busybox `cat -n` 会白补一个
/// `\n`（busybox 1.37）。这里跟 GNU 走 —— 不加号时的正文就是文件原文，
/// 加号只应该“插入前缀”，不该动换行。
fn render(lines: &[&str], first_number: usize, numbered: bool, phantom: bool) -> String {
    if !numbered {
        return lines.join("\n");
    }
    let real = if phantom {
        &lines[..lines.len() - 1]
    } else {
        lines
    };
    let mut out = real
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>width$}\t{l}", first_number + i, width = NUMBER_COLS))
        .collect::<Vec<_>>()
        .join("\n");
    // 结尾那个 `\n` 是“空串那一行”贡献的，编号时得手动补回来
    if phantom && !real.is_empty() {
        out.push('\n');
    }
    out
}

/// 读取 `text` 的第 `offset` 行（1-indexed）起的 `limit` 行。
///
/// `limit` 为 `None` 表示读到截断上限。`path` 只用于提示信息
/// （续读/超长行会告诉模型该跑哪条 bash）。`numbered` 为真时每行前加
/// `cat -n` 同款行号（全局行号）。
pub fn read(
    text: &str,
    path: &str,
    offset: usize,
    limit: Option<usize>,
    numbered: bool,
) -> anyhow::Result<ReadView> {
    let lines: Vec<&str> = text.split('\n').collect();
    // 真正的行数：文本以 `\n` 结尾时 `split` 末尾多切出的那个空串不是一行
    //（`wc -l` / `cat -n` 都不数它）。续读提示里的「共几行 / 还有几行」都按它算 ——
    // 按 `lines.len()` 的话，30 行的文件报「共 31 行」，读到最后一行还提示「还有 1 行」。
    let total = if text.is_empty() || text.ends_with('\n') {
        lines.len() - 1
    } else {
        lines.len()
    };
    // offset 从 1 开始；0 或负数会落到第 1 行（跟参考实现一致）
    let start = offset.saturating_sub(1);
    anyhow::ensure!(
        start < lines.len().min(total.max(1)),
        "offset {offset} 超出文件末尾（共 {total} 行）",
    );

    let selected: Vec<&str> = match limit {
        Some(l) => lines[start..].iter().take(l).copied().collect(),
        None => lines[start..].to_vec(),
    };
    // 文本以 `\n` 结尾（或空文件）时，`split('\n')` 会多切出一个空串 ——
    // 它不是一行，`cat -n` 不给它编号（见 `render` 的说明）。只有选中的
    // 片段真的读到文件末尾，这个空串才会出现在 `selected` 里。
    let phantom = numbered
        && start + selected.len() == lines.len()
        && (text.is_empty() || text.ends_with('\n'));
    let t = head(&selected, start + 1, numbered, phantom);

    if t.first_line_exceeds_limit {
        // 单行超 50KB：给模型一条能直接跑的 bash 命令
        let quoted = path.replace('\'', "'\\''");
        return Ok(ReadView {
            content: format!(
                "[第 {} 行有 {}，超过 {} 上限。用 bash：sed -n '{}p' '{quoted}' | head -c {MAX_BYTES}]",
                start + 1,
                fmt_size(lines[start].len()),
                fmt_size(MAX_BYTES),
                start + 1,
            ),
            truncated: true,
            shown_lines: 0,
            total_lines: total,
            total_bytes: text.len(),
        });
    }

    // 后续还有没有内容：给了 limit 就看 limit，没给就看实际截断到哪
    let consumed = match limit {
        Some(l) => start + l.min(total.saturating_sub(start)),
        None => start + t.output_lines,
    };

    // 整个文件的大小：续读提示里给模型一个「还要读几轮」的量级
    let size = fmt_size(text.len());

    let content = if t.truncated {
        let reason = if t.output_lines >= MAX_LINES {
            String::new()
        } else {
            format!("，已到 {} 上限", fmt_size(MAX_BYTES))
        };
        format!(
            "{}\n\n[已显示第 {}-{} 行，共 {} 行（{size}{reason}）。用 offset={} 继续。]",
            t.content,
            start + 1,
            start + t.output_lines,
            total,
            start + t.output_lines + 1,
        )
    } else if consumed < total {
        format!(
            "{}\n\n[文件还有 {} 行（共 {total} 行 / {size}）。用 offset={} 继续。]",
            t.content,
            total - consumed,
            consumed + 1,
        )
    } else {
        t.content
    };

    Ok(ReadView {
        content,
        truncated: t.truncated,
        shown_lines: t.output_lines,
        total_lines: total,
        total_bytes: text.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老测试不关心 `numbered`，这里默认关，省得每处都多写一个参数
    /// （局部定义的 `read` 会盖住 `super::*` 里的同名项）。
    fn read(
        text: &str,
        path: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> anyhow::Result<ReadView> {
        super::read(text, path, offset, limit, false)
    }

    /// 开了行号的那条路。
    fn read_numbered(text: &str, offset: usize, limit: Option<usize>) -> anyhow::Result<ReadView> {
        super::read(text, "/root/x.txt", offset, limit, true)
    }

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
        assert_eq!(v.total_bytes, 5);
    }

    #[test]
    fn total_bytes_counts_utf8_not_chars() {
        let v = read("中文\nabc", "/root/x.txt", 1, None).unwrap();
        assert_eq!(v.total_lines, 2);
        assert_eq!(v.total_bytes, 10); // 6 + 1 + 3
    }

    #[test]
    fn fmt_size_picks_unit() {
        assert_eq!(fmt_size(0), "0B");
        assert_eq!(fmt_size(1023), "1023B");
        assert_eq!(fmt_size(1024), "1.0KB");
        assert_eq!(fmt_size(MAX_BYTES), "50.0KB");
        assert_eq!(fmt_size(3 * 1024 * 1024), "3.0MB");
        assert_eq!(fmt_size(3 * 1024 * 1024 * 1024), "3.0GB");
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
    fn empty_file_reads_as_empty() {
        let v = read("", "/root/x.txt", 1, None).unwrap();
        assert_eq!(v.content, "");
        assert_eq!(v.total_lines, 0);
        assert_eq!(v.total_bytes, 0);
    }

    /// 以换行结尾的文件：末尾那个空串不算一行。读到最后一行就是读完了，不能再提示
    /// 「还有 1 行」；越过最后一行的 offset 报错。
    #[test]
    fn a_trailing_newline_is_not_a_line() {
        let text = format!("{}\n", lines(30, "l"));
        let v = read(&text, "/root/x.txt", 5, Some(3)).unwrap();
        assert!(
            v.content.contains("文件还有 23 行（共 30 行"),
            "{}",
            v.content
        );
        assert!(v.content.contains("offset=8"), "{}", v.content);
        assert_eq!(v.total_lines, 30);

        let v = read(&text, "/root/x.txt", 28, Some(3)).unwrap();
        assert_eq!(v.content, "l27\nl28\nl29", "读到最后一行就没有续读提示");

        let err = read(&text, "/root/x.txt", 31, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("共 30 行"), "{err}");
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
        // 续读提示里带上整个文件的行数/字节数
        assert!(v.content.contains("共 10 行 / "), "{}", v.content);
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
        let text = lines(2500, "l");
        let v = read(&text, "/root/x.txt", 1, None).unwrap();
        assert!(v.truncated);
        assert_eq!(v.shown_lines, MAX_LINES);
        assert_eq!(v.total_lines, 2500);
        assert_eq!(v.total_bytes, text.len());
        assert!(
            v.content.contains("已显示第 1-2000 行，共 2500 行（"),
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
        assert_eq!(v.total_bytes, text.len());
        let body = v.content.split("\n\n[").next().unwrap();
        assert!(body.len() <= MAX_BYTES, "正文 {} 字节，超了", body.len());
        assert!(v.content.contains("已到 50.0KB 上限"), "{}", v.content);
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
        assert_eq!(v.total_bytes, text.len());
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

    // --- numbered（`cat -n` 同款行号）-------------------------------------

    #[test]
    fn numbered_prefixes_every_line_like_cat_n() {
        // 6 列右对齐 + TAB，空行也编号（跟 `cat -n` 一致）
        let v = read_numbered("a\n\nb", 1, None).unwrap();
        assert_eq!(v.content, "     1\ta\n     2\t\n     3\tb");
        assert_eq!(v.shown_lines, 3);
    }

    #[test]
    fn numbered_uses_global_line_numbers() {
        // 号是全局行号：跟 offset / sed -n 对得上，不是从 1 重新数
        let v = read_numbered(&lines(10, "l"), 4, Some(2)).unwrap();
        assert!(
            v.content.starts_with("     4\tl3\n     5\tl4"),
            "{}",
            v.content
        );
        assert!(v.content.contains("offset=6"), "{}", v.content);
    }

    #[test]
    fn number_prefix_is_per_line_like_percent_6d() {
        // `%6d` 是**逐行**的：第 999999 行 6 列，第 1000000 行 7 列
        assert_eq!(prefix_len(1), 7); // 6 列 + TAB
        assert_eq!(prefix_len(999_999), 7);
        assert_eq!(prefix_len(1_000_000), 8);
    }

    #[test]
    fn numbering_counts_toward_the_byte_budget() {
        // 每行 ~100 字节：加上 ~7 字节行号，同样的预算能装的行李变了。
        // 不把行号算进去，2000 行会悄悄超出 50KB 一大截。
        let text = lines(600, &"x".repeat(98));
        let plain = read(&text, "/root/x.txt", 1, None).unwrap();
        let numbered = read_numbered(&text, 1, None).unwrap();
        assert!(
            numbered.shown_lines < plain.shown_lines,
            "{} vs {}",
            numbered.shown_lines,
            plain.shown_lines
        );
        assert!(numbered.truncated);
        let body = numbered.content.split("\n\n[").next().unwrap();
        assert!(body.len() <= MAX_BYTES, "正文 {} 字节，超了", body.len());
    }

    #[test]
    fn numbering_can_push_the_first_line_over_the_limit() {
        // 正好 50KB 的一行：不加号能发，加号就超了，应该改道走 bash 提示
        let text = "z".repeat(MAX_BYTES);
        assert!(!read(&text, "/root/x.txt", 1, None).unwrap().truncated);
        let v = read_numbered(&text, 1, None).unwrap();
        assert!(v.truncated);
        assert_eq!(v.shown_lines, 0);
        assert!(v.content.contains("超过 50.0KB 上限"), "{}", v.content);
    }

    #[test]
    fn numbered_keeps_the_file_terminator_like_gnu_cat_n() {
        // 结尾换行切出来的空串不是一行：不编号，但那个 `\n` 留着
        assert_eq!(
            read_numbered("a\nb\n", 1, None).unwrap().content,
            "     1\ta\n     2\tb\n"
        );
        // 空文件：一行都没有
        assert_eq!(read_numbered("", 1, None).unwrap().content, "");
        // 整个文件就一个空行
        assert_eq!(read_numbered("\n", 1, None).unwrap().content, "     1\t\n");
        // 结尾没换行就不补（GNU 行为；busybox 会白补一个）
        assert_eq!(
            read_numbered("a\nb", 1, None).unwrap().content,
            "     1\ta\n     2\tb"
        );
    }

    /// 有 GNU coreutils 的 `cat` 才能拿它当基准。
    ///
    /// busybox 的 `cat -n` 对结尾没换行的文件会白补一个 `\n`（busybox 1.37），
    /// 跟 GNU 不一样，所以不能拿 busybox 当标准。
    fn have_gnu_cat() -> bool {
        std::process::Command::new("cat")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("GNU coreutils"))
            .unwrap_or(false)
    }

    /// 跑真的 `cat -n`，返回它的 stdout。
    fn cat_n(text: &str, tag: &str) -> String {
        let file =
            std::env::temp_dir().join(format!("agent-catn-{}-{tag}.txt", std::process::id()));
        std::fs::write(&file, text).unwrap();
        let out = std::process::Command::new("cat")
            .arg("-n")
            .arg(&file)
            .output()
            .expect("`cat` 刚在上面确认过能跑");
        std::fs::remove_file(&file).ok();
        assert!(out.status.success(), "cat -n 失败: {:?}", out.status);
        String::from_utf8(out.stdout).unwrap()
    }

    /// 跟真的 GNU `cat -n` 逐字节对一遍 —— 格式这种事不能靠“我以为”。
    #[test]
    fn numbered_matches_real_gnu_cat_n() {
        if !have_gnu_cat() {
            eprintln!("跳过：环境里没有 GNU cat");
            return;
        }
        let cases = [
            "a\nb\n",   // 结尾有换行
            "a\nb",     // 结尾没换行（GNU 不补 `\n`）
            "a\n\nb\n", // 中间空行
            "a\n\n",    // 结尾空行
            "\n",       // 整个文件就一个空行
            "",         // 空文件
        ];
        for (i, text) in cases.iter().enumerate() {
            let expected = cat_n(text, &format!("case{i}"));
            let v = read_numbered(text, 1, None).unwrap();
            assert_eq!(v.content, expected, "输入 {text:?} 跟 GNU cat -n 不一致");
        }
    }

    /// 行号到 7 位的列宽也跟真的 `cat -n` 对一遍（`%6d` 唯一会变宽的地方）。
    #[test]
    fn numbered_seven_digit_width_matches_real_gnu_cat_n() {
        if !have_gnu_cat() {
            eprintln!("跳过：环境里没有 GNU cat");
            return;
        }
        // 100 万行 "x\n"（≈2MB）：第 1000000 行是 `%6d` 的加宽分界
        let text = "x\n".repeat(1_000_000);
        let all = cat_n(&text, "million");
        // 最后三行（`split` 末尾那个空串负责还原结尾 `\n`）
        let mut tail: Vec<&str> = all.split('\n').rev().take(4).collect();
        tail.reverse();
        let v = read_numbered(&text, 999_998, None).unwrap();
        assert_eq!(v.content, tail.join("\n"));
    }
}
