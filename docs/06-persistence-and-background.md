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

### 只把要用的会话放在内存里

启动时 `Db::load()` 只读**全部会话的元数据 + 活动会话的消息**；别的会话只留摘要
（`ChatSession::stub`：条数、用户消息数——侧边栏和水位线要的就这些）。会话多了以后
（每条都带几十 KB 的 bash 输出），全放内存是几十 MB，而大部分再也不会打开。

- 要用时 `AppState::ensure_loaded` 从库里读回来：开始回合、`/api/session`、
  `/api/debug/request`。搜索临时读、扫完就扔，不装回去。
- 落盘定时器每次写完盘调 `AppState::evict_idle` 放掉：不是当前会话、没有回合在跑、
  读进来超过一分钟、**而且内存里的样子已经全部落盘**（`Db::is_synced`）。
- `sync` 碰到消息不在内存里的会话只更新会话头，消息一条都不动 —— 空的 `messages`
  绝不能被当成「清空过」。

### 读不出来的消息：整个会话报错，不跳过

`load_messages` 遇到解析不了的 payload（装回旧版本时，新版本写的变体它不认识）
**整个报错**。以前是 `filter_map(.ok())` 静默跳过：内存比库里少一条，水位线跟着
变小，下次落盘从那个位置 `INSERT OR REPLACE`，就把库里后面的消息覆盖了。现在这个
会话保持「不在内存里」，库里的行原样不动，换回能读的版本还看得到。

### 清空过：`epoch`

`clear()` 会把 `ChatSession::epoch` 加一，`Saved` 里也记着它。只看条数的话，清空后
两秒内又长过了旧长度就看不出清空过，库里前面那几条旧消息会留下来。

### 磁盘上的副产物（`gc.rs`）

图片副本（`images/`）、附件（`attachments/`）、浏览器截图（`/root/.browser/`）、
bash 溢出的输出（`/tmp/agent-out-*.log`）以前只增不减。现在删会话 / 清空会话时
清它引用的那些；启动时再扫一遍，删掉库里没有任何消息引用、又是这次启动之前写的
（沙箱里那两类再多宽限一天：模型可能还在 bash 里用它们）。上传了没发的附件放
一小时，下次上传时清掉。

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

通路是核心总线上的一个事件：`Event::TurnActive { active }`（`begin_turn`/`end_turn` 里发，
会话数一变就发一次；订阅者收到重复值也无所谓）。手机壳订阅 `/api/turns/stream`（SSE）拿到它，
起/停前台服务。（**以前**这条走的是 UniFFI 回调 `TurnHost`，而「连着的那台电脑」那条**没有** ——
2026-10-14 统一成一条路，见下面「一条路：本地和远程」。）

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

> ⚠️ 上面这两条只在**切后台之后的头一分钟**成立。再往下看：系统会把整个进程冻住。

> 顺带一个 Android 17（API 37）的坑：**通知那一行显示的是「启动器图标」，不再是
> `setSmallIcon` 的小图标**（小图标只在状态栏出现）。所以改小图标在通知行上看不到
> 变化 —— 不是没生效。想强制用回小图标要加 `EXTRA_PREFER_SMALL_ICON`。

### 通知栏：那一行实时的内容（2026-10-14）

前面那条通知「agent 正在处理」说了实话，但**没有信息量**：手机撂在桌上时，人唯一能看见的
就是它，而它不会告诉你现在跑到哪一步了。这一版把它变成**实时更新**的 —— 在跑哪条命令、
在想、在写什么，一眼看得出。形状**照抄 OpenMinis**（`SessionActivityTracker` +
`AgentForegroundService` + `AgentNotificationContent` + `BackgroundTaskNotifier`）：

```
Rust status::Reporter ──Event::Turn(JSON)──▶ 总线（/api/ws、/api/turns/stream）
                                                        │ SSE
                        壳 TurnStream ─▶ LiveTurnStatus（唯一事实来源）
                                                        │ 变化
                        AgentForegroundService ◀────────┘  就地把进行中那条改一遍
                        AgentNotifications.showDone ◀─────── 刚完事那一条（独立于服务）
```

关键的一点：OpenMinis 的事实来源在 Kotlin（agent loop 也在 Kotlin），**我们的在 Rust**
（loop 在 Rust）—— 所以是他们订阅 tracker，我们是**同一个形状、状态在边界另一侧**；
「订阅」那条线在他们那儿是进程内的 StateFlow，在我们这儿是核心总线上的一个事件流。
划分：Rust 负责「什么算一个阶段、细节取哪一行」（`core/src/status.rs`，有测试），
壳负责「长什么样」（`AgentNotifications.kt`，文案在 `strings.xml`）。

