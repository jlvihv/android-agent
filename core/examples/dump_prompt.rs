//! 把当前发给模型的 system prompt 和工具表原样打出来，用于精简时的体量核算。
//!
//!   cargo run -q --example dump_prompt                 # android 形态（这台机器上有没有设备能力，自动判）
//!   cargo run -q --example dump_prompt android --phone  # **手机**那份（含 `agent device|clipboard|…` 清单）
//!   cargo run -q --example dump_prompt linux            # 宿主模式：linux / macos
//!
//! `--phone` / `--no-phone` 显式指定「这台机器有没有设备能力」：那一段只有真手机
//! 上才有，而判据是**编译目标** —— 在电脑上跑这个例子时，不显式给就渲染不出
//! 手机那份。渲染工具渲染不出真实产物，「看提示词」这件事就不成立了
//! （网页版：`scripts/render-prompt.py`）。
//!
//! 这些东西是**可缓存前缀**的一部分（见 docs/03-prompt-cache.md），
//! 缓存读取只有输入价的 1/50，所以精简它们的意义主要不是省钱，而是：
//! 首轮冷启动、上下文窗口占用、以及模型注意力被无关细节稀释。

use agent_core::agent::tools;
use agent_core::platform::Platform;
use agent_core::prompt::system_prompt_with;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let platform = match args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
    {
        None | Some("android") => Platform::Android,
        Some("linux") => Platform::Linux,
        Some("macos") => Platform::Macos,
        Some(other) => panic!("不认识的平台 {other}（android / linux / macos）"),
    };
    // 默认跟这台机器一致（电脑上是 false）；`--phone` 用来渲染手机那份
    let phone = if args.iter().any(|a| a == "--phone") {
        true
    } else if args.iter().any(|a| a == "--no-phone") {
        false
    } else {
        agent_core::linux::has_phone_capabilities()
    };
    let prompt = system_prompt_with(platform, phone);
    let cjk = |s: &str| {
        s.chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .count()
    };

    let mut plain_chars = prompt.chars().count();
    let shape = if phone {
        "手机形态"
    } else {
        "无设备能力"
    };
    println!(
        "=== SYSTEM_PROMPT · {} 字符（其中汉字 {}）· {shape} ===\n{prompt}\n",
        plain_chars,
        cjk(prompt)
    );

    for t in tools(platform) {
        let schema = serde_json::to_string_pretty(&(t.schema)()).unwrap();
        let d = t.description.chars().count();
        let s = serde_json::to_string(&(t.schema)())
            .unwrap()
            .chars()
            .count();
        plain_chars += d + s + t.name.len();
        println!(
            "=== {name} · desc {d} 字符（汉字 {c}）· schema 紧凑 {s} 字符 ===",
            name = t.name,
            c = cjk(&t.description)
        );
        println!("{}\n", t.description);
        println!("{schema}\n");
    }

    println!("=== 合计约 {plain_chars} 字符（schema 按线上紧凑写法算）===");
}
