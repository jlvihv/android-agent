//! 中继：让手机在外面也能连上家里电脑的核心。
//!
//! ```text
//! 家里电脑的核心 ──HOST（主动连出去，一直挂着）──▶ 中继 ◀──DIAL── 外面的手机
//!                ◀──CONNECT <票>────────────────
//!                ──ACCEPT <票>（新开一条）──────▶  两条接上，之后只搬字节
//! ```
//!
//! - **家里不用开端口**：电脑主动连中继，中继只负责把两边接起来。
//! - **中继看不到内容**：接上之后两头直接跑 TLS（证书是家里核心自己的，手机配对时
//!   记下了它的指纹，见 core 的 `remote.rs`）。中继只搬加密后的字节，哪怕服务器被
//!   人拿下，也拿不到会话内容和凭据。
//! - **中继自己的门**：一把共享密钥（中继第一次启动时生成）。握手是挑战-应答：
//!   中继先给一个随机数，对面回 `HMAC(密钥, 角色|id|随机数)`。密钥本身不过网，
//!   抓到的握手也重放不了（随机数每条连接都不一样）。没有密钥的人既不能冒充家里
//!   那台电脑占位，也不能拿这台服务器当免费的中转。
//!
//! 一条用户连接对应一对 TCP 连接（手机那条 + 电脑为它新开的那条），不在一条连接上
//! 做多路复用 —— 中继因此只有「接线」和「搬字节」两件事，没有自己的帧格式。
//!
//! 握手全是一行一行的文本（`\n` 结尾），之后就是原始字节：
//!
//! ```text
//! 中继 → 对面   AGR1 <随机数>
//! 对面 → 中继   HOST <id> <mac>             mac = HMAC(key, "host\n<id>\n<随机数>")
//!               DIAL <id> <mac>             mac = HMAC(key, "dial\n<id>\n<随机数>")
//!               ACCEPT <id> <票> <mac>      mac = HMAC(key, "accept\n<id>\n<票>\n<随机数>")
//! 中继 → 对面   OK  或  ERR <原因>
//! 之后（HOST） 中继 → 电脑  CONNECT <票> / PING；电脑 → 中继  PING
//! ```

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};

/// 协议版本（握手第一行的开头）。改了握手就换一个。
pub const MAGIC: &str = "AGR1";
/// 中继默认听的端口。
pub const DEFAULT_PORT: u16 = 8790;
/// 握手（连上 → 回 OK）最多等多久。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 控制连接上多久发一次心跳。NAT / 运营商会把闲着的 TCP 悄悄掐掉（常见是几分钟），
/// 心跳要比那短得多。
pub const PING_EVERY: Duration = Duration::from_secs(20);
/// 控制连接上多久没收到任何东西就当它死了（三次心跳都没来）。
pub const DEAD_AFTER: Duration = Duration::from_secs(60);
/// 手机拨号之后，等电脑为它开好那条连接最多多久。
const ACCEPT_WAIT: Duration = Duration::from_secs(10);
/// 握手的一行最长多少字节（id、票、mac 都是几十字节；再长就是乱来的）。
const LINE_MAX: usize = 256;
/// 同时最多有多少条「电脑还没接」的拨号。防有密钥的一方（或 bug）把内存撑爆。
const PENDING_MAX: usize = 256;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------------------
// 密钥 / 随机数 / MAC
// ---------------------------------------------------------------------------

/// `n` 字节的随机数，十六进制。
pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("系统随机数源不可用");
    hex(&b)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// 解析十六进制的密钥（至少 16 字节）。
pub fn parse_key(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || s.len() < 32 {
        return Err("中继密钥应当是至少 32 个十六进制字符".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| "中继密钥里有非十六进制字符".into())
        })
        .collect()
}

fn mac(key: &[u8], parts: &[&str]) -> String {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC 接受任意长度的密钥");
    m.update(parts.join("\n").as_bytes());
    hex(&m.finalize().into_bytes())
}

