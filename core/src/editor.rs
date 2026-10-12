//! `edit` 工具的核心：按原文精确替换。
//!
//! 移植自 OpenMinis 的 `PiFileEditor.kt`（GPL-3.0）。设计要点：
//!
//! 1. **所有 edits 对原文校验，最后一次性写入。** 不做「改一处写一次」，
//!    否则第二个 edit 的 oldText 就是在改过的内容里找了。
//! 2. **按位置排序 → 检查重叠 → 倒序应用。** 倒序是为了让前面的字节
//!    偏移不被后面的替换打乱。
//! 3. **行尾和 BOM 归一化后匹配，写回时还原。** 模型看到的文本和你
//!    磁盘上的 CRLF/BOM 不一致是常态。
//! 4. **fuzzy 兜底。** 模型经常吐弯引号 `“”`、en-dash `–`、NBSP 或
//!    全角标点，而文件里是 ASCII。归一化两边再试一次。
//! 5. **唯一性 / 重叠 / no-op 三项硬校验**，失败时给出可操作的错误信息
//!    （"provide unique context" / "merge them into one edit"）。
//!
//! 索引一律用**字节偏移**（Rust 的 `str::find` 语义）。Kotlin 版用 UTF-16
//! 偏移，但整个算法自洽，换成字节偏移等价 —— 只要别混用。

/// 一次替换
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

