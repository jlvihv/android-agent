//! 「现在用的是哪台核心」的**唯一**一条路（见 docs/12-relay.md）。
//!
//! 在本机 `127.0.0.1` 起一个小代理，页面（手机 App 的 WebView / 电脑上的浏览器都算）加载
//! `http://localhost:<端口>/?k=<钥匙>`；每条进来的连接自己选路，和那台电脑直接跑 TLS：
//!
//! ```text
//! 页面 ──http──▶ 这个代理 ─┬─ `/api/machines*` ──────── 本机核心（明文 loopback）
//!                          └─ 其余 ─┬─ 局域网直连 ────┬─ TLS（端到端）─▶ 那台电脑的核心
//!                                   └─ 经中继（只搬字节）┘
//! ```
//!
//! - **选路在这里，不在壳里**：先试局域网（最多等 [LAN_TRY]），不通（出门了、电脑换了 IP、
//!   那个 IP 上已经是别的设备）就走中继，并且接下来 [LAN_COOLDOWN] 内直接走中继 ——
//!   出门之后每条新连接不用都先白等一下。回家后发现服务报上地址（`Routes::set_lan`）或者
//!   网络变了（`Routes::reset`）就重新先试局域网。
//! - **只认一张证书**：配对时记下的那台电脑的证书指纹。不走 CA、不看域名 —— 中继、
//!   或者局域网里冒充那个 IP 的设备，都冒充不了那台电脑，凭据也就不会交给别人。
//! - 为什么在本机代理而不是让页面直接连：WebView 不认「只信这一张证书」；而且页面
//!   来自 `localhost`，算安全上下文（`crypto.subtle` 之类能用）。
//!
//! ## 页面不拿凭据：隧道替它带上
//!
//! 页面（含手机 App 里那个 WebView）手里只有一把**隧道钥匙**（生成地址时给的 `?k=`，
//! 换成一个 HttpOnly cookie 之后就再不露面）。每台电脑的 token 由隧道在转发时注入
//! `x-agent-token` —— 页面里的 JS 看着自己的 `location`、翻自己的 history，都拿不到
//! 「那台电脑的 shell」。切换机器时页面只是换一个隧道地址，钥匙和端口都是新的。
//!
//! 反过来，**隧道这边也要凭据**：手机上有网络权限的任何 App 都能连 `127.0.0.1`，
//! 不设防的话它连上隧道就等于拿到那台电脑的 shell。`/api/*` 只认隧道钥匙
//! （或者那台电脑自己的 token —— 壳的回合状态订阅就是这么连的，见 docs/06）。
//!
//! ## 一条请求一条连接
//!
//! 每条请求都要重新选上游（`/api/machines*` 归本机，其余归那台电脑），所以隧道**不复用**
//! 连接：转给上游时补一句 `connection: close`，上游答完就断，同一个连接上不会出现
//! 「一半请求问本机、一半问那台电脑」。代价是每个请求多一次连接建立 —— 局域网感觉不到，
//! 经中继时每次多一个往返（页面上那十来个请求；长连接 SSE / WebSocket 不受影响）。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::state::same_secret;

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
    /// 它的凭据：页面不拿，隧道转发时替它带上（见模块说明）
    pub token: String,
    pub routes: Arc<Routes>,
}

/// 本机核心（`127.0.0.1:<它自己的端口>`）：`/api/machines*` 转给它。
///
/// 那些请求问的是「我这台机器能连哪些电脑、现在连的是哪台」—— 答案在本机的库里，
/// 不在那台电脑上。转过去而不是在隧道里自己答：那份 API 只有一份实现（`http.rs`），
/// 页面直接开本机地址时和经隧道时走的是同一个 handler。
#[derive(Clone)]
pub struct Local {
    /// `IP:端口`
    pub addr: String,
    /// 本机核心的凭据（没有 = 本机不设防）。转发时换成它。
    pub token: Option<String>,
}

/// 一条开着的隧道。drop 就关（新连接不再进来；已经接上的那几条让它把话说完）。
pub struct Tunnel {
    pub port: u16,
    /// 页面的钥匙（`http://localhost:<端口>/?k=<它>` 换成 cookie 之后就不再露面）
    pub key: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        // 只收掉 accept 循环。**不能连已接上的连接一起 abort**：切换机器的那条请求
        // 正跑在这样一条连接的 `bridge` 里，它得把「新地址是什么」送回页面
        // （`machines::connect` 换掉 current 时就发生在这条请求的处理过程里）。
        self.task.abort();
    }
}

