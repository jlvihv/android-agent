//! 出门在外连家里的电脑 —— **家里电脑这一头**（见 docs/12-relay.md）。
//!
//! 两件事：
//!
//! 1. **证书**：第一次用时生成一张自签证书存在数据目录里（之后一直是这一张）。
//!    手机配对时记下它的指纹，经中继连过来时只认它（见 `tunnel.rs`）。
//! 2. **挂在中继上**：主动连出去（家里不用开端口），每来一个拨号就新开一条连接，
//!    在上面跑 TLS，再把**同一份路由**挂上去 —— 手机看到的界面、API 和局域网直连
//!    时一模一样。
//!
//! 经中继进来的请求一律**按外人对待**：对端地址记成一个非 loopback 的占位
//! （`RELAY_PEER`），所以「电脑自己来的不要凭据」那条（`trust_loopback`）对它们
//! 永远不成立 —— 哪怕中继就跑在这台电脑上。`/api/pair` 也不接（见 `http::Via`）：
//! 配对码只该在局域网里输。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpStream;

use std::sync::Mutex;

use crate::state::{AppState, RelayAddr, RelayControl, RemoteInfo};

/// 经中继进来的请求的「对端地址」。**不能是 loopback**（见模块说明）。
pub const RELAY_PEER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0);

/// 中继的配置：`AGENT_RELAY=<服务器 IP>:<端口>` + `AGENT_RELAY_KEY=<密钥>`
/// （中继第一次启动时会打印这两行）。
pub fn relay_from_env() -> Result<Option<RelayAddr>, String> {
    let addr = std::env::var("AGENT_RELAY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let key = std::env::var("AGENT_RELAY_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty());
    match (addr, key) {
        (None, None) => Ok(None),
        (Some(_), None) => Err("设了 AGENT_RELAY 但没设 AGENT_RELAY_KEY".into()),
        (None, Some(_)) => Err("设了 AGENT_RELAY_KEY 但没设 AGENT_RELAY".into()),
        (Some(a), Some(k)) => checked(RelayAddr { addr: a, key: k }).map(Some),
    }
}

/// 核对并规范化一份中继配置：密钥格式对、地址是 `主机:端口`（只给了 IP 就补默认端口）。
fn checked(r: RelayAddr) -> Result<RelayAddr, String> {
    let key = r.key.trim().to_string();
    agent_relay::parse_key(&key)?;
    let addr = r.addr.trim().to_string();
    let addr = if addr.parse::<std::net::IpAddr>().is_ok() {
        format!("{addr}:{}", agent_relay::DEFAULT_PORT)
    } else {
        addr
    };
    match addr.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {}
        _ => return Err(format!("中继地址应当是 IP:端口，拿到的是「{addr}」")),
    }
    Ok(RelayAddr { addr, key })
}

/// 写一个只给自己读的文件（0600）：里面有私钥。
///
/// 只有这里用得上（手机那一头没这个模块）—— 放在 `machines.rs` 里的话，
/// 安卓构建会报「never used」。
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(path)?.write_all(bytes)
}

/// 这台电脑的 TLS 身份（证书 + 私钥）。
pub struct Identity {
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: String,
}

impl Identity {
    /// 从 `dir` 读；没有就生成一张存进去（私钥文件 0600）。
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        let (cert_p, key_p) = (dir.join("cert.der"), dir.join("key.der"));
        let (cert, key) = match (std::fs::read(&cert_p), std::fs::read(&key_p)) {
            (Ok(c), Ok(k)) => (c, k),
            _ => {
                let c =
                    rcgen::generate_simple_self_signed(vec![crate::tunnel::SERVER_NAME.into()])?;
                let (cert, key) = (c.cert.der().to_vec(), c.signing_key.serialize_der());
                std::fs::create_dir_all(dir)?;
                write_private(&key_p, &key)?;
                std::fs::write(&cert_p, &cert)?;
                tracing::info!("生成了这台电脑的 TLS 证书：{}", cert_p.display());
                (cert, key)
            }
        };
        Ok(Self {
            fingerprint: crate::tunnel::fingerprint(&cert),
            cert: CertificateDer::from(cert),
            key: PrivatePkcs8KeyDer::from(key),
        })
    }

    fn server_config(&self) -> anyhow::Result<rustls::ServerConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Ok(rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?)
    }
}

