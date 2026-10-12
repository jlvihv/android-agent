//! 会话持久化。
//!
//! # 为什么用 SQLite 而不是 JSON 文件
//!
//! 一开始我写的是「把整个 SessionStore 序列化成一个 JSON，每 2 秒重写一次」。
//! 那有两个问题：
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
//! `libsqlite3.so` 可链。实测只让 .so 增加 53 KB。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::agent::{ChatMessage, ChatSession, SessionStore};

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
"#;

struct Inner {
    conn: Connection,
    /// session_id -> 上次落盘时的样子。
    /// 有它才能只追加新增的消息、只更新改过的会话，而不是每次全量重写。
    saved: HashMap<String, Saved>,
    /// 上次落盘时的活动会话
    saved_active: Option<String>,
}

/// 一个会话上次落盘时的状态。和内存里一样就整条跳过（不发任何 SQL）。
#[derive(Clone, PartialEq)]
struct Saved {
    /// 已经落盘的消息条数（水位线）
    len: usize,
    updated_at: u64,
    title: String,
    provider_session: String,
}

impl Saved {
    fn of(s: &ChatSession) -> Self {
        Self {
            len: s.messages.len(),
            updated_at: s.updated_at,
            title: s.title.clone(),
            provider_session: s.provider_session.clone(),
        }
    }
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
            }),
        })
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                conn,
                saved: HashMap::new(),
                saved_active: None,
            }),
        })
    }

    /// 读出全部会话。只在启动时调一次。
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
            let mut store = SessionStore::default();

            let mut stmt = g.conn.prepare(
                "SELECT id, title, created_at, updated_at, provider_session
                 FROM sessions ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;

            for row in rows {
                let (id, title, created_at, updated_at, provider_session) = row?;

                let mut m = g
                    .conn
                    .prepare("SELECT payload FROM messages WHERE session_id = ?1 ORDER BY seq")?;
                let messages: Vec<ChatMessage> = m
                    .query_map([&id], |r| r.get::<_, String>(0))?
                    .filter_map(|p| p.ok())
                    .filter_map(|p| serde_json::from_str(&p).ok())
                    .collect();

                // 空会话不给回来（跟 sync 的过滤对称）：库里可能还留着
                // 老版本攒下的空会话，读到内存里就只在侧边栏里占位。
                if messages.is_empty() {
                    continue;
                }

                store.sessions.push(ChatSession {
                    id,
                    title,
                    created_at: created_at.max(0) as u64,
                    updated_at: updated_at.max(0) as u64,
                    messages,
                    provider_session,
                });
            }

            store.active = g
                .conn
                .query_row("SELECT v FROM meta WHERE k = 'active'", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap_or_default();

            store
        };

        // 水位线 = 刚读出来的样子，这样下次 sync 只写新增的
        g.saved.clear();
        for s in &store.sessions {
            g.saved.insert(s.id.clone(), Saved::of(s));
        }
        g.saved_active = Some(store.active.clone());

        Ok(store)
    }

    /// 增量落盘。每 2 秒被定时器调一次（有改动时）。
    ///
    /// 只写改过的：没变的会话一条 SQL 都不发；变了的只追加
    /// `messages[水位线..]` —— 这是用 SQLite 而不是 JSON 的全部意义所在。
    /// 原来每次都给**每个**会话发一条 UPSERT，再拼一条 `NOT IN (全部 id)`，
    /// 会话一多，每 2 秒都在白写。
    pub fn sync(&self, store: &SessionStore) -> Result<()> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;

        // 空会话不落盘（也把它上次可能写进去的旧行清掉，见下面的 gone）：
        // 「新对话」点了没说话、或 `/api/reset` 清空过，重启后不该又冒出来。
        let keep: Vec<&ChatSession> = store.sessions.iter().filter(|s| s.has_content()).collect();
        let changed: Vec<&ChatSession> = keep
            .iter()
            .copied()
            .filter(|s| g.saved.get(&s.id) != Some(&Saved::of(s)))
            .collect();
        let gone: Vec<String> = g
            .saved
            .keys()
            .filter(|id| !keep.iter().any(|s| &s.id == *id))
            .cloned()
            .collect();
        let active_changed = g.saved_active.as_deref() != Some(store.active.as_str());
        if changed.is_empty() && gone.is_empty() && !active_changed {
            return Ok(());
        }

        let saved = g.saved.clone();
        let tx = g.conn.transaction()?;
        for s in &changed {
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

            let mut done = saved.get(&s.id).map(|x| x.len).unwrap_or(0);
            // 会话被清空过（/api/reset）：消息变少了。不处理的话 done 还停在
            // 旧长度，新消息的 seq 从 0 开始会被 skip(done) 全部跳过不落盘，
            // 而库里的旧消息还在 —— 重启后清掉的对话又回来了。
            // 变少只可能是整个清空过，前缀也不可信，整段重写。
            if s.messages.len() < done {
                tx.execute("DELETE FROM messages WHERE session_id = ?1", [&s.id])?;
                done = 0;
            }
            if s.messages.len() > done {
                let mut ins = tx.prepare(
                    "INSERT OR REPLACE INTO messages(session_id, seq, payload) VALUES(?1, ?2, ?3)",
                )?;
                for (i, m) in s.messages.iter().enumerate().skip(done) {
                    ins.execute(params![s.id, i as i64, serde_json::to_string(m)?])?;
                }
            }
        }

        // 删掉的会话（以及变空的会话），连带消息一起清
        for id in &gone {
            tx.execute("DELETE FROM messages WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        }

        if active_changed {
            tx.execute(
                "INSERT INTO meta(k, v) VALUES('active', ?1)
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                [&store.active],
            )?;
        }
        tx.commit()?;

        for s in &changed {
            g.saved.insert(s.id.clone(), Saved::of(s));
        }
        for id in &gone {
            g.saved.remove(id);
        }
        g.saved_active = Some(store.active.clone());
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
        g.saved.clear();
        g.saved_active = None;
        Ok(())
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn round_trips_sessions_and_messages() {
        let db = Db::in_memory().unwrap();
        let st = store_with(2, 3);
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions.len(), 2);
        assert_eq!(back.sessions[0].messages.len(), 3);
        assert_eq!(back.active, st.active);
        assert_eq!(back.sessions[0].title, "会话0");
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
            } => {
                assert_eq!(content, "回答");
                assert_eq!(reasoning, "思考");
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
    /// 「新对话」连点几次就会攼出一堆这种会话，重启后还在抽屉里占位 —— 踩过。
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
