//! 在电脑上扮演手机：起一条到某台电脑的隧道（和手机上 `AgentCore.connectMachine` 是
//! 同一份代码，见 `tunnel.rs`），然后用浏览器 / curl 打开打印出来的地址验证。
//!
//! ```sh
//! cargo run --example tunnel -- <实例id> <证书指纹> [--lan IP:端口] [--relay IP:端口 密钥]
//! ```
//!
//! id 和指纹从那台电脑的 `/api/remote` 拿（电脑上 `curl 127.0.0.1:8787/api/remote`）。

fn main() -> anyhow::Result<()> {
    agent_core::init_tracing();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (Some(id), Some(fp)) = (a.first(), a.get(1)) else {
        anyhow::bail!("用法：tunnel <实例id> <证书指纹> [--lan IP:端口] [--relay IP:端口 密钥]");
    };
    let flag = |name: &str, n: usize| a.iter().position(|x| x == name).and_then(|i| a.get(i + n));
    let lan = flag("--lan", 1).cloned();
    let relay = match (flag("--relay", 1), flag("--relay", 2)) {
        (Some(addr), Some(key)) => Some((
            addr.clone(),
            agent_relay::parse_key(key).map_err(anyhow::Error::msg)?,
        )),
        _ => None,
    };
    let rt = agent_core::runtime()?;
    rt.block_on(async {
        let t = agent_core::tunnel::start(agent_core::tunnel::Target {
            host_id: id.clone(),
            name: id.clone(),
            fingerprint: fp.clone(),
            routes: std::sync::Arc::new(agent_core::tunnel::Routes::new(lan, relay)),
        })
        .await?;
        println!("http://localhost:{}", t.port);
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}
