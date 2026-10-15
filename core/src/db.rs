//! 会话持久化。
//!
//! # 为什么用 SQLite 而不是 JSON 文件
//!
//! 把整个 `SessionStore` 序列化成一个 JSON、每 2 秒重写一次的话，有两个问题：
//!
//! 1. **全量重写**。一次 agent 会话 100 条消息、每条平均 2KB（`bash` 输出
//!    很大）就是 200KB；10 个会话 2MB。流式输出时每 2 秒重写 2MB，
//!    相当于持续 ~1MB/s 的闪存写入。手机闪存会磨损，而且卡。
//! 2. **崩溃丢全部**。虽然用了「写 .tmp 再 rename」保证不产生半截文件，
//!    但一次都没写成功的话，所有会话一起没。
//!
//! SQLite 的追加是一条 `INSERT`，成本 O(新增)。而且我们运气好：
//! **消息是只追加不改的**（assistant 消息在整轮完成后才 push，工具结果
//! 也是追加），所以一个 `saved_count` 水位线就能做增量落盘。
//!
//! `bundled` feature 把 SQLite 源码一起编进来 —— Android 上没有
//! `libsqlite3.so` 可链（`bundled` 让 .so 大约多 53 KB）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::agent::{ChatMessage, ChatSession, SessionStore, Stub};

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id               TEXT PRIMARY KEY,
  title            TEXT NOT NULL,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  provider_session TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  payload    TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);

-- 配置按列存，不往一个值里塞 JSON。`meta` 只放单个的值（instance_id、token……）。

-- 当前在用的 LLM（只有一行）
CREATE TABLE IF NOT EXISTS llm (
  id               INTEGER PRIMARY KEY CHECK (id = 1),
  name             TEXT NOT NULL,
  base_url         TEXT NOT NULL,
  model            TEXT NOT NULL,
  api_key          TEXT NOT NULL,
  supports_images  INTEGER NOT NULL,
  supports_files   INTEGER NOT NULL,
  api_style        TEXT NOT NULL,
  reasoning_effort TEXT
);

-- 各家服务商的 Key（base_url 只有「其他」那家用）
CREATE TABLE IF NOT EXISTS providers (
  id       TEXT PRIMARY KEY,
  api_key  TEXT NOT NULL,
  base_url TEXT NOT NULL
);

-- 这些端点不支持 Responses API（`base_url|model`），直接走 Chat Completions
CREATE TABLE IF NOT EXISTS responses_unsupported (
  endpoint TEXT PRIMARY KEY
);

-- 手机：配对过的电脑（见 machines.rs）
CREATE TABLE IF NOT EXISTS machines (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  token       TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  relay_addr  TEXT,
  relay_key   TEXT,
  lan         TEXT
);

-- 电脑：手机交来的中继（只有一行，见 remote.rs）
CREATE TABLE IF NOT EXISTS relay (
  id   INTEGER PRIMARY KEY CHECK (id = 1),
  addr TEXT NOT NULL,
  key  TEXT NOT NULL
);

-- 每次 LLM 调用的用量（一步一行，见 agent.rs 的 `stream_one`）。
--
-- 只追加：删会话、清空会话都**不删**这些行（拿来做分析的 —— 花在哪、缓存命中多少）。
-- 清空会话只把它的行标成 `cleared = 1`：输入框那个上下文圆环只算没清掉的那些。
-- `cached` / `reasoning` 是 NULL = 服务商没报这个字段（和 0 不是一回事）。
CREATE TABLE IF NOT EXISTS usage (
  id         INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  at         INTEGER NOT NULL,   -- 这一步结束的时刻（毫秒）
  provider   TEXT NOT NULL,
  model      TEXT NOT NULL,
  input      INTEGER NOT NULL,   -- prompt_tokens
  output     INTEGER NOT NULL,   -- completion_tokens
  cached     INTEGER,
  reasoning  INTEGER,
  finish     TEXT,               -- 模型报的结束原因（stop / tool_calls / length……）
  ms         INTEGER NOT NULL,   -- 这一步从发请求到流完花了多久（含重试）
  cleared    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id, id);

-- 远端资源的缓存（models.dev 的模型目录）：原样存下响应和抓取时刻
CREATE TABLE IF NOT EXISTS http_cache (
  url        TEXT PRIMARY KEY,
  body       TEXT NOT NULL,
  fetched_at INTEGER NOT NULL
);
"#;

