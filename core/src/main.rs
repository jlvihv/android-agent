//! 桌面开发用二进制。
//!
//! ```sh
//! just web-build   # 先构建前端（rust-embed 在编译期嵌入）
//! just dev         # cargo run --bin agent-dev
//! ```
//!
//! 打开 <http://127.0.0.1:8787> 就是完整 App —— 和手机上 WebView 看到的
//! 是同一份东西。这是 D3（同源 localhost）最大的好处。

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    agent_core::init_tracing();

    let port: u16 = std::env::var("AGENT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8787);

    let core = agent_core::Core::desktop();
    // 桌面上默认不设防（只有自己在用）；设了 AGENT_TOKEN 就和手机上一样要凭据
    let token = std::env::var("AGENT_TOKEN").ok().filter(|t| !t.is_empty());
    core.state.set_token(token.clone());
    let actual = core.serve(port).await.context("启动 HTTP 服务失败")?;

    println!();
    println!("  android-agent core v{}", agent_core::VERSION);
    match &token {
        Some(t) => println!("  → http://127.0.0.1:{actual}/auth?t={t}"),
        None => println!("  → http://127.0.0.1:{actual}"),
    }
    let device = core.state.device().map(|d| d.name()).unwrap_or_default();
    println!("  （设备：{device}；Ctrl-C 退出）");
    println!();

    // Ctrl-C **和 SIGTERM** 都要走正常退出路径：`Drop` 才会把自己起的那个
    // 无头 Chrome 收掉（`kill <pid>` 默认发 SIGTERM，不处理的话浏览器会变成
    // 孤儿进程）。真被 SIGKILL 就没这个机会了 —— 那种情况靠下次启动时的
    // `cdp::sweep_orphan_chromes` 收。
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    Ok(())
}
