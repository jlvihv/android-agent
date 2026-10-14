//! 中继服务器（跑在一台有公网 IP 的服务器上，见 docs/12-relay.md）。
//!
//! ```sh
//! agent-relay                     # 听 0.0.0.0:8790，密钥存在 ./relay.key
//! AGENT_RELAY_LISTEN=0.0.0.0:9000 AGENT_RELAY_KEY_FILE=/var/lib/agent-relay/key agent-relay
//! ```
//!
//! 第一次启动会生成密钥并打印出家里电脑要设的两个环境变量。

use std::path::PathBuf;

use agent_relay::{parse_key, random_hex, Relay, DEFAULT_PORT};

fn main() -> std::io::Result<()> {
    let listen =
        std::env::var("AGENT_RELAY_LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{DEFAULT_PORT}"));
    let key_file =
        PathBuf::from(std::env::var("AGENT_RELAY_KEY_FILE").unwrap_or_else(|_| "relay.key".into()));

    let key_hex = match std::fs::read_to_string(&key_file) {
        Ok(k) => k.trim().to_string(),
        Err(_) => {
            let k = random_hex(32);
            write_secret(&key_file, &k)?;
            eprintln!("生成了新的中继密钥：{}", key_file.display());
            k
        }
    };
    let key = parse_key(&key_hex).map_err(std::io::Error::other)?;

    // 中继只等 IO：两个 worker 足够（服务器上别按 CPU 核数开一堆线程）
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&listen).await?;
        let port = listener.local_addr()?.port();
        eprintln!();
        eprintln!("  agent-relay v{} 听在 {listen}", env!("CARGO_PKG_VERSION"));
        eprintln!("  家里那台电脑启动核心前设上（<服务器 IP> 换成这台服务器的公网 IP）：");
        eprintln!("    export AGENT_RELAY=<服务器 IP>:{port}");
        eprintln!("    export AGENT_RELAY_KEY={key_hex}");
        eprintln!("  服务器的防火墙 / 安全组要放行 TCP {port}。");
        eprintln!();
        tokio::select! {
            r = Relay::new(key).serve(listener) => r,
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    })
}

/// 密钥文件只给自己读（0600）。
fn write_secret(path: &PathBuf, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        writeln!(f, "{content}")
    }
    #[cfg(not(unix))]
    std::fs::write(path, format!("{content}\n"))
}
