# 定时任务

> **2026-11。** 到点了，往 Router 的队列里投一句话，把它叫醒。
> 代码：`core/src/schedule.rs`（模块文档里有完整的取舍记录）。

---

## 1. 一句话

**定时任务不指挥任何会话。它只能叫醒 Router。**

如果它能直接投给某个子会话，就有两套指挥系统（一套是用户经 Router，一套是定时器
直接给会话），迟早打架。定时任务也不需要知道目标是谁 —— 派不派活、派给谁，
是 Router 醒来之后的判断。

```
tick(10s) ──▶ 认领到点的 ──▶ 推进水位线（落库）──▶ router::wake(一句话)
                                                        │
                                                        ▼
                                              Router 醒来：派活 / 不派
```

---

## 2. 不存「下次触发时间」

上一版 `ticket::Stage` 的病根是**状态存了，但没有任何路径负责推进它**
（`Done` 零调用点 → 所有工作项烂在伪终态，见 `docs/16`）。定时任务要是存
`next_at`，同样的病立刻复发：总有某一条早退路径（队列满、模型没配、暂停）忘了推进它，
那个字段就永远停在过去。

所以：

| 存什么 | 为什么它算不出来 |
| --- | --- |
| `prompt` | 用户当初写的那句话，一字不改 |
| `every_secs` 或 `cron`（**恰好一个非空**） | 用户的意图 |
| `created_at` | 固定间隔那个节奏的**锚**（`第 k 次 = created_at + k×周期`） |
| `handled_through` | **水位线**：截止这一刻的触发点都处理过了 |
| `paused_at` | 人的决定，推不出来 |

**没有「建它时的时区」这一列。** 曾经有过（`tz_offset_minutes`），但没有任何代码读它
—— 算触发点永远用**此刻**的偏移。一个没人读的列虽然性质和无害（不可变快照，不是可变
标签），但视觉上和「存了没人推的 `Stage`」一模一样：下一个人看到它就会猜它在被用。
真要做「你当初在 UTC+8 设的 9 点」那种提示时再加（表还没发布过，改 `CREATE TABLE` 就行）。

**「下次什么时候」每次现算**（`schedule::due_at` / `next_at`），只在 HTTP 响应里
露脸，永远不落库。

### 2.1 水位线不是「上次触发时间」

它有两个别的含义，正是这两个让「暂停/恢复」「投递失败」不需要额外字段：

- **暂停 / 恢复**：两个方向都把水位线推到 `now` ⇒ 停了一周再开**不会**炸出一堆提醒
- **投递失败**：这一次也算「处理过了」⇒ 不重试（重试会每 10 秒撞一次，日志变噪音）

唯一的推进点是 `Schedules::claim_due`（在锁里推进，本身就是幂等栅栏）。

---

## 3. 防风暴

水位线落后很久（进程睡了一天、系统时钟被改）时，从水位线往后可能有几十个触发点都
早于现在。**每个 tick 每条任务最多触发一次**，取「现在之前最近的那个」：

```
due_at(task, now):
    停用中            → None
    固定间隔：k = (now - created_at) / 周期；due = created_at + k×周期
    cron：问库「now 之前（含 now）的最近一次」
    due > handled_through ? Some(due) : None
```

三条边界有测试钉着（`schedule.rs` 的 tests）：

| 情形 | 期望 | 变异会怎样挂 |
| --- | --- | --- |
| 水位线落后 100 个周期 | 只回 **1** 个，且是**最近**那个 | 改成「往后数第一个」或循环攒齐 → 变成补跑 |
| 水位线在未来（时钟回拨） | `None` | 去掉那个比较 → 会立刻炸出一次 |
| 现在正好卡在触发点 | 就是它（cron 用 `inclusive`） | `find_previous_occurrence(_, false)` → 会报上一次 |

---

## 4. 周期：两种，不是一种

```rust
pub enum Recurrence {
    Every(Period),        // 每 N 秒（60s..30d），锚在创建时刻
    Cron(Box<CronExpr>),  // cron 表达式，挂在墙上时钟
}
```