推什么（`status::Snapshot` 的 JSON）：

| 字段 | 说明 |
|---|---|
| `phase` | `thinking` / `streaming` / `tool` / `done` |
| `tool` `detail` | 工具名 + 一行细节（`bash` 的命令首行、`read`/`edit` 的路径、流式正文的最后一行）|
| `title` `sid` | 哪个会话（多会话同时跑时通知栏要分得清）|
| `started_at_ms` | 本回合开始（**wall clock**，不是 elapsedRealtime，见下）|
| `finished_at_ms` `ok` `stopped` | 结束那一条才有 |

几个刻意的地方：

- **频度在 Rust 侧控**。正文逐块流过来（一条长回复几千块），每块过一遍 FFI + 刷一次通知是
  白费：流式的更新按 **0.5s** 攒着发，**阶段切换立刻发**（想→写、轮到哪个工具、跑完）。
  同一个阶段里的重复推送直接丢掉。
- **推的不是会话记录，是核心里裁好的那一小份**（这条流走的是核心总线 + SSE，很容易
  让人以为把聊天记录也发了）。实际上每一层都有界：细节（命令首行 / 路径 / 回复尾行）
  只取一行、截到 80 字；回复预览只看正文**尾部 2048 字**；会话名本身 ≤29 字。
  实测量过：一个回合里 **12 秒 11 帧，中位 216 字节、最大 300 字节**。
  内存上也不复制：核心里每个**在跑的**会话留一份 ~200B 的快照（只为给晚连上的订阅者
  补一帧，下次回合覆盖它自己那行），壳里每个会话一个几字段的结构体 ——
  **完整正文从来没进过这条路**（它只在 agent loop 自己的局部变量里）。
  顺带说清：「推完整会话」的是**另一个客户端** —— WebView 自己那条 `/api/chat` 的 SSE，
  它只管前端渲染，跟通知这条路无关。
- **参数是边流边抠的**。工具名一到就显示（那时参数 JSON 还没写完），命令/路径用
  `status::string_arg` 从半截 JSON 里抠 —— 和前端 `MessageList.summarize` 的 `partialArgs`
  同一条思路（我们只要一个字段，所以手写扫描，不引 regex）。工具**真开跑之前**再推一次
  （那时参数才完整）。
- **时间戳是 wall clock**。OpenMinis 那边踩过：`elapsedRealtime` 直接喂给 `setWhen`，计时器
  原点落在「开机时刻」，一台开了五天的手机显示 `120:00:00`。我们让 Rust 直接给 wall clock，
  类型上就不给混用留口子。
- **图标每阶段一个**（`ic_notif_work` / `ic_notif_shell` / `ic_notif_file` / `ic_notif_done`），
  必须是只有 alpha 的单色掩码（系统按主题染色，实心圆点会被染成一坨）。
- **没有进度条、没有秒表、没有「停止」按钮** —— 和 OpenMinis 一样。agent 报不出诚实的百分比，
  而一个停不下来的计时器比没有计时器更糟。

**完事那一条**（独立 id；人在看着时走安静的通道，不在时走会响的）：跑完那一刻发，**总是发** ——
进行中那条的命跟着前台服务，服务一停它就没了，而「通知自己消失了」等于什么都没说：人回来看到
空荡荡的通知栏，还是不知道到底跑完没有。所以结果留成一条**能点掉**的通知，回到 App 看过
（`MainActivity.onResume` → `clearDone`）或自己划掉才没。

响不响看人在不在：正看着 App 时走 `agent-turn`（LOW，不响不弹 —— 他刚看完那件事）；
不在看着时走 `agent-done`（DEFAULT，响 + 小红点）。平台没有「单条静音」这个能力
（公开 SDK 里 `setSilent` 已经没了，而通知的 sound 为 null 时会退回**渠道**的声音），
所以只能靠换渠道。

**和 OpenMinis 刻意不同的两处**（不是漏抄）：

1. **App 在前台时不收起进行中那条**。他们收（眼前已有的东西不必再通知一遍），因为那边前台时
   本来就不需要保活；我们这边前台服务同时承担保活（见下面「深度后台」：离开焦点 60~75 秒
   整个进程会被 freezer 冻住），收起来等于每切一次前后台都要重新 promote 一次。宁可多一条
   安静的状态行，也不要一个可能收不回来的服务。
