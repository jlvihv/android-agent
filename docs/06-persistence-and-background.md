# 持久化与后台运行

> 两个相关的问题：会话存哪、App 切到后台时 agent 还在不在干活。

---

## 1. 会话持久化：为什么用 SQLite 而不是 JSON

### 第一版（错）

把整个 `SessionStore` 序列化成一个 JSON，每 2 秒重写一次。

两个问题：

1. **全量重写。** 一次 agent 会话 100 条消息、每条平均 2KB（`bash` 输出很大）
   就是 200KB；10 个会话 2MB。流式输出时每 2 秒重写 2MB ≈ 持续 ~1MB/s
   的闪存写入。手机闪存会磨损，而且卡。
2. **崩溃丢全部。** 虽然用了「写 `.tmp` 再 `rename`」保证不产生半截文件，
   但一次都没写成功的话所有会话一起没。

### 现在

```sql
CREATE TABLE sessions (
  id, title, created_at, updated_at, provider_session
);
CREATE TABLE messages (
  session_id, seq, payload,        -- payload = ChatMessage 的 JSON
  PRIMARY KEY (session_id, seq)
) WITHOUT ROWID;
CREATE TABLE meta (k, v);          -- 存 active session id
```

`PRAGMA journal_mode = WAL` + `synchronous = NORMAL`。

### 关键：增量落盘

我们运气好 —— **消息是只追加不改的**。assistant 消息在整轮完成后才 push，
工具结果也是追加。所以内存里记一个水位线就够了：

```rust
/// session_id -> 已经落盘的消息条数
saved: HashMap<String, usize>,

// sync 时只追加这一截
for (i, m) in s.messages.iter().enumerate().skip(done) { INSERT ... }
```

`Db::sync()` 每 2 秒被定时器调一次（`AppState::sessions_dirty` 标志决定要不要跑）。
一条消息 = 一条 `INSERT`，成本 O(新增)，而不是 O(全部)。

有测试专门守这条：`appends_only_new_messages`。

### `bundled` feature

Android 上没有 `libsqlite3.so` 可链，所以开 `rusqlite` 的 `bundled`
（把 SQLite 源码一起编进来）。实测只让 `.so` 增加 **53 KB**
（5.89 MB → 5.94 MB）。

### 一个坑

`Db::load()` 一开始我锁了两次 `self.inner`（先 `let g = lock()` 查表，
再 `let mut g = lock()` 写水位线）—— 直接死锁。改成：

```rust
let mut g = self.inner.lock()?;
let store = { let g = &*g; /* 查询 */ store };   // 借用期间只读
g.saved.clear();                                  // 借用结束才动可变字段
```

顺带一个 Rust 借用检查的细节：`(*g).conn` 的不可变借用和 `(*g).saved`
的可变借用是**不同字段**，理论上可以共存；但 `stmt` 的生命周期跨越了整个
循环，所以还是得把作用域分开。

---

## 2. 后台运行

### 已经工作的部分（我一开始以为要改，其实不用）

- ~~**前台服务**保着进程：`isForeground=true types=0x40000000`（SPECIAL_USE）~~
  —— **后来去掉了**，见下面「前台服务去掉了」
- **agent 循环不依赖 SSE 客户端**。客户端断开时 `tx.send` 失败，但代码是
  `let _ = tx.send(...)` —— 失败被忽略，循环照跑、工具照执行、LLM 照调
- Rust 跑在自己的 tokio runtime 线程上，不受 WebView 挂起影响

实测：在后台期间日志照常出现 `chat → opencode-go` 和 `prompt cache 命中 77.4%`。

### 前台服务去掉了（2026-10-12）

原来有个 `AgentForegroundService`（SPECIAL_USE）常驻，只为「让进程别被系统回收」。
它必须挂一个常驻通知 —— Android 规定前台服务不能没有通知，没有「隐形前台服务」
这回事 —— 而那个通知除了「正在运行 · 127.0.0.1:端口」没有任何信息量。用户不想看
它，连同服务一起去掉了。

**代价**：切到后台的回合不再有保活。系统内存紧张时可以把进程杀掉，正在跑的回合
跟着死。**能接受的原因**：服务端每完成一步就把 assistant 消息写进会话（下面那节），
所以回来时看到的是「做到哪一步了」，不是丢整轮。

想把这个能力要回来，更好的形态是**只在回合进行中挂**：回合开始起服务、结束停服务，
通知就只在真有东西在跑时出现。但要先有一条「回合开始/结束」的 JS→Kotlin 通路，
现在没有。

> 顺带一个 Android 17（API 37）的坑：**通知那一行显示的是「启动器图标」，不再是
> `setSmallIcon` 的小图标**（小图标只在状态栏出现）。所以改小图标在通知行上看不到
> 变化 —— 不是没生效。想强制用回小图标要加 `EXTRA_PREFER_SMALL_ICON`。

### 缺的部分：前端回来时的同步

WebView 切到后台会挂起 JS，SSE 流可能就断了。切回来时本地视图是旧的。

**做法**（刻意选简单的）：

1. `visibilitychange` / `focus` / `pageshow` → 从 `/api/session` 拉最新状态
2. `/api/session` 返回 `busy`（服务端的 `turn_active` 标志）
3. 如果 `busy`，每 1.5 秒轮询一次直到结束，界面上显示「agent 正在运行…」

为什么用轮询而不是「重连 SSE」：重连 SSE 需要在服务端缓冲整个回合的
chunk 并支持断点续传（offset），复杂得多。而服务端每个步骤完成都会把
assistant 消息写进会话，所以轮询已经能看到逐步骤推进。

轮询是**增量**的：`rev` 不变直接 `unchanged`；变了也只重发「当前回合」
那一段（`append: true` + `replaceFrom`），不重传整段历史。不这样做的话，
跑长任务时每 1.5 秒要把整段会话（含每个 50KB 的 bash 输出）传一遍，
客户端还要再 `JSON.parse` 一遍。

`turn_active` 用 `Drop` guard 清：

```rust
struct TurnGuard(Core);
impl Drop for TurnGuard {
    fn drop(&mut self) { /* 清 busy + 标 dirty */ }
}
```

这样任何路径（正常结束、提前 return、panic）都会清 —— 否则前端会一直
显示「运行中」。

### 本地流在跑时不覆盖

`loadMessages()` 里：

```ts
if (!busy) messages = d.messages ?? [];   // 流还在跑就别换掉正在流式的对象
```

### 实测

1. 在 UI 里发「use bash to time sleep 6 and then report the alpine version」
2. 立刻按 Home 切到后台
3. 等 35 秒
4. `am start` 切回来

结果：完整显示工具调用折叠条 + 6.02s 计时 + Alpine v3.21 的回答。
日志确认这两次 LLM 调用都发生在后台期间。

---

## 3. 还欠的

- **会话管理操作的落盘**：新建/改名/删除/切换现在都会打 dirty 标志了，
  但**空会话**（建了没发消息）只有在下一轮 sync 时才写盘。可以接受。
- **`busy` 是全局的**，不是每会话的。多会话同时跑的时候会不准 ——
  目前一次只跑一个回合，不影响。
- **`sessions.db` 会一直长**。没做清理策略（比如保留最近 N 个会话、
  或者按时间清理）。工具输出是主要体积来源。