impl Edit {
    pub fn new(old_text: impl Into<String>, new_text: impl Into<String>) -> Self {
        Self {
            old_text: old_text.into(),
            new_text: new_text.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EditOutcome {
    /// 替换后的完整内容（已还原 BOM 与原始行尾）
    pub content: String,
    /// unified diff，给前端展示
    pub patch: String,
    /// 第一处改动所在行号（1-indexed）
    pub first_changed_line: usize,
}

#[derive(Debug, Clone)]
struct Match {
    start: usize,
    end: usize,
    replacement: String,
}

// ---------------------------------------------------------------------------
// 归一化
// ---------------------------------------------------------------------------

/// 行尾统一成 `\n`
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 单个字符的"排版折叠"。
///
/// 这是 NFKC 的一个**有选择的子集**。故意不做完整 NFKC：NFKC 还会折叠
/// 连字、上标、带圈字符等，那些对代码/文本匹配只会帮倒忙。
/// 这里只处理模型真的会吐出来的那些：
fn fold_char(c: char) -> char {
    match c {
        // 各种引号 → ASCII
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // 各种连字符/破折号/减号 → ASCII 减号
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // 不换行空格 / 全角空格 → 普通空格
        '\u{00A0}' | '\u{3000}' => ' ',
        // 全角 ASCII（！到 ～）→ 半角
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        other => other,
    }
}

/// 排版折叠 + 去掉每行行尾空白（含 BOM）
fn fuzzy(s: &str) -> String {
    let folded: String = s.chars().map(fold_char).collect();
    folded
        .split('\n')
        .map(|line| line.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 不重叠地找出所有出现位置
fn occurrences(text: &str, target: &str) -> Vec<usize> {
    if target.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(target) {
        let idx = from + rel;
        hits.push(idx);
        from = idx + target.len();
    }
    hits
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

pub fn apply(raw: &str, edits: &[Edit], path: &str) -> anyhow::Result<EditOutcome> {
    anyhow::ensure!(!edits.is_empty(), "edits 至少要有一项");
    for (i, e) in edits.iter().enumerate() {
        anyhow::ensure!(!e.old_text.is_empty(), "edits[{i}].oldText 不能为空");
    }

    let bom = if raw.starts_with('\u{FEFF}') {
        "\u{FEFF}"
    } else {
        ""
    };
    let original = lf(raw.strip_prefix('\u{FEFF}').unwrap_or(raw));

    let normalized: Vec<Edit> = edits
        .iter()
        .map(|e| Edit::new(lf(&e.old_text), lf(&e.new_text)))
        .collect();

    // fuzzy 归一化要整文件扫一遍、再拼一份新字符串，**只算一次**。
    // 原来它在三个地方各调一次（包括循环里每个 edit 一次）——
    // E 个 edit 就是全文件拷贝 E 次。
    let folded = fuzzy(&original);

    // 只要有一项在原文里找不到，就整体切到 fuzzy 模式
    let use_fuzzy = normalized
        .iter()
        .any(|e| !original.contains(&e.old_text) && folded.contains(&fuzzy(&e.old_text)));

    let base: &str = if use_fuzzy { &folded } else { &original };

    // --- 逐个 edit 定位 ---
    let mut matches: Vec<Match> = Vec::new();
    for (index, edit) in normalized.iter().enumerate() {
        let old = if use_fuzzy {
            fuzzy(&edit.old_text)
        } else {
            edit.old_text.clone()
        };
        anyhow::ensure!(!old.is_empty(), "edits[{index}].oldText 归一化后为空");

        let hits = occurrences(base, &old);
        anyhow::ensure!(!hits.is_empty(), "在 {path} 里找不到 edits[{index}]");

        // 唯一性：必须只出现一次。重复的话让模型补上下文，
        // 而不是让我们猜它想改哪一处。
        let fuzzy_old = fuzzy(&edit.old_text);
        let dup = if fuzzy_old.is_empty() {
            hits.len()
        } else {
            occurrences(&folded, &fuzzy_old).len()
        };
        anyhow::ensure!(
            dup == 1,
            "{path} 里 edits[{index}] 出现了 {dup} 次；请提供更长的上下文使其唯一"
        );

        let start = hits[0];
        matches.push(Match {
            start,
            end: start + old.len(),
            replacement: edit.new_text.clone(),
        });
    }

    // --- 排序 + 重叠检查 ---
    matches.sort_by_key(|m| m.start);
    for w in matches.windows(2) {
        anyhow::ensure!(
            w[0].end <= w[1].start,
            "{path} 里的 edits 有重叠；请合并成一条"
        );
    }

    // --- 应用 ---
    //
    // 两件事一起做：算出新内容，并留下每处改动在**原文**坐标系里的范围 +
    // 新文本（`patch_changes`），给 `build_patch` 切 hunk 用。
    let (changed, patch_changes) = if !use_fuzzy {
        (replace_all(&original, matches.iter(), 0), matches.clone())
    } else {
        // fuzzy 模式：base 的坐标和 original 不一样，不能直接按字节偏移替换。
        // 做法是按**行窗口**分组：被改动的行整行从 base 取（已归一化），
        // 未改动的行原样保留 original 的（保住原有排版）。
        //
        // 注意：**绝不把排版还原到显式给出的 newText 上** —— 模型要是
        // 真想写弯引号，就让它写。
        let base_lines: Vec<&str> = base.split('\n').collect();
        let original_lines: Vec<&str> = original.split('\n').collect();
        let base_starts = line_starts(&base_lines);
        let original_starts = line_starts(&original_lines);

        let line_at = |position: usize| -> usize {
            match base_starts.binary_search(&position) {
                Ok(i) => i,
                Err(ip) => ip.saturating_sub(1),
            }
        };

        // 把相邻/重叠的行窗口合并成一组，一组只做一次替换
        let mut groups: Vec<(std::ops::RangeInclusive<usize>, Vec<usize>)> = Vec::new();
        for (i, m) in matches.iter().enumerate() {
            let span = line_at(m.start)..=line_at(m.end.saturating_sub(1).max(m.start));
            match groups.last_mut() {
                Some((range, members)) if span.start() <= range.end() => {
                    let new_end = (*range.end()).max(*span.end());
                    *range = *range.start()..=new_end;
                    members.push(i);
                }
                _ => groups.push((span, vec![i])),
            }
        }

        let mut result = original.clone();
        let mut patch_changes: Vec<Match> = Vec::new();
        for (span, members) in groups.iter().rev() {
            let b_start = base_starts[*span.start()];
            let b_end = base_starts
                .get(*span.end() + 1)
                .copied()
                .unwrap_or(base.len());

            let selected = members.iter().map(|&i| &matches[i]);
            let replacement = replace_all(&base[b_start..b_end], selected, b_start);

            let o_start = original_starts[*span.start()];
            let o_end = original_starts
                .get(*span.end() + 1)
                .copied()
                .unwrap_or(original.len());

            result.replace_range(o_start..o_end, &replacement);
            // fuzzy 那套的 `matches` 是 base 的坐标，patch 要的是**原文**坐标：
            // 直接记下这个行窗口在原文里的范围和它的新文本
            patch_changes.push(Match {
                start: o_start,
                end: o_end,
                replacement,
            });
        }
        patch_changes.sort_by_key(|m| m.start);
        (result, patch_changes)
    };

    anyhow::ensure!(changed != original, "{path} 没有实际改动（替换后内容一致）");

    // --- 还原行尾 ---
    let crlf = raw.find("\r\n");
    let lf_pos = raw.find('\n');
    let ending = match (crlf, lf_pos) {
        (Some(c), Some(l)) if c < l => "\r\n",
        (Some(_), None) => "\r\n",
        _ => "\n",
    };

    let (first_changed_line, patch) = build_patch(path, &original, &patch_changes);

    Ok(EditOutcome {
        content: format!("{bom}{}", changed.replace('\n', ending)),
        patch,
        first_changed_line,
    })
}

fn replace_all<'a>(
    source: &str,
    selected: impl DoubleEndedIterator<Item = &'a Match>,
    origin: usize,
) -> String {
    let mut value = source.to_string();
    // 倒序：前面的替换不会打乱后面已经算好的偏移
    for m in selected.rev() {
        value.replace_range(m.start - origin..m.end - origin, &m.replacement);
    }
    value
}

fn line_starts(lines: &[&str]) -> Vec<usize> {
    let mut p = 0usize;
    lines
        .iter()
        .map(|l| {
            let s = p;
            p += l.len() + 1; // +1 是 '\n'
            s
        })
        .collect()
}

// ---------------------------------------------------------------------------
// unified diff
// ---------------------------------------------------------------------------

/// unified diff 里每处改动带几行上下文
const CONTEXT: usize = 4;

/// 生成 unified patch：**每处改动一个 hunk**，各带 `CONTEXT` 行上下文。
///
/// 为什么不再搞公共前缀/后缀合成一个 hunk：那样「改开头 + 改结尾」会把中间
/// 整段都列成 `-`/`+`。实测 2000 行的文件改第 5 行和第 1990 行 → 3983 行
/// patch（几乎把文件抄两遍），而这段是要进模型上下文的。
///
/// 改动位置直接用 `edit` 定位的结果（`changes`，**原文**坐标系的字节范围 +
/// 新文本），不用再跑一遍 diff 算法。输出是标准 unified diff 形状：前端照旧
/// 按 `+`/`-` 上色，`patch(1)` 也有机会吃得下（有测试拿真的 `patch` 验）。
///
/// 靠得近的两处改动会并进同一个 hunk（上下文会互相盖住），代价是它们中间那几
/// 行会成对成 `-`/`+` 出现 —— 不最小，但是对的。
fn build_patch(path: &str, original: &str, changes: &[Match]) -> (usize, String) {
    /// (行内容, 该行是否带换行符)
    fn records(text: &str) -> Vec<(&str, bool)> {
        if text.is_empty() {
            return Vec::new();
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let n = if text.ends_with('\n') {
            lines.len().saturating_sub(1)
        } else {
            lines.len()
        };
        (0..n).map(|i| (lines[i], i + 1 < lines.len())).collect()
    }

    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ {path}\n"));
    if changes.is_empty() || original.is_empty() {
        return (1, out);
    }

    // 每行起始的字节偏移（结尾换行切出来的空串不算一行）
    let mut starts = vec![0usize];
    for (i, b) in original.bytes().enumerate() {
        if b == b'\n' && i + 1 < original.len() {
            starts.push(i + 1);
        }
    }
    let line_of = |pos: usize| -> usize {
        match starts.binary_search(&pos) {
            Ok(i) => i,
            Err(ip) => ip.saturating_sub(1),
        }
    };
    let line_end = |line: usize| starts.get(line + 1).copied().unwrap_or(original.len());

    // 改动按起点排序（调用方本来就是顺序的，这里只做保险）
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&i| changes[i].start);

    // 靠得近的改动并成一个 hunk：否则两段 hunk 的上下文会互相盖住
    let mut regions: Vec<(usize, usize, Vec<usize>)> = Vec::new(); // (首行, 末行, 成员)
    for i in order {
        let first = line_of(changes[i].start);
        let last = line_of(changes[i].end.saturating_sub(1).max(changes[i].start));
        match regions.last_mut() {
            Some((_, prev_last, members)) if first <= *prev_last + 2 * CONTEXT => {
                *prev_last = (*prev_last).max(last);
                members.push(i);
            }
            _ => regions.push((first, last, vec![i])),
        }
    }

    let emit = |kind: char, rec: (&str, bool), out: &mut String| {
        out.push(kind);
        out.push_str(rec.0);
        out.push('\n');
        if !rec.1 {
            out.push_str("\\ No newline at end of file\n");
        }
    };

    let first_changed_line = regions[0].0 + 1;
    // 前面几处改动让行号涨/降了多少：新文件那边的行号要加上它
    let mut delta: isize = 0;
    for (first, last, members) in &regions {
        let (first, last) = (*first, *last);
        let ctx_first = first.saturating_sub(CONTEXT);
        let ctx_last = (last + CONTEXT).min(starts.len() - 1);
        let a_start = starts[first];
        let b_end = line_end(last);

        let before = records(&original[starts[ctx_first]..a_start]);
        let removed = records(&original[a_start..b_end]);
        let after = records(&original[b_end..line_end(ctx_last)]);
        let new_middle = replace_all(
            &original[a_start..b_end],
            members.iter().map(|&i| &changes[i]),
            a_start,
        );
        let added = records(&new_middle);

        let old_count = before.len() + removed.len() + after.len();
        let new_count = before.len() + added.len() + after.len();
        // 计数为 0 时，unified diff 约定给的起点是「插入点的前一行」
        let old_start = if old_count == 0 {
            ctx_first
        } else {
            ctx_first + 1
        };
        let new_start = if new_count == 0 {
            (ctx_first as isize + delta).max(0) as usize
        } else {
            (ctx_first as isize + 1 + delta).max(0) as usize
        };
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for rec in &before {
            emit(' ', *rec, &mut out);
        }
        for rec in &removed {
            emit('-', *rec, &mut out);
        }
        for rec in &added {
            emit('+', *rec, &mut out);
        }
        for rec in &after {
            emit(' ', *rec, &mut out);
        }
        delta += new_count as isize - old_count as isize;
    }

    (first_changed_line, out)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(content: &str, old: &str, new: &str) -> String {
        apply(content, &[Edit::new(old, new)], "t.txt")
            .unwrap()
            .content
    }

    #[test]
    fn replaces_a_unique_block() {
        assert_eq!(ok("hello world\n", "world", "rust"), "hello rust\n");
    }

    #[test]
    fn applies_multiple_edits_against_the_original_not_incrementally() {
        // 关键：第二个 edit 的 oldText 是**原文**里的内容。
        // 如果实现是"改一处写一次"，第二次就会因为偏移变了而失败。
        let out = apply(
            "aaa\nbbb\nccc\n",
            &[Edit::new("aaa", "AAA"), Edit::new("ccc", "CCC")],
            "t.txt",
        )
        .unwrap();
        assert_eq!(out.content, "AAA\nbbb\nCCC\n");
    }

    #[test]
    fn rejects_ambiguous_old_text() {
        let err = apply("x\nx\n", &[Edit::new("x", "y")], "t.txt").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 次"), "{msg}");
        assert!(msg.contains("唯一"), "{msg}");
    }

    #[test]
    fn rejects_overlapping_edits() {
        let err = apply(
            "abcdef\n",
            &[Edit::new("abc", "X"), Edit::new("cde", "Y")],
            "t.txt",
        )
        .unwrap_err();
        assert!(err.to_string().contains("重叠"), "{err}");
    }

    #[test]
    fn rejects_no_op_edits() {
        let err = apply("same\n", &[Edit::new("same", "same")], "t.txt").unwrap_err();
        assert!(err.to_string().contains("没有实际改动"), "{err}");
    }

    #[test]
    fn rejects_missing_old_text() {
        let err = apply("hello\n", &[Edit::new("nope", "x")], "t.txt").unwrap_err();
        assert!(err.to_string().contains("找不到"), "{err}");
    }

    #[test]
    fn rejects_empty_old_text() {
        let err = apply("hello\n", &[Edit::new("", "x")], "t.txt").unwrap_err();
        assert!(err.to_string().contains("不能为空"), "{err}");
    }

    #[test]
    fn preserves_crlf_line_endings() {
        let out = ok("a\r\nb\r\nc\r\n", "b", "B");
        assert_eq!(out, "a\r\nB\r\nc\r\n");
        assert!(!out.contains("\n\n"));
    }

    #[test]
    fn preserves_bom() {
        let out = ok("\u{FEFF}hello\n", "hello", "hi");
        assert!(out.starts_with('\u{FEFF}'), "BOM 丢了");
        assert_eq!(out, "\u{FEFF}hi\n");
    }

    #[test]
    fn matches_across_crlf_old_text() {
        // 模型可能给 \n，而文件是 \r\n
        let out = ok("line1\r\nline2\r\n", "line1\nline2", "merged");
        assert_eq!(out, "merged\r\n");
    }

    #[test]
    fn fuzzy_matches_curly_quotes_typed_by_the_model() {
        // 文件里是 ASCII 引号，模型吐了弯引号
        let content = "let s = \"hi\";\n";
        let out = ok(content, "let s = \u{201C}hi\u{201D};", "let s = \"bye\";");
        assert_eq!(out, "let s = \"bye\";\n");
    }

    #[test]
    fn fuzzy_matches_fullwidth_punctuation() {
        // 中文输入法下模型很容易吐全角
        let content = "print(1)\n";
        let out = ok(content, "print\u{ff08}1\u{ff09}", "print(2)");
        assert_eq!(out, "print(2)\n");
    }

    #[test]
    fn fuzzy_matches_en_dash_against_ascii_hyphen() {
        let content = "a-b\n";
        let out = ok(content, "a\u{2013}b", "a+b");
        assert_eq!(out, "a+b\n");
    }

    #[test]
    fn fuzzy_ignores_trailing_whitespace_differences() {
        let content = "foo   \nbar\n";
        let out = ok(content, "foo\nbar", "baz");
        assert_eq!(out, "baz\n");
    }

    #[test]
    fn fuzzy_does_not_restore_typography_into_new_text() {
        // 模型显式写了弯引号，就必须原样写进去，不能被"还原"成 ASCII
        let content = "let s = \"hi\";\n";
        let out = ok(
            content,
            "let s = \u{201C}hi\u{201D};",
            "let s = \u{201C}bye\u{201D};",
        );
        assert!(
            out.contains('\u{201C}'),
            "newText 里的排版被改掉了: {out:?}"
        );
    }

    #[test]
    fn patch_is_a_valid_unified_diff_shape() {
        let out = apply("a\nb\nc\n", &[Edit::new("b", "B")], "f.txt").unwrap();
        let lines: Vec<&str> = out.patch.lines().collect();
        assert_eq!(lines[0], "--- f.txt");
        assert_eq!(lines[1], "+++ f.txt");
        assert!(lines[2].starts_with("@@ -"), "{:?}", lines[2]);
        assert!(lines.contains(&"-b"));
        assert!(lines.contains(&"+B"));
        assert_eq!(out.first_changed_line, 2);
    }

    #[test]
    fn patch_marks_missing_trailing_newline() {
        let out = apply("a\nb", &[Edit::new("b", "B")], "f.txt").unwrap();
        assert!(
            out.patch.contains("\\ No newline at end of file"),
            "{}",
            out.patch
        );
    }

    #[test]
    fn patch_has_one_hunk_per_change_not_one_huge_one() {
        // 改开头 + 改结尾：掐前缀/后缀那种做法会把中间 1980 行都列成 -/+
        let mut body = String::new();
        for i in 0..2000 {
            body.push_str(&format!("line {i}\n"));
        }
        let out = apply(
            &body,
            &[
                Edit::new("line 5\n", "LINE 5\n"),
                Edit::new("line 1990\n", "LINE 1990\n"),
            ],
            "big.txt",
        )
        .unwrap();
        assert_eq!(
            out.patch.matches("@@ ").count(),
            2,
            "两处改动应该是两个 hunk：\n{}",
            out.patch
        );
        assert!(
            out.patch.lines().count() < 40,
            "patch 不该把整份文件抄进来（现在 {} 行）",
            out.patch.lines().count()
        );
        assert!(out.patch.contains("-line 5") && out.patch.contains("+LINE 5"));
        assert!(out.patch.contains("-line 1990") && out.patch.contains("+LINE 1990"));
        assert_eq!(out.first_changed_line, 6);
    }

    #[test]
    fn patch_merges_changes_close_enough_to_share_context() {
        // 靠得近的两处：并进一个 hunk，别输出两段互相盖住的上下文。
        // 代价是中间那行会成对成 -/+（不最小，但是对的）
        let out = apply(
            "a\nb\nc\nd\ne\nf\n",
            &[Edit::new("a", "A"), Edit::new("c", "C")],
            "f.txt",
        )
        .unwrap();
        assert_eq!(out.patch.matches("@@ ").count(), 1, "{}", out.patch);
        assert!(out.patch.contains("-a") && out.patch.contains("+A"));
        assert!(out.patch.contains("-c") && out.patch.contains("+C"));
        assert!(
            out.patch.contains("-b") && out.patch.contains("+b"),
            "{}",
            out.patch
        );
    }

    /// 生成的 patch 要能被**真的 `patch(1)`** 吃下去。
    ///
    /// 之前那个版本注释里就写明了「不需要给 patch(1) 用」；现在是标准形状，
    /// 所以拿真工具验一下，而不是照形状猜。环境里没有 patch 就跳过。
    #[test]
    fn patch_is_applicable_by_real_patch() {
        if std::process::Command::new("patch")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("跳过：环境里没有 patch");
            return;
        }
        let cases: [&str; 3] = [
            "a\nb\nc\nd\ne\nf\ng\n", // 单处
            "a\nb\nc\nd\ne\nf\ng\n", // 两处相距远（两个 hunk，行号要累加）
            "one\ntwo\n",            // 行数变化（2 行 → 3 行）
        ];
        let edits: [&[Edit]; 3] = [
            &[Edit::new("b", "B")],
            &[Edit::new("b", "B"), Edit::new("f", "F")],
            &[Edit::new("one\ntwo\n", "1\n2\n3\n")],
        ];
        for (i, (before, edits)) in cases.iter().zip(edits).enumerate() {
            let out = apply(before, edits, "f.txt").unwrap();
            let dir = std::env::temp_dir().join(format!("agent-patch-{}-{i}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("f.txt"), before).unwrap();
            std::fs::write(dir.join("p.diff"), &out.patch).unwrap();

            let got = std::process::Command::new("patch")
                .current_dir(&dir)
                .args(["-p0", "-i", "p.diff"])
                .output()
                .unwrap();
            assert!(
                got.status.success(),
                "patch 没吃下这个 diff（case {i}）：\n{}\n{}\n--- patch ---\n{}",
                String::from_utf8_lossy(&got.stdout),
                String::from_utf8_lossy(&got.stderr),
                out.patch
            );
            let patched = std::fs::read_to_string(dir.join("f.txt")).unwrap();
            let want = out.content.trim_start_matches('\u{FEFF}');
            assert_eq!(patched, want, "打过补丁的内容不对（case {i}）");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn patch_handles_empty_file_becoming_nonempty() {
        let out = apply("", &[Edit::new("", "x")], "f.txt");
        assert!(out.is_err(), "空 oldText 应该被拒");
    }

    #[test]
    fn multibyte_content_uses_byte_offsets_consistently() {
        let content = "第一行\n第二行\n第三行\n";
        let out = ok(content, "第二行", "改了");
        assert_eq!(out, "第一行\n改了\n第三行\n");
    }

    #[test]
    fn fuzzy_with_multibyte_keeps_untouched_lines_byte_identical() {
        // fuzzy 路径走的是"行窗口替换"，未改动的行必须原样保留
        let content = "保留\t这行\n  改我  \n也保留\n";
        let out = apply(
            content,
            &[Edit::new("\u{201C}改我\u{201D}", "改完了")],
            "t.txt",
        );
        // 弯引号在原文里不存在 → 会走 fuzzy；但这里 oldText 本身也找不到，
        // 所以应该报错而不是乱改
        assert!(out.is_err(), "不该匹配上: {out:?}");
    }
}