/// 耗时和内容无关的比较（别让快慢泄露 mac 的前几位）。
fn mac_ok(key: &[u8], parts: &[&str], got: &str) -> bool {
    let want = mac(key, parts);
    want.len() == got.len()
        && want
            .bytes()
            .zip(got.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// id 和票只允许这些字符：它们会出现在以空格分隔的握手行里。
fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// 一行一行地读写
// ---------------------------------------------------------------------------

/// 读一行（不含 `\n`）。**逐字节读**：握手之后紧跟的是原始字节（TLS），带缓冲的
/// 读法会把那一部分也吞进缓冲区里，后面就接不上了。握手就几行，逐字节不贵。
async fn read_line<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<String> {
    let mut buf = Vec::with_capacity(64);
    loop {
        let b = r.read_u8().await?;
        if b == b'\n' {
            break;
        }
        if buf.len() >= LINE_MAX {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "握手行太长"));
        }
        buf.push(b);
    }
    String::from_utf8(buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "握手行不是 UTF-8"))
}

async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, line: &str) -> io::Result<()> {
    w.write_all(format!("{line}\n").as_bytes()).await?;
    w.flush().await
}

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

// ---------------------------------------------------------------------------
// 客户端（家里的核心、手机的核心都用这一份）
// ---------------------------------------------------------------------------

/// 连上中继时说自己是谁。
pub enum Role<'a> {
    /// 家里的电脑：挂一条控制连接，等 `CONNECT`
    Host { id: &'a str },
    /// 家里的电脑：为某张票新开的那条数据连接
    Accept { id: &'a str, ticket: &'a str },
    /// 手机：要连 `id` 那台电脑
    Dial { id: &'a str },
}

/// 连上中继并握手。返回的连接已经「接通」：`Dial` / `Accept` 之后就是对面的字节。
pub async fn connect(addr: &str, key: &[u8], role: Role<'_>) -> io::Result<TcpStream> {
    let mut s = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| other(format!("连中继 {addr} 超时")))??;
    let _ = s.set_nodelay(true);
    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&mut s, key, role))
        .await
        .map_err(|_| other("中继握手超时"))??;
    Ok(s)
}

async fn handshake(s: &mut TcpStream, key: &[u8], role: Role<'_>) -> io::Result<()> {
    let hello = read_line(s).await?;
    let nonce = match hello.split_once(' ') {
        Some((MAGIC, n)) => n.to_string(),
        _ => return Err(other("对面不是中继（或者版本对不上）")),
    };
    let line = match role {
        Role::Host { id } => format!("HOST {id} {}", mac(key, &["host", id, &nonce])),
        Role::Dial { id } => format!("DIAL {id} {}", mac(key, &["dial", id, &nonce])),
        Role::Accept { id, ticket } => format!(
            "ACCEPT {id} {ticket} {}",
            mac(key, &["accept", id, ticket, &nonce])
        ),
    };
    write_line(s, &line).await?;
    let reply = read_line(s).await?;
    match reply
        .split_once(' ')
        .map_or((reply.as_str(), ""), |(a, b)| (a, b))
    {
        ("OK", _) => Ok(()),
        ("ERR", why) => Err(other(why.to_string())),
        _ => Err(other(format!("中继回了看不懂的东西：{reply}"))),
    }
}

