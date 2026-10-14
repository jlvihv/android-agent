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
                crate::machines::write_private(&key_p, &key)?;
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
pub fn start(
    state: Arc<AppState>,
    id: Identity,
    env: Option<RelayAddr>,
    dir: &Path,
) -> anyhow::Result<()> {
    let from_env = env.is_some();
    let db = state.db();
    if let Some(db) = &db {
        migrate_relay_file(db, &dir.join("relay.json"));
    }
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

/// **一次性搬家**（2026-10-14）：手机交来的中继以前记在 `remote/relay.json`，搬进
/// `relay` 表、删掉文件。所有电脑都搬完就可以删掉它。
fn migrate_relay_file(db: &crate::db::Db, path: &Path) {
    let Ok(raw) = std::fs::read(path) else { return };
    if let Ok(r) = serde_json::from_slice::<RelayAddr>(&raw) {
        if db.set_relay(&r).is_err() {
            return;
        }
    }
    let _ = std::fs::remove_file(path);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machines::Machines;

    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn the_identity_is_created_once_and_then_reused() {
        let dir = tmp();
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint, "重启之后还得是同一张证书");
        assert_eq!(a.fingerprint.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 以前的 `remote/relay.json` 搬进 `relay` 表、文件删掉。
    #[test]
    fn the_old_relay_file_moves_into_the_database() {
        let dir = tmp();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("relay.json");
        std::fs::write(&file, r#"{"addr":"1.2.3.4:8790","key":"k"}"#).unwrap();
        let db = crate::db::Db::in_memory().unwrap();
        migrate_relay_file(&db, &file);
        assert!(!file.exists());
        assert_eq!(
            db.relay().unwrap().map(|r| r.addr).as_deref(),
            Some("1.2.3.4:8790")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_relay_peer_is_never_mistaken_for_this_machine() {
        // `trust_loopback` 看的就是这个：经中继来的请求永远要凭据
        assert!(!RELAY_PEER.ip().is_loopback());
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("agent-remote-{}", uuid::Uuid::new_v4()))
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

    /// 一个中继，返回地址。
    async fn relay() -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(agent_relay::Relay::new(agent_relay::parse_key(KEY).unwrap()).serve(l));
        addr
    }

    /// 一台「家里的电脑」：和 `Core::serve_on` 同一个接入循环（明文和 TLS 同一个端口），
    /// 要凭据、**信任本机**（和宿主模式一样）、开着配对。返回 (状态, 端口, 配对码, 数据目录)。
    async fn computer(relay: Option<&str>) -> (Arc<AppState>, u16, String, std::path::PathBuf) {
        let st = AppState::new();
        *st.db.write().unwrap() = Some(Arc::new(crate::db::Db::in_memory().unwrap()));
        st.set_token(Some("s3cret".into()));
        st.trust_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let pairing = Arc::new(crate::desktop::Pairing::new());
        let code = pairing.code().to_string();
        *st.pairing.write().unwrap() = Some(pairing);
        let dir = tmp();
        let relay = relay.map(|a| RelayAddr {
            addr: a.into(),
            key: KEY.into(),
        });
        start(
            st.clone(),
            Identity::load_or_create(&dir).unwrap(),
            relay,
            &dir,
        )
        .unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let (app, st2) = (crate::http::router(st.clone()), st.clone());
        tokio::spawn(async move {
            while let Ok((s, peer)) = l.accept().await {
                let tls = st2.lan_tls.get().cloned();
                tokio::spawn(crate::http::accept_conn(s, peer, app.clone(), tls));
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        (st, port, code, dir)
    }

    /// 从 `connect` 给的地址里取出隧道端口。
    fn port_of(url: &str) -> u16 {
        url.trim_start_matches("http://localhost:")
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// 整条路：手机在局域网里配对（经 TLS）→ 连上（局域网直连）→ 局域网不通了自动走中继；
    /// 同一个端口上明文照样能用（电脑上自己开的浏览器）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_phone_pairs_then_reaches_the_computer_on_the_lan_or_through_the_relay() {
        crate::llm::install_crypto_provider();
        let relay = relay().await;
        let (st, port, code, dir) = computer(Some(&relay)).await;

        // 同一个端口，明文：本机来的不要凭据（电脑自己开浏览器）
        let r = get(port, "/api/health", None).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");

        let phone = Machines::load(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        assert!(
            phone.pair("127.0.0.1", port, "000000").await.is_err(),
            "码不对不给"
        );
        let m = phone.pair("127.0.0.1", port, &code).await.unwrap();
        assert_eq!(m.id, st.instance_id());
        assert_eq!(
            m.relay.as_ref().map(|r| r.addr.as_str()),
            Some(relay.as_str())
        );
        assert!(phone.list()[0].relay.is_some(), "说得出能不能出门连");

        // 连上：局域网直连（TLS）。凭据由 /auth 那一步带上
        let url = phone.connect(&m.id).await.unwrap();
        assert!(url.ends_with("/auth?t=s3cret"), "{url}");
        let t = port_of(&url);
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");

        // 出门了：局域网那个地址不通 → 走中继，照样通
        phone.lan_seen(&m.id, "127.0.0.1:1".into());
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // 经中继来的永远要凭据 —— 哪怕中继、手机、电脑全在这一台机器上（电脑信任本机）
        let r = get(t, "/api/health", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        // 经中继来的不能配对（配对码只该在局域网里输）
        let r = get(t, "/api/pair", None).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");

        // 记账落盘：重新读回来还在
        let again = Machines::load(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        assert_eq!(again.get(&m.id).map(|x| x.token), Some("s3cret".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没配中继、局域网又不通：一页 502 说清楚；指纹不对（冒充的）也是。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_or_impostor_computers_get_a_page_that_says_why() {
        crate::llm::install_crypto_provider();
        let (_st, port, code, dir) = computer(None).await;
        let phone = Machines::load(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        let m = phone.pair("127.0.0.1", port, &code).await.unwrap();

        // 冒充：同一个地址，但我们记着的指纹不是它的
        let mut fake = m.clone();
        fake.id = "fake".into();
        fake.fingerprint = "0".repeat(64);
        phone.upsert(fake);
        let t = port_of(&phone.connect("fake").await.unwrap());
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 502") && r.contains("指纹"), "{r}");

        // 不在同一个网络、也没配中继
        let t = port_of(&phone.connect(&m.id).await.unwrap());
        phone.lan_seen(&m.id, "127.0.0.1:1".into());
        let r = get(t, "/", None).await;
        assert!(
            r.starts_with("HTTP/1.1 502") && r.contains("没配中继"),
            "{r}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手机把中继交给一台还没配的电脑：连上它之后自动交过去，那台电脑立刻挂上、记下来；
    /// 环境变量配的那台不让改。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relay_known_to_the_phone_is_handed_to_a_computer_without_one() {
        crate::llm::install_crypto_provider();
        let relay = relay().await;
        let (st, port, code, dir) = computer(None).await;
        let phone = Machines::load(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        // 手机从别的电脑那里知道了这个中继
        phone.upsert(crate::machines::Machine {
            id: "other".into(),
            name: "别的电脑".into(),
            token: "x".into(),
            fingerprint: "0".repeat(64),
            relay: Some(RelayAddr {
                addr: relay.clone(),
                key: KEY.into(),
            }),
            lan: None,
        });
        let m = phone.pair("127.0.0.1", port, &code).await.unwrap();
        assert!(m.relay.is_none());
        phone.connect(&m.id).await.unwrap();
        // 后台那一下（`sync`）
        for _ in 0..50 {
            if phone.get(&m.id).and_then(|x| x.relay).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(
            phone.get(&m.id).and_then(|x| x.relay).map(|r| r.addr),
            Some(relay.clone()),
            "手机这边记下了"
        );
        let on_computer = st.remote.read().unwrap().as_ref().unwrap().relay.clone();
        assert_eq!(
            on_computer.map(|r| r.addr),
            Some(relay.clone()),
            "电脑挂上了"
        );
        assert_eq!(
            st.db().unwrap().relay().unwrap().map(|r| r.addr),
            Some(relay.clone()),
            "电脑记下了"
        );

        // 坏的不收；环境变量配的不让改
        let host = st.relay_host.read().unwrap().clone().unwrap();
        assert!(host
            .attach(RelayAddr {
                addr: relay.clone(),
                key: "短".into()
            })
            .is_err());
        let st3 = AppState::new();
        let env = RelayAddr {
            addr: relay.clone(),
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