2. **没有浮动胶囊**，但 **Live Updates（Android 16+ 的常驻芯片）做了**。前者是 OpenMinis
   那套 overlay（要 `SYSTEM_ALERT_WINDOW`）；后者是真的接上了，靠两件事：

   - **`POST_PROMOTED_NOTIFICATIONS` 权限**（`AndroidManifest.xml`）。Android 17 新加的
     `normal|appop` 权限 —— 安装时自动授予，但**必须声明**：没声明的话
     `canPostPromotedNotifications()` 一律回 false（实测：同一台 Pixel 10 上，OpenMinis
     声明了它就有芯片，我们没声明就一直只能当普通通知）。AOSP 16 的框架里还没这个权限，
     那时只看 rich-ongoing 特性开关。
   - **上色按版本分开写**：16 源码（`android16-release` 的 `Notification.java`）里
     `hasPromotableCharacteristics()` 最后一行是 `isColorizedRequested() &&
     hasPromotableStyle()` —— **必须请求上色**；而 17 真机 `framework.jar` 反汇编出来是
     `… && !isColorizedRequested() && !isBridged()` —— **请求了上色反而不给提升**。
     两边正相反（Google 改了），所以 `setColorized(SDK_INT < 37)`。

   真机上把两道都对完（权限 + 上色）之后：`dumpsys notification` 里那条的 flags 变成
   `ONGOING_EVENT|ONLY_ALERT_ONCE|NO_CLEAR|FOREGROUND_SERVICE|**PROMOTED_ONGOING**`，
   状态栏/锁屏上出现芯片，通知栏里图标换成 App 图标（系统对提升过的通知就是这么画的）。
   不提升时 logcat 里（tag `AgentNotif`）会把每一项前提都列出来 —— 包括上色、样式
   （`android.template`）和那条 `hasPromotableCharacteristics()=false`，不靠猜。

   ⚠️ **验证的时候注意**：芯片上的**字**只在 **App 不在前台**时才画。App 正开着（你在
   看聊天）时，系统只给一枚小图标 —— 你已经在看它了，不必再重复一遍。（SystemUI 的
   `PromotedNotificationContentModel` 里就带着 `isAppVisible`/`lastAppVisibleTime`。）
   实踩：盯着 App 截图，看见「只有图标、没字」，差点去排查一个不存在的 bug；按一下
   HOME 再截，同一秒就是 `>_ 运行 TIM…` 那种药丸——芯片文字的口径和 OpenMinis 的
   `chipContentText` 一样：CJK 算两格、上限 9 格 / 6 个字符，所以四个汉字正好截成
   「写胰岛素…」。

一个实测踩到的坑（改了两处才收干净）：**更新前台服务的通知要用 `startForeground()`，不要用
`NotificationManager.notify()`**。`notify()` 能把同 id 的通知换掉，但会把记录上的
`FLAG_FOREGROUND_SERVICE` 抹掉 —— 于是**服务死掉时系统不再顺手收它**，通知栏里留一条
「已完成」不走。另外 `onDestroy` 里除了 `stopForeground(REMOVE)` 再按 id `cancel()` 一次。

实测（模拟器 API 35；`dumpsys notification --noredact` 看 `Notification List`）：

```
进行中： id=1001 channel=agent-turn flags=ONGOING_EVENT|ONLY_ALERT_ONCE|NO_CLEAR|FOREGROUND_SERVICE
         icon=ic_notif_file  title=「用 read 看一下 /etc/alpine-relea…」 text=「读取 /etc/os-release」
         （一路看到：正在处理… → 读取 /etc/os-release → 正在思考… → 正文最后一行）
回合结束：id=1001 消失（服务停 → 通知跟着走），但 id=1002 已经在那儿了：
         channel=agent-turn（人正看着，所以不响）title=「已完成 用 read 看一下 …」
切后台跑完：id=1002 channel=agent-done flags=ONLY_ALERT_ONCE|AUTO_CANCEL
         title=「已完成 用 read 看一下 /etc/alpine-relea…」 text=「（约 330 字）」
回到前台：id=1002 被 onResume 清掉
```

**没验到的**：Live Updates（API 36+）那条分支只有真机（Pixel 10 / Android 17）才走得到，
模拟器是 35，只验了「不满足条件时干净退回普通通知」。

### 一条路：本地和远程（2026-10-14）

问得对：既然本地核心也是 HTTP 服务，「连到电脑上」那条为什么收不到状态？

