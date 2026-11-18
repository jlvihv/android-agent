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

const SCHEMA: &str = r"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
-- checkpoint 之后把 WAL 截回 1MB：默认不截，文件一直停在历史最高点（一次大写入之后
-- WAL 比主库还大，实测 8MB 对 6MB）
PRAGMA journal_size_limit = 1048576;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  id               TEXT PRIMARY KEY,
  -- 给人看的短编号（`#7`，见 agent.rs 的 `ChatSession::seq`）。
  -- **NULL = 还没发号**（这一列加进来之前建的老会话），`load()` 会补。
  -- UNIQUE：万一发号逻辑被改坏，落盘这一步就会报错，而不是静默重号。
  seq              INTEGER,
  title            TEXT NOT NULL,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  provider_session TEXT NOT NULL,
  -- worker | router（见 agent::Role）。默认 worker：老库加这一列时已有的会话全是 worker。
  role             TEXT NOT NULL DEFAULT 'worker',
  -- 下面五列以前在 `tickets` 表上（那一层已经删了，见 docs/16）。
  -- 它们是**不可变记录**，不是状态机：只记「当初要它干什么 / 谁派的 /
  -- 最后说成了什么 / 人有没有把它划掉」。没有一处代码靠它们做状态判定。
  --
  -- 为什么单独存而不只留历史里：历史会被压缩、会被 evict，而
  -- brief 是验收的合同、conclusion 是交付物 —— 两者都必须免疫压缩。
  brief            TEXT,
  dispatched_by    TEXT,
  conclusion       TEXT NOT NULL DEFAULT '',
  dispatches       INTEGER NOT NULL DEFAULT 0,
  closed_at        INTEGER NOT NULL DEFAULT 0,
  closed_by        TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS messages (
  session_id TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  payload    TEXT NOT NULL,
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
-- **`idx_sessions_seq` 不能放这儿**：它索引的 `seq` 列在老库上要等 `migrate()`
-- 才 ALTER 出来，而 `execute_batch(SCHEMA)` 在 `migrate()` **之前**跑 ——
-- 放这儿就是 `no such column: seq`，**任何**升级上来的老库都打不开
-- （实测：一个只有 id/title/... 的 162 条会话的库，`Db::open` 直接失败）。
-- 它现在由 `migrate()` 负责建（见那里）。

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

-- 每家服务商选的协议（auto | responses | chat）。不在表里 = 用预设的（providers.rs）
CREATE TABLE IF NOT EXISTS provider_api_style (
  id        TEXT PRIMARY KEY,
  api_style TEXT NOT NULL
);

-- 远程 MCP 服务（见 mcp.rs）。headers 是 JSON 对象（一般就是 Authorization）
CREATE TABLE IF NOT EXISTS mcp_servers (
  name    TEXT PRIMARY KEY,
  url     TEXT NOT NULL,
  headers TEXT NOT NULL
);

-- 停用的东西：kind = 'mcp'（服务名）| 'skill'（skill 的目录名）。不在表里 = 启用。
-- skill 是磁盘上的文件，开关只能记在这儿；MCP 跟着用同一张表
CREATE TABLE IF NOT EXISTS disabled (
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  PRIMARY KEY (kind, name)
);

-- 收藏的页面（AI 在沙箱里做的网页，抽屉里点一下直达）。
--
-- `path` 存的是**沙箱里的绝对路径**（`/workspace/report/index.html`），不带
-- `/sandbox/<钥匙>` 那一段 —— 钥匙每次运行/每台机器都不一样，存完整 URL
-- 换台机器、重开一次就废了。要打开时前端自己拼（`sandboxPath`）。
-- `title` 是收藏那一刻从页面 `<title>` 里取的；取不到就是文件名。
CREATE TABLE IF NOT EXISTS favorites (
  path       TEXT PRIMARY KEY,
  title      TEXT NOT NULL,
  created_at INTEGER NOT NULL
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
-- `cached`（缓存读取）/ `cache_write`（缓存写入）/ `reasoning` 是 NULL = 服务商
-- 没报这个字段（和 0 不是一回事）。
CREATE TABLE IF NOT EXISTS usage (
  id          INTEGER PRIMARY KEY,
  session_id  TEXT NOT NULL,
  at          INTEGER NOT NULL,   -- 这一步结束的时刻（毫秒）
  provider    TEXT NOT NULL,
  model       TEXT NOT NULL,
  input       INTEGER NOT NULL,   -- 输入总量（含缓存 = 未缓存 + 缓存读 + 缓存写）
  output      INTEGER NOT NULL,   -- completion_tokens
  cached      INTEGER,            -- 缓存读取（命中）
  cache_write INTEGER,            -- 缓存写入（创建；OpenAI 系不报）
  reasoning   INTEGER,
  finish     TEXT,               -- 模型报的结束原因（stop / tool_calls / length……）
  ms         INTEGER NOT NULL,   -- 这一步从发请求到流完花了多久（含重试）
  cleared    INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id, id);

-- 【已废弃，只读不写】工作项（Ticket）表。
--
-- 这一层已经删掉了：ticket 的字段并进了 `sessions`（见上面的五列）。
-- 表**留着不删**只为一件事：老库里的派工记录（brief / result / 谁派的）
-- 还有价值，`migrate()` 会把它们搬进 sessions。搬完之后这张表没人再读。
--
-- **不要往这里写新数据**。
--
-- 原来的说明（留档）：双层架构里**被派出去的那件事**本身。
--
-- 为什么不是「一个会话」：会话是容器 —— 没有状态、没有产出契约、没有终点，
-- 可以一直开着改十件事。Router 派完就失去了对这件事的掌控（那个活变成了
-- 「某个会话里的一段对话」）。而 Router 需要回答的是「这活干完了吗 / 结果在哪 /
-- 需要我做什么吗」，这些都得有个东西承载。
--
-- 会话仍然是**载体**（`session_id`）：真正干活的还是那条会话里的一次回合。
-- 但「这件事到哪一步了」记在这里，不记在会话上。
CREATE TABLE IF NOT EXISTS tickets (
  id            TEXT PRIMARY KEY,
  -- 一句话标题（Router 起的名，用户在列表里看到的就是它）
  title         TEXT NOT NULL,
  -- 要做什么。Worker 只拿到这一段 —— 它看不到 Router 的对话（上下文隔离）
  brief         TEXT NOT NULL,
  -- 派给哪条会话（worker）。干活的是它
  session_id    TEXT NOT NULL,
  -- 谁派的（Router 会话 id）
  router_id     TEXT NOT NULL,
  -- 状态机，见 `ticket::Stage`
  stage         TEXT NOT NULL,
  -- 最新一次汇报的正文（Worker 最后说的那句话），没汇报过是空串
  result        TEXT NOT NULL DEFAULT '',
  -- 汇报的类型：ready / blocked / failed / progress / ''（还没汇报）
  report        TEXT NOT NULL DEFAULT '',
  -- 结束的时刻；0 = 还开着
  ended_at      INTEGER NOT NULL DEFAULT 0,
  -- 结束方式：done / cancelled / ''（还开着）
  ended_by      TEXT NOT NULL DEFAULT '',
  -- 给它派过几次活（同一个工作项中途补充要求会 +1）
  dispatches    INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tickets_router ON tickets(router_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tickets_session ON tickets(session_id);

-- 【已废弃，只读不写】「这条会话现在在干哪个工作项」。
--
-- 已由 `sessions.dispatched_by` 取代。留着只为兼容老库。
--
-- 一个会话同一时刻只干一件事（回合本来就是串行的），所以主键是 session_id。
-- Worker 的回合结束时，靠它找到「这一轮该汇报给哪个工作项」。
-- 会话被派了新活就覆盖 —— 旧的那个工作项在被覆盖前已经记下过它的结果了。
CREATE TABLE IF NOT EXISTS session_ticket (
  session_id TEXT PRIMARY KEY,
  ticket_id  TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);

-- 远端资源的缓存（models.dev 的模型目录）：原样存下响应和抓取时刻
CREATE TABLE IF NOT EXISTS http_cache (
  url        TEXT PRIMARY KEY,
  body       TEXT NOT NULL,
  fetched_at INTEGER NOT NULL
);

";

/// `schedules` 表的建表语句 —— **只有这一份**。
///
/// ## 为什么单独拎出来
///
/// 它有两个使用点：`SCHEMA`（新库一次建全）和 [`migrate_schedules`]（旧形状的表
/// 改名之后要按当前形状重建）。以前这两处各抄了一份 —— 而「建表语句和迁移逻辑
/// 不同步」正是上一轮那个「升级上来库打不开」的 bug 的成因（`anchor_at` 改名的
/// 时候 `SCHEMA` 改了、迁移里那份没跟上/反过来）。同一张表的形状只能有一个真相。
///
/// **为什么不是把 `SCHEMA` 改成 `format!` 拼进去**：`SCHEMA` 是 `const`，而 `format!`
/// 不是 const fn；改成 `LazyLock<String>` 会让每次 `execute_batch` 多一次分配
/// （`Db::open` / `in_memory` 各一次，还有十几处测试），换来的只是「不用担心两处
/// 走偏」—— 而那个目的用「一个常量 + 一个建库函数」已经达到了（见 [`create_all`]）。
/// 常量拼接这条路走不通：`concat!` 只吃字面量。
const SCHEDULES_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS schedules (
  id              TEXT PRIMARY KEY,
  -- 到点投给 Router 的那句话，原文一字不改
  prompt          TEXT NOT NULL,
  -- 固定间隔（秒）。NULL = 这条是 cron
  every_secs      INTEGER CHECK (every_secs IS NULL OR (every_secs >= 60 AND every_secs <= 2592000)),
  -- cron 表达式原文（5 或 6 字段）。NULL = 这条是固定间隔
  cron            TEXT,
  -- **这一版节奏的起点**，不是「创建时刻」：改节奏时会被重置成那一刻（见 schedule.rs
  -- 的 `Schedules::edit`）。第 k 次 = `anchor_at + k*周期`。
  anchor_at       INTEGER NOT NULL,
  -- 截止此刻的触发点都处理过了（投出去，或按策略放弃）。不补跑，见 schedule.rs
  handled_through INTEGER NOT NULL,
  -- NULL = 启用中。用 NULL 而不是 0 当哨兵（和 usage.cached / machines.relay_addr 一致）
  paused_at       INTEGER,
  CHECK (handled_through >= anchor_at),
  -- **没有 `paused_at >= anchor_at` 这条**（以前有）。它的原意是「停用发生在它存在
  -- 之后」，而 anchor_at 现在**可以被 `edit` 往后挪**（改节奏 = 开一版新节奏，见
  -- schedule.rs）：一条昨天停用、今天改节奏的任务，`anchor_at` 是今天、`paused_at`
  -- 是昨天 —— 合法数据，却会被那条 CHECK 拒掉（实测：编辑停用中的任务直接 500）。
  -- 「停用时刻 >= 建的时刻」这件事不再由 anchor_at 表达（它已经不是创建时刻），
  -- 所以这条不变式没有载体，就不该留着半条。
  --
  -- `handled_through >= anchor_at` **留着**：水位线不能早于「当前这版节奏的起点」，
  -- 而 edit 是同时把两者设成 now 的，所以它恒成立。
  -- 「两种周期，恰好一种」—— 两个都空 / 两个都有 都在这里被挡掉
  CHECK ((every_secs IS NULL) <> (cron IS NULL))
)";

/// 建一张库的**全部**表（新库 / 内存库都走它）。
///
/// `SCHEMA` + `SCHEDULES_TABLE_SQL`：后者单独成常量是为了让 [`migrate_schedules`]
/// 重建时**用同一份 SQL**（表形状只有一个真相）。两者都由这里执行，顺序无所谓
/// （都是 `IF NOT EXISTS`）。
fn create_all(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA).context("建表")?;
    conn.execute_batch(SCHEDULES_TABLE_SQL)
        .context("建 schedules 表")?;
    Ok(())
}

/// 把老库补齐到当前 schema。**幂等**：每次打开都跑一遍，已经升过的一句也不做。
///
/// 为什么不用 `user_version` 版本号：这里只需要「这一列在不在」一种判断，而
/// `user_version` 一引进来就得为以后每一种可能的改动预先编号，还得处理「中间
/// 某个版本跳过了」的兼容。现在的规模下，逐个检查列/表在不在更直接，也更好读。
/// 数量真多到需要版本号了再换。
///
/// 引导了 `execute_batch(SCHEMA)` 之后：新库在 `CREATE TABLE` 里已经有这些列，
/// 这里就全是空操作；只有「在加列之前建的老库」会真的 ALTER。
fn migrate(conn: &Connection) -> Result<()> {
    // sessions.seq（给人看的短编号，见 agent.rs 的 `ChatSession::seq`）。
    // 不加 DEFAULT：NULL 就是「还没发号」，`load()` 里补（`SessionStore::recover_seq`）。
    if !has_column(conn, "sessions", "seq")? {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN seq INTEGER")
            .context("加 sessions.seq")?;
    }
    // **索引在列之后建，且无条件建**（`IF NOT EXISTS` 自己就是幂等的）：
    // 它**不能**放在上面的 `SCHEMA` 里 —— 那段在 `migrate()` 之前跑，而老库的
    // `seq` 列这时还没 ALTER 出来，`CREATE INDEX ... (seq)` 会直接
    // `no such column: seq`，于是**任何**升级上来的老库都打不开（实测过）。
    // 也不能放进上面那个 `if` 里：新库的列是 `CREATE TABLE` 建的，
    // `has_column` 为真，索引就永远建不上了。
    conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_seq ON sessions(seq)")
        .context("建 sessions.seq 的唯一索引")?;

    // sessions.role（worker | router）—— 双层架构那一批加的。
    // 用 `DEFAULT 'worker'`：老库里的会话全都是 worker，跟它们本来的行为一致。
    if !has_column(conn, "sessions", "role")? {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN role TEXT NOT NULL DEFAULT 'worker'")
            .context("加 sessions.role")?;
    }

    // ticket 那一层并进 session 之后加的五列（见 docs/16）。
    //
    // 老库的 `tickets` 表**不删**：里面有派过的活的 brief / 结果，那是历史，
    // 删了就找不回来。它**只读不写**：`migrate()` 从这里搬一次，之后没人再碰。
    for (col, ddl) in [
        ("brief", "ALTER TABLE sessions ADD COLUMN brief TEXT"),
        (
            "dispatched_by",
            "ALTER TABLE sessions ADD COLUMN dispatched_by TEXT",
        ),
        (
            "conclusion",
            "ALTER TABLE sessions ADD COLUMN conclusion TEXT NOT NULL DEFAULT ''",
        ),
        (
            "dispatches",
            "ALTER TABLE sessions ADD COLUMN dispatches INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "closed_at",
            "ALTER TABLE sessions ADD COLUMN closed_at INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "closed_by",
            "ALTER TABLE sessions ADD COLUMN closed_by TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        if !has_column(conn, "sessions", col)? {
            conn.execute_batch(ddl)
                .with_context(|| format!("加 sessions.{col}"))?;
        }
    }

    // 老 `tickets` 里有、而新 `sessions` 里还没有的记录，**搬一次**：
    // 每条 ticket 本来就对应一条 worker 会话（`tickets.session_id`），
    // 把 brief / 结论 / 派工次数搬过去，会话就同时是那条活的载体了。
    //
    // ## 为什么每个子查询都要 `ORDER BY ... LIMIT 1`
    //
    // 同一个会话**可能有多条老 ticket**（旧架构允许「再派一次」复用同一个
    // `session_id`，每次 `ticket_create` 是新行）。没有 `ORDER BY` 时 SQLite 给的是
    // 物理扫描到的第一行 —— 实测可能是最旧那条，而它往往还没收尾
    // （`ended_at = 0`）。后果是：`brief`/`result`/`dispatches` 取了旧值，
    // `closed_at`/`closed_by` 变成 0 / 空串 —— **已收尾那条的信息完全丢掉，且不报错**。
    //
    // 所以每条子查询都显式取**最新**那条：`created_at DESC, id DESC`（id 兜底，
    // 同一毫秒建的两条也能定序）。
    //
    // 幂等：只在 `brief IS NULL` 时写（搬过的不会再被覆盖）。
    let newest = "WHERE t.session_id = sessions.id
                  ORDER BY t.created_at DESC, t.id DESC LIMIT 1";
    let migrated = conn
        .execute(
            &format!(
                "UPDATE sessions SET
               brief = COALESCE(brief, (SELECT t.brief FROM tickets t {newest})),
               dispatched_by = COALESCE(dispatched_by,
                                        (SELECT t.router_id FROM tickets t {newest})),
               conclusion = CASE WHEN conclusion = '' THEN
                                 COALESCE((SELECT t.result FROM tickets t {newest}), '')
                            ELSE conclusion END,
               dispatches = MAX(dispatches,
                                COALESCE((SELECT t.dispatches FROM tickets t {newest}), 0)),
               closed_at = COALESCE(NULLIF(closed_at, 0),
                                    (SELECT t.ended_at FROM tickets t {newest}), 0),
               closed_by = CASE WHEN closed_by = '' THEN
                                COALESCE((SELECT t.ended_by FROM tickets t {newest}), '')
                           ELSE closed_by END
             WHERE brief IS NULL
               AND EXISTS (SELECT 1 FROM tickets t WHERE t.session_id = sessions.id);"
            ),
            [],
        )
        .context("把老 tickets 搬进 sessions")
        .or_else(|e| {
            // 全新库里没有 tickets 表：`EXISTS` 那句会报 no such table。
            // 那不是错误，只是没东西可搬。
            if e.to_string().contains("no such table") {
                Ok(0)
            } else {
                Err(e)
            }
        })?;

    // 搬不成的那些：ticket 指向的会话已经不在了（会话被删过）。
    // 这些**不报错**（数据本来就缺一半，救不回来），但数字要能在日志里看到 ——
    // 否则「我明明派过那么多活，怎么一条都没搬过来」永远查不出原因。
    if let Ok(orphans) = conn.query_row(
        "SELECT COUNT(*) FROM tickets t
          WHERE NOT EXISTS (SELECT 1 FROM sessions s WHERE s.id = t.session_id)",
        [],
        |r| r.get::<_, i64>(0),
    ) {
        if orphans > 0 {
            tracing::warn!(
                "{orphans} 条老工作项的载体会话已经不在了，搬不进 sessions（{migrated} 条会话搬成功）"
            );
        }
    }
    if migrated > 0 {
        tracing::info!("从老 tickets 搬了 {migrated} 条会话的派活记录");
    }

    migrate_schedules(conn)?;
    Ok(())
}

/// 把**已存在但列名不同**的 `schedules` 表修正到当前 schema。
///
/// ## 为什么必须有这一步
///
/// `CREATE TABLE IF NOT EXISTS` 遇到同名表**什么都不做** —— 它不会比对列。
/// 而这张表在开发中间版本里叫过 `created_at`（那一版把「这一版节奏的起点」还叫
/// 「创建时刻」），后来改成了 `anchor_at`。用户的库里存在那张旧表时：
///
/// - `SCHEMA` 的 `CREATE TABLE IF NOT EXISTS` 静默跳过（表在）
/// - `Db::open` 本身**成功**（建表这一步没报错）
/// - 但 `Db::schedules()` 的 `SELECT ... anchor_at` 会 `no such column`
///
/// 实测（`/tmp/onlyseq.db` 那种库）：打开成功、读任务失败。对**定时任务**是致命的
/// （整个功能不可用），对会话不是（会话读得出来）。所以它比「库打不开」轻，
/// 但同样是**上线就会中**的那类。
///
/// ## 怎么修
///
/// 表**还没发布过**（`git log --all -S schedules` 只在本机分支上），所以不需要
/// 写数据迁移：直接把旧表改名让开，`SCHEMA` 那段会在下一次 `execute_batch` 里
/// 建出正确的新表。旧数据留在 `<表名>_legacy` 里（不删 —— 删了找不回来），
/// 而且**只在这个旧表确实是旧形状时**才动它（新库/已修好的库一句也不做）。
fn migrate_schedules(conn: &Connection) -> Result<()> {
    // 表不存在（新库）：`SCHEMA` 已经按当前形状建好了，什么都不用做
    if !has_column(conn, "schedules", "id")? {
        return Ok(());
    }
    // 已经是对的（有 anchor_at）：也不必做
    if has_column(conn, "schedules", "anchor_at")? {
        return Ok(());
    }
    // 旧形状（有 created_at 没 anchor_at）：让开，新表由下面重建。
    //
    // **空表直接 DROP，不留档**：用户库里那张旧 `schedules` 通常是 0 行
    //（这张表还没发布过，中间版本的定时任务没人用），改名留档只会在他目录里
    // 多出一张空的 `schedules_legacy` —— 纯噪音，还会让人以为「我的任务去哪了」。
    // 有数据才留档（那时候数据本身有价值，删了就找不回来）。
    //
    // 实测过真库副本：`schedules` 0 行 → 修之前会建出空的 `schedules_legacy`。
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get(0))
        .unwrap_or(0);
    if rows == 0 {
        conn.execute_batch("DROP TABLE schedules")
            .context("清掉空的旧 schedules 表")?;
        tracing::info!("旧的 schedules 表是空的（0 条任务），直接清掉后重建");
        conn.execute_batch(SCHEDULES_TABLE_SQL)
            .context("按当前形状重建 schedules 表")?;
        return Ok(());
    }
    tracing::warn!(
        "schedules 表是旧形状（created_at 而不是 anchor_at），{rows} 条数据改名留档后重建"
    );
    // **目标名可能已经被占了**（`schedules_legacy` 上一次改名留下的 —— 比如用户
    // 升级过一次、又退回了旧版本、再升回来）。那时 `RENAME` 会报
    // 「there is already another table or index with this name」把整个
    // `Db::open` 拖死。所以先挑一个没被占的名字（`_legacy` / `_legacy2` / …）。
    let mut target = "schedules_legacy".to_string();
    let mut n = 1;
    while has_column(conn, &target, "id")? {
        n += 1;
        target = format!("schedules_legacy{n}");
    }
    conn.execute_batch(&format!("ALTER TABLE schedules RENAME TO {target}"))
        .with_context(|| format!("把旧 schedules 表改名留档（-> {target}）"))?;
    tracing::warn!("旧 schedules 表改名到 {target}（数据不删）");
    // 建当前形状的表 —— **用同一份 SQL**（`SCHEDULES_TABLE_SQL`），不另抄一遍
    conn.execute_batch(SCHEDULES_TABLE_SQL)
        .context("按当前形状重建 schedules 表")?;
    Ok(())
}

/// 这张表里有这一列吗（`PRAGMA table_info`）。
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    // 表名只能拼进来（PRAGMA 不支持参数绑定）；调用方都是写死的常量。
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

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

/// `favorites` 表的一行（一个收藏的页面）。
///
/// `path` 是沙箱里的绝对路径，`created_at` 是毫秒。标题只是显示用的，
/// 打开页面靠 `path`（拼上当前的 `/sandbox/<钥匙>`）。
#[derive(Clone, Debug, PartialEq)]
pub struct Favorite {
    pub path: String,
    pub title: String,
    pub created_at: u64,
}

/// `usage` 表的一行（一次 LLM 调用）。
#[derive(Clone, Debug, Default)]
pub struct UsageRow {
    pub session_id: String,
    /// 毫秒
    pub at: u64,
    pub provider: String,
    pub model: String,
    /// 输入总量（含缓存 = 未缓存 + 缓存读 + 缓存写）
    pub input: u32,
    pub output: u32,
    /// 缓存读取（NULL = 服务商没报；和 0 不是一回事）
    pub cached: Option<u32>,
    /// 缓存写入（OpenAI 系不报，一般是 NULL）
    pub cache_write: Option<u32>,
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
    seq: Option<u64>,
    title: String,
    provider_session: String,
    role: crate::agent::Role,
    /// 下面几项必须进水线：不认它们的话「只是验收了、没新消息」的改动
    /// 永远落不了盘（`plan` 会认为这条会话没变）。
    ///
    /// 用内存里的类型（`Option<Dispatch>` / `Closed`）而不是拆开的列：
    /// 水位线比的是「会话变没变」，那就该比会话自己的那个值。
    dispatch: Option<crate::agent::Dispatch>,
    conclusion: String,
    closed: crate::agent::Closed,
}

impl Saved {
    fn of(s: &ChatSession) -> Self {
        Self {
            len: s.message_count(),
            epoch: s.epoch,
            updated_at: s.updated_at,
            // **必须参与比对**：`load()` 会给没号的老会话补号，补完之后
            // `Saved::of(s)` 就和库里读下来的那份对不上了 → 下一次落盘把号写进去。
            // 少了这一行，补的号只活在内存里，重启就白补了。
            seq: s.seq,
            title: s.title.clone(),
            provider_session: s.provider_session.clone(),
            role: s.role,
            dispatch: s.dispatch.clone(),
            conclusion: s.conclusion.clone(),
            closed: s.closed,
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

/// `plan` 第一步的产出：**光比对，不序列化**。
///
/// 借的是 `store.sessions` 里那一条（所以 `plan` 拿到了 `store` 整段生命周期），
/// 只需记下「从第几条开始、要不要整段重写」。
struct PendingSession<'a> {
    sess: &'a ChatSession,
    /// 从 `sess.messages` 的第几条开始序列化（都是增量）
    from: usize,
    /// 要不要先把库里的旧行删掉（清空过 / 水位线不可信）
    rewrite: bool,
    /// 写成之后的水位线
    saved: Saved,
}

struct PlannedSession {
    id: String,
    seq: Option<u64>,
    title: String,
    created_at: u64,
    updated_at: u64,
    provider_session: String,
    role: crate::agent::Role,
    dispatch: Option<crate::agent::Dispatch>,
    conclusion: String,
    closed: crate::agent::Closed,
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

/// `Db::usage_summary` 的结果。
#[derive(Debug, Default)]
pub struct UsageSummary {
    pub by_day: Vec<(String, i64, i64, i64, i64)>,
    pub by_model: Vec<(String, i64, i64, i64, i64)>,
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
        create_all(&conn)?;
        migrate(&conn).context("迁移")?;
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
        create_all(&conn)?;
        migrate(&conn)?;
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
        //
        // **Router 留着**：它可以是空的（刚建、还没说话），但它的 id 被
        // `meta.router_session` 记着，删了下次就得重建一个、换一个新 id。
        g.conn.execute_batch(
            "DELETE FROM sessions WHERE id NOT IN (SELECT DISTINCT session_id FROM messages)
                 AND role <> 'router';
             DELETE FROM messages WHERE session_id NOT IN (SELECT id FROM sessions);",
        )?;

        let store = {
            let g = &*g;
            // 不再从库里读那个 `active`：服务端已经没有「当前会话」这个概念了
            //（归界面的地址栏）。老库里的 `meta.active` 行留着不动 —— 它只是一
            // 条没人读的旧数据，删它就得写一次 schema 迁移，不值当。
            let mut store = SessionStore::default();

            // 用户消息条数靠 payload 的开头认：`ChatMessage` 是外部标签的枚举，
            // 用户消息序列化出来是 `{"User":…`（有测试盯着）。
            let mut stmt = g.conn.prepare(
                // LEFT JOIN，不是 JOIN：空的 Router 会话一行消息都没有，JOIN 会把它漏掉
                //（漏掉 = 读不回来 = 下次又要重建一个、换新 id，见上面那段）。
                // 普通空会话不落盘，所以 LEFT JOIN 不会让它们冒出来 —— 库里本来
                // 就没有它们的行。
                r#"SELECT s.id, s.seq, s.title, s.created_at, s.updated_at, s.provider_session, s.role,
                          COUNT(m.seq), COALESCE(SUM(m.payload LIKE '{"User%'), 0),
                          s.brief, s.dispatched_by, s.conclusion, s.dispatches, s.closed_at, s.closed_by
                   FROM sessions s LEFT JOIN messages m ON m.session_id = s.id
                   GROUP BY s.id
                   ORDER BY s.updated_at DESC"#,
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, i64>(12)?,
                    r.get::<_, i64>(13)?,
                    r.get::<_, String>(14)?,
                ))
            })?;

            for row in rows {
                let (
                    id,
                    seq,
                    title,
                    created_at,
                    updated_at,
                    provider_session,
                    role,
                    len,
                    users,
                    brief,
                    dispatched_by,
                    conclusion,
                    dispatches,
                    closed_at,
                    closed_by,
                ) = row?;
                // **列 → 类型的唯一转换点**（写入方向见 `Saved::of` / INSERT 那段）。
                //
                // `tickets` 时代允许「有 brief 没 by」这种半份数据（迁移会产出它）。
                // 这里把「有 by 才算派出去的」当判据：`by` 空就没法回执，那这份
                // dispatch 记录毫无用处，整份丢掉 —— 好过留一个半份对象在后面
                // 每个权限过滤上静默消失。
                let dispatch =
                    dispatched_by
                        .filter(|b| !b.is_empty())
                        .map(|by| crate::agent::Dispatch {
                            by,
                            brief: brief.unwrap_or_default(),
                            times: dispatches.max(0) as u32,
                        });
                let s = ChatSession {
                    id,
                    // NULL = 还没发号（这一列加进来之前建的老会话）：
                    // `AppState::load_sessions` 随后会调 `recover_seq` 补上。
                    seq: seq.map(|n| n.max(0) as u64),
                    title,
                    created_at: created_at.max(0) as u64,
                    updated_at: updated_at.max(0) as u64,
                    role: crate::agent::Role::from_db(&role),
                    messages: Vec::new(),
                    provider_session,
                    dispatch,
                    conclusion,
                    closed: crate::agent::Closed::from_db(closed_at.max(0) as u64, &closed_by),
                    stub: Some(Stub {
                        len: len.max(0) as usize,
                        users: users.max(0) as usize,
                    }),
                    loaded_at: None,
                    epoch: 0,
                };
                // 一条都不预读：以前会把「活动会话」直接读进内存（用户一打开就
                // 能看到），现在没有「活动会话」了，谁被打开谁再 `ensure_loaded`。
                // 冷启动落在空对话，本来也不需要预读。
                store.sessions.push(s);
            }
            store
        };

        // 发号水位线从读出来的号恢复；没号的老会话在这里补上
        //（补完之后 `Saved::of` 会和库里的对不上 → 下一次落盘把号写进去，
        // 这正是我们要的：不新增机制，复用现有的 `sessions_dirty` 通路）。
        let mut store = store;
        let assigned = store.recover_seq();

        // 水位线 = 刚读出来的样子，这样下次 sync 只写新增的
        g.generation += 1;
        g.saved.clear();
        for s in &store.sessions {
            let mut was = Saved::of(s);
            // **补过号的那几条：水位线里记「库里还是 NULL」**，于是下一次 `plan`
            // 会认为它们变了、把号写进去。不这么写的话，库里那行一直是 NULL 而
            // 水位线说「有号了」，补的号永远落不了盘（每次重启重补一遍）。
            if assigned.contains(&s.id) {
                was.seq = None;
            }
            g.saved.insert(s.id.clone(), was);
        }
        g.saved_active = None;

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
    /// 只挑可能带文件的那几行来解析，不把整个库读一遍：`attachments` 非空的
    /// `User`、`images` 非空的 `Tool`（字段按声明顺序序列化，没附件 / 没图的那行
    /// 一定以 `"attachments":[]}}` / `"images":[]}}` 结尾，有测试盯着）。
    /// 任何一行解析不了就整个报错 —— 调用方据此**不清扫**，宁可留着。
    pub fn messages_with_files(&self) -> Result<Vec<ChatMessage>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        let mut stmt = g.conn.prepare(
            r#"SELECT payload FROM messages
               WHERE (payload LIKE '{"User"%' AND payload NOT LIKE '%"attachments":[]}}')
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
            .is_ok_and(|g| g.saved.get(&s.id) == Some(&Saved::of(s)))
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
    ///
    /// **锁只用来比对水位线，不用来序列化。** 一次落盘的增量可能有几 MB（几十条
    /// tool 输出每条几十 KB），而 `serde_json::to_string` 是纯 CPU 活 —— 把它放在
    /// `inner` 锁里的话，这几 MB 的序列化期间整个库都动不了：别的会话的 `get_meta`、
    /// 附件登记、工作项读写全在排队。手机上一个回合产出几 MB 输出是常态。
    ///
    /// 所以分两步：锁里只拿「谁要写、从第几条开始写、epoch 对不对」这几件事（纯比对，
    /// 几个 HashMap 查找），出锁之后再序列化。`generation` 照旧在锁里取 —— 它才是
    /// 「这份计划还算不算数」的判据（见 `write`）。
    pub fn plan(&self, store: &SessionStore) -> Result<Option<SyncPlan>> {
        // 第一步（持锁）：只比对，不序列化。`pending` 是「要写的会话 + 从第几条开始」。
        let (generation, gone, pending) = {
            let g = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;

            // 空会话不落盘（也把它上次可能写进去的旧行清掉，见下面的 gone）：
            // 「新对话」点了没说话、或 `/api/reset` 清空过，重启后不该又冒出来。
            //
            // **Router 例外**：它只有一个，而且身份靠 `meta.router_session` 记着。
            // 要是空着不落盘，下次启动时那个 id 在库里找不到 → `ensure_router`
            // 只能重建一个、换一个新 id。用户什么都没做，Router 却每次重启都变一个，
            // 前言不搭后语（而且 `meta` 里那条永远指向一个不存在的会话）。
            // 所以 Router 一建出来就落盘，哪怕一条消息都没有。
            let keep: Vec<&ChatSession> = store
                .sessions
                .iter()
                .filter(|s| s.has_content() || s.role == crate::agent::Role::Router)
                .collect();
            let gone: Vec<String> = g
                .saved
                .keys()
                .filter(|id| !keep.iter().any(|s| &s.id == *id))
                .cloned()
                .collect();

            let mut pending = Vec::new();
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
                let (from, rewrite) = match (s.resident(), prev) {
                    (false, _) => (0, false),
                    (true, None) => (0, false),
                    (true, Some(x)) => {
                        let rewrite = s.messages.len() < x.len || x.epoch != s.epoch;
                        (if rewrite { 0 } else { x.len }, rewrite)
                    }
                };
                pending.push(PendingSession {
                    sess: s,
                    from: from.min(s.messages.len()),
                    rewrite,
                    saved: now,
                });
            }
            (g.generation, gone, pending)
        };

        // 第二步（不持锁）：序列化。几个 MB 的 JSON 都在这一步。
        let mut sessions = Vec::with_capacity(pending.len());
        for p in pending {
            let s = p.sess;
            let messages = if s.resident() {
                let rows = s.messages[p.from..]
                    .iter()
                    .map(serde_json::to_string)
                    .collect::<serde_json::Result<Vec<_>>>()?;
                Some(PlannedMessages {
                    rewrite: p.rewrite,
                    from: p.from,
                    rows,
                })
            } else {
                None
            };
            sessions.push(PlannedSession {
                id: s.id.clone(),
                seq: s.seq,
                title: s.title.clone(),
                created_at: s.created_at,
                updated_at: s.updated_at,
                provider_session: s.provider_session.clone(),
                role: s.role,
                dispatch: s.dispatch.clone(),
                conclusion: s.conclusion.clone(),
                closed: s.closed,
                messages,
                saved: p.saved,
            });
        }

        // 不再往库里写「当前会话」（服务端没这个概念了）。见 `load` 那段。
        let active: Option<String> = None;
        if sessions.is_empty() && gone.is_empty() && active.is_none() {
            return Ok(None);
        }
        Ok(Some(SyncPlan {
            generation,
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
                "INSERT INTO sessions(id, seq, title, created_at, updated_at, provider_session, role,
                                      brief, dispatched_by, conclusion, dispatches, closed_at, closed_by)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET
                   -- 号只在「库里还没有」时写进去（`COALESCE`）：已经发过的号不许被
                   -- 一次正常的落盘覆盖成别的（那等于回收/换号，见 `ChatSession::seq`）。
                   seq = COALESCE(sessions.seq, excluded.seq),
                   title = excluded.title,
                   updated_at = excluded.updated_at,
                   provider_session = excluded.provider_session,
                   role = excluded.role,
                   brief = excluded.brief,
                   dispatched_by = excluded.dispatched_by,
                   conclusion = excluded.conclusion,
                   dispatches = excluded.dispatches,
                   closed_at = excluded.closed_at,
                   closed_by = excluded.closed_by",
                params![
                    s.id,
                    s.seq.map(|n| n as i64),
                    s.title,
                    s.created_at as i64,
                    s.updated_at as i64,
                    s.provider_session,
                    s.role.as_str(),
                    // **类型 → 列的唯一转换点**（读方向见 `load` 里那个 `from_db`）
                    s.dispatch.as_ref().map(|d| d.brief.as_str()),
                    s.dispatch.as_ref().map(|d| d.by.as_str()),
                    s.conclusion,
                    s.dispatch.as_ref().map_or(0, |d| d.times) as i64,
                    s.closed.at().unwrap_or(0) as i64,
                    s.closed.to_db().1,
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

    /// 只给测试用：直接跑一条 SQL。
    ///
    /// 用来**造真的坏库**（`DROP TABLE meta` 之后 `get_meta` 返回 `Err` 而不是
    /// `Ok(None)`）—— 「读失败不能当成没有」这类保证，光靠在正常库上调用是
    /// 测不出来的。
    #[cfg(test)]
    pub fn exec_for_test(&self, sql: &str) -> Result<()> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        g.conn.execute_batch(sql)?;
        Ok(())
    }

    /// 只给测试用：跑一条返回单值单列的查询。
    #[cfg(test)]
    pub fn query_for_test(&self, sql: &str) -> Result<Option<String>> {
        let g = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("db 锁坏了"))?;
        let v = g.conn.query_row(sql, [], |r| r.get::<_, String>(0));
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

    pub(crate) fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
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

    pub fn mcp_servers(&self) -> Result<Vec<crate::mcp::Server>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT name, url, headers FROM mcp_servers ORDER BY name")?;
            let rows = st.query_map([], |r| {
                let headers: String = r.get(2)?;
                Ok(crate::mcp::Server {
                    name: r.get(0)?,
                    url: r.get(1)?,
                    headers: serde_json::from_str(&headers).unwrap_or_default(),
                })
            })?;
            rows.collect()
        })
    }

    /// 每家服务商选的协议：(服务商 id, auto | responses | chat)。
    pub fn api_styles(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, api_style FROM provider_api_style")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
    }

    /// 存 / 删（`None` = 回到预设的）一家的协议。
    pub fn set_api_style(&self, id: &str, style: Option<&str>) -> Result<()> {
        self.with(|c| match style {
            None => c
                .execute("DELETE FROM provider_api_style WHERE id = ?1", [id])
                .map(|_| ()),
            Some(s) => c
                .execute(
                    "INSERT OR REPLACE INTO provider_api_style(id, api_style) VALUES(?1, ?2)",
                    [id, s],
                )
                .map(|_| ()),
        })
    }

    /// 停用名单：(kind, name)。
    pub fn disabled(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT kind, name FROM disabled")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
    }

    pub fn set_disabled(&self, kind: &str, name: &str, disabled: bool) -> Result<()> {
        self.with(|c| {
            if disabled {
                c.execute(
                    "INSERT OR IGNORE INTO disabled(kind, name) VALUES(?1, ?2)",
                    [kind, name],
                )
            } else {
                c.execute(
                    "DELETE FROM disabled WHERE kind = ?1 AND name = ?2",
                    [kind, name],
                )
            }
            .map(|_| ())
        })
    }

    /// 存 / 删一个 MCP 服务（`None` = 删）。
    pub fn set_mcp_server(&self, name: &str, srv: Option<&crate::mcp::Server>) -> Result<()> {
        self.with(|c| match srv {
            None => c
                .execute("DELETE FROM mcp_servers WHERE name = ?1", [name])
                .map(|_| ()),
            Some(s) => c
                .execute(
                    "INSERT OR REPLACE INTO mcp_servers(name, url, headers) VALUES(?1, ?2, ?3)",
                    params![
                        s.name,
                        s.url,
                        serde_json::to_string(&s.headers).unwrap_or_else(|_| "{}".into())
                    ],
                )
                .map(|_| ()),
        })
    }

    /// 收藏的页面，最近收的在前。
    pub fn favorites(&self) -> Result<Vec<Favorite>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT path, title, created_at FROM favorites ORDER BY created_at DESC, path",
            )?;
            let rows = st.query_map([], |r| {
                Ok(Favorite {
                    path: r.get(0)?,
                    title: r.get(1)?,
                    // SQLite 里是 INTEGER；rusqlite 不给 u64 实现 FromSql（见会话那几个同理）
                    created_at: r.get::<_, i64>(2)? as u64,
                })
            })?;
            rows.collect()
        })
    }

    /// 收藏 / 取消收藏（`None` = 取消）。
    ///
    /// 同一个 path 再收一次只更新标题（比如页面改名了再点一次星），**不动**
    /// `created_at` —— 否则抽屉里那一行会莫名其妙跳到最上面。
    pub fn set_favorite(&self, path: &str, title: Option<&str>) -> Result<()> {
        self.with(|c| match title {
            None => c
                .execute("DELETE FROM favorites WHERE path = ?1", [path])
                .map(|_| ()),
            Some(t) => c
                .execute(
                    "INSERT INTO favorites(path, title, created_at) VALUES(?1, ?2, ?3)
                     ON CONFLICT(path) DO UPDATE SET title = excluded.title",
                    params![path, t, crate::agent::now_ms() as i64],
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

    // ---- 定时任务（`schedules` 表，见 schedule.rs） -------------------------
    //
    // 这一层只做 SQL：它不认识 `Recurrence`，也不判「该不该触发」——那是纯函数
    // `schedule::due_at` 的事。唯一有点意思的是 `every_secs` / `cron` 那两列的
    // 二选一：库里用「恰好一个 NULL」表达，翻译成本在这里收敛成唯一一处。

    /// 全部定时任务（启动时读一次）。
    pub fn schedules(&self) -> Result<Vec<crate::schedule::ScheduledTask>> {
        use crate::schedule::{CronExpr, Period, Recurrence, ScheduledTask};
        // `QueryReturnedNoRows` 之类不是错误路径，所以先在闭包里把**原始列**取出来，
        // 出来再翻译成类型 —— 这样「周期看不懂就跳过这一行」不用写在 SQL 闭包里。
        type Row = (
            String,
            String,
            Option<i64>,    // every_secs
            Option<String>, // cron
            i64,            // anchor_at
            i64,            // handled_through
            Option<i64>,    // paused_at
        );
        let rows: Vec<Row> = self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, prompt, every_secs, cron, anchor_at, handled_through, paused_at
                   FROM schedules ORDER BY anchor_at, id",
            )?;
            let it = st.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?;
            it.collect()
        })?;

        let mut out = Vec::with_capacity(rows.len());
        for (id, prompt, every_secs, cron, anchor_at, handled_through, paused_at) in rows {
            // 「恰好一列非空」由建表时的 CHECK 守着。这里再兜一层：周期看不懂的那行
            // **跳过**（一条脏数据不该让**所有**闹钟都不响）——但要 warn，别静默。
            let every: Option<Recurrence> = match (every_secs, cron) {
                (Some(s), None) => u64::try_from(s)
                    .ok()
                    .and_then(Period::try_new_secs)
                    .map(Recurrence::Every),
                (None, Some(e)) => CronExpr::parse(&e)
                    .ok()
                    .map(|c| Recurrence::Cron(Box::new(c))),
                _ => None,
            };
            let Some(every) = every else {
                tracing::warn!("定时任务 {id} 的周期读不出来，跳过（它在库里还能改）");
                continue;
            };
            out.push(ScheduledTask {
                id,
                prompt,
                every,
                anchor_at: anchor_at.max(0) as u64,
                handled_through: handled_through.max(0) as u64,
                paused_at: paused_at.map(|v| v.max(0) as u64),
            });
        }
        Ok(out)
    }

    /// 写一条（新建或改）。
    pub fn put_schedule(&self, t: &crate::schedule::ScheduledTask) -> Result<()> {
        use crate::schedule::Recurrence;
        // 类型 → 列的唯一转换点（读方向见 `schedules()`）
        let (every_secs, cron) = match &t.every {
            Recurrence::Every(p) => (Some(p.secs() as i64), None),
            Recurrence::Cron(c) => (None, Some(c.as_str())),
        };
        self.with(|c| {
            c.execute(
                "INSERT INTO schedules(id, prompt, every_secs, cron, anchor_at, handled_through,
                                      paused_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                   prompt = excluded.prompt,
                   every_secs = excluded.every_secs,
                   cron = excluded.cron,
                   -- **anchor_at 必须在这里**：改节奏会重置它（见 schedule.rs 的
                   -- `Schedules::edit`）。漏了的话内存里是新的、库里是旧的 ——
                   -- 重启之后下一次触发时间会跳回旧相位（有测试钉着）。
                   anchor_at = excluded.anchor_at,
                   handled_through = excluded.handled_through,
                   paused_at = excluded.paused_at",
                params![
                    t.id,
                    t.prompt,
                    every_secs,
                    cron,
                    t.anchor_at as i64,
                    t.handled_through as i64,
                    t.paused_at.map(|v| v as i64),
                ],
            )
            .map(|_| ())
        })
    }

    /// 只推水位线（tick 里每条触发一次，最热的那个写）。
    pub fn set_schedule_cursor(&self, id: &str, handled_through: u64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE schedules SET handled_through = MAX(handled_through, ?2) WHERE id = ?1",
                params![id, handled_through as i64],
            )
            .map(|_| ())
        })
    }

    pub fn delete_schedule(&self, id: &str) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM schedules WHERE id = ?1", [id])
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
                "INSERT INTO usage(session_id, at, provider, model, input, output, cached, cache_write, reasoning, finish, ms)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    r.session_id,
                    r.at as i64,
                    r.provider,
                    r.model,
                    r.input,
                    r.output,
                    r.cached,
                    r.cache_write,
                    r.reasoning,
                    r.finish,
                    r.ms as i64
                ],
            )
            .map(|_| ())
        })
    }

    /// **一批会话**各自最近一次调用用的模型 + 输入量（`list_sessions` 每行那两项）。
    ///
    /// 一次查完，不是 50 次查询：`MAX(id) GROUP BY session_id` 走
    /// `idx_usage_session(session_id, id)` 的覆盖索引（`EXPLAIN QUERY PLAN` 实测
    /// 是 `SCAN usage USING COVERING INDEX`），50 条会话也是毫秒级的一次扫描。
    ///
    /// 返回 `session_id -> (model, input)`。
    ///
    /// ## 「模型」这两个字的准确含义
    ///
    /// 这是**这条会话最近一次调用实际发的模型**（`usage.model` 那一列）。
    /// `ChatSession` 上**没有**存模型 —— 会话是个容器，模型是**发起请求那一刻**
    /// 从全局 provider 配置里取的（`agent.rs` 的 `stream_one` 用 `provider.model`）。
    /// 所以：
    ///
    /// - 用户中途换了模型 → 每条会话显示的是**它最后一次跑的时候**用的那个，
    ///   不是「现在配置的是什么」。这正是要的语义（它确实是那条会话用过的）。
    /// - 一次都没跑过的会话 → 没有记录 → 调用方显示成「没跑过」。
    pub fn last_models(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, (String, u64)>> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        self.with(|c| {
            // 逐个会话一条（用主键 + 覆盖索引），不拼一个巨大的 IN：`list_sessions`
            // 最多 50 条，逐条走索引比拼 50 个占位符的 SQL 更好读，实测同量级。
            let mut st = c.prepare(
                "SELECT model, input FROM usage
                  WHERE session_id = ?1 AND cleared = 0
                  ORDER BY id DESC LIMIT 1",
            )?;
            for id in ids {
                let row = st.query_row([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)));
                if let Ok((model, input)) = row {
                    out.insert(id.clone(), (model, input.max(0) as u64));
                }
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// 这个会话（上次清空之后）的用量：累计 + 最近那一次。一次都没有是 `None`。
    pub fn session_usage(&self, sid: &str) -> Result<Option<crate::state::UsageStats>> {
        self.with(|c| {
            #[allow(clippy::type_complexity)]
            let (calls, input, uncached, output, cache_read, cache_write, reasoning, reported): (
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
                i64,
            ) = c.query_row(
                // 最后一列：报了缓存字段（`cached` 不是 NULL）的那些调用的输入 ——
                // 累计命中率的分母，见 `openai::cache_hit_ratio`
                "SELECT COUNT(*), COALESCE(SUM(input), 0),
                        COALESCE(SUM(input - COALESCE(cached, 0) - COALESCE(cache_write, 0)), 0),
                        COALESCE(SUM(output), 0), COALESCE(SUM(cached), 0),
                        COALESCE(SUM(cache_write), 0), COALESCE(SUM(reasoning), 0),
                        COALESCE(SUM(CASE WHEN cached IS NOT NULL THEN input END), 0)
                 FROM usage WHERE session_id = ?1 AND cleared = 0",
                [sid],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )?;
            if calls == 0 {
                return Ok(None);
            }
            // 最近那次：从存下来的桶重建 UsageView（命中率的算法只有那一处）
            let last = c.query_row(
                "SELECT input, output, cached, cache_write, reasoning FROM usage
                 WHERE session_id = ?1 AND cleared = 0 ORDER BY id DESC LIMIT 1",
                [sid],
                |r| {
                    let input: u32 = r.get(0)?;
                    let output: u32 = r.get(1)?;
                    let cached: Option<u32> = r.get(2)?;
                    let cache_write: Option<u32> = r.get(3)?;
                    let reasoning: Option<u32> = r.get(4)?;
                    Ok(crate::openai::UsageView::from_buckets(
                        input,
                        cached.unwrap_or(0),
                        cache_write.unwrap_or(0),
                        output,
                        reasoning.unwrap_or(0),
                        cached.is_some(),
                    ))
                },
            )?;
            Ok(Some(crate::state::UsageStats {
                last: Some(last),
                input: input as u64,
                uncached_input: uncached as u64,
                cache_read: cache_read as u64,
                cache_write: cache_write as u64,
                output: output as u64,
                reasoning: reasoning as u64,
                calls: calls as u64,
                cache_reported_input: reported as u64,
                cache_hit_ratio: crate::openai::cache_hit_ratio(cache_read as u64, reported as u64),
            }))
        })
    }

    /// 用量汇总（设置页「用量」）：全部历史（包括清空过、删掉的会话 —— 行一直留着）。
    ///
    /// `by_day` 是最近 `days` 天、按本地日期分组（最新在前）；`by_model` 是全部历史按
    /// 服务商 + 模型分组（用得多的在前）。一行：`(键, 次数, 输入, 缓存读, 输出)`，
    /// 输入含缓存（和 `usage.input` 同一个口径）。
    pub fn usage_summary(&self, days: u32) -> Result<UsageSummary> {
        type Row = (String, i64, i64, i64, i64);
        let since_ms = crate::agent::now_ms().saturating_sub(u64::from(days) * 86_400_000) as i64;
        self.with(|c| {
            let collect = |sql: &str, p: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<Vec<Row>> {
                let mut st = c.prepare(sql)?;
                let rows = st.query_map(p, |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?;
                rows.collect()
            };
            let by_day = collect(
                "SELECT date(at / 1000, 'unixepoch', 'localtime') AS d, COUNT(*), SUM(input),
                        COALESCE(SUM(cached), 0), SUM(output)
                 FROM usage WHERE at >= ?1 GROUP BY d ORDER BY d DESC",
                &[&since_ms],
            )?;
            let by_model = collect(
                "SELECT provider || ' / ' || model AS m, COUNT(*), SUM(input),
                        COALESCE(SUM(cached), 0), SUM(output)
                 FROM usage GROUP BY m ORDER BY SUM(input) + SUM(output) DESC",
                &[],
            )?;
            Ok(UsageSummary { by_day, by_model })
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

    /// 造一个「ticket 时代的」库，然后跑 `migrate()` 看它搬成什么样。
    ///
    /// 这是 `migrate()` 唯一的测试入口 —— 之前 6 个 db 测试没有一个覆盖它，
    /// 所以「同一会话两条 ticket 时搬错了哪条」这种 bug 一直没被发现。
    ///
    /// ## schema 必须是 `main` 上的**真实起点**
    ///
    /// 主 case 造的是 **`main`（`refactor/session-only` 分出去那一点）上的库**：
    /// `sessions` 已经有 `role` 列（双层架构那一批加的，比 ticket 层晚、比本次早），
    /// 但没有本次新加的六列。`tickets` 的列名 / 类型 / 默认值也逐字照抄。
    ///
    /// 为什么这很重要：造一个缺 `role` 的假 schema，验证的是「从最老版本直接跳到
    /// 最新」，而**真正的升级路径是「从 main 的库升上来」**。两条路径的 SQL 执行
    /// 序列不同（`role` 那段 `ALTER TABLE` 在假 schema 上会真跑，真实升级时被
    /// `has_column` 跳过）—— 那等于测了一条用户永远不会走的路。
    ///
    /// 最老那个版本（连 `role` 都没有）单独用 [`legacy_db_oldest`] 测。
    fn legacy_db(
        sessions: &[(&str, &str)],
        tickets: &[(&str, &str, &str, i64, &str)],
    ) -> Connection {
        legacy_db_with(sessions, tickets, true)
    }

    /// 最老那个版本：`sessions` 连 `role` 都没有（双层架构之前）。
    fn legacy_db_oldest(
        sessions: &[(&str, &str)],
        tickets: &[(&str, &str, &str, i64, &str)],
    ) -> Connection {
        legacy_db_with(sessions, tickets, false)
    }

    /// **升级上来的老库必须打得开。**
    ///
    /// 这是本次最要紧的一条：用户机器上跑着一个常驻实例，升级后库打不开就是灾难。
    /// 两个独立的坑，各造一个库：
    ///
    /// 1. 老库没有 `sessions.seq`，而 `idx_sessions_seq` 一度写在 `SCHEMA` 里 ——
    ///    那段在 `migrate()` **之前**跑，于是 `CREATE INDEX ... (seq)` 报
    ///    `no such column: seq`，**任何**升级上来的库都打不开（实测一个 162 条会话
    ///    的真库直接失败）。
    /// 2. 老库的 `schedules` 表用 `created_at`（中间版本叫法）而不是 `anchor_at`，
    ///    而 `CREATE TABLE IF NOT EXISTS` 遇到同名表静默跳过 —— 打开成功、但
    ///    `schedules()` 读的时候 `no such column: anchor_at`。
    ///
    /// 变异验证：
    /// - 把 `idx_sessions_seq` 挪回 `SCHEMA` → 第一个场景 `Db::open` 直接报错；
    /// - 把 `migrate_schedules` 那一步删掉 → 第二个场景读任务失败。
    #[test]
    fn an_upgraded_old_db_opens_and_can_read_everything() {
        // ---- 场景 1：最老那种库（没有 seq、没有 schedules 表）----
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
               id TEXT PRIMARY KEY, title TEXT NOT NULL, created_at INTEGER NOT NULL,
               updated_at INTEGER NOT NULL, provider_session TEXT NOT NULL,
               role TEXT NOT NULL DEFAULT 'worker', brief TEXT, dispatched_by TEXT,
               conclusion TEXT NOT NULL DEFAULT '', dispatches INTEGER NOT NULL DEFAULT 0,
               closed_at INTEGER NOT NULL DEFAULT 0, closed_by TEXT NOT NULL DEFAULT '');
             INSERT INTO sessions(id,title,created_at,updated_at,provider_session,role)
               VALUES('s1','老会话',1,1,'p','worker');",
        )
        .unwrap();
        // 走**和真库一样的两步**（SCHEMA 先、migrate 后）—— 这正是那个坑的现场
        create_all(&conn).expect("建表这一关就该过（索引不能在 SCHEMA 里面）");
        migrate(&conn).expect("migrate 该成功");
        // 会话还在，seq 列补上了
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "老会话不能丢"
        );
        assert!(
            has_column(&conn, "sessions", "seq").unwrap(),
            "seq 列要补上"
        );
        // 新表也建出来了，能读
        conn.query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get::<_, i64>(0))
            .expect("新 schedules 表要能读");

        // ---- 场景 2：`schedules` 是旧形状（created_at，没有 anchor_at）----
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schedules (
               id TEXT PRIMARY KEY, prompt TEXT NOT NULL, every_secs INTEGER, cron TEXT,
               created_at INTEGER NOT NULL, handled_through INTEGER NOT NULL, paused_at INTEGER,
               CHECK (handled_through >= created_at),
               CHECK (paused_at IS NULL OR paused_at >= created_at),
               CHECK ((every_secs IS NULL) <> (cron IS NULL)));
             INSERT INTO schedules(id,prompt,every_secs,created_at,handled_through)
               VALUES('t1','旧任务',60,1,1);",
        )
        .unwrap();
        create_all(&conn).unwrap();
        migrate(&conn).unwrap(); // 这一步要把旧表改名让开
        assert!(
            has_column(&conn, "schedules", "anchor_at").unwrap(),
            "旧 schedules 表要换成当前形状（有 anchor_at）"
        );
        assert!(
            !has_column(&conn, "schedules", "created_at").unwrap(),
            "旧列不该还在"
        );
        // 旧数据**不删**，留在 _legacy 里
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM schedules_legacy", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
            1,
            "旧表的 1 行要留在 schedules_legacy（不许删）"
        );
        // 新表是空的、可读
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    /// **旧 `schedules` 表是空的** → 直接 DROP，不留 `schedules_legacy`。
    ///
    /// 实测过真库副本：`schedules` 0 行时，改名留档会在用户目录里留下一个空的
    /// `schedules_legacy` —— 纯噪音。有数据才留档（数据本身有价值）。
    ///
    /// 变异验证：把 `if rows == 0 { DROP }` 那段删掉（一律改名留档）→ 这条挂。
    #[test]
    fn an_empty_old_schedules_table_is_dropped_not_archived() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schedules (
               id TEXT PRIMARY KEY, prompt TEXT NOT NULL, every_secs INTEGER, cron TEXT,
               created_at INTEGER NOT NULL, handled_through INTEGER NOT NULL, paused_at INTEGER);",
        )
        .unwrap();
        create_all(&conn).unwrap();
        migrate(&conn).unwrap();
        assert!(
            has_column(&conn, "schedules", "anchor_at").unwrap(),
            "新表要建好"
        );
        assert!(
            !has_column(&conn, "schedules_legacy", "id").unwrap(),
            "空表不该留档（用户不该看到一个空的 schedules_legacy）"
        );
    }

    /// **建表语句只有一份**：`create_all` 和 `migrate_schedules` 的重建用同一个常量。
    ///
    /// 这条钉的是「两处各抄一遍」这个技术债 —— 上一轮那个「升级上来库打不开」的
    /// bug 根因就是建表语句和迁移逻辑不同步。
    ///
    /// **关键是三条路都要比**（第一版只比了前两条，`migrate_schedules` 那条漏了，
    /// 于是「迁移里抄一份不同的 SQL」这个变异体**存活**）：
    ///
    /// 1. `create_all`（新库）
    /// 2. `SCHEDULES_TABLE_SQL` 本身
    /// 3. **`migrate_schedules` 从旧表重建出来的那张**
    ///
    /// 三条的 `sqlite_master.sql` 必须逐字相同。
    #[test]
    fn the_schedules_table_has_exactly_one_definition() {
        let sql_of = |c: &Connection| -> String {
            c.query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='schedules'",
                [],
                |r| r.get(0),
            )
            .expect("要有 schedules 表")
        };
        let cols_of = |c: &Connection| -> Vec<String> {
            let mut st = c.prepare("PRAGMA table_info(schedules)").unwrap();
            let it = st.query_map([], |r| r.get::<_, String>(1)).unwrap();
            it.collect::<rusqlite::Result<Vec<String>>>().unwrap()
        };

        // ① 新库（`create_all`）
        let a = Connection::open_in_memory().unwrap();
        create_all(&a).unwrap();
        // ② 常量单独执行
        let b = Connection::open_in_memory().unwrap();
        b.execute_batch(SCHEDULES_TABLE_SQL).unwrap();
        // ③ **旧表（有数据）→ migrate 走 RENAME 分支重建**
        // ④ **旧表（空）→ migrate 走 DROP 分支重建**
        // 两条分支各自 execute_batch 一次 `SCHEDULES_TABLE_SQL`，都要和常量一致。
        let rebuild_via = |with_data: bool| -> Connection {
            let c = Connection::open_in_memory().unwrap();
            c.execute_batch(
                "CREATE TABLE schedules (
                   id TEXT PRIMARY KEY, prompt TEXT NOT NULL, every_secs INTEGER, cron TEXT,
                   created_at INTEGER NOT NULL, handled_through INTEGER NOT NULL, paused_at INTEGER);",
            )
            .unwrap();
            if with_data {
                c.execute_batch(
                    "INSERT INTO schedules(id,prompt,every_secs,created_at,handled_through)
                     VALUES('t1','旧任务',60,1,1)",
                )
                .unwrap();
            }
            create_all(&c).unwrap();
            migrate(&c).unwrap();
            assert!(
                has_column(&c, "schedules", "anchor_at").unwrap(),
                "migrate 该把旧表换成当前形状"
            );
            c
        };
        let renamed = rebuild_via(true); // RENAME 分支
        let dropped = rebuild_via(false); // DROP 分支

        assert_eq!(sql_of(&a), sql_of(&b), "create_all vs 常量");
        assert_eq!(
            sql_of(&a),
            sql_of(&renamed),
            "**migrate_schedules 的 RENAME 分支必须用同一份 SQL**（不能自己抄一份）"
        );
        assert_eq!(
            sql_of(&a),
            sql_of(&dropped),
            "**migrate_schedules 的 DROP 分支必须用同一份 SQL**"
        );
        assert_eq!(cols_of(&a), cols_of(&renamed), "重建后的列也要一致");
        assert_eq!(
            cols_of(&a),
            vec![
                "id",
                "prompt",
                "every_secs",
                "cron",
                "anchor_at",
                "handled_through",
                "paused_at"
            ]
        );
    }

    /// `migrate()` 对**已经是当前形状**的库一句也不做（幂等）。
    ///
    /// 钉的是 `migrate_schedules` 那两个提前返回：少了它们，一个正常库每启动一次
    /// 就会把 `schedules` 改名成 `schedules_legacy`（第二次就撞上重名而失败）。
    #[test]
    fn migrating_a_current_db_twice_changes_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        create_all(&conn).unwrap();
        migrate(&conn).unwrap();
        // 造一条任务，再 migrate 一次（模拟重启）
        conn.execute_batch(
            "INSERT INTO schedules(id,prompt,every_secs,anchor_at,handled_through)
             VALUES('t1','我的任务',60,1,1)",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM schedules", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "正常库重启一次不该把任务弄丢"
        );
        assert!(
            !has_column(&conn, "schedules_legacy", "id").unwrap(),
            "正常库不该被改名（表都不该存在）"
        );
    }

    /// 旧 `schedules` 表 + **已经存在**的 `schedules_legacy`：不能把 `Db::open` 拖死。
    ///
    /// 真实路径：用户升级过一次（旧表改名成 `_legacy`）→ 退回旧版本（又建出一张
    /// 旧形状的 `schedules`）→ 再升回来。这时 `RENAME TO schedules_legacy` 会报
    /// 「there is already another table or index with this name」，库里**所有会话
    /// 都读不出来**。
    ///
    /// 变异验证：去掉挑空名字那个循环（直接 `RENAME TO schedules_legacy`）→ 红。
    #[test]
    fn a_second_old_schedules_table_does_not_block_the_open() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schedules (
               id TEXT PRIMARY KEY, prompt TEXT NOT NULL, every_secs INTEGER, cron TEXT,
               created_at INTEGER NOT NULL, handled_through INTEGER NOT NULL, paused_at INTEGER);
             INSERT INTO schedules VALUES('t1','旧',60,NULL,1,1,NULL);
             -- 上一次改名留下的
             CREATE TABLE schedules_legacy (id TEXT PRIMARY KEY, prompt TEXT NOT NULL);",
        )
        .unwrap();
        create_all(&conn).unwrap();
        migrate(&conn).expect("不能被已存在的 schedules_legacy 挡死");
        assert!(has_column(&conn, "schedules", "anchor_at").unwrap());
        // 两份旧数据都还在（一个在 legacy，一个在 legacy2）
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM schedules_legacy2", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "这次的旧表改名到 legacy2（legacy 已经有东西了）"
        );
    }

    /// `with_role` = 造 `main` 上的 schema（有 `role`）还是最老那个（没有）。    /// `with_role` = 造 `main` 上的 schema（有 `role`）还是最老那个（没有）。
    fn legacy_db_with(
        sessions: &[(&str, &str)],
        tickets: &[(&str, &str, &str, i64, &str)],
        with_role: bool,
    ) -> Connection {
        let conn = Connection::open_in_memory().unwrap();

        // sessions：`main` 的样子（带 role）/ 最老的样子（不带 role）。
        // 共同点是**都没有**本次新加的六列。
        let role_col = if with_role {
            ",\n  role             TEXT NOT NULL DEFAULT 'worker'"
        } else {
            ""
        };
        // tickets：逐字照抄 `main` 上的 `CREATE TABLE IF NOT EXISTS tickets`
        // （列名 / 类型 / NOT NULL / DEFAULT 一个不差）。
        conn.execute_batch(&format!(
            "CREATE TABLE sessions (
  id               TEXT PRIMARY KEY,
  title            TEXT NOT NULL,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  provider_session TEXT NOT NULL{role_col}
);
CREATE TABLE tickets (
  id            TEXT PRIMARY KEY,
  title         TEXT NOT NULL,
  brief         TEXT NOT NULL,
  session_id    TEXT NOT NULL,
  router_id     TEXT NOT NULL,
  stage         TEXT NOT NULL,
  result        TEXT NOT NULL DEFAULT '',
  report        TEXT NOT NULL DEFAULT '',
  ended_at      INTEGER NOT NULL DEFAULT 0,
  ended_by      TEXT NOT NULL DEFAULT '',
  dispatches    INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);"
        ))
        .unwrap();

        // 会话：`main` 上的库一定有 role（默认 worker），所以插的时候不指定它。
        for (id, title) in sessions {
            conn.execute(
                "INSERT INTO sessions(id, title, created_at, updated_at, provider_session)
                 VALUES(?1, ?2, 1, 1, 'p')",
                params![id, title],
            )
            .unwrap();
        }
        // (id, session_id, brief, created_at, ended_by) —— ended_at 从 ended_by 推：
        // 空串 = 还没收尾（0），否则给一个时刻
        for (id, sid, brief, created_at, ended_by) in tickets {
            let ended_at = if ended_by.is_empty() { 0 } else { 900 };
            conn.execute(
                "INSERT INTO tickets(id, title, brief, session_id, router_id, stage, result,
                                     ended_at, ended_by, dispatches, created_at, updated_at)
                 VALUES(?1, 't', ?2, ?3, 'router-1', 'waiting_review', ?4, ?5, ?6, 1, ?7, ?7)",
                params![
                    id,
                    brief,
                    sid,
                    format!("结果-{id}"),
                    ended_at,
                    ended_by,
                    created_at
                ],
            )
            .unwrap();
        }
        conn
    }

    /// 跑完 migrate 之后，某个 session 的六个新列长什么样。
    fn migrated_row(conn: &Connection, sid: &str) -> (String, String, String, i64, i64, String) {
        conn.query_row(
            "SELECT COALESCE(brief,''), COALESCE(dispatched_by,''), conclusion,
                    dispatches, closed_at, closed_by
             FROM sessions WHERE id = ?1",
            [sid],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap()
    }

    /// **同一会话两条 ticket：搬最新那条，不是最旧那条。**
    ///
    /// 这是审查实测出来的真 bug：没有 `ORDER BY` 时 SQLite 给物理扫描到的第一行，
    /// 可能是最旧、且还没收尾（`ended_at = 0`）的那条 —— 于是
    /// `brief` / `result` / `dispatches` 取旧值，`closed_at` / `closed_by` 变 0 / 空，
    /// **已收尾那条的信息完全丢掉**。
    #[test]
    fn migration_takes_the_newest_ticket_not_the_first_scanned() {
        let conn = legacy_db(
            &[("s1", "干活的那条")],
            &[
                // 旧的、还没收尾（created_at=100）
                ("t-old", "s1", "旧的要求", 100, ""),
                // 新的、已验收（created_at=200）
                ("t-new", "s1", "新的要求", 200, "done"),
            ],
        );
        migrate(&conn).unwrap();

        let (brief, by, conclusion, dispatches, closed_at, closed_by) = migrated_row(&conn, "s1");
        assert_eq!(brief, "新的要求", "要搬最新那条的 brief");
        assert_eq!(by, "router-1");
        assert_eq!(conclusion, "结果-t-new", "结论也要最新那条的");
        assert_eq!(dispatches, 1);
        assert_eq!(closed_at, 900, "**已收尾那条的 closed_at 不能丢**");
        assert_eq!(closed_by, "done", "**closed_by 不能变成空串**");
    }

    /// 反过来放（新的先插、旧的后插）也要取新的 —— 证明靠的是 `created_at`
    /// 而不是插入顺序。
    #[test]
    fn migration_orders_by_created_at_not_insertion_order() {
        let conn = legacy_db(
            &[("s1", "t")],
            &[
                // 新的先插进去（created_at=200）
                ("t-new", "s1", "新的", 200, "cancelled"),
                // 旧的后插进去（created_at=100）
                ("t-old", "s1", "旧的", 100, ""),
            ],
        );
        migrate(&conn).unwrap();
        let (brief, _, _, _, closed_at, closed_by) = migrated_row(&conn, "s1");
        assert_eq!(brief, "新的", "按 created_at 取最新，不看插入顺序");
        assert_eq!(closed_by, "cancelled");
        assert_eq!(closed_at, 900);
    }

    /// **幂等**：跑两遍 migrate，第二次一个字节都不改。
    #[test]
    fn migration_is_idempotent() {
        let conn = legacy_db(&[("s1", "t")], &[("t1", "s1", "要求", 100, "done")]);
        migrate(&conn).unwrap();
        let first = migrated_row(&conn, "s1");

        // 手动改掉搬进来的值，再跑一遍：**不该被覆盖回去**（WHERE brief IS NULL）
        conn.execute("UPDATE sessions SET brief = '人工改过' WHERE id = 's1'", [])
            .unwrap();
        migrate(&conn).unwrap();
        let (brief, _, _, _, _, _) = migrated_row(&conn, "s1");
        assert_eq!(brief, "人工改过", "搬过的行不该被二次迁移覆盖");

        // 而没改过的行跑第二遍仍是原值
        let conn2 = legacy_db(&[("s1", "t")], &[("t1", "s1", "要求", 100, "done")]);
        migrate(&conn2).unwrap();
        assert_eq!(migrated_row(&conn2, "s1"), first);
        migrate(&conn2).unwrap();
        assert_eq!(migrated_row(&conn2, "s1"), first, "第二次跑必须不变");
    }

    /// ticket 指向的会话不存在：**不 panic**，也不写脏数据。
    /// （「没搬成」的计数在 `migrate` 里 `tracing::warn!` 出来，测试只验不炸。）
    #[test]
    fn migration_survives_orphan_tickets() {
        let conn = legacy_db(
            &[("s1", "还在的会话")],
            &[
                ("t1", "s1", "正常", 100, ""),
                // 这条的载体会话不在了
                ("t-orphan", "gone", "孤儿", 200, "done"),
            ],
        );
        migrate(&conn).unwrap();

        let (brief, _, _, _, _, _) = migrated_row(&conn, "s1");
        assert_eq!(brief, "正常", "正常那条照样搬");
        // 孤儿那条没有对应的 sessions 行，所以什么都没写
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE id = 'gone'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0, "不该为一个孤儿 ticket 凭空建会话");
    }

    /// **最老那个版本（连 `role` 都没有）也能升上来。**
    ///
    /// 主 case 造的是 `main` 的 schema（有 `role`）；这个测「双层架构之前」的库，
    /// 那条路径上 `role` 的 `ALTER TABLE` 会真跑。两条路都得覆盖。
    #[test]
    fn migration_from_the_oldest_schema_without_role() {
        let conn = legacy_db_oldest(&[("s1", "t")], &[("t1", "s1", "要求", 100, "done")]);
        migrate(&conn).expect("最老的库也该升得上来");
        let (brief, by, _, _, closed_at, closed_by) = migrated_row(&conn, "s1");
        assert_eq!(brief, "要求");
        assert_eq!(by, "router-1");
        assert_eq!(closed_at, 900);
        assert_eq!(closed_by, "done");
        // `role` 被补上了，且默认 worker（老会话全是 worker）
        let role: String = conn
            .query_row("SELECT role FROM sessions WHERE id = 's1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(role, "worker");
    }

    /// **主 case 的 schema 必须和 `main` 上真实的一致**（`sessions` 已经有
    /// `role`）。这条测试守着 helper 本身：如果有人把 `legacy_db` 改成少造一列，
    /// 那它就不再验证真实的升级路径了 —— 而这种「测试造了个刚好能过的假库」
    /// 是最容易悄悄发生的退化。
    #[test]
    fn the_legacy_fixture_matches_the_real_main_schema() {
        let conn = legacy_db(&[], &[]);
        // `main` 的 sessions 有 6 列（含 role），缺的是本次新加的 6 列
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(sessions)").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for must in [
            "id",
            "title",
            "created_at",
            "updated_at",
            "provider_session",
            "role",
        ] {
            assert!(
                cols.contains(&must.to_string()),
                "main 的 sessions 有 {must}：{cols:?}"
            );
        }
        // 本次新加的那六列**不该**在起点里（否则 migrate 就没事可做了）
        for newly in [
            "brief",
            "dispatched_by",
            "conclusion",
            "dispatches",
            "closed_at",
            "closed_by",
        ] {
            assert!(
                !cols.contains(&newly.to_string()),
                "起点不该有 {newly}：{cols:?}"
            );
        }
        // tickets 的列名逐字对齐 main
        let tcols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(tickets)").unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for must in [
            "id",
            "title",
            "brief",
            "session_id",
            "router_id",
            "stage",
            "result",
            "report",
            "ended_at",
            "ended_by",
            "dispatches",
            "created_at",
            "updated_at",
        ] {
            assert!(
                tcols.contains(&must.to_string()),
                "tickets 有 {must}：{tcols:?}"
            );
        }
    }

    /// 全新库里没有 `tickets` 表：migrate 不能报错。
    #[test]
    fn migration_on_a_fresh_db_without_tickets_table_is_a_noop() {
        let conn = Connection::open_in_memory().unwrap();
        create_all(&conn).unwrap();
        migrate(&conn).expect("全新库不该因为「没有 tickets 表」而失败");
        // 再跑一遍（幂等）
        migrate(&conn).expect("第二遍也不该失败");
    }

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

    /// **空的 Router 会话必须能落盘并读回来。**
    ///
    /// 它的 id 被 `meta.router_session` 记着，是「那唯一一个协调层会话」的身份。
    /// 如果它跟普通空会话一样不落盘，下次启动时库里没有它 → `ensure_router`
    /// 只能重建一个、换一个新 id。表现就是：用户什么都没做，Router 却每次重启
    /// 都变成另一条会话，`meta` 里那条永远指向一个不存在的 id。
    ///
    /// 守两条路：`plan` 要把它留在 keep 里（不然不写），`load` 的 LEFT JOIN 要能
    /// 把零消息的会话行读回来（JOIN 会漏掉它）。
    #[test]
    fn an_empty_router_session_survives_a_restart() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        // 一个 worker（有内容，正常落盘）+ 一个一条消息都没有的 router
        let w = st.create();
        st.get_mut(&w).unwrap().messages.push(msg_user("干活"));
        let router = crate::agent::ChatSession::with_role(crate::agent::Role::Router);
        let rid = router.id.clone();
        st.sessions.push(router);

        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        let found = back.get(&rid).expect("空的 router 会话没落盘 / 没读回来");
        assert_eq!(found.role, crate::agent::Role::Router, "角色没保住");
        assert_eq!(found.message_count(), 0, "它确实是空的");
        // worker 也在，谁都没被误删
        assert!(back.get(&w).is_some());
    }

    /// **老数据补号**：库里 `seq IS NULL` 的会话，`load()` 之后都有号，且从 1 开始
    /// 连续、不重复。
    ///
    /// 这条钉的是 `SessionStore::recover_seq`。变异验证：把它整段删掉（或只恢复
    /// 水位线、不补号）→ 这条挂。
    #[test]
    fn loading_an_old_db_assigns_numbers_to_every_session() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        // 三条有内容的会话（空会话不落盘）
        let mut ids = Vec::new();
        for t in ["第一件", "第二件", "第三件"] {
            let id = st.push_new(crate::agent::ChatSession::new());
            st.get_mut(&id).unwrap().messages.push(msg_user(t));
            ids.push(id);
        }
        db.sync(&st).unwrap();

        // 模拟「`seq` 这一列加进来之前」的库：把号清空
        db.with(|c| c.execute_batch("UPDATE sessions SET seq = NULL"))
            .unwrap();

        let back = db.load().unwrap();
        assert_eq!(back.sessions.len(), ids.len(), "三条都读回来了");
        let seqs: Vec<u64> = back.sessions.iter().filter_map(|s| s.seq).collect();
        assert_eq!(seqs.len(), ids.len(), "每条都要有号（不能留 NULL）");
        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seqs.len(), "号不能重复：{seqs:?}");
        assert_eq!(
            sorted,
            (1..=ids.len() as u64).collect::<Vec<_>>(),
            "补出来的号从 1 开始连续：{seqs:?}"
        );
        // 补完的号也恢复出水位线了：下一个新会话要接着发
        let mut back = back;
        let next = back.push_new(crate::agent::ChatSession::new());
        let n = back.get(&next).unwrap().seq.expect("新会话有号");
        assert_eq!(
            n,
            ids.len() as u64 + 1,
            "水位线要从读出来的最大号恢复（拿到 {n}）"
        );
    }

    /// 补的号要**落盘**（否则重启一次补一遍，号每次都可能变）。
    ///
    /// 钉的是 `Saved::of` / `PlannedSession` / INSERT 三处都带上 `seq`。
    /// 变异验证：把 `Saved::of` 里那行 `seq: s.seq` 删掉 → 补的号只活在内存里，
    /// 这条挂。
    #[test]
    fn numbers_assigned_on_load_are_saved_back() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        let id = st.push_new(crate::agent::ChatSession::new());
        st.get_mut(&id).unwrap().messages.push(msg_user("活"));
        db.sync(&st).unwrap();
        db.with(|c| c.execute_batch("UPDATE sessions SET seq = NULL"))
            .unwrap();

        // load（补号）→ sync（应该把号写回去）
        let back = db.load().unwrap();
        db.sync(&back).unwrap();
        let n1 = back.get(&id).unwrap().seq.expect("内存里有号");
        // **直接查库**：补出来的号必须真的写进去了。
        //
        // 不能只靠「再 load 一次看号一样」—— 补号是按 `created_at` 排的确定性算法，
        // 每次补出来都是同一个号，那样测不出「有没有落盘」（变异体验证过：
        // `Saved::of` 里去掉 `seq` 之后，那个弱版本照样绿）。
        let in_db: Option<i64> = db
            .with(|c| {
                c.query_row("SELECT seq FROM sessions WHERE id = ?1", [&id], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(in_db, Some(n1 as i64), "号必须写进库，不能只活在内存里");
    }

    /// 普通空会话还是**不该**落盘 —— 「新对话」点了没说话，重启后不该冒出来。
    /// 上面那条给 router 开了例外，这里守的是「例外没有扩大到所有空会话」。
    #[test]
    fn an_empty_worker_session_still_does_not_persist() {
        let db = Db::in_memory().unwrap();
        let mut st = SessionStore::default();
        let empty = st.create(); // worker，没消息
        db.sync(&st).unwrap();
        let back = db.load().unwrap();
        assert!(
            back.get(&empty).is_none(),
            "空的 worker 会话不该落盘，否则每点一次「新对话」就攒一个"
        );
    }

    fn msg_user(t: &str) -> ChatMessage {
        ChatMessage::user(t)
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
        // 不再有「当前会话」：读回来的就是纯会话列表（见 `SessionStore` 的注释）
        let s0 = back.sessions.iter().find(|s| s.title == "会话0").unwrap();
        assert_eq!(db.load_messages(&s0.id).unwrap().len(), 3);
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

    #[test]
    fn clearing_a_session_does_not_resurrect_or_drop_messages() {
        let db = Db::in_memory().unwrap();
        let mut st = store_with(1, 3);
        db.sync(&st).unwrap();

        st.sessions[0].messages.clear();
        st.sessions[0].messages.push(msg_user("清空后第一条"));
        db.sync(&st).unwrap();

        let back = db.load().unwrap();
        // `load()` 只回会话头 + 摘要，**不预读任何一条会话的消息**（以前会把
        // 「活动会话」读进来；现在没有「活动会话」了 —— 谁被打开谁再按需读）。
        assert_eq!(
            back.sessions[0].message_count(),
            1,
            "摘要里的条数得是清空后的 1"
        );
        let msgs = db.load_messages(&back.sessions[0].id).unwrap();
        assert_eq!(msgs.len(), 1, "旧消息不该回来");
        match &msgs[0] {
            ChatMessage::User { text: t, .. } => assert_eq!(t, "清空后第一条"),
            _ => panic!("类型不对"),
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
}