两种语义确实不同，各有各的用处：

- 「每 90 分钟」cron 表达不了（cron 的最小粒度是分钟，但 `*/90` 是「每小时的第 0 分钟
  和…」那种怪东西），而 `Every` 天然能表达
- 「每天 9 点」「每周一 9 点」`Every` 表达不了（24 小时会随创建时刻漂），cron 天然能

### 4.1 为什么引 `croner`

手写 cron 要处理月末、闰年、DST 边界 —— 没有一天能写对。评估过四个候选：

| crate | 依赖问题 |
| --- | --- |
| **`croner` 4.0.3** | 默认拉 `chrono`（`clock` feature → `iana-time-zone` → `windows-*` 一串）。**关掉默认 feature 之后只用 `chrono` 的 `alloc`** —— 实测新增 12 个 crate |
| `cron` 0.17 | 强依赖 `chrono` **带 `clock`**（不可关），还多 `phf` + `winnow` |
| `saffron` 0.1 | 只吃 `chrono` + `nom`，但没有「给定时刻之后的下一次」这种 API（要自己按月迭代），而且是 2019 年的 crate |
| `cronp` | 新的，要 `jiff`；`jiff` 带 tzdb 就上 MB 了 |

`croner` 的 `find_next_occurrence` / `find_previous_occurrence` 正好是我们要的两个
原语，而且它对 DST 的处理（gap / overlap / fold）有专门的测试。

**代价**：`Cargo.lock` 多了 12 个 crate（`chrono`、`croner`、`derive_builder` 三件套、
`strum` 三件套、`darling` 三件套、`fnv`、`ident_case`），aarch64 的 `.so` 胖了
**148 KB / 1.7%**（8,724,024 → 8,876,088 字节，release + strip，实测）。

### 4.2 时区

**算触发点永远用「此刻」的偏移**，不在建任务时定死。用户会旅行、会手动改时区、DST 会切。

- 偏移从**宿主 libc** 读（`localtime_r` 的 `tm_gmtoff`）：桌面和安卓都走同一条路，
  一次百纳秒级，**每 tick 读一次**（不是每条任务一次）—— 不需要缓存，也就没有
  「缓存什么时候失效」这个问题
- 安卓壳注入的 `AndroidEnv.tz_offset_minutes` 只作**兜底**（`libc` 读不到的场合：
  32 位 `time_t` 的 2038 之后）

**产品决策：在北京设的「每天 9 点」，飞到伦敦之后响的是伦敦的 9 点。**

理由：任务属于**这台机器**，不属于某个时区。「每天 9 点」这五个字的全部意义是
「在我醒来的时候」，而「醒来」是随地理位置变的。反过来（钉死北京 9 点 = 伦敦凌晨 1 点）
会让一条「每天早上提醒我」的任务在旅行时彻底失效 —— 而这个功能恰恰是给
「人不在的时候」用的。HTTP 响应里带一个**顶层的** `tzOffsetMinutes`（此刻的偏移，
给界面显示「按当前时区算」用），**不是每条任务一份** —— 每任务一份没有消费者。

**已知代价**：跨 DST 边界那一刻最多偏一小时（用「此刻的偏移」把整个搜索窗口当一个
固定偏移算）。要精确得带时区数据库（安卓上几 MB 的 tzdb），这不是这个功能的量级。

---

## 5. 与 Router 的接口

### 5.1 `Origin` 加了第四个变体

```rust
pub enum Origin {
    Human,
    Dispatch,
    WorkerReport { session_id: String },
    Schedule { task_id: String },   // 新
}
```

**为什么不能复用**（`agent.rs` 的变体注释里有完整版）：

- `Human` → 一句三天前写下的话会取得「用户刚才同意过」的身份，而 Router 的提示词里
  明写着「用户是否已经同意某件事，永远以**用户在这个对话里说的**为准」
