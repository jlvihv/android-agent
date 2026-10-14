//! 在电脑上扮演手机：起一条到某台电脑的隧道（和手机上 `AgentCore.connectMachine` 是
//! 同一份代码，见 `tunnel.rs`），然后用浏览器 / curl 打开打印出来的地址验证。
//!
//! ```sh
//! cargo run --example tunnel -- <实例id> <证书指纹> <那台电脑的 token> \
//!     [--lan IP:端口] [--relay IP:端口 密钥] [--local 127.0.0.1:8787]
//! ```
//!
//! id / 指纹 / token 从那台电脑的配对结果里拿（`POST https://它/api/pair`）。
//! 没有 `--local` 时 `/api/machines*` 那几条会回 502（那台电脑上根本没这个上游可转）。

fn main() -> anyhow::Result<()> {
    agent_core::init_tracing();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (Some(id), Some(fp)) = (a.first(), a.get(1)) else {
        anyhow::bail!(
            "用法：tunnel <实例id> <证书指纹> <token> [--lan IP:端口] [--relay IP:端口 密钥] [--local IP:端口 [token]]"
        );
    };
    let flag = |name: &str, n: usize| a.iter().position(|x| x == name).and_then(|i| a.get(i + n));
    let token = a.get(2).cloned().unwrap_or_default();
    let lan = flag("--lan", 1).cloned();
    // 本机核心：隧道把 `/api/machines*` 转给它。不给就只当纯转发（那几条会 502）。
    let (laddr, ltoken) = match flag("--local", 1) {
        Some(addr) => (addr.clone(), flag("--local", 2).cloned()),
        None => ("127.0.0.1:1".to_string(), None),
    };
    let relay = match (flag("--relay", 1), flag("--relay", 2)) {
        (Some(addr), Some(key)) => Some((
            addr.clone(),
            agent_relay::parse_key(key).map_err(anyhow::Error::msg)?,
        )),
        _ => None,
    };
    let rt = agent_core::runtime()?;
    rt.block_on(async {
        let t = agent_core::tunnel::start(
            agent_core::tunnel::Target {
                host_id: id.clone(),
                name: id.clone(),
                fingerprint: fp.clone(),
                token,
                routes: std::sync::Arc::new(agent_core::tunnel::Routes::new(lan, relay)),
            },
            agent_core::tunnel::Local {
                addr: laddr,
                token: ltoken,
            },
            None,
        )
        .await?;
        println!("http://localhost:{}/?k={}", t.port, t.key);
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}