/// 当前在用的 LLM（`llm` 表那一行）。
#[derive(Clone, Debug, serde::Deserialize)]
pub struct LlmConfig {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    #[serde(default = "crate::llm::default_supports_images")]
    pub supports_images: bool,
    #[serde(default)]
    pub supports_files: bool,
    #[serde(default)]
    pub api_style: crate::llm::ApiStyle,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// `usage` 表的一行（一次 LLM 调用）。
#[derive(Clone, Debug, Default)]
pub struct UsageRow {
    pub session_id: String,
    /// 毫秒
    pub at: u64,
    pub provider: String,
    pub model: String,
    pub input: u32,
    pub output: u32,
    pub cached: Option<u32>,
    pub reasoning: Option<u32>,
    pub finish: Option<String>,
    pub ms: u64,
}

struct Inner {
    conn: Connection,
    /// session_id -> 上次落盘时的样子。
    /// 有它才能只追加新增的消息、只更新改过的会话，而不是每次全量重写。
    saved: HashMap<String, Saved>,
    /// 上次落盘时的活动会话
    saved_active: Option<String>,
    /// 水位线被整个换掉的次数（`load_sessions` / `wipe`）。`plan` 记下它，`write`
    /// 时对不上就放弃这一份：那是对着旧水位线算出来的。
    generation: u64,
}

/// 一个会话上次落盘时的状态。和内存里一样就整条跳过（不发任何 SQL）。
#[derive(Clone, PartialEq)]
struct Saved {
    /// 已经落盘的消息条数（水位线）
    len: usize,
    /// 落盘时的清空次数（`ChatSession::epoch`）。变了 = 整段换过
    epoch: u32,
    updated_at: u64,
    title: String,
    provider_session: String,
}

impl Saved {
    fn of(s: &ChatSession) -> Self {
        Self {
            len: s.message_count(),
            epoch: s.epoch,
            updated_at: s.updated_at,
            title: s.title.clone(),
            provider_session: s.provider_session.clone(),
        }
    }
}

/// 一次落盘要写的东西（`Db::plan` 算、`Db::write` 写）。
pub struct SyncPlan {
    generation: u64,
    sessions: Vec<PlannedSession>,
    gone: Vec<String>,
    /// 活动会话变了才有
    active: Option<String>,
}

struct PlannedSession {
    id: String,
    title: String,
    created_at: u64,
    updated_at: u64,
    provider_session: String,
    /// 消息不在内存里的会话只更新会话头：`None`
    messages: Option<PlannedMessages>,
    /// 写成之后的水位线
    saved: Saved,
}

struct PlannedMessages {
    /// 先把这个会话库里的消息全删了（清空过）
    rewrite: bool,
    /// `rows[0]` 的 seq
    from: usize,
    rows: Vec<String>,
}

pub struct Db {
    inner: Mutex<Inner>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("打开 {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("建表")?;
        Ok(Self {
            inner: Mutex::new(Inner {
                conn,
                saved: HashMap::new(),
                saved_active: None,
                generation: 0,
            }),
        })
    }

