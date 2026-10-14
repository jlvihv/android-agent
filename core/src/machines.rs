//! 手机这一头的「机器管理」：配对过哪些电脑、怎么连上它们（见 docs/12-relay.md）。
//!
//! 壳（Kotlin）只管三件事：mDNS 发现（`lan_seen` 把地址报进来）、WebView、「现在连的是
//! 哪台」。其余的都在这里 —— 配对、记账、连接、把中继交给还没配的电脑 —— 这样壳里不再
//! 有「局域网还是中继」的分叉：连哪台都是 `connect(id)` 拿到一个 `http://localhost:<端口>`，
//! 选路由隧道在每条连接上自己做（见 `tunnel.rs`）。
//!
//! 记账在库里的 `machines` 表（每台电脑一行：凭据、证书指纹、中继、局域网地址）。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::state::RelayAddr;
use crate::tunnel::{Routes, Target, Tunnel};

/// 一台配对过的电脑（`machines` 表的一行）。
#[derive(Clone, Debug, Deserialize)]
pub struct Machine {
    /// 核心的实例 id（IP 会变，它不变）
    pub id: String,
    pub name: String,
    /// 它的凭据（配对时换来的）
    pub token: String,
    /// 它的证书指纹：之后只认这一张
    pub fingerprint: String,
    /// 它挂的中继（没有 = 出门在外连不上它）
    #[serde(default)]
    pub relay: Option<RelayAddr>,
    /// 局域网里最近见到它的地址（`IP:端口`）
    #[serde(default)]
    pub lan: Option<String>,
}

/// 正连着的那台。
struct Current {
    id: String,
    routes: Arc<Routes>,
    _tunnel: Tunnel,
}

pub struct Machines {
    db: Arc<Db>,
    /// 库里那张表的内存副本（读得多、写得少：每次改都同时写库）
    list: Mutex<Vec<Machine>>,
    current: Mutex<Option<Current>>,
}

impl Machines {
    pub fn load(db: Arc<Db>) -> Arc<Self> {
        let list = db.machines().unwrap_or_else(|e| {
            tracing::warn!("读配对过的电脑失败：{e:#}");
            Vec::new()
        });
        Arc::new(Self {
            db,
            list: Mutex::new(list),
            current: Mutex::new(None),
        })
    }

    /// 改了一台：写库。
    fn save(&self, m: &Machine) {
        if let Err(e) = self.db.put_machine(m) {
            tracing::warn!("记不下「{}」：{e:#}", m.name);
        }
    }

    pub fn get(&self, id: &str) -> Option<Machine> {
        self.list.lock().ok()?.iter().find(|m| m.id == id).cloned()
    }