/// 记下「出门在外怎么连这台」（配对和 `/api/remote` 交给手机），有中继就挂上去。
///
/// 中继配置从哪来，按顺序：
/// 1. 环境变量 `AGENT_RELAY` / `AGENT_RELAY_KEY`（`env`）—— 设了就以它为准，手机改不了；
/// 2. 库里的 `relay` 表 —— **手机交过来的**（`POST /api/remote/relay`，见 `Host::attach`）。
///    手机连过一台配了中继的电脑，就记得那个中继；在家连上别的电脑时顺手把它交过去，
///    那台电脑就不用再手动复制密钥了。
///
/// 中继连不上不是错误：它在后台一直重试，状态往日志里说（只在变化时说一句）。
pub fn start(state: Arc<AppState>, id: Identity, env: Option<RelayAddr>) -> anyhow::Result<()> {
    let from_env = env.is_some();
    let db = state.db();
    let relay = env.or_else(|| checked(db.as_ref()?.relay().ok()??).ok());
    if let Ok(mut g) = state.remote.write() {
        *g = Some(RemoteInfo {
            fingerprint: id.fingerprint.clone(),
            relay: None,
        });
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(id.server_config()?));
    // 局域网上也认 TLS（同一个端口，见 `http::accept_conn`）：手机 App 在家也走加密的那条
    let _ = state.lan_tls.set(acceptor.clone());
    let host = Arc::new(Host {
        state: state.clone(),
        acceptor,
        from_env,
        task: Mutex::new(None),
    });
    if let Some(r) = relay {
        host.connect(r)?;
    }
    if let Ok(mut g) = state.relay_host.write() {
        *g = Some(host);
    }
    Ok(())
}

/// 挂在中继上的那一头。可以在运行时换中继（`attach`），换的时候把旧的那条连接停掉。
struct Host {
    state: Arc<AppState>,
    acceptor: tokio_rustls::TlsAcceptor,
    /// 中继是环境变量配的：以它为准，手机改不了
    from_env: bool,
    /// 正挂着的那条（`run_host` 永不返回，换中继时 abort 掉）
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Host {
    fn connect(&self, relay: RelayAddr) -> anyhow::Result<()> {
        let key = agent_relay::parse_key(&relay.key).map_err(anyhow::Error::msg)?;
        let acceptor = self.acceptor.clone();
        let router = crate::http::router(self.state.clone());
        let log_state = self.state.clone();
        let task = tokio::spawn(agent_relay::run_host(
            relay.addr.clone(),
            key,
            self.state.instance_id(),
            move |s| {
                tokio::spawn(serve_one(s, acceptor.clone(), router.clone()));
            },
            move |msg| log_state.log("info", msg),
        ));
        if let Ok(mut g) = self.task.lock() {
            if let Some(old) = g.replace(task) {
                old.abort();
            }
        }
        if let Ok(mut g) = self.state.remote.write() {
            if let Some(info) = g.as_mut() {
                info.relay = Some(relay);
            }
        }
        Ok(())
    }
}

impl RelayControl for Host {
    fn attach(&self, relay: RelayAddr) -> Result<(), String> {
        if self.from_env {
            return Err("这台电脑的中继是用环境变量配的（AGENT_RELAY），要换请改环境变量".into());
        }
        let relay = checked(relay)?;
        // 已经挂着同一个就什么都不做（手机每次在家连上都会来对一遍）
        let same = self
            .state
            .remote
            .read()
            .ok()
            .and_then(|g| g.as_ref().and_then(|i| i.relay.clone()))
            .is_some_and(|r| r.addr == relay.addr && r.key == relay.key);
        if same {
            return Ok(());
        }
        let db = self.state.db().ok_or("这台电脑没有库，记不下中继配置")?;
        db.set_relay(&relay)
            .map_err(|e| format!("记不下中继配置：{e:#}"))?;
        self.state.log(
            "info",
            format!(
                "手机交来了中继 {}，挂上去（已记下，下次启动照样挂）",
                relay.addr
            ),
        );
        self.connect(relay).map_err(|e| format!("{e:#}"))
    }
}

/// 一条经中继来的连接：TLS 握手，然后交给和局域网同一个 `serve_conn`。
async fn serve_one(s: TcpStream, acceptor: tokio_rustls::TlsAcceptor, router: axum::Router) {
    match acceptor.accept(s).await {
        Ok(tls) => crate::http::serve_conn(tls, router, RELAY_PEER, crate::http::Via::Relay).await,
        // 多半是对面的指纹对不上（不是配对过的手机）或者半路断了
        Err(e) => tracing::debug!("中继：TLS 握手失败：{e}"),
    }
}
