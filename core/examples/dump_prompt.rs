//! 把当前发给模型的 system prompt 和工具表原样打出来，用于精简时的体量核算。
//!
//!   cargo run -q --example dump_prompt
//!
//! 这些东西是**可缓存前缀**的一部分（见 docs/03-prompt-cache.md），
//! 缓存读取只有输入价的 1/50，所以精简它们的意义主要不是省钱，而是：
//! 首轮冷启动、上下文窗口占用、以及模型注意力被无关细节稀释。

use agent_core::agent::{tools, SYSTEM_PROMPT};

fn main() {
    let cjk = |s: &str| s.chars().filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c)).count();

    let mut plain_chars = SYSTEM_PROMPT.chars().count();
    println!(
        "=== SYSTEM_PROMPT · {} 字符（其中汉字 {}） ===\n{SYSTEM_PROMPT}\n",
        plain_chars,
        cjk(SYSTEM_PROMPT)
    );

    for t in tools() {
        let schema = serde_json::to_string_pretty(&(t.schema)()).unwrap();
        let d = t.description.chars().count();
        let s = serde_json::to_string(&(t.schema)()).unwrap().chars().count();
        plain_chars += d + s + t.name.len();
        println!(
            "=== {name} · desc {d} 字符（汉字 {c}）· schema 紧凑 {s} 字符 ===",
            name = t.name,
            c = cjk(t.description)
        );
        println!("{}\n", t.description);
        println!("{schema}\n");
    }

    println!("=== 合计约 {plain_chars} 字符（schema 按线上紧凑写法算）===");
}
