//! 编译时把 git 提交号写进去（`AGENT_GIT`），版本号显示成 `0.1.0（386baff）`。
//!
//! 只有 `0.1.0` 的话，手机上装的是哪一版根本看不出来（每次都是 0.1.0）。拿不到 git
//! （不在仓库里编、没装 git）就是空串，版本号只显示 `0.1.0`。

use std::process::Command;

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let hash = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=AGENT_GIT={hash}");

    // 提交变了才重跑：HEAD（换分支）和它指着的那个 ref（新提交）
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={dir}/{r}");
        }
    }
}