    /// 内存里的库（测试用；也给没有数据目录的场合）。
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                conn,
                saved: HashMap::new(),
                saved_active: None,
                generation: 0,
            }),
        })
    }

    /// 启动时读会话：**全部会话的元数据 + 活动会话的消息**。只在启动时调一次。
    ///
    /// 别的会话只留摘要（`ChatSession::stub`），要用时 `load_messages` 再读 ——
    /// 全读进来的话，历史一多就是几十 MB 常驻内存，而大部分会话再也不会打开。
    pub fn load(&self) -> Result<SessionStore> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;

        // 查询期间只借用 &*g；结束后才动 g.saved。
        // 不分开的话「借用 (*g).conn」和「可变借用 (*g).saved」会打架。
        // 增量落盘只认识读进内存的会话：库里那些不会被读出来的行（空会话、
        // 没有会话头的孤儿消息）以后就没人删了，读之前先清掉。
        g.conn.execute_batch(
            "DELETE FROM sessions WHERE id NOT IN (SELECT DISTINCT session_id FROM messages);
             DELETE FROM messages WHERE session_id NOT IN (SELECT id FROM sessions);",
        )?;

        let store = {
            let g = &*g;
            let active: String = g
                .conn
                .query_row("SELECT v FROM meta WHERE k = 'active'", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap_or_default();
            let mut store = SessionStore {
                active,
                ..Default::default()
            };

            // 用户消息条数靠 payload 的开头认：`ChatMessage` 是外部标签的枚举，
            // 用户消息序列化出来是 `{"User":…` 或 `{"UserFiles":…`（有测试盯着）。
            let mut stmt = g.conn.prepare(
                r#"SELECT s.id, s.title, s.created_at, s.updated_at, s.provider_session,
                          COUNT(m.seq), COALESCE(SUM(m.payload LIKE '{"User%'), 0)
                   FROM sessions s JOIN messages m ON m.session_id = s.id
                   GROUP BY s.id
                   ORDER BY s.updated_at DESC"#,
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?;

            for row in rows {
                let (id, title, created_at, updated_at, provider_session, len, users) = row?;
                let mut s = ChatSession {
                    id,
                    title,
                    created_at: created_at.max(0) as u64,
                    updated_at: updated_at.max(0) as u64,
                    messages: Vec::new(),
                    provider_session,
                    stub: Some(Stub {
                        len: len.max(0) as usize,
                        users: users.max(0) as usize,
                    }),
                    loaded_at: None,
                    epoch: 0,
                };
                // 活动会话一打开就要看，直接读进来；读不出来就让它留着摘要
                // （库里的行原样不动，换个能读的版本还能看到）
                if s.id == store.active {
                    match Self::messages_of(&g.conn, &s.id) {
                        Ok(m) => s.install(m),
                        Err(e) => tracing::warn!("会话 {} 读不出来: {e:#}", s.id),
                    }
                }
                store.sessions.push(s);
            }
            store
        };

        // 水位线 = 刚读出来的样子，这样下次 sync 只写新增的
        g.generation += 1;
        g.saved.clear();
        for s in &store.sessions {
            g.saved.insert(s.id.clone(), Saved::of(s));
        }
        g.saved_active = Some(store.active.clone());

        Ok(store)
    }

    /// 读一个会话的全部消息（按需加载用）。
    ///
    /// **任何一条解析不了就整个报错**，不跳过：跳过的话内存里比库里少，水位线
    /// 跟着变小，下次落盘从那个位置 `INSERT OR REPLACE`，就把库里后面的消息
    /// 覆盖了。典型场景是装回旧版本 —— 新版本写的变体旧版本认不出来。
    pub fn load_messages(&self, id: &str) -> Result<Vec<ChatMessage>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        Self::messages_of(&g.conn, id)
    }

    fn messages_of(conn: &Connection, id: &str) -> Result<Vec<ChatMessage>> {
        let mut stmt =
            conn.prepare("SELECT seq, payload FROM messages WHERE session_id = ?1 ORDER BY seq")?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, payload) = row?;
            let m = serde_json::from_str(&payload).with_context(|| {
                format!("第 {seq} 条消息解析不了（可能是新版本写的，当前版本不认识）")
            })?;
            out.push(m);
        }
        Ok(out)
    }

    /// 库里所有**带文件**的消息（附件、工具返回的图片），给启动清扫算「哪些
    /// 文件还有人引用」（见 `gc`）。
    ///
    /// 只挑可能带文件的那几行来解析，不把整个库读一遍：`UserFiles`，以及
    /// `images` 非空的 `Tool`（`Tool` 的字段按 `tool_call_id, content, images`
    /// 的顺序序列化，没图的那行一定以 `"images":[]}}` 结尾，有测试盯着）。
    /// 任何一行解析不了就整个报错 —— 调用方据此**不清扫**，宁可留着。
    pub fn messages_with_files(&self) -> Result<Vec<ChatMessage>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        let mut stmt = g.conn.prepare(
            r#"SELECT payload FROM messages
               WHERE payload LIKE '{"UserFiles"%'
                  OR (payload LIKE '{"Tool"%' AND payload NOT LIKE '%"images":[]}}')"#,
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?).context("消息解析不了")?);
        }
        Ok(out)
    }

    /// 这个会话内存里的样子是不是都已经落盘了（放掉消息之前要确认这一点）。
    pub fn is_synced(&self, s: &ChatSession) -> bool {
        self.inner
            .lock()
            .map(|g| g.saved.get(&s.id) == Some(&Saved::of(s)))
            .unwrap_or(false)
    }

    /// 增量落盘。每 2 秒被定时器调一次（有改动时）。
    ///
    /// 只写改过的：没变的会话一条 SQL 都不发；变了的只追加
    /// `messages[水位线..]` —— 这是用 SQLite 而不是 JSON 的全部意义所在。
    /// 给**每个**会话都发一条 UPSERT、再拼一条 `NOT IN (全部 id)` 的话，
    /// 会话一多就是每 2 秒白写一遍。
    ///
    /// 分两步（`plan` + `write`）：调用方拿着会话的读锁时只做 `plan`（比对 + 序列化），
    /// 放开锁再 `write`。写盘（事务 + fsync，手机闪存上可能几十毫秒）期间回合照样能
    /// 往会话里追加 —— 不然每 2 秒回合就要等一次盘。
    pub fn sync(&self, store: &SessionStore) -> Result<()> {
        match self.plan(store)? {
            Some(p) => self.write(p),
            None => Ok(()),
        }
    }

    /// 算出这一次要写什么（不碰盘）。没什么要写的是 `None`。
    pub fn plan(&self, store: &SessionStore) -> Result<Option<SyncPlan>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;

        // 空会话不落盘（也把它上次可能写进去的旧行清掉，见下面的 gone）：
        // 「新对话」点了没说话、或 `/api/reset` 清空过，重启后不该又冒出来。
        let keep: Vec<&ChatSession> = store.sessions.iter().filter(|s| s.has_content()).collect();
        let gone: Vec<String> = g
            .saved
            .keys()
            .filter(|id| !keep.iter().any(|s| &s.id == *id))
            .cloned()
            .collect();
        let active = (g.saved_active.as_deref() != Some(store.active.as_str()))
            .then(|| store.active.clone());

        let mut sessions = Vec::new();
        for s in keep {
            let now = Saved::of(s);
            let prev = g.saved.get(&s.id);
            if prev == Some(&now) {
                continue;
            }
            // 会话被清空过（/api/reset）：消息变少了。不处理的话水位线还停在
            // 旧长度，新消息的 seq 从 0 开始会被跳过不落盘，而库里的旧消息还在 ——
            // 重启后清掉的对话又回来了。变少只可能是整个清空过，前缀也不可信，整段重写。
            //
            // 消息不在内存里的会话只可能改了会话头（改名、换 provider session），
            // 消息一条都不能动 —— 空的 `messages` 在这里会被当成「清空过」。
            let messages = if s.resident() {
                let mut from = prev.map(|x| x.len).unwrap_or(0);
                let rewrite = s.messages.len() < from || prev.is_some_and(|x| x.epoch != s.epoch);
                if rewrite {
                    from = 0;
                }
                let rows = s.messages[from.min(s.messages.len())..]
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<serde_json::Result<Vec<_>>>()?;
                Some(PlannedMessages {
                    rewrite,
                    from,
                    rows,
                })
            } else {
                None
            };
            sessions.push(PlannedSession {
                id: s.id.clone(),
                title: s.title.clone(),
                created_at: s.created_at,
                updated_at: s.updated_at,
                provider_session: s.provider_session.clone(),
                messages,
                saved: now,
            });
        }

        if sessions.is_empty() && gone.is_empty() && active.is_none() {
            return Ok(None);
        }
        Ok(Some(SyncPlan {
            generation: g.generation,
            sessions,
            gone,
            active,
        }))
    }

    /// 把 `plan` 算好的写进库，水位线记成**算计划那一刻**的样子（不是现在内存里的）。
    pub fn write(&self, plan: SyncPlan) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        anyhow::ensure!(
            g.generation == plan.generation,
            "算计划之后库被重置过，这一份作废（下次重来）"
        );

        let tx = g.conn.transaction()?;
        for s in &plan.sessions {
            tx.execute(
                "INSERT INTO sessions(id, title, created_at, updated_at, provider_session)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                   title = excluded.title,
                   updated_at = excluded.updated_at,
                   provider_session = excluded.provider_session",
                params![
                    s.id,
                    s.title,
                    s.created_at as i64,
                    s.updated_at as i64,
                    s.provider_session
                ],
            )?;
            let Some(m) = &s.messages else { continue };
            if m.rewrite {
                tx.execute("DELETE FROM messages WHERE session_id = ?1", [&s.id])?;
            }
            if !m.rows.is_empty() {
                let mut ins = tx.prepare(
                    "INSERT OR REPLACE INTO messages(session_id, seq, payload) VALUES(?1, ?2, ?3)",
                )?;
                for (i, row) in m.rows.iter().enumerate() {
                    ins.execute(params![s.id, (m.from + i) as i64, row])?;
                }
            }
        }

        // 删掉的会话（以及变空的会话），连带消息一起清
        for id in &plan.gone {
            tx.execute("DELETE FROM messages WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        }

        if let Some(active) = &plan.active {
            tx.execute(
                "INSERT INTO meta(k, v) VALUES('active', ?1)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                [active],
            )?;
        }
        tx.commit()?;

        for s in plan.sessions {
            g.saved.insert(s.id, s.saved);
        }
        for id in &plan.gone {
            g.saved.remove(id);
        }
        if let Some(active) = plan.active {
            g.saved_active = Some(active);
        }
        Ok(())
    }

    /// 读一条设置（`meta` 表）。provider 配置、访问凭据都在这里。
    pub fn get_meta(&self, k: &str) -> Result<Option<String>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        let v = g
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| {
                r.get::<_, String>(0)
            });
        match v {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 删一条设置。
    pub fn delete_meta(&self, k: &str) -> Result<()> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        g.conn.execute("DELETE FROM meta WHERE k = ?1", [k])?;
        Ok(())
    }

    /// 写一条设置。单条 UPSERT，原子的。
    pub fn set_meta(&self, k: &str, v: &str) -> Result<()> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        g.conn.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            [k, v],
        )?;
        Ok(())
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        Ok(f(&g.conn)?)
    }

    // —— 配置（按列存，见 SCHEMA）——

    pub fn llm(&self) -> Result<Option<LlmConfig>> {
        self.with(|c| {
            let r = c.query_row(
                "SELECT name, base_url, model, api_key, supports_images, supports_files, api_style, reasoning_effort FROM llm WHERE id = 1",
                [],
                |r| {
                    Ok(LlmConfig {
                        name: r.get(0)?,
                        base_url: r.get(1)?,
                        model: r.get(2)?,
                        api_key: r.get(3)?,
                        supports_images: r.get(4)?,
                        supports_files: r.get(5)?,
                        api_style: crate::llm::ApiStyle::parse(&r.get::<_, String>(6)?),
                        reasoning_effort: r.get(7)?,
                    })
                },
            );
            match r {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    pub fn set_llm(&self, l: Option<&LlmConfig>) -> Result<()> {
        self.with(|c| match l {
            None => c.execute("DELETE FROM llm", []).map(|_| ()),
            Some(l) => c
                .execute(
                    "INSERT OR REPLACE INTO llm(id, name, base_url, model, api_key, supports_images, supports_files, api_style, reasoning_effort) VALUES(1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        l.name,
                        l.base_url,
                        l.model,
                        l.api_key,
                        l.supports_images,
                        l.supports_files,
                        l.api_style.as_str(),
                        l.reasoning_effort
                    ],
                )
                .map(|_| ()),
        })
    }

    pub fn providers(&self) -> Result<crate::providers::Store> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, api_key, base_url FROM providers")?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    crate::providers::Creds {
                        api_key: r.get(1)?,
                        base_url: r.get(2)?,
                    },
                ))
            })?;
            rows.collect()
        })
    }

    /// 存 / 删一家的 Key（`None` = 删）。
    pub fn set_provider(&self, id: &str, creds: Option<&crate::providers::Creds>) -> Result<()> {
        self.with(|c| match creds {
            None => c
                .execute("DELETE FROM providers WHERE id = ?1", [id])
                .map(|_| ()),
            Some(k) => c
                .execute(
                    "INSERT OR REPLACE INTO providers(id, api_key, base_url) VALUES(?1, ?2, ?3)",
                    params![id, k.api_key, k.base_url],
                )
                .map(|_| ()),
        })
    }

    pub fn responses_unsupported(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT endpoint FROM responses_unsupported")?;
            let rows = st.query_map([], |r| r.get(0))?;
            rows.collect()
        })
    }

    pub fn add_responses_unsupported(&self, endpoint: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO responses_unsupported(endpoint) VALUES(?1)",
                [endpoint],
            )
            .map(|_| ())
        })
    }

    pub fn machines(&self) -> Result<Vec<crate::machines::Machine>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, name, token, fingerprint, relay_addr, relay_key, lan FROM machines ORDER BY rowid",
            )?;
            let rows = st.query_map([], |r| {
                let addr: Option<String> = r.get(4)?;
                let key: Option<String> = r.get(5)?;
                Ok(crate::machines::Machine {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    token: r.get(2)?,
                    fingerprint: r.get(3)?,
                    relay: addr
                        .zip(key)
                        .map(|(addr, key)| crate::state::RelayAddr { addr, key }),
                    lan: r.get(6)?,
                })
            })?;
            rows.collect()
        })
    }

    pub fn put_machine(&self, m: &crate::machines::Machine) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO machines(id, name, token, fingerprint, relay_addr, relay_key, lan) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, token = excluded.token, fingerprint = excluded.fingerprint,
                   relay_addr = excluded.relay_addr, relay_key = excluded.relay_key, lan = excluded.lan",
                params![
                    m.id,
                    m.name,
                    m.token,
                    m.fingerprint,
                    m.relay.as_ref().map(|r| &r.addr),
                    m.relay.as_ref().map(|r| &r.key),
                    m.lan
                ],
            )
            .map(|_| ())
        })
    }

    pub fn delete_machine(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM machines WHERE id = ?1", [id])
                .map(|_| ())
        })
    }

    pub fn relay(&self) -> Result<Option<crate::state::RelayAddr>> {
        self.with(|c| {
            let r = c.query_row("SELECT addr, key FROM relay WHERE id = 1", [], |r| {
                Ok(crate::state::RelayAddr {
                    addr: r.get(0)?,
                    key: r.get(1)?,
                })
            });
            match r {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    pub fn set_relay(&self, r: &crate::state::RelayAddr) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO relay(id, addr, key) VALUES(1, ?1, ?2)",
                [&r.addr, &r.key],
            )
            .map(|_| ())
        })
    }

    // —— 用量（见 SCHEMA 里 `usage` 那张表）——

    pub fn add_usage(&self, r: &UsageRow) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO usage(session_id, at, provider, model, input, output, cached, reasoning, finish, ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    r.session_id,
                    r.at as i64,
                    r.provider,
                    r.model,
                    r.input,
                    r.output,
                    r.cached,
                    r.reasoning,
                    r.finish,
                    r.ms as i64
                ],
            )
            .map(|_| ())
        })
    }

    /// 这个会话（上次清空之后）的用量：累计 + 最近那一次。一次都没有是 `None`。
    pub fn session_usage(&self, sid: &str) -> Result<Option<crate::state::UsageStats>> {
        self.with(|c| {
            let (calls, input, output, cached): (i64, i64, i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(input), 0), COALESCE(SUM(output), 0), COALESCE(SUM(cached), 0)
                 FROM usage WHERE session_id = ?1 AND cleared = 0",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            if calls == 0 {
                return Ok(None);
            }
            let last = c.query_row(
                "SELECT input, output, cached, reasoning FROM usage
                 WHERE session_id = ?1 AND cleared = 0 ORDER BY id DESC LIMIT 1",
                [sid],
                |r| {
                    let input: u32 = r.get(0)?;
                    let output: u32 = r.get(1)?;
                    let cached: Option<u32> = r.get(2)?;
                    let reasoning: Option<u32> = r.get(3)?;
                    Ok(crate::openai::Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        prompt_tokens_details: cached.map(|n| crate::openai::PromptTokensDetails {
                            cached_tokens: Some(n),
                        }),
                        completion_tokens_details: reasoning.map(|n| {
                            crate::openai::CompletionTokensDetails {
                                reasoning_tokens: Some(n),
                            }
                        }),
                        ..Default::default()
                    })
                },
            )?;
            Ok(Some(crate::state::UsageStats {
                last: Some(last),
                input: input as u64,
                output: output as u64,
                cached: cached as u64,
                calls: calls as u64,
            }))
        })
    }

    /// 会话清空了：它之前的用量不再算进「当前上下文」（行留着，见 SCHEMA）。
    pub fn clear_usage(&self, sid: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE usage SET cleared = 1 WHERE session_id = ?1 AND cleared = 0",
                [sid],
            )
            .map(|_| ())
        })
    }

    /// 缓存的响应，和抓取时刻（秒）。
    pub fn cached(&self, url: &str) -> Result<Option<(String, u64)>> {
        self.with(|c| {
            let r = c.query_row(
                "SELECT body, fetched_at FROM http_cache WHERE url = ?1",
                [url],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            );
            match r {
                Ok(v) => Ok(Some(v)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    pub fn put_cached(&self, url: &str, body: &str, fetched_at: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO http_cache(url, body, fetched_at) VALUES(?1, ?2, ?3)",
                params![url, body, fetched_at as i64],
            )
            .map(|_| ())
        })
    }

    /// 清空全部会话（`/api/reset` 之外的重置用）
    ///
    /// 只清会话：`meta` 里还有 provider 配置和访问凭据，不能跟着没了。
    pub fn wipe(&self) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        g.conn.execute_batch(
            "DELETE FROM sessions; DELETE FROM messages; DELETE FROM meta WHERE k = 'active';",
        )?;
        g.generation += 1;
        g.saved.clear();
        g.saved_active = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 用量是拿来分析的：清空会话只是标记，删会话也不动它 —— 行一直在。
    #[test]
    fn usage_rows_are_kept_when_a_session_is_cleared() {
        let db = Db::in_memory().unwrap();
        let row = |input| UsageRow {
            session_id: "s".into(),
            at: 1,
            provider: "opencode-go".into(),
            model: "deepseek-flash".into(),
            input,
            output: 7,
            cached: Some(input / 2),
            reasoning: None,
            finish: Some("stop".into()),
            ms: 1200,
        };
        db.add_usage(&row(100)).unwrap();
        db.add_usage(&row(300)).unwrap();
        let u = db.session_usage("s").unwrap().unwrap();
        assert_eq!((u.input, u.output, u.cached, u.calls), (400, 14, 200, 2));
        let last = u.last.unwrap();
        assert_eq!(last.prompt_tokens, 300);
        assert_eq!(last.cached_tokens(), Some(150));
        assert_eq!(last.reasoning_tokens(), None, "没报的还是没报，不变成 0");

        db.clear_usage("s").unwrap();
        assert!(db.session_usage("s").unwrap().is_none());
        let rows: i64 = db
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM usage WHERE session_id = 's'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(rows, 2, "行还在");
    }
    use crate::agent::RecordedToolCall;

    /// 增量落盘：只改了标题（重命名不动 updated_at）也得写进去；
    /// 删掉的会话要从库里清掉；清会话不能把 meta 里的配置一起清了。
    #[test]
    fn incremental_sync_catches_renames_deletes_and_keeps_settings() {
        let db = Db::in_memory().unwrap();
        db.set_meta("llm", "{}").unwrap();
        let mut st = store_with(2, 3);
        db.sync(&st).unwrap();

        let (a, b) = (st.sessions[0].id.clone(), st.sessions[1].id.clone());
        st.get_mut(&a).unwrap().title = "改过的名字".into();
        st.delete(&b);
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions.len(), 1);
        assert_eq!(back.sessions[0].title, "改过的名字");

        db.wipe().unwrap();
        assert!(db.load().unwrap().sessions.is_empty());
        assert_eq!(
            db.get_meta("llm").unwrap().as_deref(),
            Some("{}"),
            "provider 配置不该跟着会话一起清掉"
        );
    }

    fn msg_user(t: &str) -> ChatMessage {
        ChatMessage::User(t.into())
    }

    fn store_with(n_sessions: usize, msgs: usize) -> SessionStore {
        let mut st = SessionStore::default();
        for i in 0..n_sessions {
            let id = st.create();
            let s = st.sessions.iter_mut().find(|s| s.id == id).unwrap();
            s.title = format!("会话{i}");
            for j in 0..msgs {
                s.messages.push(msg_user(&format!("第{j}条")));
            }
        }
        st
    }

    /// 落盘分两步（算计划时拿着会话锁，写盘时不拿）：计划之后又追加的消息不能丢，
    /// 也不能被当成「已经落盘了」—— 不然放掉内存（`is_synced`）时就真丢了。
    #[test]
    fn messages_added_after_planning_are_written_next_time() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 2);
        let plan = db.plan(&st).unwrap().unwrap();
        st.sessions[0].messages.push(msg_user("计划之后来的"));
        db.write(plan).unwrap();
        assert!(
            !db.is_synced(&st.sessions[0]),
            "水位线是计划那一刻的，不是现在的"
        );

        db.sync(&st).unwrap();
        assert!(db.is_synced(&st.sessions[0]));
        let id = st.sessions[0].id.clone();
        assert_eq!(db.load_messages(&id).unwrap().len(), 3);
    }

    /// 计划和写盘之间库被清空过：那份计划是对着旧水位线算的，写进去就把清掉的又写回来了。
    #[test]
    fn a_plan_made_before_a_wipe_is_discarded() {
        let db = Db::in_memory().unwrap();
        let st = store_with(1, 2);
        let plan = db.plan(&st).unwrap().unwrap();
        db.wipe().unwrap();
        assert!(db.write(plan).is_err());
        assert!(db.load().unwrap().sessions.is_empty());
    }

    #[test]
    fn round_trips_sessions_and_messages() {
        let db = Db::in_memory().unwrap();
        let st = store_with(2, 3);
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.active, st.active);
        let s0 = back.sessions.iter().find(|s| s.title == "会话0").unwrap();
        assert_eq!(db.load_messages(&s0.id).unwrap().len(), 3);
    }

    /// 启动只把活动会话的消息读进内存；别的只留摘要（条数、用户消息数），
    /// 要用时 `load_messages` 读。
    #[test]
    fn only_the_active_session_is_loaded_eagerly() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(2, 3);
        // 用户消息计数要把带附件的也算上
        st.sessions[0].messages.push(ChatMessage::UserFiles {
            text: "看图".into(),
            attachments: vec![],
        });
        st.sessions[0].messages.push(ChatMessage::Assistant {
            content: "{\"User\":假的".into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_items: vec![],
        });
        let active = st.sessions[1].id.clone();
        st.activate(&active);
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        let a = back.get(&active).unwrap();
        assert!(a.resident());
        assert_eq!(a.messages.len(), 3);

        let other = back.sessions.iter().find(|s| s.id != active).unwrap();
        assert!(!other.resident());
        assert!(other.messages.is_empty());
        assert_eq!((other.message_count(), other.user_count()), (5, 4));
        assert_eq!(db.load_messages(&other.id).unwrap().len(), 5);
    }

    /// 消息不在内存里的会话只能改会话头：改名要落盘，消息一条都不能动
    /// （空的 `messages` 绝不能被当成「清空过」）。
    #[test]
    fn syncing_an_evicted_session_never_touches_its_messages() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(2, 3);
        db.sync(&st).unwrap();

        let id = st.sessions[0].id.clone();
        st.sessions[0].evict();
        assert!(db.is_synced(&st.sessions[0]), "放掉消息不算改动");
        st.sessions[0].title = "改名".into();
        st.sessions[0].rotate_provider_session();
        assert!(!db.is_synced(&st.sessions[0]));
        db.sync(&st).unwrap();

        assert_eq!(db.load_messages(&id).unwrap().len(), 3);
        let back = db.load().unwrap();
        assert_eq!(back.get(&id).unwrap().title, "改名");
    }

    /// 解析不了的消息（装回旧版本时，新版本写的变体它不认识）：整个会话
    /// 报错，而不是悄悄跳过 —— 跳过会让水位线变小，下次落盘把后面的覆盖掉。
    #[test]
    fn an_unreadable_message_fails_the_session_instead_of_being_skipped() {
        let db = Db::in_memory().unwrap();
        let st = store_with(1, 3);
        let id = st.sessions[0].id.clone();
        db.sync(&st).unwrap();
        db.inner
            .lock()
            .unwrap()
            .conn
            .execute(
                "UPDATE messages SET payload = '{\"FromTheFuture\":1}' WHERE seq = 1",
                [],
            )
            .unwrap();

        assert!(db.load_messages(&id).is_err());

        // 活动会话读不出来：留着摘要，落盘不碰它的行
        let back = db.load().unwrap();
        let s = back.get(&id).unwrap();
        assert!(!s.resident());
        assert_eq!(s.message_count(), 3);
        db.sync(&back).unwrap();
        let rows: i64 = db
            .inner
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 3);
    }

    /// 清空之后两秒内又长过了旧长度：只看条数的话看不出来清空过，
    /// 旧消息会留在库里。
    #[test]
    fn clearing_then_growing_past_the_old_length_drops_the_old_rows() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 2);
        let id = st.sessions[0].id.clone();
        db.sync(&st).unwrap();

        st.sessions[0].clear();
        for i in 0..3 {
            st.sessions[0].messages.push(msg_user(&format!("新{i}")));
        }
        db.sync(&st).unwrap();

        let msgs = db.load_messages(&id).unwrap();
        let texts: Vec<&str> = msgs.iter().filter_map(|m| m.user_text()).collect();
        assert_eq!(texts, vec!["新0", "新1", "新2"]);
    }

    /// 启动清扫用的那条查询：只挑带文件的行，不把整个库读一遍。
    #[test]
    fn messages_with_files_picks_only_rows_that_carry_files() {
        use crate::agent::{Attachment, AttachmentKind, ImagePart};
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        let id = st.create();
        let s = st.get_mut(&id).unwrap();
        s.messages = vec![
            msg_user("hi"),
            ChatMessage::UserFiles {
                text: String::new(),
                attachments: vec![Attachment {
                    id: "a.pdf".into(),
                    path: "/d/attachments/a.pdf".into(),
                    mime: "application/pdf".into(),
                    name: "a.pdf".into(),
                    kind: AttachmentKind::Pdf,
                }],
            },
            // 正文里恰好有那串字也不该被误挑出来
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "\"images\":[{}]".into(),
                images: vec![],
            },
            ChatMessage::Tool {
                tool_call_id: "c2".into(),
                content: "{}".into(),
                images: vec![ImagePart {
                    path: "/d/images/1.jpg".into(),
                    mime: "image/jpeg".into(),
                    source: "/root/.browser/shot-1.jpg".into(),
                    tool: "browser".into(),
                }],
            },
        ];
        db.sync(&st).unwrap();

        let got = db.messages_with_files().unwrap();
        assert_eq!(got.len(), 2);
        assert!(matches!(got[0], ChatMessage::UserFiles { .. }));
        assert!(matches!(&got[1], ChatMessage::Tool { tool_call_id, .. } if tool_call_id == "c2"));
    }

    /// `provider_session` 是缓存路由的锚点，换过了就必须落盘 ——
    /// 不然重启后又用旧 id 去命中已经作废的前缀。
    #[test]
    fn a_rotated_provider_session_is_persisted() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 1);
        db.sync(&st).unwrap();
        let before = db.load().unwrap().sessions[0].provider_session.clone();

        st.sessions[0].rotate_provider_session();
        let after = st.sessions[0].provider_session.clone();
        assert_ne!(before, after, "应该换了新 id");

        db.sync(&st).unwrap();
        assert_eq!(db.load().unwrap().sessions[0].provider_session, after);
    }

    #[test]
    fn keeps_message_order() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        let id = st.create();
        let s = st.sessions.iter_mut().find(|s| s.id == id).unwrap();
        for i in 0..50 {
            s.messages.push(msg_user(&format!("m{i}")));
        }
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        let msgs = &back.sessions[0].messages;
        for (i, m) in msgs.iter().enumerate() {
            match m {
                ChatMessage::User(t) => assert_eq!(t, &format!("m{i}")),
                _ => panic!("类型不对"),
            }
        }
    }

    #[test]
    fn clearing_a_session_does_not_resurrect_or_drop_messages() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 3);
        db.sync(&st).unwrap();

        st.sessions[0].messages.clear();
        st.sessions[0].messages.push(msg_user("清空后第一条"));
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        let msgs = &back.sessions[0].messages;
        assert_eq!(msgs.len(), 1, "旧消息不该回来");
        match &msgs[0] {
            ChatMessage::User(t) => assert_eq!(t, "清空后第一条"),
            _ => panic!("类型不对"),
        }
    }

    #[test]
    fn appends_only_new_messages() {
        // 这是用 SQLite 的全部理由：第二轮 sync 不该重写已有的消息
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 2);
        db.sync(&st).unwrap();

        st.sessions[0].messages.push(msg_user("新增"));
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions[0].messages.len(), 3);
        match &back.sessions[0].messages[2] {
            ChatMessage::User(t) => assert_eq!(t, "新增"),
            _ => panic!(),
        }
    }

    #[test]
    fn preserves_rich_message_types() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        let id = st.create();
        let s = st.sessions.iter_mut().find(|s| s.id == id).unwrap();
        s.messages = vec![
            ChatMessage::System("上下文".into()),
            ChatMessage::User("问题".into()),
            ChatMessage::Assistant {
                content: "回答".into(),
                reasoning: "思考".into(),
                tool_calls: vec![RecordedToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
                }],
                reasoning_items: vec![serde_json::json!({ "type": "reasoning", "id": "rs_1" })],
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                content: "ok".into(),
                images: vec![],
            },
        ];
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        let m = &back.sessions[0].messages;
        assert_eq!(m.len(), 4);
        match &m[2] {
            ChatMessage::Assistant {
                content,
                reasoning,
                tool_calls,
                reasoning_items,
            } => {
                assert_eq!(content, "回答");
                assert_eq!(reasoning, "思考");
                assert_eq!(reasoning_items[0]["id"], "rs_1", "思考条目原样落盘");
                assert_eq!(tool_calls[0].name, "bash");
            }
            _ => panic!("assistant 类型丢了"),
        }
    }

    #[test]
    fn deleted_sessions_are_removed_from_disk_too() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(3, 1);
        db.sync(&st).unwrap();
        assert_eq!(db.load().unwrap().sessions.len(), 3);

        let victim = st.sessions[1].id.clone();
        st.delete(&victim);
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions.len(), 2);
        assert!(!back.sessions.iter().any(|s| s.id == victim));
    }

    #[test]
    fn active_session_survives() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(3, 1);
        let target = st.sessions[2].id.clone();
        st.activate(&target);
        db.sync(&st).unwrap();

        assert_eq!(db.load().unwrap().active, target);
    }

    #[test]
    fn syncing_an_empty_store_clears_everything() {
        let db = Db::in_memory().unwrap();
        db.sync(&store_with(2, 2)).unwrap();
        db.sync(&SessionStore::default()).unwrap();
        assert_eq!(db.load().unwrap().sessions.len(), 0);
    }

    /// 空会话（建了、没说过话）不落盘。
    ///
    /// 「新对话」连点几次就会攼出一堆这种会话，重启后还在抽屉里占位。
    #[test]
    fn empty_sessions_are_not_persisted() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 1);
        db.sync(&st).unwrap();
        assert_eq!(db.load().unwrap().sessions.len(), 1);

        // 再建两条空的：不该进库
        st.create();
        st.create();
        db.sync(&st).unwrap();
        assert_eq!(db.load().unwrap().sessions.len(), 1);

        // 给其中一条写上话，它就正常落盘了
        let id = st.active.clone();
        st.get_mut(&id).unwrap().messages.push(msg_user("说话了"));
        db.sync(&st).unwrap();
        assert_eq!(db.load().unwrap().sessions.len(), 2);
    }

    /// 库里已经有的空会话（旧版本或别的客户端写的）不再恢复，也会被下次 sync 清掉。
    #[test]
    fn old_empty_sessions_are_dropped() {
        let db = Db::in_memory().unwrap();
        db.inner
            .lock()
            .unwrap()
            .conn
            .execute(
                "INSERT INTO sessions(id, title, created_at, updated_at, provider_session)
                 VALUES('legacy', '新对话', 1, 1, 'ps')",
                [],
            )
            .unwrap();

        let back = db.load().unwrap();
        assert!(back.sessions.is_empty(), "空会话不回到内存里");

        db.sync(&back).unwrap();
        let left: i64 = db
            .inner
            .lock()
            .unwrap()
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "下次 sync 把它从盘上清掉");
    }
}