- `Dispatch` → 会翻转「这一轮算不算派活」那个判定，进而影响 `conclusion` 的覆盖
- `WorkerReport` → 带 `session_id`、前端会画成「子会话报告」

`Origin` **不进请求体**（`to_openai` / `build_responses_input` 都不读它），所以对
provider 零风险。**要让模型也分得清，只能靠正文里的标记**：

```
<schedule task="0199e1f0-…">
每 30 分钟看一下那台机器还活着没
</schedule>
（上面这条是**定时器**到点投的，不是用户此刻说的：它是当初设这条闹钟时写下的原话。
 该不该派活、派给谁，你自己判断；不派就直接说明，别为了响应它而硬派。）
```

和 `<worker_report>` 同一个道理（见 `docs/16`）。正文里**不带时间戳**：Router 现在
没有「现在几点」这个概念，只给它一条会造出「一条消息里有时钟、别处没有」的不一致。

### 5.2 `router::wake` —— 叫醒 Router 的唯一实现

以前 `report_back` 自己写了一遍「`enqueue` + `continue_with_queued`」。多一处就多一个
「忘了叫醒、消息躺在队列里没人取」的机会。现在两条路都走它：

```rust
pub fn wake(core: &Core, router: &str, msg: QueuedMsg) -> Result<(), QueuedMsg>
```

队列满 → `Err(消息原样交还)`，由调用方决定怎么记一笔（回执 warn、定时任务「这一次丢了」），
**都不重试**。

---

## 6. 执行顺序：先落水位线，再投递

崩在中间的话，这一次提醒丢了；反过来的话会投两次（同一次提醒开两个回合、白烧两份
token）。**宁可少叫一次。**

队列满时按「用户拍板」的口径：`claim_due` 照样推水位线（这一次算处理过），
`fire` 投不进去就 `warn` 一行 —— 不重试。

---

## 7. 三层，各自的职责

| 层 | 在哪 | 干什么 | 不该干什么 |
| --- | --- | --- | --- |
| 推导 | `due_at` / `next_at` | 纯函数，给定任务+now+偏移算「响不响、响哪一次」 | 不碰锁、不分配、不 IO |
| 存储 | `Schedules`（`AppState::schedules`） | 内存表 + **唯一写点**（水位线只能从 `claim_due` 过）；写库全在锁外 | 锁里不做 IO；不认识 tick |
| 循环 | `run`（`lib.rs` 里 spawn 一次） | 一个 10 秒的 `tokio::interval`：认领 → 落水位线 → 投递 | 不做格式化（真到点才拼正文） |
| 触发 | `fire` | 拼正文、调 `router::wake` | **投递目标只有 Router** |

### 7.0 锁里不做 IO

`create` / `set_paused` / `delete` 三处写操作都是「**锁里只改内存，出锁再写库**」。
理由：SQLite 一次 INSERT 在手机闪存上几毫秒到几十毫秒，而这期间 `claim_due`
（10 秒 tick 的唯一入口）会被挡住 —— `persist_cursor` 特意用了 `spawn_blocking`，
`save_sessions` 也把序列化移出锁外，三处该一致。

写库失败时内存要回到原样（否则界面上多/少一条任务，重启之后又对不上）：

| 操作 | 顺序 | 失败时 |
| --- | --- | --- |
| `create` | 锁里查上限 + **占位** → 出锁写库 | 撤掉占位（`retain`） |
| `set_paused` | 锁里存一份 `before` + 改内存 → 出锁写库 | 用 `before` **整份换回去**（比逐字段还原可靠） |
| `delete` | **先写库** → 出锁改内存 | 库失败就**什么都不动**（内存是权威副本，多响一次比悄悄少一条用户闹钟安全） |

三条都有测试（`core/src/schedule.rs` 的 `*_rolls_back_*` / `*_does_not_touch_memory_*`），
造失败的办法是把 `schedules` 表 `DROP` 掉。

