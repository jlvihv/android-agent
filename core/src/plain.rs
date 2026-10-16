//! 结构化数据（JSON）→ 给模型看的纯文本。
//!
//! `agent` 命令的输出都是纯文本（和工具结果一个写法）：原样透出 JSON 的话，模型看到的是
//! 一串引号、花括号和 `\n` 转义，读起来费劲、也占 token。但手机系统能力（短信、联系人、
//! 日历…）和天气回的本来就是结构化数据，一个命令一个形状 —— 不值得每个手写一遍排版，
//! 这里给一个通用的：
//!
//! - 对象：一行一个 `键: 值`，值是对象 / 数组的就往里缩进一层
//! - 一组**扁平且字段相同**的记录（逐小时天气、短信列表、联系人）：表格 —— 一行表头，
//!   一条记录一行，列用 ` | ` 隔开（比每条记录摊成好几行省得多，也好对比）
//! - 别的数组：一项一行，前面加 `- `
//! - 空值写 `-`，空数组写 `（无）`

use serde_json::Value;

/// 渲染成文字（不带结尾换行）。
pub fn render(v: &Value) -> String {
    let mut lines = Vec::new();
    block(v, 0, &mut lines);
    lines.join("\n")
}

fn block(v: &Value, indent: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(indent);
    match v {
        Value::Object(m) => {
            if m.is_empty() {
                out.push(format!("{pad}（无）"));
            }
            for (k, x) in m {
                if is_scalar(x) {
                    out.push(format!("{pad}{k}: {}", scalar(x)));
                } else if x.as_array().is_some_and(|a| a.is_empty()) {
                    out.push(format!("{pad}{k}: （无）"));
                } else {
                    out.push(format!("{pad}{k}:"));
                    block(x, indent + 1, out);
                }
            }
        }
        Value::Array(a) => {
            if a.is_empty() {
                out.push(format!("{pad}（无）"));
            } else if let Some(cols) = table_columns(a) {
                out.push(format!("{pad}{}", cols.join(" | ")));
                for row in a {
                    let cells: Vec<String> = cols.iter().map(|c| cell(&row[c.as_str()])).collect();
                    out.push(format!("{pad}{}", cells.join(" | ")));
                }
            } else {
                for x in a {
                    if is_scalar(x) {
                        out.push(format!("{pad}- {}", scalar(x)));
                    } else {
                        // 非扁平的记录：第一行带 `- `，其余对齐缩进
                        let mut sub = Vec::new();
                        block(x, 0, &mut sub);
                        for (i, l) in sub.iter().enumerate() {
                            let lead = if i == 0 { "- " } else { "  " };
                            out.push(format!("{pad}{lead}{l}"));
                        }
                    }
                }
            }
        }
        x => out.push(format!("{pad}{}", scalar(x))),
    }
}

/// 这几列放最前面（「哪一条」先说，再说内容）；其余按键名排（serde_json 的对象本来就是
/// 按键名排的，同样的数据永远排出同样的字节）。
const LEADING: &[&str] = &["time", "date", "name", "id", "title"];

/// 一组记录能不能排成表：全是对象、值全是标量、字段集合一样。
fn table_columns(a: &[Value]) -> Option<Vec<String>> {
    let first = a.first()?.as_object()?;
    if first.is_empty() || first.values().any(|x| !is_scalar(x)) {
        return None;
    }
    let mut cols: Vec<String> = first.keys().cloned().collect();
    cols.sort_by_key(|c| LEADING.iter().position(|l| l == c).unwrap_or(LEADING.len()));
    for row in a {
        let o = row.as_object()?;
        if o.len() != cols.len() || !cols.iter().all(|c| o.get(c).is_some_and(is_scalar)) {
            return None;
        }
    }
    // 只有一条的话表格不比 `键: 值` 好读
    (a.len() > 1).then_some(cols)
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Object(_) | Value::Array(_))
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) if s.is_empty() => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 表格里的一格：换行和 `|` 会把表弄乱，压成一行
fn cell(v: &Value) -> String {
    scalar(v).replace(['\n', '\r'], " ").replace('|', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_become_key_value_lines() {
        let v = json!({ "battery": 82, "charging": true, "model": "Pixel", "note": null });
        assert_eq!(
            render(&v),
            "battery: 82\ncharging: true\nmodel: Pixel\nnote: -"
        );
    }

    #[test]
    fn uniform_records_become_a_table() {
        let v = json!({ "messages": [
            { "from": "10086", "body": "话费\n余额 | 12 元" },
            { "from": "妈", "body": "吃了吗" },
        ]});
        assert_eq!(
            render(&v),
            "messages:\n  body | from\n  话费 余额 / 12 元 | 10086\n  吃了吗 | 妈",
            "换行和竖线压掉，表不乱"
        );
    }

    #[test]
    fn time_and_name_columns_come_first() {
        let v = json!([
            { "temperature_c": 20, "time": "10:00", "code": 3 },
            { "temperature_c": 21, "time": "11:00", "code": 1 },
        ]);
        assert_eq!(
            render(&v),
            "time | code | temperature_c\n10:00 | 3 | 20\n11:00 | 1 | 21"
        );
    }

    #[test]
    fn other_shapes_nest_with_dashes() {
        let v = json!({
            "tags": ["a", "b"],
            "empty": [],
            "people": [
                { "name": "张三", "phones": ["1", "2"] },
                { "name": "李四", "phones": [] },
            ],
        });
        assert_eq!(
            render(&v),
            "empty: （无）\npeople:\n  - name: 张三\n    phones:\n      - 1\n      - 2\n  - name: 李四\n    phones: （无）\ntags:\n  - a\n  - b"
        );
    }

    #[test]
    fn scalars_and_empties() {
        assert_eq!(render(&json!("就一句话")), "就一句话");
        assert_eq!(render(&json!(null)), "-");
        assert_eq!(render(&json!([])), "（无）");
        assert_eq!(render(&json!({})), "（无）");
    }
}