/// 家里电脑这一头：一直挂在中继上，每来一个拨号就新开一条数据连接交给 `on_conn`。
///
/// 断了就重连（退避 2 秒起、翻倍到 60 秒，连上过就归位），**永远不返回** ——
/// 中继重启、家里断网、服务器换 IP 都靠它自己接回来。`log` 收「连上了 / 断了」这类
/// 一行状态（只在变化时调）。
pub async fn run_host<F, L>(addr: String, key: Vec<u8>, id: String, on_conn: F, log: L)
where
    F: Fn(TcpStream) + Send + Sync + 'static,
    L: Fn(String) + Send + Sync + 'static,
{
    let on_conn = Arc::new(on_conn);
    let mut backoff = Duration::from_secs(2);
    let mut last_err = String::new();
    loop {
        match connect(&addr, &key, Role::Host { id: &id }).await {
            Ok(s) => {
                log(format!("已连上中继 {addr}，出门在外也能连这台电脑了"));
                last_err.clear();
                backoff = Duration::from_secs(2);
                let why = host_control(s, &addr, &key, &id, on_conn.clone()).await;
                log(format!("和中继的连接断了（{why}），重连中"));
            }
            Err(e) => {
                let msg = e.to_string();
                if msg != last_err {
                    log(format!("连不上中继 {addr}：{msg}（会一直重试）"));
                    last_err = msg;
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// 控制连接：读 `CONNECT` / `PING`，定时发 `PING`。返回断开的原因。
async fn host_control<F>(s: TcpStream, addr: &str, key: &[u8], id: &str, on_conn: Arc<F>) -> String
where
    F: Fn(TcpStream) + Send + Sync + 'static,
{
    let (mut r, mut w) = s.into_split();
    let pinger = tokio::spawn(async move {
        loop {
            tokio::time::sleep(PING_EVERY).await;
            if write_line(&mut w, "PING").await.is_err() {
                return;
            }
        }
    });
    let why = loop {
        let line = match tokio::time::timeout(DEAD_AFTER, read_line(&mut r)).await {
            Err(_) => break "太久没有心跳".to_string(),
            Ok(Err(e)) => break e.to_string(),
            Ok(Ok(l)) => l,
        };
        let Some(ticket) = line.strip_prefix("CONNECT ") else {
            continue; // PING
        };
        if !valid_token(ticket) {
            continue;
        }
        let (addr, key, id, ticket, on_conn) = (
            addr.to_string(),
            key.to_vec(),
            id.to_string(),
            ticket.to_string(),
            on_conn.clone(),
        );
        tokio::spawn(async move {
            match connect(
                &addr,
                &key,
                Role::Accept {
                    id: &id,
                    ticket: &ticket,
                },
            )
            .await
            {
                Ok(s) => on_conn(s),
                Err(e) => eprintln!("中继：接一个拨号失败：{e}"),
            }
        });
    };
    pinger.abort();
    why
}

// ---------------------------------------------------------------------------
// 中继服务器
// ---------------------------------------------------------------------------

struct HostSlot {
    /// 往这台电脑的控制连接写一行
    tx: mpsc::Sender<String>,
    /// 同一个 id 重连时新的会顶掉旧的；旧连接退场时只删「还是自己」的那一格
    generation: u64,
}

/// 中继的全部状态：谁在线、哪些拨号在等电脑接。
pub struct Relay {
    key: Vec<u8>,
    hosts: Mutex<HashMap<String, HostSlot>>,
    pending: Mutex<HashMap<String, oneshot::Sender<TcpStream>>>,
    generation: AtomicU64,
}

impl Relay {
    pub fn new(key: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            key,
            hosts: Mutex::default(),
            pending: Mutex::default(),
            generation: AtomicU64::new(0),
        })
    }

    /// 在线的电脑有几台（日志用）。
    pub fn online(&self) -> usize {
        self.hosts.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// 接客，直到进程结束。
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        loop {
            let (s, peer) = listener.accept().await?;
            let _ = s.set_nodelay(true);
            let me = self.clone();
            tokio::spawn(async move {
                if let Err(e) = me.handle(s).await {
                    // 握手失败多半是扫端口的；只记一行，不刷屏
                    eprintln!("{peer}: {e}");
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut s: TcpStream) -> io::Result<()> {
        let nonce = random_hex(16);
        let line = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            write_line(&mut s, &format!("{MAGIC} {nonce}")).await?;
            read_line(&mut s).await
        })
        .await
        .map_err(|_| other("握手超时"))??;
        let words: Vec<&str> = line.split(' ').collect();
        match words.as_slice() {
            ["HOST", id, m] if valid_token(id) && mac_ok(&self.key, &["host", id, &nonce], m) => {
                let id = id.to_string();
                self.host(s, id).await
            }
            ["DIAL", id, m] if valid_token(id) && mac_ok(&self.key, &["dial", id, &nonce], m) => {
                let id = id.to_string();
                self.dial(s, &id).await
            }
            ["ACCEPT", id, t, m]
                if valid_token(id)
                    && valid_token(t)
                    && mac_ok(&self.key, &["accept", id, t, &nonce], m) =>
            {
                let t = t.to_string();
                self.accept(s, &t).await
            }
            _ => {
                let _ = write_line(&mut s, "ERR 密钥不对，或者握手格式不对").await;
                Err(other("握手没通过"))
            }
        }
    }

    /// 一台电脑挂上来了：登记它，然后守着这条控制连接直到它断。
    async fn host(self: Arc<Self>, mut s: TcpStream, id: String) -> io::Result<()> {
        write_line(&mut s, "OK").await?;
        let (tx, mut rx) = mpsc::channel::<String>(16);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = self.hosts.lock() {
            // 同一个 id 再连上来（电脑重启、换了网络）：顶掉旧的那条。旧的 `tx` 被
            // drop，它那边的 `rx.recv()` 返回 None，旧连接自己收场
            g.insert(id.clone(), HostSlot { tx, generation });
        }
        eprintln!("上线：{id}（在线 {} 台）", self.online());

        let (mut r, mut w) = s.into_split();
        let mut ping = tokio::time::interval(PING_EVERY);
        let reader = async {
            loop {
                match tokio::time::timeout(DEAD_AFTER, read_line(&mut r)).await {
                    Ok(Ok(_)) => continue, // PING
                    _ => return,
                }
            }
        };
        tokio::pin!(reader);
        loop {
            tokio::select! {
                _ = &mut reader => break,
                msg = rx.recv() => match msg {
                    Some(line) => if write_line(&mut w, &line).await.is_err() { break },
                    None => break, // 被同 id 的新连接顶掉了
                },
                _ = ping.tick() => if write_line(&mut w, "PING").await.is_err() { break },
            }
        }

        if let Ok(mut g) = self.hosts.lock() {
            if g.get(&id).is_some_and(|h| h.generation == generation) {
                g.remove(&id);
            }
        }
        eprintln!("下线：{id}（在线 {} 台）", self.online());
        Ok(())
    }

    /// 手机要连 `id`：让那台电脑新开一条连接，接上之后两头对搬字节。
    async fn dial(self: Arc<Self>, mut phone: TcpStream, id: &str) -> io::Result<()> {
        let host_tx = self
            .hosts
            .lock()
            .ok()
            .and_then(|g| g.get(id).map(|h| h.tx.clone()));
        let Some(host_tx) = host_tx else {
            write_line(&mut phone, "ERR 那台电脑现在不在线（它没连上中继）").await?;
            return Ok(());
        };
        let ticket = random_hex(16);
        let (tx, rx) = oneshot::channel();
        // 锁只在这个块里拿着（守卫跨 await 的话 future 就不是 Send 了）
        let full = match self.pending.lock() {
            Ok(mut g) if g.len() < PENDING_MAX => {
                g.insert(ticket.clone(), tx);
                false
            }
            Ok(_) => true,
            Err(_) => return Err(other("中继内部状态坏了")),
        };
        if full {
            write_line(&mut phone, "ERR 中继太忙了，稍后再试").await?;
            return Ok(());
        }
        if host_tx.send(format!("CONNECT {ticket}")).await.is_err() {
            self.forget(&ticket);
            write_line(&mut phone, "ERR 那台电脑刚刚断开了").await?;
            return Ok(());
        }
        let mut host = match tokio::time::timeout(ACCEPT_WAIT, rx).await {
            Ok(Ok(h)) => h,
            _ => {
                self.forget(&ticket);
                write_line(&mut phone, "ERR 那台电脑没有应答").await?;
                return Ok(());
            }
        };
        write_line(&mut phone, "OK").await?;
        // 之后只搬字节（里面是两头之间的 TLS，中继看不懂）
        let _ = tokio::io::copy_bidirectional(&mut phone, &mut host).await;
        Ok(())
    }

    /// 电脑为某张票开好的那条连接：交给正在等它的拨号。
    async fn accept(self: Arc<Self>, mut s: TcpStream, ticket: &str) -> io::Result<()> {
        let waiter = self.pending.lock().ok().and_then(|mut g| g.remove(ticket));
        let Some(waiter) = waiter else {
            write_line(&mut s, "ERR 这张票已经过期了").await?;
            return Ok(());
        };
        write_line(&mut s, "OK").await?;
        // 拨号那边已经超时走了的话，这条连接随 `s` 一起丢掉就行
        let _ = waiter.send(s);
        Ok(())
    }

    fn forget(&self, ticket: &str) {
        if let Ok(mut g) = self.pending.lock() {
            g.remove(ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "00112233445566778899aabbccddeeff";

    async fn relay() -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(Relay::new(parse_key(KEY).unwrap()).serve(l));
        addr
    }

    /// 家里那台：每条接进来的连接把收到的东西原样回去（「echo 服务器」）。
    async fn echo_host(addr: &str, id: &str) {
        let key = parse_key(KEY).unwrap();
        tokio::spawn(run_host(
            addr.to_string(),
            key,
            id.to_string(),
            |mut s: TcpStream| {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            },
            |_| {},
        ));
        // 等它挂上
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn a_dial_reaches_the_host_and_bytes_flow_both_ways() {
        let addr = relay().await;
        echo_host(&addr, "home").await;
        let key = parse_key(KEY).unwrap();
        // 两条并发的拨号各自接到自己的那条（一条用户连接对应一对 TCP）
        for msg in [b"hello".as_slice(), b"world".as_slice()] {
            let mut s = connect(&addr, &key, Role::Dial { id: "home" })
                .await
                .unwrap();
            s.write_all(msg).await.unwrap();
            let mut buf = vec![0u8; msg.len()];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, msg);
        }
    }

    #[tokio::test]
    async fn dialling_an_offline_host_says_so() {
        let addr = relay().await;
        let key = parse_key(KEY).unwrap();
        let e = connect(&addr, &key, Role::Dial { id: "nobody" })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("不在线"), "{e}");
    }

    #[tokio::test]
    async fn the_wrong_key_is_turned_away() {
        let addr = relay().await;
        let bad = parse_key("ffeeddccbbaa99887766554433221100").unwrap();
        let e = connect(&addr, &bad, Role::Host { id: "home" })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("密钥"), "{e}");
    }

    #[test]
    fn a_mac_is_bound_to_its_nonce_and_role() {
        let key = parse_key(KEY).unwrap();
        let m = mac(&key, &["dial", "home", "n1"]);
        assert!(mac_ok(&key, &["dial", "home", "n1"], &m));
        // 换个随机数（重放）、换个角色（拿拨号的 mac 去冒充电脑）都不行
        assert!(!mac_ok(&key, &["dial", "home", "n2"], &m));
        assert!(!mac_ok(&key, &["host", "home", "n1"], &m));
    }

    #[test]
    fn keys_and_tokens_are_validated() {
        assert!(parse_key("abc").is_err());
        assert!(parse_key("zz112233445566778899aabbccddeeff").is_err());
        assert!(valid_token("a1-b_2"));
        assert!(!valid_token("has space"));
        assert!(!valid_token(""));
    }
}
