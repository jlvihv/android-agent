//! 「我这台机器连过哪些电脑、现在连的是哪台」（见 docs/12-relay.md）。
//!
//! **一份实现，两个宿主**：手机 App 的壳和电脑上的浏览器走的是同一份 —— 配对、记账、连接、
//! 把中继交给还没配的电脑、页面上那个「选择机器」面板要的列表，全在这里。平台只差两件薄的：
//! mDNS 发现（手机是壳的 `NsdManager`，电脑是 `desktop::browse` 起的 `avahi-browse`）
//! 和「谁来导航」（手机是壳的 WebView，电脑是页面自己 `location.href`）。两边都只是把
//! **看到的**地址报进来（`lan_found` / `lan_lost`），剩下的都在这里。
//!
//! 连哪台都是 `connect(id)` 拿到一个 `http://localhost:<端口>/?k=…`（见 `tunnel.rs`）：
//! 选路、TLS、替页面带上凭据都在隧道里，壳里没有「局域网还是中继」的分叉。
//!
//! 记账在库里的 `machines` 表（每台电脑一行：凭据、证书指纹、中继、局域网地址），
//! 「上次连的是哪台」也在库里（`meta` 表的 `current_machine`）—— 手机下次启动接着连。

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::state::{AppState, RelayAddr};
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

/// 局域网里**看到过**的一个核心：配没配对都记，面板要把没配对的也列出来。
///
/// 用一次丢一次（`lan_found` 报进来、`lan_lost` 报走）—— 没配对过的那些没必要落盘，
/// 下次启动重新找就是。
#[derive(Clone, Debug)]
struct Found {
    id: String,
    name: String,
    host: String,
    port: u16,
}

/// 「选择机器」面板的一行（`GET /api/machines` 的 `list`）。
#[derive(Clone, Debug)]
pub struct Panel {
    pub id: String,
    pub name: String,
    /// 局域网上那个地址；经中继才连得上的那行是空的
    pub host: String,
    pub port: u16,
    /// 配对过（点一下直接连；没配对过要先输配对码）
    pub paired: bool,
    /// 局域网里找不到，得经中继连
    pub relay: bool,
    /// 现在连着的就是它
    pub current: bool,
}

/// 一条连上的隧道：地址由 `Conn` 的两个方法拼（壳和页面拿的不一样，见它们自己的说明）。
pub struct Conn {
    pub id: String,
    pub name: String,
    pub port: u16,
    /// 页面的隧道钥匙
    pub key: String,
    /// 那台电脑的凭据（只给壳和核心里自己用，不进页面）
    pub token: String,
}

impl Conn {
    /// 页面上「选择机器」里的每一行都是它：`http://localhost:<端口>/?k=<钥匙>`。
    /// 钥匙换成一个 HttpOnly cookie 之后就不再露面；页面里的 JS 拿不到任何机器的凭据。
    pub fn page_url(&self) -> String {
        format!("http://localhost:{}/?k={}", self.port, self.key)
    }

    /// 壳（WebView）加载的地址：**多一个 `t=`**。壳要它订阅那台电脑的回合状态
    /// （通知栏那一行，见 docs/06）—— 那是壳自己的 HTTP 客户端，不经过页面的 JS。
    /// `k=` 那一段照样先经过隧道换成 cookie，`/auth` 其实不会真跑到那台电脑上。
    pub fn shell_url(&self) -> String {
        format!(
            "http://localhost:{}/auth?t={}&k={}",
            self.port, self.token, self.key
        )
    }
}

/// 正连着的那台。
struct Current {
    /// 起这条隧道时算出来的地址（端口、钥匙、那台电脑的凭据都在这）——
    /// 壳要问「现在这条隧道是哪个地址」时照它拼，不用重连（见 `ffi`）
    conn: Conn,
    routes: Arc<Routes>,
    _tunnel: Tunnel,
}

pub struct Machines {
    /// 本机核心（隧道转发 `/api/machines*` 时要它的地址和凭据）。
    ///
    /// `Weak`：和 `AppState` 互相指着，强的那一头在 AppState 那边（它握着 Arc<Machines>）。
    /// 测试里没有核心上下文（`with_db`），只有记账和配对能跑。
    state: Weak<AppState>,
    db: Arc<Db>,
    /// 库里那张表的内存副本（读得多、写得少：每次改都同时写库）
    list: Mutex<Vec<Machine>>,
    /// 局域网里看到过的（配没配对都在）
    found: Mutex<Vec<Found>>,
    current: Mutex<Option<Current>>,
}

