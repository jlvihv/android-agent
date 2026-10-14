//! 手机连电脑的**唯一**一条路（见 docs/12-relay.md）。
//!
//! 在本机 `127.0.0.1` 起一个小代理，WebView 加载 `http://localhost:<端口>`；每条进来的连接
//! 自己选路，然后和那台电脑直接跑 TLS：
//!
//! ```text
//! WebView ──http──▶ 这个代理 ─┬─ 局域网直连 ──────────┬─ TLS（端到端）─▶ 电脑的核心
//!                             └─ 经中继（只搬字节）──┘
//! ```
//!
//! - **选路在这里，不在壳里**：先试局域网（最多等 [LAN_TRY]），不通（出门了、电脑换了 IP、
//!   那个 IP 上已经是别的设备）就走中继，并且接下来 [LAN_COOLDOWN] 内直接走中继 ——
//!   出门之后每条新连接不用都先白等一下。回家后发现服务报上地址（`Routes::set_lan`）或者
//!   网络变了（`Routes::reset`）就重新先试局域网。
//! - **只认一张证书**：配对时记下的那台电脑的证书指纹。不走 CA、不看域名 —— 中继、
//!   或者局域网里冒充那个 IP 的设备，都冒充不了那台电脑，凭据也就不会交给别人。
//! - 为什么在本机代理而不是让 WebView 直接连：WebView 不认「只信这一张证书」；而且页面
//!   来自 `localhost`，算安全上下文（`crypto.subtle` 之类能用）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// TLS 里用的名字。证书只按指纹认，这个名字只是协议要求得有一个。
pub const SERVER_NAME: &str = "agent-core";

/// 局域网直连最多等多久。在家里连上只要几毫秒；等不到多半是出门了。
const LAN_TRY: Duration = Duration::from_millis(500);
/// 局域网不通之后多久内不再试它（直接走中继）。
const LAN_COOLDOWN: Duration = Duration::from_secs(30);

/// 证书的指纹：DER 的 SHA-256，十六进制。
pub fn fingerprint(der: &[u8]) -> String {
    agent_relay::hex(&Sha256::digest(der))
}

/// 一台电脑能走的路。局域网地址会变（发现服务报上来的），所以放在锁里。
pub struct Routes {
    /// 局域网里最近见到它的地址（`IP:端口`）
    lan: Mutex<Option<String>>,
    /// 中继 `(IP:端口, 密钥)`
    relay: Option<(String, Vec<u8>)>,
    /// 局域网在这个时刻之前别试了（刚刚不通过）
    lan_down_until: Mutex<Option<Instant>>,
}

impl Routes {
    pub fn new(lan: Option<String>, relay: Option<(String, Vec<u8>)>) -> Self {
        Self {
            lan: Mutex::new(lan),
            relay,
            lan_down_until: Mutex::new(None),
        }
    }

    /// 发现服务刚在局域网里看到它：记下地址，下一条连接就先试它。
    pub fn set_lan(&self, addr: String) {
        if let Ok(mut g) = self.lan.lock() {
            *g = Some(addr);
        }
        self.reset();
    }

    /// 网络变了（换 Wi-Fi、出门、回家）：之前「局域网不通」的判断作废。
    pub fn reset(&self) {
        if let Ok(mut g) = self.lan_down_until.lock() {
            *g = None;
        }
    }

    /// 现在该不该先试局域网，试哪个地址。
    fn lan_now(&self) -> Option<String> {
        let down = self
            .lan_down_until
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|t| Instant::now() < t);
        if down {
            return None;
        }
        self.lan.lock().ok().and_then(|g| g.clone())
    }

    fn lan_failed(&self) {
        if let Ok(mut g) = self.lan_down_until.lock() {
            *g = Some(Instant::now() + LAN_COOLDOWN);
        }
    }
}

/// 要连的那台电脑。
pub struct Target {
    /// 实例 id（中继按它找人）
    pub host_id: String,
    /// 给人看的名字（连不上时的说明里用）
    pub name: String,
    /// 证书指纹（配对时记下的）
    pub fingerprint: String,
    pub routes: Arc<Routes>,
}