/// 起一条隧道：本机监听一个端口（`want_port` 是上次那个，占了就换一个），返回它。
pub async fn start(t: Target, local: Local, want_port: Option<u16>) -> anyhow::Result<Tunnel> {
    let listener = match want_port.filter(|p| *p != 0) {
        Some(p) => match TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => l,
            // 上次那个端口被占了：换个空的。地址变了页面就得重新导航（壳里会看到）
            Err(_) => TcpListener::bind(("127.0.0.1", 0)).await?,
        },
        None => TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let port = listener.local_addr()?.port();
    let key = crate::state::random_secret();
    let connector =
        tokio_rustls::TlsConnector::from(Arc::new(pinned_config(Some(&t.fingerprint)).0));
    tracing::info!("隧道：127.0.0.1:{port} →「{}」", t.name);
    let t = Arc::new(t);
    let local = Arc::new(local);
    let task = tokio::spawn({
        let key = key.clone();
        async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    continue;
                };
                let _ = sock.set_nodelay(true);
                tokio::spawn(bridge(
                    sock,
                    t.clone(),
                    local.clone(),
                    connector.clone(),
                    key.clone(),
                ));
            }
        }
    });
    Ok(Tunnel { port, key, task })
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// 上游：本机核心（明文 loopback）还是那台电脑（TLS）。
enum Up {
    Local(String),
    Machine,
}

/// 一条本机连接：读一次请求头 → 决定上游 → 转发。见模块说明。
async fn bridge(
    mut sock: TcpStream,
    t: Arc<Target>,
    local: Arc<Local>,
    connector: tokio_rustls::TlsConnector,
    key: String,
) {
    let head = match read_head(&mut sock).await {
        Ok(h) => h,
        Err(why) => {
            tracing::debug!("隧道：{why}");
            reply(&mut sock, 400, "接收请求失败", &why, false).await;
            return;
        }
    };
    let Some(req) = Request::parse(&head.head) else {
        reply(&mut sock, 400, "接收请求失败", "看不懂这个请求", false).await;
        return;
    };

    // 1. 持钥匙那一步：`/?k=<钥匙>` → 换成 cookie 再回首页。地址里的钥匙因此只出现一次
    //    （不落进 history 深处，也不进 JS 能读的地方）。
    if let Some(k) = req.query("k") {
        if !same_secret(k, &key) {
            reply(
                &mut sock,
                403,
                "钥匙不对",
                "这个地址的钥匙不对（隧道每次连接都不一样）。",
                false,
            )
            .await;
            return;
        }
        let res = format!(
            "HTTP/1.1 302 Found\r\nlocation: /\r\nset-cookie: {COOKIE}={key}; Path=/; HttpOnly; SameSite=Strict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        let _ = sock.write_all(res.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
        return;
    }

    // 2. `/api/*` 要凭据（和本机核心一个规矩）。页面、js/css、沙箱文件不设防 ——
    //    它们本来就是公开的代码，沙箱文件另有一把路径上的钥匙。
    if req.path().starts_with("/api/") && !req.credential_ok(&key, &t.token) {
        reply(
            &mut sock,
            401,
            "需要凭据",
            "这个隧道的钥匙不在请求里（从本机核心给的地址进来才有）。",
            true,
        )
        .await;
        return;
    }

    // 3. 选上游：问「能连哪些机器」的那些归本机，其余的归那台电脑。
    let (up, token) = if req.path().starts_with("/api/machines") {
        (Up::Local(local.addr.clone()), local.token.clone())
    } else {
        (Up::Machine, Some(t.token.clone()))
    };
    let forwarded = rewrite_head(&req, token.as_deref());
    match up {
        Up::Local(addr) => match TcpStream::connect(&addr).await {
            Ok(up) => forward(&mut sock, up, &head.rest, &forwarded).await,
            Err(e) => {
                let why = format!("本机核心连不上（{addr}）：{e}");
                tracing::warn!("隧道：{why}");
                reply(&mut sock, 502, "连不上本机核心", &why, true).await;
            }
        },
        Up::Machine => match open(&t, &connector).await {
            Ok(up) => forward(&mut sock, up, &head.rest, &forwarded).await,
            Err(why) => {
                tracing::warn!("隧道：{why}");
                reply(&mut sock, 502, "连不上那台电脑", &why, true).await;
            }
        },
    }
}

/// 把已经读掉的那段请求头（改写过的）和剩下读到的东西送去上游，然后对搬字节。
async fn forward<U>(sock: &mut TcpStream, mut up: U, rest: &[u8], head: &[u8])
where
    U: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if up.write_all(head).await.is_err() || up.write_all(rest).await.is_err() {
        return;
    }
    let _ = up.flush().await;
    let _ = tokio::io::copy_bidirectional(sock, &mut up).await;
}