impl Machines {
    pub fn new(db: Arc<Db>, state: Weak<AppState>) -> Arc<Self> {
        let list = db.machines().unwrap_or_else(|e| {
            tracing::warn!("读配对过的电脑失败：{e:#}");
            Vec::new()
        });
        Arc::new(Self {
            state,
            db,
            list: Mutex::new(list),
            found: Mutex::new(Vec::new()),
            current: Mutex::new(None),
        })
    }

    /// 测试用：没有本机核心上下文（连不了机器，记账和配对照旧）。
    #[cfg(test)]
    pub fn with_db(db: Arc<Db>) -> Arc<Self> {
        Self::new(db, Weak::new())
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

    pub fn current_id(&self) -> Option<String> {
        self.current
            .lock()
            .ok()?
            .as_ref()
            .map(|c| c.conn.id.clone())
    }

    /// 正连着的那台：地址和凭据（壳用；页面拿到的地址不带凭据，见 `Conn`）。
    pub fn current(&self) -> Option<(String, String, u16)> {
        let g = self.current.lock().ok()?;
        let c = g.as_ref()?;
        Some((c.conn.id.clone(), c.conn.shell_url(), c.conn.port))
    }

    /// 发现服务（壳的 `NsdManager` / 电脑上那个 `avahi-browse`）在局域网里看到了它。
    ///
    /// 两件事：记下面板要列的那一行；配对过的话，把地址记到它头上 —— 连着它时下一条
    /// 连接就先试局域网（`Routes::set_lan`）。
    pub fn lan_found(&self, id: &str, name: &str, host: &str, port: u16) {
        self.set_lan(id, host_port(host, port));
        let entry = Found {
            id: id.to_string(),
            name: name.to_string(),
            host: host.to_string(),
            port,
        };
        if let Ok(mut g) = self.found.lock() {
            match g.iter_mut().find(|f| f.id == id) {
                Some(f) => *f = entry,
                None => g.push(entry),
            }
        }
    }

    /// 局域网里看不到它了（广播撤了 / 换网了）：面板上那一行拿掉。
    ///
    /// 配对过的那台的地址**留着**：那是选路的退路（下次先试它，不通再走中继）。
    pub fn lan_lost(&self, id: &str) {
        if let Ok(mut g) = self.found.lock() {
            g.retain(|f| f.id != id);
        }
    }

    /// 一次扫描结束了（手机上面板关掉时）：没配对过的那些忘掉。
    ///
    /// 配对过的留着 —— 它们记在库里，本来就是「我的机器」，不是「这次扫到的」。
    pub fn lan_reset(&self) {
        if let Ok(mut g) = self.found.lock() {
            g.clear();
        }
    }

    /// 库里的那个局域网地址（只给配对过的记）。
    fn set_lan(&self, id: &str, addr: String) {
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
            if let Some(c) = g.as_ref().filter(|c| c.conn.id == id) {
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

    /// 「选择机器」面板要的那份：局域网里看到的 ∪ 配对过的，按实例 id 去重。
    /// 局域网里找得到的那台**只出一行直连的**。
    ///
    /// **按名字排**（同名再按 id），不按「先看到的在前」：发现结果陆续到、一台在
    /// 「局域网 / 经服务器」之间换来换去时，它在面板上的位置不跟着跳。
    pub fn panel(&self) -> Vec<Panel> {
        let cur = self.current_id();
        let found = self.found.lock().map(|g| g.clone()).unwrap_or_default();
        let list = self.list();
        let here: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
        let mut out: Vec<Panel> = found
            .iter()
            .map(|f| {
                let paired = list.iter().find(|m| m.id == f.id);
                Panel {
                    id: f.id.clone(),
                    // 配对过就用库里那个名字（配对是它给的）；没配对过用广播里的
                    name: paired
                        .map(|m| m.name.clone())
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| f.name.clone()),
                    host: f.host.clone(),
                    port: f.port,
                    paired: paired.is_some(),
                    relay: false,
                    current: cur.as_deref() == Some(f.id.as_str()),
                }
            })
            .collect();
        for m in list {
            if here.contains(&m.id.as_str()) {
                continue;
            }
            let current = cur.as_deref() == Some(m.id.as_str());
            // 局域网里找不到、又没配中继、也不是正连着的：列出来也没法连
            if !m.relay.is_some() && !current {
                continue;
            }
            out.push(Panel {
                id: m.id.clone(),
                name: m.name.clone(),
                host: String::new(),
                port: 0,
                paired: true,
                relay: m.relay.is_some(),
                current,
            });
        }
        out.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        out
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

    /// 连上这台：起隧道，返回两个地址（`Conn`，见它自己的说明）。
    ///
    /// 已经连着别的就先断开（只连得着一台）。只是在本机绑个端口，**不联网** —— 真正选路
    /// 和拨号发生在页面第一次请求的时候，连不上的原因会作为一页 502 显示出来。
    /// 连上之后在后台对一遍「出门在外怎么连」（见 `sync`）。
    pub async fn connect(self: &Arc<Self>, id: &str) -> anyhow::Result<Conn> {
        let m = self
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("没配对过这台电脑"))?;
        let (addr, token) = self.local_upstream()?;
        let relay = m.relay.as_ref().and_then(|r| {
            agent_relay::parse_key(&r.key)
                .ok()
                .map(|k| (r.addr.clone(), k))
        });
        let routes = Arc::new(Routes::new(m.lan.clone(), relay));
        let tunnel = crate::tunnel::start(
            Target {
                host_id: m.id.clone(),
                name: m.name.clone(),
                fingerprint: m.fingerprint.clone(),
                token: m.token.clone(),
                routes: routes.clone(),
            },
            crate::tunnel::Local { addr, token },
            self.saved_port(id),
        )
        .await?;
        let conn = Conn {
            id: m.id.clone(),
            name: m.name.clone(),
            port: tunnel.port,
            key: tunnel.key.clone(),
            token: m.token.clone(),
        };
        if let Ok(mut g) = self.current.lock() {
            *g = Some(Current {
                conn: Conn {
                    id: conn.id.clone(),
                    name: conn.name.clone(),
                    port: conn.port,
                    key: conn.key.clone(),
                    token: conn.token.clone(),
                },
                routes,
                _tunnel: tunnel,
            });
        }
        self.remember(&conn);
        tokio::spawn(self.clone().sync(m.id.clone(), conn.port, m.token.clone()));
        Ok(conn)
    }

    /// 上一次连的那台，接着连（隧道端口每次不一样，地址得重新算）。
    ///
    /// 手机启动时壳要它加载 WebView；电脑上重启核心后浏览器那个标签也能接着用
    /// （端口会尽量复用上次那个）。
    pub async fn resume(self: &Arc<Self>) -> Option<Conn> {
        let id = self.db.get_meta("current_machine").ok().flatten()?;
        if id.is_empty() {
            return None;
        }
        match self.clone().connect(&id).await {
            Ok(c) => {
                tracing::info!("接着连「{}」（127.0.0.1:{}）", c.name, c.port);
                Some(c)
            }
            Err(e) => {
                tracing::warn!("接着连「{id}」失败：{e:#}");
                let _ = self.db.delete_meta("current_machine");
                None
            }
        }
    }

    /// 本机核心（隧道把 `/api/machines*` 转给它）的地址和凭据。
    fn local_upstream(&self) -> anyhow::Result<(String, Option<String>)> {
        let Some(st) = self.state.upgrade() else {
            // 没有核心上下文（只有测试这样建 `with_db`）：隧道照样起得来，只是
            // `/api/machines*` 那条上游是死的（回 502）。除了这一点，选路、TLS、
            // 转发的行为和真的一模一样 —— 端到端测试就靠它。
            return Ok(("127.0.0.1:1".into(), None));
        };
        let port = st.port.load(std::sync::atomic::Ordering::Relaxed);
        anyhow::ensure!(port != 0, "本机核心还没开始服务");
        Ok((format!("127.0.0.1:{port}"), st.auth().token))
    }

    /// 记住「现在连的是哪台」（库里的 `meta`）：下次启动接着连。
    fn remember(&self, c: &Conn) {
        let _ = self.db.set_meta("current_machine", &c.id);
        let _ = self.db.set_meta("tunnel_port", &c.port.to_string());
    }

    /// 上次那条隧道用的端口（还是同一台机器的话）。
    ///
    /// 为什么要复用：浏览器那个标签停在 `http://localhost:<端口>`，端口每次重启都换的话
    /// 它就白开着了（而手机上壳每次启动都会重新导航，无所谓）。占了就算了，换个空的。
    fn saved_port(&self, id: &str) -> Option<u16> {
        if self.db.get_meta("current_machine").ok().flatten()?.as_str() != id {
            return None;
        }
        self.db.get_meta("tunnel_port").ok().flatten()?.parse().ok()
    }

    /// 断开（回到本机自己的核心）。下次启动不再自动连它。
    pub fn disconnect(&self) {
        if let Ok(mut g) = self.current.lock() {
            *g = None;
        }
        let _ = self.db.delete_meta("current_machine");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<Machines> {
        Machines::with_db(Arc::new(crate::db::Db::in_memory().expect("内存库")))
    }

    fn machine(id: &str, name: &str, relay: bool) -> Machine {
        Machine {
            id: id.into(),
            name: name.into(),
            token: "t".into(),
            fingerprint: "f".into(),
            relay: relay.then(|| RelayAddr {
                addr: "1.2.3.4:8790".into(),
                key: "k".into(),
            }),
            lan: None,
        }
    }

    #[test]
    fn the_panel_lists_what_is_on_the_lan_and_what_is_only_reachable_through_the_relay() {
        let m = store();
        // 配对过两台：一台现在就在局域网里，另一台只能经中继
        m.upsert(machine("lan-1", "书房的", false));
        m.upsert(machine("relay-1", "家里的", true));
        // 局域网里还看到一台没配对的
        m.lan_found("new-1", "客厅的", "192.168.0.9", 8787);
        m.lan_found("lan-1", "书房的", "192.168.0.5", 8787);

        let panel = m.panel();
        let ids: Vec<&str> = panel.iter().map(|p| p.id.as_str()).collect();
        // 按名字排（书 < 客 < 家，按码点），不管谁先被看到、走局域网还是中继
        assert_eq!(ids, vec!["lan-1", "new-1", "relay-1"]);
        let by = |id: &str| panel.iter().find(|p| p.id == id).unwrap().clone();
        assert!(!by("new-1").paired && by("new-1").host == "192.168.0.9");
        assert!(by("lan-1").paired && by("lan-1").port == 8787);
        assert!(by("relay-1").relay && by("relay-1").host.is_empty());
        // 局域网里看到的地址记到了配对过的那台头上（选路要用）
        assert_eq!(
            m.get("lan-1").unwrap().lan.as_deref(),
            Some("192.168.0.5:8787")
        );
    }

    /// 面板顺序不跟着发现顺序跳：同样几台，先看到谁都一样；一台从「经中继」变成
    /// 「局域网里看到了」也不挪位置。
    #[test]
    fn the_panel_order_does_not_depend_on_what_was_seen_first() {
        let order = |found: &[(&str, &str)]| {
            let m = store();
            m.upsert(machine("relay-1", "a-家里的", true));
            for (id, name) in found {
                m.lan_found(id, name, "192.168.0.9", 8787);
            }
            m.panel().into_iter().map(|p| p.id).collect::<Vec<_>>()
        };
        let one = order(&[("x", "c-客厅"), ("y", "b-书房")]);
        let two = order(&[("y", "b-书房"), ("x", "c-客厅")]);
        assert_eq!(one, vec!["relay-1", "y", "x"]);
        assert_eq!(one, two);
        // 经中继那台回到局域网里：还是第一个
        let three = order(&[("x", "c-客厅"), ("relay-1", "a-家里的"), ("y", "b-书房")]);
        assert_eq!(three, one);
    }

    #[test]
    fn a_machine_that_is_neither_on_the_lan_nor_behind_a_relay_is_not_listed() {
        let m = store();
        // 配对过、但现在不在局域网里、也没配中继：列出来点了也连不上
        m.upsert(machine("gone", "出门的", false));
        assert!(m.panel().is_empty());
    }

    #[test]
    fn machines_seen_on_the_lan_come_and_go_with_the_scan() {
        let m = store();
        m.lan_found("a", "甲", "192.168.0.2", 8787);
        m.lan_found("b", "乙", "192.168.0.3", 8787);
        assert_eq!(m.panel().len(), 2);
        m.lan_lost("a");
        assert_eq!(m.panel().len(), 1);
        // 一次扫描结束：没配对过的那些忘掉
        m.lan_reset();
        assert!(m.panel().is_empty());
        // 配对过的不受影响（它记在库里，是「我的机器」）
        m.upsert(machine("keep", "我的", false));
        m.lan_found("keep", "我的", "192.168.0.4", 8787);
        m.lan_reset();
        assert_eq!(m.panel().len(), 0, "看不到又不经中继：不列");
        m.lan_found("keep", "我的", "192.168.0.4", 8787);
        m.lan_reset();
        assert!(m.get("keep").is_some(), "库里的那行还在");
    }
}