`db.rs` 那层只做 SQL：它不认识 `Recurrence`，也不判「该不该触发」。库里用
「`every_secs` / `cron` 恰好一个 NULL」表达「这条是哪种周期」（有 CHECK 钉着），
翻译成本收敛在 `db.rs` 的唯一一处。

### 7.1 为什么不搭落盘定时器的便车

`lib.rs` 那个 2 秒 loop 的语义是「**有脏数据才写**」（`sessions_dirty.swap(false)`），
而这里要的是「**每 tick 都看一遍有没有到点**」。混在一起的话，调落盘频率会顺手改掉
闹钟的准时性 —— 两件无关的事不该共用一个周期。

---

## 8. 管理入口（两个，一套校验）

- **HTTP**：`GET /api/schedules`、`POST /api/schedules`、
  `POST /api/schedules/paused`、`POST /api/schedules/delete`
- **设置页**：`web/src/components/ScheduleSection.svelte`
- **Router 的工具（2 个）**：`schedule_task`（建）、`schedules`（list/pause/resume/delete）

三条入口都只是调 `schedule::create` / `set_state` / `rows` —— **校验只有一份**，
所以「工具能建、HTTP 不能」这种不一致不可能发生。

**为什么工具只加 2 个而不是 3 个**：`schedule_task` 是高频路径（用户说「每小时提醒我」），
剩下三个低频 CRUD 收进一个多动作工具。工具面从 4 → 6 而不是 7，而「选错工具」的概率
跟工具数一起涨。

**`schedule_task` 的描述第一句就是拦截**：它是 Router 工具面里唯一一个「会一直响下去」
的动作，而模型很容易把「帮我查一下 X」听成「长期盯着 X」。

---

## 9. 上限

| 项 | 值 | 为什么 |
| --- | --- | --- |
| 条数 | 32（`MAX_SCHEDULES`） | Router 也能建，模型一时兴起就能排一堆；每条到点都开一轮 LLM 请求 |
| `prompt` | 2000 字（`MAX_PROMPT_CHARS`），**超了报错不截断** | 截断等于悄悄改掉用户写的那句话 |
| 最短周期 | 60 秒（`Period::MIN_SECS`，类型层面钉住） | 比它快没有意义：一轮就是一个 LLM 请求 |
| 最长固定间隔 | 30 天（`Period::MAX_SECS`） | 比它慢的用 cron 写 |

---

## 9.5 排队中的那一条也带来源

服务端 `queued_view` 给 `fromWorker` / `fromSchedule`，前端这条链路有三处必须一路带着
（否则排队前后会换个样子）：

1. `QueueMirror#replace`（`web/src/lib/queue.svelte.ts`）
2. `App` 的 `queuedMsgs`（拼成 `Message` 时）
3. `Chat#onData` 的 `data-queued-delivered`（`deliver` 返回的那几条推进正文时）

`web/tests/queue.test.ts` 钉住 1 和 3。

## 10. 已知代价

1. **只有核心进程活着时才生效**：Android 上 Doze / 被 force-stop、桌面关掉核心都不响。
   **没有补跑**（错过了就永远错过，只在日志里留一行 warn）。
2. **跨 DST 边界偏最多一小时**（见 §4.2）。
3. **单次投递失败就丢这一次**（Router 队列满时），不重试。
4. **`Origin` 加变体对降级不友好**：老二进制读到 `{"schedule":…}` 会 serde 失败，
   而 `db.rs` 的策略是「任何一条解析不了就整个会话报错」—— 降级后 **Router 那条会话**
   读不出来（其它会话不受影响）。`WorkerReport` 当初也是这么加的。
5. **加工具会让所有现存 Router 会话的 prompt cache 前缀失效一次**（工具表是可缓存
   前缀的一部分，见 `docs/03`）。一次性冷启动。
6. **系统时钟被回拨**会让水位线比 `due` 大 → 那段时间不触发（可接受）。
7. **时区只到「固定偏移」**，不是 IANA 时区。跨 DST 的语义就是 §4.2 说的那样。
