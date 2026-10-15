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
        start(st.clone(), Identity::load_or_create(&dir).unwrap(), relay).unwrap();
        let port = serve(st.clone()).await;
        (st, port, code, dir)
    }

    /// 起一个和 `Core::serve_on` 一样的监听（明文 + TLS 同一个端口），返回端口。
    ///
    /// 端口要记进 `state`：隧道把 `/api/machines*` 转回本机时就是找它（`local_upstream`）。
    async fn serve(st: Arc<AppState>) -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        st.port.store(port, std::sync::atomic::Ordering::Relaxed);
        let (app, st2) = (crate::http::router(st.clone()), st.clone());
        tokio::spawn(async move {
            loop {
                let (hn, peer) = crate::accept(&l).await;
                let tls = st2.lan_tls.get().cloned();
                tokio::spawn(crate::http::accept_conn(hn, peer, app.clone(), tls));
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        port
    }

    /// 电脑连电脑（和手机连电脑是同一条路）：本机核心 A 和另一台核心 B 都在这一台机器上
    /// 跑着 —— 局域网、配对、TLS、隧道全是同一份代码，只是少了两台物理机器。
    ///
    /// 要的是两件事：**B 的东西归 B 答**，而「选择机器」那份面板归 **A** 自己答
    /// （隧道把 `/api/machines*` 转回来，见 `tunnel.rs`）—— 不然后者在 B 的库里查，
    /// 永远是空的。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_computer_switching_to_another_one_keeps_the_panel_on_itself() {
        crate::llm::install_crypto_provider();
        let (st_b, port_b, code_b, dir_b) = computer(None).await;

        let dir_a = tmp();
        let a = AppState::new();
        a.open_db(&dir_a); // 机器记账跟着库一起建
        a.set_token(Some("a-secret".into()));
        a.trust_loopback
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _port_a = serve(a.clone()).await;
        let machines = a.machines().unwrap();

        // 局域网里看到了 B（手机上是壳的 NsdManager，电脑上是 avahi-browse，报进来的东西一样）
        machines.lan_found(&st_b.instance_id(), "B", "127.0.0.1", port_b);
        let panel = machines.panel();
        assert_eq!(panel.len(), 1, "看到一台就列一台");
        assert!(!panel[0].paired, "还没配对");

        // 第一次连：输 B 终端上那个 6 位码
        let b = machines.pair("127.0.0.1", port_b, &code_b).await.unwrap();
        assert_eq!(b.id, st_b.instance_id());
        let c = machines.clone().connect(&b.id).await.unwrap();

        // 页面拿到的地址里只有隧道钥匙：别的机器的凭据不进页面（见 `tunnel::Conn`）
        assert!(!c.page_url().contains(&c.token), "{}", c.page_url());
        assert!(c.shell_url().contains("t="), "壳那份才有凭据");

        // ① B 自己的东西归 B 答
        let r = get(c.port, "/api/health", Some(&c.token)).await;
        assert!(
            r.starts_with("HTTP/1.1 200") && r.contains("\"ok\":true"),
            "{r}"
        );

        // ② 「选择机器」归 A 答：列表里有刚配上的 B，且写着现在连的是它
        let r = get(c.port, "/api/machines", Some(&c.token)).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains(&b.id), "{r}");
        assert!(r.contains("\"current\":\""), "{r}");

        // ③ 隧道自己的门：没带钥匙的请求进不去（手机上有网络权限的任何 App 都能连
        //    127.0.0.1 扫端口 —— 不挡住的话连上隧道就等于拿到 B 的 shell）
        let r = get(c.port, "/api/machines", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        let r = get(c.port, "/api/health", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");

        // ④ 页面本来是从 `/?k=…` 进来的：换成一个 HttpOnly cookie，钥匙不再留在地址里
        let r = get(c.port, &format!("/?k={}", c.key), None).await;
        assert!(r.starts_with("HTTP/1.1 302"), "{r}");
        assert!(
            r.to_lowercase().contains("set-cookie: agent_tunnel="),
            "{r}"
        );
        // 钥匙不对就不认
        let r = get(c.port, "/?k=瞎写的", None).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");

        // ⑤ 回到本机：隧道收掉、库里那行「现在连的是哪台」也清掉（下次启动不再自动连）
        assert_eq!(machines.current_id().as_deref(), Some(b.id.as_str()));
        machines.disconnect();
        assert!(machines.current_id().is_none());
        assert_eq!(a.db().unwrap().get_meta("current_machine").unwrap(), None);
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", c.port))
                .await
                .is_err(),
            "隧道已经收了，那个端口上没人听"
        );

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
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

        let phone = Machines::with_db(Arc::new(
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

        // 连上：局域网直连（TLS）。壳拿到的地址里带那台电脑的凭据（页面那份不带，见 `Conn`）
        let c = phone.connect(&m.id).await.unwrap();
        assert!(
            c.shell_url().contains("/auth?t=s3cret"),
            "{}",
            c.shell_url()
        );
        assert!(!c.page_url().contains("s3cret"), "{}", c.page_url());
        let t = c.port;
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");

        // 出门了：局域网那个地址不通 → 走中继，照样通
        phone.lan_found(&m.id, "电脑", "127.0.0.1", 1);
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        // 经中继来的永远要凭据 —— 哪怕中继、手机、电脑全在这一台机器上（电脑信任本机）
        let r = get(t, "/api/health", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");
        // 经中继来的不能配对（配对码只该在局域网里输）
        let r = get(t, "/api/pair", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        // 没有凭据的请求到不了那台电脑：隧道自己就挡住了（手机上有网络权限的
        // 任何 App 都能连 127.0.0.1，见 `tunnel.rs`）
        let r = get(t, "/api/linux", None).await;
        assert!(r.starts_with("HTTP/1.1 401"), "{r}");

        // 记账落盘：重新读回来还在
        let again = Machines::with_db(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        assert_eq!(again.get(&m.id).map(|x| x.token), Some("s3cret".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个只搬字节的转发（放在「电脑」前面），记着接了几条连接。
    async fn counting_proxy(to: u16) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        tokio::spawn(async move {
            loop {
                let (mut a, _) = crate::accept(&l).await;
                n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Ok(mut b) = TcpStream::connect(("127.0.0.1", to)).await {
                        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
                    }
                });
            }
        });
        (port, n)
    }

    /// 隧道复用上游连接：经中继时一条新连接是好几个往返，而页面每 1.5 秒就轮询一次。
    /// 页面那头 keep-alive 也认；WebSocket 照样穿得过去；隧道收掉之后，页面那头挂着的
    /// 空闲连接也断开（不然旧隧道和它的连接池一直挂着）。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_tunnel_reuses_upstream_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        crate::llm::install_crypto_provider();
        let (_st, port, code, dir) = computer(None).await;
        let (front, dialed) = counting_proxy(port).await;
        let phone = Machines::with_db(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        let m = phone.pair("127.0.0.1", port, &code).await.unwrap();
        phone.lan_found(&m.id, "电脑", "127.0.0.1", front);
        let c = phone.connect(&m.id).await.unwrap();

        // ① 连上之后再来的请求（页面那头各开各的连接）：不再新建上游连接。
        //    先等一下：`connect` 在后台经隧道对一次账（`Machines::sync`），它和第一个请求
        //    同时在路上的话，池子会正当地多开一条
        let r = get(c.port, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let before = dialed.load(std::sync::atomic::Ordering::SeqCst);
        assert!(before >= 1);
        for _ in 0..3 {
            let r = get(c.port, "/api/health", Some("s3cret")).await;
            assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        }
        assert_eq!(
            dialed.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "上游连接要复用"
        );

        // ② 页面那头 keep-alive：一条连接上两个请求
        let mut s = TcpStream::connect(("127.0.0.1", c.port)).await.unwrap();
        s.write_all(
            b"GET /api/health HTTP/1.1\r\nhost: localhost\r\ncookie: agent_token=s3cret\r\n\r\n\
              GET /api/health HTTP/1.1\r\nhost: localhost\r\ncookie: agent_token=s3cret\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let out = String::from_utf8_lossy(&out);
        assert_eq!(out.matches("HTTP/1.1 200").count(), 2, "{out}");

        // ③ WebSocket：升级穿过隧道，那台电脑先说的那句（hello）传得回来
        let mut ws = TcpStream::connect(("127.0.0.1", c.port)).await.unwrap();
        ws.write_all(
            b"GET /api/ws HTTP/1.1\r\nhost: localhost\r\ncookie: agent_token=s3cret\r\n\
              connection: Upgrade\r\nupgrade: websocket\r\nsec-websocket-version: 13\r\n\
              sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), ws.read(&mut chunk))
                .await
                .expect("升级没有回音")
                .unwrap();
            assert!(n > 0, "升级时断了：{}", String::from_utf8_lossy(&buf));
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        if buf.len() == head_end {
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), ws.read(&mut chunk))
                .await
                .expect("升级之后那台电脑什么都没说")
                .unwrap();
            assert!(n > 0, "升级之后就断了");
        }

        // ④ 隧道收掉：页面那头挂着的空闲 keep-alive 连接也断
        let mut idle = TcpStream::connect(("127.0.0.1", c.port)).await.unwrap();
        idle.write_all(
            b"GET /api/health HTTP/1.1\r\nhost: localhost\r\ncookie: agent_token=s3cret\r\n\r\n",
        )
        .await
        .unwrap();
        let n = idle.read(&mut chunk).await.unwrap();
        assert!(String::from_utf8_lossy(&chunk[..n]).starts_with("HTTP/1.1 200"));
        phone.disconnect();
        let mut rest = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            idle.read_to_end(&mut rest),
        )
        .await
        .expect("隧道收了，空闲连接还挂着")
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没配中继、局域网又不通：一页 502 说清楚；指纹不对（冒充的）也是。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unreachable_or_impostor_computers_get_a_page_that_says_why() {
        crate::llm::install_crypto_provider();
        let (_st, port, code, dir) = computer(None).await;
        let phone = Machines::with_db(Arc::new(
            crate::db::Db::open(&dir.join("phone.db")).unwrap(),
        ));
        let m = phone.pair("127.0.0.1", port, &code).await.unwrap();

        // 冒充：同一个地址，但我们记着的指纹不是它的
        let mut fake = m.clone();
        fake.id = "fake".into();
        fake.fingerprint = "0".repeat(64);
        phone.upsert(fake);
        let t = phone.connect("fake").await.unwrap().port;
        let r = get(t, "/api/health", Some("s3cret")).await;
        assert!(r.starts_with("HTTP/1.1 502") && r.contains("指纹"), "{r}");

        // 不在同一个网络、也没配中继
        let t = phone.connect(&m.id).await.unwrap().port;
        phone.lan_found(&m.id, "电脑", "127.0.0.1", 1);
        let r = get(t, "/", Some("s3cret")).await;
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
        let phone = Machines::with_db(Arc::new(
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
        )
        .unwrap();
        let host3 = st3.relay_host.read().unwrap().clone().unwrap();
        assert!(host3.attach(env).unwrap_err().contains("环境变量"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