/// 隧道自己的 cookie 名（钥匙换成它，之后每个请求自动带上）。
const COOKIE: &str = "agent_tunnel";

/// 一次请求的头：`\r\n\r\n` 为止（含空行），以及同一次读里多出来的字节（body 的开头）。
struct Head {
    head: Vec<u8>,
    rest: Vec<u8>,
}

/// 请求头最多这么大。正常就几百字节；超过说明对面不是浏览器。
const HEAD_MAX: usize = 64 * 1024;

/// 读一次请求头（含空行）。剩下的（body 的开头）原样留着，转发时一起送出去。
async fn read_head(sock: &mut TcpStream) -> Result<Head, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            return Ok(Head { head: buf, rest });
        }
        if buf.len() > HEAD_MAX {
            return Err("请求头太大".into());
        }
        match tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await {
            Ok(Ok(0)) if buf.is_empty() => return Err("对面什么都没说就断了".into()),
            Ok(Ok(0)) => return Err("请求头没读完就断了".into()),
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) => return Err(format!("读请求失败：{e}")),
            Err(_) => return Err("等请求头超时".into()),
        }
    }
}

/// 请求行 + 头。只解我们需要的那几样，别的一律原样留着转发。
struct Request<'a> {
    /// 整个头（转发时按行重排，所以留着原文）
    raw: &'a [u8],
    /// 请求行去掉结尾 `\r\n`
    line: &'a str,
    /// 请求目标（`/api/x?y=1`）
    target: &'a str,
    /// 头字段（原名，小写比较）
    headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Request<'a> {
    fn parse(raw: &'a [u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?;
        let (first, rest) = text.split_once("\r\n")?;
        let mut parts = first.split(' ');
        let method = parts.next()?;
        let target = parts.next()?;
        let version = parts.next()?;
        if method.is_empty() || !version.starts_with("HTTP/1.") {
            return None;
        }
        let mut headers = Vec::new();
        for line in rest.split("\r\n") {
            if line.is_empty() {
                break;
            }
            let (k, v) = line.split_once(':')?;
            headers.push((k.trim(), v.trim()));
        }
        Some(Self {
            raw,
            line: first,
            target,
            headers,
        })
    }

    /// 路径（不含查询串）。
    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(self.target)
    }

    /// 查询串里的一个参数（不解码：我们只拿来做等值比较）。
    fn query(&self, name: &str) -> Option<&str> {
        self.target
            .split_once('?')?
            .1
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v)
    }

    /// 一个 cookie 的值。
    fn cookie(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("cookie"))
            .flat_map(|(_, v)| v.split(';'))
            .filter_map(|kv| kv.trim().split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    /// 这条请求带没带「能过隧道」的凭据：隧道自己的钥匙，或者那台电脑的 token
    /// （壳的回合状态订阅、核心里「出门在外怎么连」那次对账都走后者，见 docs/06）。
    fn credential_ok(&self, key: &str, machine_token: &str) -> bool {
        let ok = |v: &str| same_secret(v, key) || same_secret(v, machine_token);
        self.header("x-agent-token").is_some_and(ok)
            || self.cookie(COOKIE).is_some_and(ok)
            || self.cookie("agent_token").is_some_and(ok)
    }
}