**因为本地那条根本没走 HTTP**：同一个进程里，agent loop 用一个 UniFFI 回调
（`TurnHost.status`）直接把 JSON 递给壳 —— 最短路径，当初就这么写的。而「手机连到电脑上的
核心」那条，**什么通道都没有**：电脑上的核心只把事件发给它的 HTTP 客户端（前端 SSE、`/api/ws`），
而 WebView 的 JS 在 App 切到后台时会被系统挂起 —— 而通知栏恰恰是「App 不在前台」时才有用。

于是两份形状、同一份状态语义要各维护一遍。现在合成一条：

```
Rust  status::Reporter ──▶ Event::Turn{status} / Event::TurnActive{active}   （总线）
                              ├─ /api/ws              前端（原来就有）
                              └─ /api/turns/stream    手机壳（SSE，带补发）

壳    TurnStream(base, token) ──▶ LiveTurnStatus ──▶ 前台服务 + 通知（一行没改）

base = http://127.0.0.1:<本地端口>  或  http://<电脑>:<端口>
token = 本地那本凭据（`files/token`）或配对时存下的 token
```

为什么用 SSE 而不是 `/api/ws`：壳那边**只收不发**，而 Kotlin 写一个 WebSocket 客户端要自己
做握手和帧解析；SSE 就是「HTTP 长连接 + 几行 `data:`」，`HttpURLConnection` 读一行处理一行
就完了。两边发的是**同一个 `Event` 的 JSON**。

删掉的东西比加的多：`TurnApi`（Rust 里的 trait）、`TurnAdapter`、`TurnHost`（uniffi callback
interface）、壳里的 `TurnHostImpl`。跨语言边界现在只剩 `DeviceHost` 和 `BrowserHost` 两条 ——
都是「非它不可」的平台能力，而回合状态本来就不需要一条专用回调。

几个实现上的选择：

- **连上先补一帧**（`AppState::turn_snapshot`）：订阅者可能在回合跑了一半才连上（壳刚起、刚从
  系统冻结里醒来、网刚恢复），而一条 `bash` 可能几分钟不产生变化 —— 不补的话它的通知栏就一直空着。
- **断线重连带退避**：连上了退回 2 秒，一直连不上就翻倍到 30 秒（旧核心没这个端点时不要每 2 秒刷日志），
  所以核心重启、网抖动都能自己接回来。
- **前台服务的起停 = 所有源的总和**：连着电脑时手机自己的核心也还跑着（`just phone` 那种从电脑驱动
  手机的回合走的就是它），两边都可能各自开一个回合。
- **URL 和 token 的来处**：本地是 `AgentApp.port` + `core.token()`；远程是 SharedPreferences 里的
  `remote_core` + `paired_<id>`（配对时存下的）。切核心/回到前台都只是重新算一遍目标清单（幂等）。

边界（和以前一样，只是现在说清了）：

- **手机 App 切后台 60~75 秒会被系统冻住**（下面那节），那段收不到东西；回到前台重连再补一帧。
- 回合由**电脑上的脚本**发起、而手机 App 在后台时：起不了前台服务（Android 12+ 的限制），
  所以只能等回到 App 补一条。

实测（模拟器当客户端 + 本机另起一个临时核心当「电脑」，独立数据目录，互不干扰）：

重启核心时用 `scripts/restart-core.sh`：它按「先编译（编不动就什么都不动）→ 等 `busy=false`
（等你这句话答完、消息已经发到手机上、并且**落盘**）→ 才杀 → 起新的 → 验健康（起不来就用
备份的旧二进制兜回去）→ 顺手用 adb 把手机上的 App 重开一次」的顺序做，从 agent 的 bash 里跑会
自己 `setsid` 脱离会话（agent 的进程树挂在被杀的核心里）。手机**不用重新配对**：配对给手机的
就是核心自己的 token，它在会话库里、跨重启不变（换的只是新设备用的那个 6 位码）。

```
本机临时核心（8799）：回合跑在电脑上
模拟器（当手机）：    TurnStream: 订阅回合状态: http://127.0.0.1:47817
                                   http://10.0.2.2:8799      ← 两个源同时订着
运行中：              前台服务自己起来，通知文本跟着变
                      「运行 sleep 8 && echo "done after 8s"」 → 「正在思考…」 → 「正在处理…」
跑完：                id=1002「已完成 先用 bash 跑 sleep 8…」 + 回复正文那一行
```

同一天在本机（127.0.0.1 那条）也回归验了一遍：订阅日志、进行中那条、完成那条都在。

### 深度后台：整个进程会被冻住（2026-10-13）

