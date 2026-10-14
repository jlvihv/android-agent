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

/// 中继配置记在哪（数据目录里，和证书放在一起）。
const RELAY_FILE: &str = "relay.json";

/// 记下「出门在外怎么连这台」（配对和 `/api/remote` 交给手机），有中继就挂上去。
///
/// 中继配置从哪来，按顺序：
/// 1. 环境变量 `AGENT_RELAY` / `AGENT_RELAY_KEY`（`env`）—— 设了就以它为准，手机改不了；
/// 2. `dir/relay.json` —— **手机交过来的**（`POST /api/remote/relay`，见 `Host::attach`）。
///    手机连过一台配了中继的电脑，就记得那个中继；在家连上别的电脑时顺手把它交过去，
///    那台电脑就不用再手动复制密钥了。
///
/// 中继连不上不是错误：它在后台一直重试，状态往日志里说（只在变化时说一句）。
pub fn start(
    state: Arc<AppState>,
    id: Identity,
    env: Option<RelayAddr>,
    dir: &Path,
) -> anyhow::Result<()> {
    let from_env = env.is_some();
    let file = dir.join(RELAY_FILE);
    let relay = env.or_else(|| {
        let raw = std::fs::read(&file).ok()?;
        let r: RelayAddr = serde_json::from_slice(&raw).ok()?;
        checked(r).ok()
    });
    if let Ok(mut g) = state.remote.write() {
        *g = Some(RemoteInfo {
            fingerprint: id.fingerprint.clone(),
            relay: None,
        });
    }
    let host = Arc::new(Host {
        state: state.clone(),
        acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(id.server_config()?)),
        from_env,
        file,
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
    /// 手机交过来的中继记在这儿
    file: std::path::PathBuf,
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
        let json = serde_json::to_vec_pretty(&relay).map_err(|e| e.to_string())?;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("记不下中继配置：{e}"))?;
        }
        // 里面有中继密钥：只给自己读
        write_private(&self.file, &json).map_err(|e| format!("记不下中继配置：{e}"))?;
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
        start(st.clone(), id, Some(relay), &dir).unwrap();
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

    /// 手机把中继交给一台还没配的电脑：立刻挂上（经中继能连）、记下来（下次启动照样挂）。
    /// 环境变量配的那台不让改。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relay_handed_over_by_the_phone_is_attached_and_remembered() {
        const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_addr = l.local_addr().unwrap().to_string();
        tokio::spawn(agent_relay::Relay::new(agent_relay::parse_key(KEY).unwrap()).serve(l));

        let st = AppState::new();
        st.set_token(Some("s3cret".into()));
        let dir = std::env::temp_dir().join(format!("agent-remote-{}", uuid::Uuid::new_v4()));
        let id = Identity::load_or_create(&dir).unwrap();
        let fp = id.fingerprint.clone();
        start(st.clone(), id, None, &dir).unwrap();
        assert!(st.remote.read().unwrap().as_ref().unwrap().relay.is_none());

        // 手机交过来（`POST /api/remote/relay` 调的就是它）
        let host = st.relay_host.read().unwrap().clone().unwrap();
        let bad = RelayAddr {
            addr: relay_addr.clone(),
            key: "短".into(),
        };
        assert!(host.attach(bad).is_err(), "坏密钥不收");
        host.attach(RelayAddr {
            addr: relay_addr.clone(),
            key: KEY.into(),
        })
        .unwrap();
        assert!(dir.join(RELAY_FILE).exists(), "要记下来");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let t = crate::tunnel::start(crate::tunnel::Target {
            relay: relay_addr.clone(),
            key: KEY.into(),
            host_id: st.instance_id(),
            fingerprint: fp.clone(),
        })
        .await
        .unwrap();
        let r = get(t.port, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "交过来就立刻能用：{r}");

        // 重启：没有环境变量，从记下的那份挂
        let st2 = AppState::new();
        start(
            st2.clone(),
            Identity::load_or_create(&dir).unwrap(),
            None,
            &dir,
        )
        .unwrap();
        let relay = st2.remote.read().unwrap().as_ref().unwrap().relay.clone();
        assert_eq!(relay.map(|r| r.addr), Some(relay_addr.clone()));

        // 环境变量配的：以它为准，手机改不了
        let st3 = AppState::new();
        let env = RelayAddr {
            addr: relay_addr.clone(),
            key: KEY.into(),
        };
        start(
            st3.clone(),
            Identity::load_or_create(&dir).unwrap(),
            Some(env.clone()),
            &dir,
        )
        .unwrap();
        let host3 = st3.relay_host.read().unwrap().clone().unwrap();
        assert!(host3.attach(env).unwrap_err().contains("环境变量"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
