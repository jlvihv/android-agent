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
//! 永远不成立 —— 哪怕中继就跑在这台电脑上。`/api/pair` 也不接（见 `http::ViaRelay`）：
//! 配对码只该在局域网里输。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpStream;

use crate::state::{AppState, RelayAddr, RemoteInfo};

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
        (Some(a), Some(k)) => {
            agent_relay::parse_key(&k)?;
            let a = a.trim().to_string();
            // 只给了 IP：补默认端口
            let a = if a.parse::<std::net::IpAddr>().is_ok() {
                format!("{a}:{}", agent_relay::DEFAULT_PORT)
            } else {
                a
            };
            Ok(Some(RelayAddr {
                addr: a,
                key: k.trim().to_string(),
            }))
        }
    }
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

/// 记下「出门在外怎么连这台」（配对和 `/api/remote` 交给手机），配了中继就挂上去。
///
/// 中继连不上不是错误：它在后台一直重试，状态往日志里说（只在变化时说一句）。
pub fn start(state: Arc<AppState>, id: Identity, relay: Option<RelayAddr>) -> anyhow::Result<()> {
    if let Ok(mut g) = state.remote.write() {
        *g = Some(RemoteInfo {
            fingerprint: id.fingerprint.clone(),
            relay: relay.clone(),
        });
    }
    let Some(relay) = relay else {
        return Ok(());
    };
    let key = agent_relay::parse_key(&relay.key).map_err(anyhow::Error::msg)?;
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(id.server_config()?));
    let router = crate::http::router(state.clone());
    let log_state = state.clone();
    tokio::spawn(agent_relay::run_host(
        relay.addr,
        key,
        state.instance_id(),
        move |s| {
            tokio::spawn(serve_one(s, acceptor.clone(), router.clone()));
        },
        move |msg| log_state.log("info", msg),
    ));
    Ok(())
}

/// 一条经中继来的连接：TLS 握手，然后把路由挂上去（HTTP/1.1，带 WebSocket 升级）。
async fn serve_one(s: TcpStream, acceptor: tokio_rustls::TlsAcceptor, router: axum::Router) {
    let tls = match acceptor.accept(s).await {
        Ok(t) => t,
        Err(e) => {
            // 多半是对面的指纹对不上（不是配对过的手机）或者半路断了
            tracing::debug!("中继：TLS 握手失败：{e}");
            return;
        }
    };
    let svc = tower::service_fn(move |mut req: http::Request<hyper::body::Incoming>| {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(RELAY_PEER));
        req.extensions_mut().insert(crate::http::ViaRelay);
        let router = router.clone();
        async move { tower::ServiceExt::oneshot(router, req).await }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(
            hyper_util::rt::TokioIo::new(tls),
            hyper_util::service::TowerToHyperService::new(svc),
        )
        .with_upgrades()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_is_created_once_and_then_reused() {
        let dir = std::env::temp_dir().join(format!("agent-remote-{}", uuid::Uuid::new_v4()));
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "重启之后还得是同一张证书");
        assert_eq!(a.fingerprint.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_relay_peer_is_never_mistaken_for_this_machine() {
        // `trust_loopback` 看的就是这个：经中继来的请求永远要凭据
        assert!(!RELAY_PEER.ip().is_loopback());
    }

    /// 用原始 HTTP/1.1 打一个请求（不引客户端库），返回整个响应。
    async fn get(port: u16, path: &str, cookie: Option<&str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let cookie = cookie
            .map(|t| format!("cookie: agent_token={t}\r\n"))
            .unwrap_or_default();
        let req =
            format!("GET {path} HTTP/1.1\r\nhost: localhost\r\n{cookie}connection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        if let Err(e) = s.read_to_end(&mut out).await {
            panic!("{path}: {e}; got {:?}", String::from_utf8_lossy(&out));
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// 整条路：中继 + 家里那头（真路由）+ 手机那头的隧道，全在这个进程里。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_phone_reaches_the_home_core_through_the_relay() {
        const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = l.local_addr().unwrap().to_string();
        tokio::spawn(agent_relay::Relay::new(agent_relay::parse_key(KEY).unwrap()).serve(l));

        // 家里那台：要凭据，而且**信任本机**（和宿主模式一样）—— 中继也在本机，
        // 但经中继进来的请求照样得要凭据
        let st = AppState::new();
        st.set_token(Some("s3cret".into()));
        st.trust_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agent-remote-{}", uuid::Uuid::new_v4()));
        let id = Identity::load_or_create(&dir).unwrap();
        let fp = id.fingerprint.clone();
        let relay = RelayAddr {
            addr: relay_addr.clone(),
            key: KEY.into(),
        };
        start(st.clone(), id, Some(relay)).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let target = |fingerprint: &str| crate::tunnel::Target {
            relay: relay_addr.clone(),
            key: KEY.into(),
            host_id: st.instance_id(),
            fingerprint: fingerprint.into(),
        };
        let t = crate::tunnel::start(target(&fp)).await.unwrap();

        // 带凭据：通的
        let r = get(t.port, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // 不带凭据：经中继来的不算「本机」，要凭据
        let r = get(t.port, "/api/health", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        // 配对只能在局域网里做
        let r = get(t.port, "/api/pair", None).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        // 出门在外怎么连：带着指纹和中继
        let r = get(t.port, "/api/remote", Some("s3cret")).await;
        assert!(r.contains(&fp), "{r}");

        // 指纹不对（中继或者路上有人冒充）：握手不过，WebView 看到一页说明
        let bad = crate::tunnel::start(target(&"0".repeat(64))).await.unwrap();
        let r = get(bad.port, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 502"), "{r}");
        assert!(r.contains("指纹"), "{r}");

        // 那台电脑不在线（id 不对）：也说清楚
        let mut gone = target(&fp);
        gone.host_id = "nobody".into();
        let gone = crate::tunnel::start(gone).await.unwrap();
        let r = get(gone.port, "/", None).await;
        assert!(r.starts_with("HTTP/1.1 502") && r.contains("不在线"), "{r}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