**症状**：手机撂下一会儿之后，从电脑发过去的请求**原地挂住** —— 不报错、不超时，
`curl` 就一直等；用户一碰手机（App 到前台），**瞬间**全通了。

第一反应都会猜「WebView 在后台不能操作」。不是。冻住的是**整个进程**：
`/api/health` 这种纯本地、根本不碰 WebView 的请求一样挂 —— 连沙箱里 `echo` 都跑不了。

**真因**：Android 的 cached app freezer（cgroup v2 freezer）。读这个文件就知道答案：

```sh
pid=$(adb shell pidof dev.vihv.agent)
cg=$(adb shell "cat /proc/$pid/cgroup" | sed -n 's|^0::/apps/\(.*\)$|\1|p')
adb shell cat /sys/fs/cgroup/apps/$cg/cgroup.freeze     # 1 = 冻着
```

实测（Pixel 10 / Android 17）：

| 状态 | `cgroup.freeze` | `/api/health` |
|---|---|---|
| App 是焦点 Activity（屏幕亮 / 熄屏 Dozing 都好） | 0 | 25ms |
| 焦点被别的 App 抢走（或按 HOME 回桌面）后 60~75 秒 | **1** | 挂死 |
| 上一条 + **加到 doze 白名单** | **1** | 挂死 |
| 熄屏很久 → 强制深度 doze（`dumpsys deviceidle force-idle`，无白名单） | **1** | 挂死 |
| 同上，但**有**白名单 | 0 | 24ms |
| 冻住之后再叫到前台（`am start`） | 0 | 24ms |

**是两个机制，要两招分别治**（这也解释了为什么「关掉电池优化」治不好这件事）：

1. **失去焦点 → 60~75 秒后被冻**（「前一个 App」也是 cached 进程）。白名单**治不了**。
   治法是让它保持焦点：`am start` 把我们的 Activity 提到前面 —— 锁屏挡着也算，
   熄屏 Dozing 下也稳（只要焦点还在我们身上，oom adj 就是 0，freezer 不碰）。
2. **深度 doze（熄屏 30 分钟以上）→ 冻 + 掐网**。这个白名单能治
   （= 设置里「电池 → 不受限」，`dumpsys deviceidle whitelist +dev.vihv.agent`）。

**为什么专坑「从电脑驱动手机」**：回合是脚本起的 → 前台服务起不来
（Android 12+ 不许从后台起，见上一节）→ 进程就是个 cached 进程 → 用户不碰手机，
它就一直 cached → 被冻。人在界面上发起回合时反而没事（前台服务挂着）。

**落到工具上**：`scripts/phone-wake.sh` 把两招都做了（`just wake on`），
`phone-forward.sh` 会在每次远程驱动前跑它的 `auto` 版 —— **已经在前台就什么都不做**，
不在才叫到前台并在 stderr 说一句（`AGENT_NO_WAKE=1` 关掉）。实测：

```
冻着（cgroup.freeze=1, adj=900）→ just phone "用 bash 跑 date +%T"
  ⚠️  手机上那个 App 不在前台 …… 已把它叫到前台
  🔧 bash {"command": "date +%T"}   exit=0 0.59s → 12:49:30
```

还剩一个洞：**用户在手机上用别的 App 的时候**（焦点真被抢走），远程回合仍会被冻。
那个只能靠常驻前台服务（Termux 那种「一直挂个通知」）—— 目前的取舍是不要常驻通知，
所以没做；真要长期无人值守地远程驱动，就把它加回来。

### 缺的部分：前端回来时的同步

WebView 切到后台会挂起 JS，SSE 流可能就断了。切回来时本地视图是旧的。

**做法**（刻意选简单的）：

1. `visibilitychange` / `focus` / `pageshow` → 从 `/api/session` 拉最新状态
2. `/api/session` 返回 `busy`（服务端的 `turn_active` 标志）
3. 如果 `busy`，每 1.5 秒轮询一次直到结束

界面**不再为这个状态单独加一行提示**（原来输入卡片上方有句「agent 正在运行…」
和一个闪的点，2026-10-13 删掉了）：这时的输入卡片右边就是停止键，发出去的消
息会进队列 —— 该说的都说了，再多一行字只是噪音。

那个队列也在**核心**（`AppState::queued`），不在 WebView 里 —— 前端被冻住的
时候，用户刚发出去、还在排队的那条照样会在下一个步边界交给模型，回来时
`/api/session` 的 `queued` 又把它摆回界面上（2026-10-14 改的，见 docs/05 的
「排队」）。

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
