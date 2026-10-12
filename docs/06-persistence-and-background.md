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

- **前台服务**保着进程：`isForeground=true types=0x40000000`（SPECIAL_USE）
  —— 中间去掉过一阵，现在是**回合范围内**的版本，见下一节
- **agent 循环不依赖 SSE 客户端**。客户端断开时 `tx.send` 失败，但代码是
  `let _ = tx.send(...)` —— 失败被忽略，循环照跑、工具照执行、LLM 照调
- Rust 跑在自己的 tokio runtime 线程上，不受 WebView 挂起影响

实测：在后台期间日志照常出现 `chat → opencode-go` 和 `prompt cache 命中 77.4%`。

### 前台服务：只在回合进行中挂（2026-10-12）

历史上它是个**常驻**服务，只为「让进程别被系统回收」。它必须挂一个常驻通知
（Android 规定前台服务不能没有通知，没有「隐形前台服务」这回事），而那个通知
除了「正在运行 · 127.0.0.1:端口」没有任何信息量 —— 所以连服务一起删掉了。

删除的代价当时就写在这页：切到后台的回合不再有保活，内存紧的时候进程会被杀，
正在跑的回合跟着死。当时判断可以接受，因为每完成一步都落盘。

**做浏览器之后这个帐算不平了**：进程被杀 = agent 那只 WebView 没了 ——
页面、滚动位置、**登录态后面的会话状态**全没（cookie 在磁盘上不丢）。
接个电话回来发现 agent 的活儿从一半开始重来，这是用户能直接感受到的。

所以把服务请回来了，但换了形态：

- **回合开始起、结束停**（`TurnGuard` 建/毁时发通知），通知只在真有东西在跑时出现；
- 文案说的是实话：「agent 正在处理 · 点开回到对话」；
- `START_NOT_STICKY`：被杀后不重建 —— 回合已经跟着没了，重建个空服务没意义。

通路是 `TurnHost`（uniffi callback）—— 和 `BrowserHost` / `DeviceHost` 同一形状。
Rust 侧的 `begin_turn`/`end_turn` 在会话数变化时调一次（幂等，重复收到同样的值很正常）。

两个平台限制决定了它的边界：

- **Android 12+ 不许从后台起前台服务。** 回合通常是人在界面上发起的（那时 App 可见），
  顺带在 [MainActivity] 里问一次通知权限；脚本（agentctl）在后头起的回合，服务
  起不来就只降级、不报错。
- **通知权限**（Android 13+）没有也能跑，但通知不显示。所以只在真起回合时问一次，
  不在启动时问。

实测（Pixel 10）：

```
回合进行中： dumpsys activity services → AgentForegroundService isForeground=true types=0x40000000
              dumpsys notification        → channel=agent-turn, flags=ONGOING_EVENT|FOREGROUND_SERVICE
回合结束后： 服务 0 个、通知 0 条
```

真正要验的是「来电」那个场景（切后台、屏幕不在我们手里），所以又跑了两轮：

- `sleep 60` 的 bash 回合：按 HOME 回桌面 → 服务与进程都在 → 回合在后台跑完；
- **浏览器回合**：切后台后 `browser navigate` + `get_text` 照常完成 —— 无头 WebView
  不需要可见，这正是它和「共用用户的 WebView」的分水岭。

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
