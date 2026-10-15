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
//! ## 连接复用
//!
//! 隧道是个按**请求**选路的 HTTP/1 代理（hyper）：页面那头 keep-alive，每个请求自己选
//! 上游（`/api/machines*` 归本机，其余归那台电脑）；上游连接进池复用。经中继时一条新连接
//! 是「连中继 + 中继配对 + TLS 握手」好几个往返，而页面每 1.5 秒轮询一次 —— 不复用的话
//! 每个请求都要付一遍。
//!
//! 池子里的连接可能已经死了却没人知道（手机换了网络）：网络一变（`Routes::reset`）或者
//! 该换路了（`Routes::set_lan`），池子的键就换一个（路由代号），新请求不再用旧连接。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http::{header, HeaderMap, HeaderValue, StatusCode};
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::{Connected, Connection};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
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
    /// 路由代号：该换路了（网络变了、回到家）就加一。上游连接池按它分开（见
    /// `generation`），换路之后新请求不会再用池子里走旧路的连接。
    generation: AtomicU64,
}

impl Routes {
    pub fn new(lan: Option<String>, relay: Option<(String, Vec<u8>)>) -> Self {
        Self {
            lan: Mutex::new(lan),
            relay,
            lan_down_until: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }

    /// 发现服务刚在局域网里看到它：记下地址，下一条连接就先试它。
    ///
    /// 发现服务会反复报同一个地址：地址没变、也没在走中继的话，池子里的连接照用
    /// （不然每报一次池子就清一次，复用就白做了）。
    pub fn set_lan(&self, addr: String) {
        let changed = self
            .lan
            .lock()
            .map(|mut g| g.replace(addr.clone()).as_deref() != Some(addr.as_str()))
            .unwrap_or(true);
        let was_down = self
            .lan_down_until
            .lock()
            .map(|mut g| g.take().is_some_and(|t| Instant::now() < t))
            .unwrap_or(false);
        if changed || was_down {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 网络变了（换 Wi-Fi、出门、回家）：之前「局域网不通」的判断作废，池子里的连接
    /// 也不用了（它们走的是旧网络，可能已经死了，只是还没人发现）。
    pub fn reset(&self) {
        if let Ok(mut g) = self.lan_down_until.lock() {
            *g = None;
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// 现在的路由代号（上游连接池的键里带着它）。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
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

/// 一条开着的隧道。drop 就关：新连接不再进来，已经接上的那几条答完手上这个请求就断
/// （不能当场掐：切换机器的那条请求正跑在这样一条连接上，它得把「新地址是什么」送回页面）。
pub struct Tunnel {
    pub port: u16,
    /// 页面的钥匙（`http://localhost:<端口>/?k=<它>` 换成 cookie 之后就不再露面）
    pub key: String,
    task: tokio::task::JoinHandle<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
        let _ = self.stop.send(true);
    }
}

/// 上游空闲连接留多久。不能太久：手机换了网络，池子里那条可能已经死了却还没人知道
/// （网络一变的情况由 `Routes::reset` 换掉整个池子的键，见 `Routes::generation`）。
const POOL_IDLE: Duration = Duration::from_secs(30);

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
    tracing::info!("隧道：127.0.0.1:{port} →「{}」", t.name);
    let t = Arc::new(t);
    let local = Arc::new(local);
    let dial = Dial {
        t: t.clone(),
        local: local.clone(),
        tls: tokio_rustls::TlsConnector::from(Arc::new(pinned_config(Some(&t.fingerprint)).0)),
    };
    let client = Client::builder(TokioExecutor::new())
        .pool_idle_timeout(POOL_IDLE)
        .pool_timer(TokioTimer::new())
        .build(dial);
    let shared = Arc::new(Shared {
        t,
        local,
        key: key.clone(),
        client,
    });
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            let (sock, _) = crate::accept(&listener).await;
            let _ = sock.set_nodelay(true);
            tokio::spawn(serve(sock, shared.clone(), stopped.clone()));
        }
    });
    Ok(Tunnel {
        port,
        key,
        task,
        stop,
    })
}

type Tls = tokio_rustls::client::TlsStream<TcpStream>;
type Body = axum::body::Body;

/// 一条隧道里大家共用的东西。
struct Shared {
    t: Arc<Target>,
    local: Arc<Local>,
    key: String,
    /// 上游连接池：键是 URI 的 authority（`local` / `machine-<代号>`，见 `handle`）
    client: Client<Dial, Body>,
}

/// 一条本机连接（页面那头）。HTTP/1 keep-alive：一条连接上可以来很多个请求，
/// 每个请求自己选上游（`handle`）。
async fn serve(sock: TcpStream, sh: Arc<Shared>, mut stopped: tokio::sync::watch::Receiver<bool>) {
    let svc = hyper::service::service_fn(move |req| {
        let sh = sh.clone();
        async move { Ok::<_, std::convert::Infallible>(handle(&sh, req).await) }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(sock), svc)
        .with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        r = conn.as_mut() => {
            if let Err(e) = r {
                tracing::debug!("隧道：{e}");
            }
        }
        // 隧道收了（切到别的机器）：手上这个请求答完就断
        _ = async { drop(stopped.wait_for(|s| *s).await) } => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

/// 一个请求：钥匙换 cookie / 查凭据 / 选上游转发。
async fn handle(sh: &Shared, req: hyper::Request<Incoming>) -> hyper::Response<Body> {
    // 1. 持钥匙那一步：`/?k=<钥匙>` → 换成 cookie 再回首页。地址里的钥匙因此只出现一次
    //    （不落进 history 深处，也不进 JS 能读的地方）。
    if let Some(k) = query(req.uri(), "k") {
        if !same_secret(k, &sh.key) {
            return page(
                StatusCode::FORBIDDEN,
                "钥匙不对",
                "这个地址的钥匙不对（隧道每次连接都不一样）。",
                req,
            )
            .await;
        }
        return hyper::Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, "/")
            .header(
                header::SET_COOKIE,
                format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict", sh.key),
            )
            .body(Body::empty())
            .expect("固定的合法响应");
    }

    // 2. `/api/*` 要凭据（和本机核心一个规矩）。页面、js/css、沙箱文件不设防 ——
    //    它们本来就是公开的代码，沙箱文件另有一把路径上的钥匙。
    let path = req.uri().path();
    if path.starts_with("/api/") && !credential_ok(req.headers(), &sh.key, &sh.t.token) {
        return page(
            StatusCode::UNAUTHORIZED,
            "需要凭据",
            "这个隧道的钥匙不在请求里（从本机核心给的地址进来才有）。",
            req,
        )
        .await;
    }

    // 3. 选上游：问「能连哪些机器」的那些归本机，其余的归那台电脑。
    //    那台电脑的键里带着路由代号：网络一变（`Routes::reset`）就换一批新连接，
    //    池子里走旧路的那些不再用（它们可能已经死了，只是还没人发现）。
    let (authority, token, down) = if path.starts_with("/api/machines") {
        (
            "local".to_string(),
            sh.local.token.clone(),
            "连不上本机核心",
        )
    } else {
        (
            format!("machine-{}", sh.t.routes.generation()),
            Some(sh.t.token.clone()),
            "连不上那台电脑",
        )
    };
    match forward(sh, req, &authority, token.as_deref()).await {
        Ok(res) => res,
        Err(why) => {
            tracing::warn!("隧道：{why}");
            text_page(StatusCode::BAD_GATEWAY, down, &why)
        }
    }
}

/// 逐跳的头：只管这一跳（页面 ↔ 隧道、隧道 ↔ 上游各是各的），不往下传。
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 转给上游：换掉凭据、去掉逐跳的头；WebSocket 升级的话两头各自升级，然后对搬字节。
async fn forward(
    sh: &Shared,
    mut req: hyper::Request<Incoming>,
    authority: &str,
    token: Option<&str>,
) -> Result<hyper::Response<Body>, String> {
    let upgrade = req.headers().contains_key(header::UPGRADE);
    let page_side = upgrade.then(|| hyper::upgrade::on(&mut req));

    let (mut parts, body) = req.into_parts();
    let pq = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    parts.uri = format!("http://{authority}{pq}")
        .parse()
        .map_err(|e| format!("请求地址不对：{e}"))?;
    let h = &mut parts.headers;
    // 浏览器带来的凭据不往上游带：上游只该看到我们给它准备的那一个
    // （两个来源的凭据混在一起，谁都说不清最后认的是谁）
    h.remove(header::COOKIE);
    h.remove("x-agent-token");
    if !upgrade {
        for k in HOP_BY_HOP {
            h.remove(k);
        }
    }
    if let Some(t) = token {
        h.insert(
            "x-agent-token",
            HeaderValue::from_str(t).map_err(|e| format!("凭据不是合法的头：{e}"))?,
        );
    }

    let mut res = sh
        .client
        .request(hyper::Request::from_parts(parts, Body::new(body)))
        .await
        .map_err(|e| innermost(&e))?;

    if let (Some(page_side), StatusCode::SWITCHING_PROTOCOLS) = (page_side, res.status()) {
        let up_side = hyper::upgrade::on(&mut res);
        tokio::spawn(async move {
            match tokio::try_join!(page_side, up_side) {
                Ok((a, b)) => {
                    let _ =
                        tokio::io::copy_bidirectional(&mut TokioIo::new(a), &mut TokioIo::new(b))
                            .await;
                }
                Err(e) => tracing::debug!("隧道：升级没成：{e}"),
            }
        });
        return Ok(res.map(|_| Body::empty()));
    }

    for k in ["connection", "keep-alive", "proxy-connection"] {
        res.headers_mut().remove(k);
    }
    Ok(res.map(Body::new))
}

/// 连不上时的原因在错误链的最里面（我们自己的那句话），外面那几层是 hyper 的套话。
fn innermost(e: &(dyn std::error::Error + 'static)) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// 隧道自己的 cookie 名（钥匙换成它，之后每个请求自动带上）。
const COOKIE: &str = "agent_tunnel";

/// 查询串里的一个参数（不解码：我们只拿来做等值比较）。
fn query<'a>(uri: &'a http::Uri, name: &str) -> Option<&'a str> {
    uri.query()?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// 一个 cookie 的值（可能有好几行 `cookie` 头）。
fn cookie<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

/// 这条请求带没带「能过隧道」的凭据：隧道自己的钥匙，或者那台电脑的 token
/// （壳的回合状态订阅、核心里「出门在外怎么连」那次对账都走后者，见 docs/06）。
fn credential_ok(h: &HeaderMap, key: &str, machine_token: &str) -> bool {
    let ok = |v: &str| same_secret(v, key) || same_secret(v, machine_token);
    h.get("x-agent-token")
        .and_then(|v| v.to_str().ok())
        .is_some_and(ok)
        || cookie(h, COOKIE).is_some_and(ok)
        || cookie(h, "agent_token").is_some_and(ok)
}

/// 连不上 / 不认凭据：给页面回一页说清楚为什么（不然它只会显示一个笼统的「无法访问」）。
///
/// 先把请求的 body 读掉（最多 64KB）：带着没读的数据关连接，系统会发 RST，浏览器只看到
/// 「连接被重置」，这页说明就白写了。
async fn page(
    status: StatusCode,
    title: &str,
    why: &str,
    req: hyper::Request<Incoming>,
) -> hyper::Response<Body> {
    let _ = axum::body::to_bytes(Body::new(req.into_body()), 64 * 1024).await;
    text_page(status, title, why)
}

fn text_page(status: StatusCode, title: &str, why: &str) -> hyper::Response<Body> {
    hyper::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(format!("{title}\n\n{why}\n")))
        .expect("固定的合法响应")
}

/// 上游连接从哪来：`local` → 本机核心（明文 loopback），其余 → 那台电脑（TLS，先局域网
/// 再中继，见 `open`）。连接池（`Shared::client`）建新连接时调它。
#[derive(Clone)]
struct Dial {
    t: Arc<Target>,
    local: Arc<Local>,
    tls: tokio_rustls::TlsConnector,
}

impl tower::Service<http::Uri> for Dial {
    type Response = TokioIo<Upstream>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let me = self.clone();
        Box::pin(async move {
            let up = if uri.host() == Some("local") {
                let addr = &me.local.addr;
                let s = TcpStream::connect(addr)
                    .await
                    .map_err(|e| format!("本机核心连不上（{addr}）：{e}"))?;
                let _ = s.set_nodelay(true);
                Upstream::Plain(s)
            } else {
                Upstream::Tls(Box::new(open(&me.t, &me.tls).await?))
            };
            Ok(TokioIo::new(up))
        })
    }
}

/// 一条上游连接：本机核心是明文，那台电脑是 TLS。
enum Upstream {
    Plain(TcpStream),
    Tls(Box<Tls>),
}

impl Connection for Upstream {
    fn connected(&self) -> Connected {
        Connected::new()
    }
}

impl tokio::io::AsyncRead for Upstream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Upstream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for Upstream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Upstream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            Upstream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Upstream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Upstream::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
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

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_bytes(v.as_bytes()).unwrap());
        }
        h
    }

    #[test]
    fn the_query_string_is_there_for_reading_the_tunnel_key() {
        let uri: http::Uri = "/?k=abc&x=1".parse().unwrap();
        assert_eq!(query(&uri, "k"), Some("abc"));
        assert_eq!(query(&uri, "nope"), None);
        // 路径不带查询串：`/api/machines` 那一条据此认出来
        assert_eq!(uri.path(), "/");
    }

    #[test]
    fn a_tunnel_credential_can_be_the_key_or_the_machines_own_token() {
        let ok = |h: &[(&'static str, &'static str)]| credential_ok(&headers(h), "K", "T");
        assert!(ok(&[("cookie", "agent_tunnel=K")]));
        assert!(ok(&[("cookie", "x=1; agent_token=T")]));
        // 浏览器可能把 cookie 拆成好几行
        assert!(ok(&[("cookie", "x=1"), ("cookie", "agent_tunnel=K")]));
        assert!(ok(&[("x-agent-token", "K")]));
        // 什么都没带、带了别的：不放行
        assert!(!ok(&[("host", "x")]));
        assert!(!ok(&[("x-agent-token", "别的")]));
    }

    #[test]
    fn the_reason_for_a_failed_connection_is_the_innermost_error() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("client error (Connect)")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Outer(std::io::Error::other("连不上「电脑」：它没配中继"));
        assert_eq!(innermost(&e), "连不上「电脑」：它没配中继");
    }
}
