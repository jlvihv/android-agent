//! 出门在外连家里的电脑 —— **手机这一头**（见 docs/12-relay.md）。
//!
//! 在本机 `127.0.0.1` 起一个小代理，WebView 照常加载 `http://localhost:<端口>`；
//! 每条进来的连接都经中继拨到家里那台电脑，然后和它直接跑 TLS：
//!
//! ```text
//! WebView ──http──▶ 这个代理 ──TLS（端到端）──▶ 中继（只搬字节） ──▶ 家里电脑的核心
//! ```
//!
//! **只认一张证书**：配对时记下的那台电脑的证书指纹（`RemoteInfo::fingerprint`）。
//! 不走 CA、不看域名 —— 中继（或者路上任何人）冒充不了那台电脑，凭据也就不会
//! 交给别人。
//!
//! 为什么在本机代理而不是让 WebView 直接连：WebView 不认「只信这一张证书」；
//! 而且页面来自 `localhost`，算安全上下文（`crypto.subtle` 之类能用）。

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// TLS 里用的名字。证书只按指纹认，这个名字只是协议要求得有一个。
pub const SERVER_NAME: &str = "agent-core";

/// 证书的指纹：DER 的 SHA-256，十六进制。
pub fn fingerprint(der: &[u8]) -> String {
    agent_relay::hex(&Sha256::digest(der))
}

/// 要连的那台电脑。
#[derive(Clone)]
pub struct Target {
    /// 中继 `IP:端口`
    pub relay: String,
    /// 中继密钥（十六进制）
    pub key: String,
    /// 那台电脑的实例 id（`AppState::instance_id`）
    pub host_id: String,
    /// 那台电脑的证书指纹
    pub fingerprint: String,
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
    let key = agent_relay::parse_key(&t.key).map_err(anyhow::Error::msg)?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config(&t.fingerprint)));
    tracing::info!(
        "中继隧道：127.0.0.1:{port} → {}（经中继 {}）",
        t.host_id,
        t.relay
    );
    let t = Arc::new(t);
    let key = Arc::new(key);
    let task = tokio::spawn(async move {
        loop {
            let Ok((local, _)) = listener.accept().await else {
                continue;
            };
            let _ = local.set_nodelay(true);
            tokio::spawn(bridge(local, t.clone(), key.clone(), connector.clone()));
        }
    });
    Ok(Tunnel { port, task })
}

/// 一条本机连接：拨到那台电脑、握手、然后对搬字节。
async fn bridge(
    mut local: TcpStream,
    t: Arc<Target>,
    key: Arc<Vec<u8>>,
    connector: tokio_rustls::TlsConnector,
) {
    let relay = match agent_relay::connect(
        &t.relay,
        &key,
        agent_relay::Role::Dial { id: &t.host_id },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("中继隧道：拨号失败：{e}");
            refuse(&mut local, &format!("连不上家里的电脑：{e}")).await;
            return;
        }
    };
    let name = ServerName::try_from(SERVER_NAME).expect("固定的合法名字");
    let mut tls = match connector.connect(name, relay).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("中继隧道：TLS 握手失败：{e}");
            // rustls 会在我们自己的说明前面加一句 "unexpected error: "，对人没用
            let why = e.to_string().replace("unexpected error: ", "");
            refuse(&mut local, &format!("和家里电脑的加密握手没通过：{why}")).await;
            return;
        }
    };
    let _ = tokio::io::copy_bidirectional(&mut local, &mut tls).await;
}

/// 连不上：给 WebView 回一页 502，说清楚为什么（不然它只会显示一个笼统的「无法访问」）。
async fn refuse(local: &mut TcpStream, why: &str) {
    // 先把请求读掉再回：带着没读的数据关连接，系统会发 RST，浏览器只看到「连接被重置」，
    // 这页说明就白写了。请求头就几百字节，读到空行或者等一小会儿就够
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    while !seen.windows(4).any(|w| w == b"\r\n\r\n") && seen.len() < 64 * 1024 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            tokio::io::AsyncReadExt::read(local, &mut buf),
        )
        .await
        {
            Ok(Ok(n)) if n > 0 => seen.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let body = format!("{why}\n\n家里那台电脑的核心开着吗？它连上中继了吗？（看它的终端输出）\n");
    let head = format!(
        "HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = local.write_all(head.as_bytes()).await;
    let _ = local.write_all(body.as_bytes()).await;
    let _ = local.shutdown().await;
}

fn client_config(fingerprint: &str) -> rustls::ClientConfig {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Pinned {
        fingerprint: fingerprint.trim().to_ascii_lowercase(),
        algs: provider.signature_verification_algorithms,
    });
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认的协议版本")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth()
}

/// 只认一张证书（按指纹）。签名还是照常验 —— 不然拿到证书的人（证书是公开的）
/// 就能冒充；验签保证对面手里真有那把私钥。
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
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
        if fingerprint(end_entity.as_ref()) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "对面不是配对过的那台电脑（证书指纹对不上）".into(),
            ))
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
