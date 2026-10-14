//! 在电脑上扮演「出门在外的手机」：起一条经中继到家里那台电脑的隧道（和手机上
//! `AgentCore.startTunnel` 是同一份代码，见 `tunnel.rs`），然后用浏览器 / curl
//! 打开打印出来的地址验证。
//!
//! ```sh
//! cargo run --example tunnel -- <中继 IP:端口> <中继密钥> <电脑的实例 id> <证书指纹>
//! ```
//!
//! 后两个参数从那台电脑的 `/api/remote` 拿（电脑上 `curl 127.0.0.1:8787/api/remote`）。

fn main() -> anyhow::Result<()> {
    agent_core::init_tracing();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [relay, key, host_id, fingerprint] = a.as_slice() else {
        anyhow::bail!("用法：tunnel <中继 IP:端口> <中继密钥> <电脑的实例 id> <证书指纹>");
    };
    let rt = agent_core::runtime()?;
    rt.block_on(async {
        let t = agent_core::tunnel::start(agent_core::tunnel::Target {
            relay: relay.clone(),
            key: key.clone(),
            host_id: host_id.clone(),
            fingerprint: fingerprint.clone(),
        })
        .await?;
        println!("http://localhost:{}", t.port);
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}