/// 一条开着的隧道。drop 就关。
pub struct Tunnel {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 起一条隧道：本机监听一个空闲端口，返回它。
pub async fn start(t: Target) -> anyhow::Result<Tunnel> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(pinned_config(Some(&t.fingerprint)).0));
    tracing::info!("隧道：127.0.0.1:{port} →「{}」", t.name);
    let t = Arc::new(t);
    let task = tokio::spawn(async move {
        loop {
            let Ok((local, _)) = listener.accept().await else {
                continue;
            };
            let _ = local.set_nodelay(true);
            tokio::spawn(bridge(local, t.clone(), connector.clone()));
        }
    });
    Ok(Tunnel { port, task })
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// 一条本机连接：选路、握手、然后对搬字节。
async fn bridge(mut local: TcpStream, t: Arc<Target>, connector: tokio_rustls::TlsConnector) {
    match open(&t, &connector).await {
        Ok(mut tls) => {
            let _ = tokio::io::copy_bidirectional(&mut local, &mut tls).await;
        }
        Err(why) => {
            tracing::warn!("隧道：{why}");
            refuse(&mut local, &why).await;
        }
    }
}

/// 先局域网、再中继，拿到一条和那台电脑之间的 TLS。`Err` 是给人看的原因。
async fn open(t: &Target, connector: &tokio_rustls::TlsConnector) -> Result<Tls, String> {
    let mut why = Vec::new();
    if let Some(addr) = t.routes.lan_now() {
        match tokio::time::timeout(LAN_TRY, TcpStream::connect(&addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                // 连上了但证书不对（那个 IP 上已经是别的设备）也算局域网不通
                match handshake(connector, s).await {
                    Ok(tls) => return Ok(tls),
                    Err(e) => why.push(format!("局域网 {addr}：{e}")),
                }
            }
            Ok(Err(e)) => why.push(format!("局域网 {addr}：{e}")),
            Err(_) => why.push(format!("局域网 {addr}：没有应答")),
        }
        t.routes.lan_failed();
    }
    match &t.routes.relay {
        Some((relay, key)) => {
            let s = agent_relay::connect(relay, key, agent_relay::Role::Dial { id: &t.host_id })
                .await
                .map_err(|e| format!("连不上「{}」：经中继：{e}", t.name))?;
            handshake(connector, s)
                .await
                .map_err(|e| format!("连不上「{}」：经中继：{e}", t.name))
        }
        None => {
            why.push("它没配中继，出门在外连不上".into());
            Err(format!("连不上「{}」：{}", t.name, why.join("；")))
        }
    }
}

async fn handshake(connector: &tokio_rustls::TlsConnector, s: TcpStream) -> Result<Tls, String> {
    let name = ServerName::try_from(SERVER_NAME).expect("固定的合法名字");
    connector
        .connect(name, s)
        .await
        // rustls 会在我们自己的说明前面加一句 "unexpected error: "，对人没用
        .map_err(|e| e.to_string().replace("unexpected error: ", ""))
}

/// 连不上：给 WebView 回一页 502，说清楚为什么（不然它只会显示一个笼统的「无法访问」）。
async fn refuse(local: &mut TcpStream, why: &str) {
    // 先把请求读掉再回：带着没读的数据关连接，系统会发 RST，浏览器只看到「连接被重置」，
    // 这页说明就白写了。请求头就几百字节，读到空行或者等一小会儿就够
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.len() < 64 * 1024 {
        match tokio::time::timeout(
            Duration::from_millis(500),
            tokio::io::AsyncReadExt::read(local, &mut buf),
        )
        .await
        {
            Ok(Ok(n)) if n > 0 => seen.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let body = format!("{why}\n");
    let head = format!(
        "HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = local.write_all(head.as_bytes()).await;
    let _ = local.write_all(body.as_bytes()).await;
    let _ = local.shutdown().await;
}

/// 只认一张证书的 TLS 配置。`expect = None` 是**配对**用的：那时还不知道指纹，先收下
/// 对面的证书，把指纹记在返回的格子里（由配对结果核对，见 `machines.rs`）。
pub fn pinned_config(expect: Option<&str>) -> (rustls::ClientConfig, Arc<Mutex<Option<String>>>) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(Mutex::new(None));
    let verifier = Arc::new(Pinned {
        expect: expect.map(|f| f.trim().to_ascii_lowercase()),
        seen: seen.clone(),
        algs: provider.signature_verification_algorithms,
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认的协议版本")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    (config, seen)
}

/// 只认一张证书（按指纹）。签名还是照常验 —— 不然拿到证书的人（证书是公开的）
/// 就能冒充；验签保证对面手里真有那把私钥。
#[derive(Debug)]
struct Pinned {
    expect: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        if let Ok(mut g) = self.seen.lock() {
            *g = Some(fp.clone());
        }
        match &self.expect {
            Some(want) if *want != fp => Err(rustls::Error::General(
                "对面不是配对过的那台电脑（证书指纹对不上）".into(),
            )),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}