    pub fn list(&self) -> Vec<Machine> {
        self.list.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// 记下（同 id 的覆盖）。
    pub fn upsert(&self, m: Machine) {
        self.save(&m);
        if let Ok(mut g) = self.list.lock() {
            g.retain(|x| x.id != m.id);
            g.push(m);
        }
    }

    /// 忘掉一台（它不认我们的凭据了：换过库、重新配对）。正连着就断开。
    pub fn forget(&self, id: &str) {
        if let Ok(mut g) = self.list.lock() {
            g.retain(|x| x.id != id);
        }
        if let Err(e) = self.db.delete_machine(id) {
            tracing::warn!("删不掉 {id}：{e:#}");
        }
        if self.current_id().as_deref() == Some(id) {
            self.disconnect();
        }
    }

    fn current_id(&self) -> Option<String> {
        self.current.lock().ok()?.as_ref().map(|c| c.id.clone())
    }

    /// 发现服务在局域网里看到了它（`IP:端口`）：记下地址；正连着它的话，下一条连接
    /// 就先试局域网。
    pub fn lan_seen(&self, id: &str, addr: String) {
        let changed = self.list.lock().ok().and_then(|mut g| {
            let m = g.iter_mut().find(|m| m.id == id)?;
            (m.lan.as_deref() != Some(&addr)).then(|| {
                m.lan = Some(addr.clone());
                m.clone()
            })
        });
        if let Some(m) = changed {
            self.save(&m);
        }
        if let Ok(g) = self.current.lock() {
            if let Some(c) = g.as_ref().filter(|c| c.id == id) {
                c.routes.set_lan(addr);
            }
        }
    }

    /// 网络变了（换 Wi-Fi、出门、回家）：正连着的那台重新先试局域网。
    pub fn network_changed(&self) {
        if let Ok(g) = self.current.lock() {
            if let Some(c) = g.as_ref() {
                c.routes.reset();
            }
        }
    }

    /// 和局域网里的一台电脑配对：输它终端（或设置页）上显示的 6 位码，换回凭据。
    ///
    /// 走 TLS：先收下对面的证书（这时还不知道指纹），再核对配对结果里它自己报的指纹和
    /// 握手时看到的是同一张 —— 之后只认这一张。「人在电脑前、输对了码」就是信任的起点。
    pub async fn pair(&self, host: &str, port: u16, code: &str) -> anyhow::Result<Machine> {
        crate::llm::install_crypto_provider();
        let (cfg, seen) = crate::tunnel::pinned_config(None);
        let client = reqwest::ClientBuilder::new()
            .tls_backend_preconfigured(cfg)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?;
        let addr = host_port(host, port);
        let r = client
            .post(format!("https://{addr}/api/pair"))
            .json(&json!({ "code": code.trim() }))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("连不上 {addr}：{e}"))?;
        if !r.status().is_success() {
            let why = r.text().await.unwrap_or_default();
            anyhow::bail!(
                "{}",
                if why.is_empty() {
                    "配对失败".into()
                } else {
                    why
                }
            );
        }
        #[derive(Deserialize)]
        struct Remote {
            fingerprint: String,
            relay: Option<RelayAddr>,
        }
        #[derive(Deserialize)]
        struct Paired {
            token: String,
            id: String,
            name: String,
            remote: Option<Remote>,
        }
        let p: Paired = r.json().await?;
        let seen = seen
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| anyhow::anyhow!("没拿到那台电脑的证书"))?;
        let remote = p
            .remote
            .ok_or_else(|| anyhow::anyhow!("那台电脑没有 TLS 身份（核心太旧？先更新它）"))?;
        anyhow::ensure!(
            remote.fingerprint == seen,
            "那台电脑报的证书和握手时看到的不是同一张，不配对（局域网里有人在冒充？）"
        );
        let m = Machine {
            id: p.id,
            name: p.name,
            token: p.token,
            fingerprint: seen,
            relay: remote.relay,
            lan: Some(addr),
        };
        self.upsert(m.clone());
        Ok(m)
    }

    /// 连上这台：起隧道，返回 WebView 要加载的地址（`http://localhost:<端口>/auth?t=…`）。
    ///
    /// 已经连着别的就先断开（只连得着一台）。只是在本机绑个端口，**不联网** —— 真正选路
    /// 和拨号发生在 WebView 第一次请求的时候，连不上的原因会作为一页 502 显示出来。
    /// 连上之后在后台对一遍「出门在外怎么连」（见 `sync`）。
    pub async fn connect(self: &Arc<Self>, id: &str) -> anyhow::Result<String> {
        let m = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("没配对过这台电脑"))?;
        let relay = m.relay.as_ref().and_then(|r| {
            agent_relay::parse_key(&r.key)
                .ok()
                .map(|k| (r.addr.clone(), k))
        });
        let routes = Arc::new(Routes::new(m.lan.clone(), relay));
        let tunnel = crate::tunnel::start(Target {
            host_id: m.id.clone(),
            name: m.name.clone(),
            fingerprint: m.fingerprint.clone(),
            routes: routes.clone(),
        })
        .await?;
        let port = tunnel.port;
        if let Ok(mut g) = self.current.lock() {
            *g = Some(Current {
                id: m.id.clone(),
                routes,
                _tunnel: tunnel,
            });
        }
        tokio::spawn(self.clone().sync(m.id.clone(), port, m.token.clone()));
        Ok(format!("http://localhost:{port}/auth?t={}", m.token))
    }

    /// 断开（回到手机自己的核心）。
    pub fn disconnect(&self) {
        if let Ok(mut g) = self.current.lock() {
            *g = None;
        }
    }

    /// 连上之后问那台电脑一句「出门在外怎么连」（`GET /api/remote`，经刚起的隧道）：
    ///
    /// - 它的中继变了（后来才配、换了服务器）：记下来，下次连就用新的；
    /// - **它还没有中继、而手机知道一个**（从别的电脑那里记下的）：交给它
    ///   （`POST /api/remote/relay`）。那台电脑立刻挂上、记下来 —— 人不用去复制密钥。
    ///
    /// 失败就算了（下次连上再对）。
    async fn sync(self: Arc<Self>, id: String, port: u16, token: String) {
        crate::llm::install_crypto_provider();
        let Ok(client) = reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(15))
            .build()
        else {
            return;
        };
        let base = format!("http://127.0.0.1:{port}");
        let cookie = format!("agent_token={token}");
        let get = client
            .get(format!("{base}/api/remote"))
            .header("cookie", &cookie)
            .send()
            .await;
        let Ok(r) = get else { return };
        let Ok(v) = r.json::<serde_json::Value>().await else {
            return;
        };
        let mut relay: Option<RelayAddr> = v
            .pointer("/remote/relay")
            .and_then(|r| serde_json::from_value(r.clone()).ok());
        if relay.is_none() {
            let known = self.list.lock().ok().and_then(|g| {
                g.iter()
                    .filter(|m| m.id != id)
                    .find_map(|m| m.relay.clone())
            });
            if let Some(known) = known {
                let post = client
                    .post(format!("{base}/api/remote/relay"))
                    .header("cookie", &cookie)
                    .json(&known)
                    .send()
                    .await;
                if post.is_ok_and(|r| r.status().is_success()) {
                    tracing::info!("把中继 {} 交给了 {id}", known.addr);
                    relay = Some(known);
                }
            }
        }
        let changed = self.list.lock().ok().and_then(|mut g| {
            let m = g.iter_mut().find(|m| m.id == id)?;
            let same = m.relay.as_ref().map(|r| (&r.addr, &r.key))
                == relay.as_ref().map(|r| (&r.addr, &r.key));
            (!same).then(|| {
                m.relay = relay;
                m.clone()
            })
        });
        if let Some(m) = changed {
            self.save(&m);
        }
    }
}

/// `IP:端口`，IPv6 加方括号。
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// 写一个只给自己读的文件（0600）：里面有私钥。
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    o.mode(0o600);
    o.open(path)?.write_all(bytes)
}