/// 改写要转出去的那份头：
///
/// - 抹掉浏览器带来的 `cookie` 和 `x-agent-token`：上游只该看到我们给它准备的那一个凭据
///   （两个来源的凭据混在一起，谁都说不清最后认的是谁）；
/// - 注入上游的 `x-agent-token`（没有就不注入）；
/// - 补 `connection: close`（一次请求一条连接，见模块说明）。**升级请求**（WebSocket）不碰
///   `connection`：那句 `Upgrade` 就靠它，动了升级就没了。
fn rewrite_head(req: &Request, token: Option<&str>) -> Vec<u8> {
    let upgrade = req.header("upgrade").is_some();
    let mut out = String::with_capacity(req.raw.len() + 96);
    out.push_str(req.line);
    out.push_str("\r\n");
    for (k, v) in &req.headers {
        let lower = k.to_ascii_lowercase();
        if lower == "x-agent-token" || lower == "cookie" {
            continue;
        }
        if lower == "connection" && !upgrade {
            continue;
        }
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    if let Some(t) = token {
        out.push_str(&format!("x-agent-token: {t}\r\n"));
    }
    if !upgrade {
        out.push_str("connection: close\r\n");
    }
    out.push_str("\r\n");
    out.into_bytes()
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

/// 连不上 / 不认凭据：给页面回一页说清楚为什么（不然它只会显示一个笼统的「无法访问」）。
///
/// `drain`：请求那头还有没读掉的数据（body）时，先把它读掉再回 —— 带着没读的数据关连接，
/// 系统会发 RST，浏览器只看到「连接被重置」，这页说明就白写了。
async fn reply(sock: &mut TcpStream, status: u16, title: &str, why: &str, drain: bool) {
    if drain {
        let mut buf = [0u8; 4096];
        let mut seen = 0usize;
        while seen < 64 * 1024 {
            match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => seen += n,
                _ => break,
            }
        }
    }
    let body = format!("{title}\n\n{why}\n");
    let head = format!(
        "HTTP/1.1 {status} {title}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = sock.write_all(head.as_bytes()).await;
    let _ = sock.write_all(body.as_bytes()).await;
    let _ = sock.flush().await;
    let _ = sock.shutdown().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    const GET: &str =
        "GET /api/health HTTP/1.1\r\nhost: localhost:5555\r\ncookie: agent_tunnel=K\r\n\r\n";

    #[test]
    fn a_request_head_is_read_up_to_the_blank_line() {
        let raw = head(GET);
        let req = Request::parse(&raw).expect("正常请求");
        assert_eq!(req.path(), "/api/health");
        assert_eq!(req.header("Host"), Some("localhost:5555"));
        assert_eq!(req.cookie("agent_tunnel"), Some("K"));
    }

    #[test]
    fn the_query_string_is_there_for_reading_the_tunnel_key() {
        let raw = head("GET /?k=abc&x=1 HTTP/1.1\r\nhost: x\r\n\r\n");
        let req = Request::parse(&raw).unwrap();
        assert_eq!(req.query("k"), Some("abc"));
        assert_eq!(req.query("nope"), None);
        // 路径不带查询串：`/api/machines` 那一条据此认出来
        assert_eq!(req.path(), "/");
    }

    #[test]
    fn a_tunnel_credential_can_be_the_key_or_the_machines_own_token() {
        let key = "K";
        let token = "T";
        let ok = |text: &str| {
            let raw = head(text);
            Request::parse(&raw).unwrap().credential_ok(key, token)
        };
        assert!(ok(GET));
        assert!(ok(
            "GET /api/health HTTP/1.1\r\ncookie: agent_token=T\r\n\r\n"
        ));
        assert!(ok("GET /api/health HTTP/1.1\r\nx-agent-token: K\r\n\r\n"));
        // 什么都没带、带了别的：不放行
        assert!(!ok("GET /api/health HTTP/1.1\r\nhost: x\r\n\r\n"));
        assert!(!ok(
            "GET /api/health HTTP/1.1\r\nx-agent-token: 别的\r\n\r\n"
        ));
    }

    #[test]
    fn forwarding_replaces_the_credential_and_makes_the_connection_one_shot() {
        let raw = head(GET);
        let req = Request::parse(&raw).unwrap();
        let out = String::from_utf8(rewrite_head(&req, Some("T"))).unwrap();
        // 浏览器的 cookie 不再往上游带，换成我们的那一个
        assert!(!out.contains("agent_tunnel"), "{out}");
        assert!(out.contains("x-agent-token: T\r\n"), "{out}");
        assert!(out.contains("connection: close\r\n"), "{out}");
        assert!(out.starts_with("GET /api/health HTTP/1.1\r\n"), "{out}");
        assert!(out.ends_with("\r\n\r\n"), "{out}");
        // 本机核心不设防时（没有凭据）就不注入 —— 但一样是一次性的
        let out = String::from_utf8(rewrite_head(&req, None)).unwrap();
        assert!(!out.contains("x-agent-token"), "{out}");
        assert!(out.contains("connection: close\r\n"), "{out}");
    }

    #[test]
    fn an_upgrade_keeps_its_connection_header() {
        let raw = head(
            "GET /api/ws HTTP/1.1\r\nhost: localhost:5555\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\r\n",
        );
        let req = Request::parse(&raw).unwrap();
        let out = String::from_utf8(rewrite_head(&req, Some("T"))).unwrap();
        // `connection: upgrade` 是升级的一部分，动了 WebSocket 就建立不起来
        assert!(out.to_lowercase().contains("connection: upgrade"));
        assert!(!out.contains("connection: close"), "{out}");
        assert!(out.contains("x-agent-token: T\r\n"), "{out}");
    }

    #[test]
    fn a_head_that_is_not_http_is_not_a_request() {
        assert!(Request::parse(b"ding dong\r\n\r\n").is_none());
        assert!(Request::parse(b"GET / HTTP/1.1\r\nno-colon-here\r\n\r\n").is_none());
    }
}
