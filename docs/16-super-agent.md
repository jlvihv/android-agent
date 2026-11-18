# 双层架构：Router 与 Worker

> L1。会话第一次有了「角色」：**协调层**（Router）和**干活层**（Worker）。
> 用户跟 Router 说话，Router 把活派给 Worker。
>
> 来源：DeepSeek Harness《Super-agent 设计探索》第 2 节。本文只记**我们怎么做的、
> 为什么这么做、以及哪些没做**，不复述原文。

---

## 1. 一句话

**Router 只做两件事：派活、回话。它没有 shell。**

这句话的后半句是全部的关键。前半句是提示词的事，后半句是工具表的事 —— 而后者才拦得住。

---

## 2. 判据：靠工具表，不靠提示词

DeepSeek 那篇的消融实验给了这条判据最硬的证据：

| 配置 | 5 次运行里委派了几次 | p95 完成时间 |
| --- | --- | --- |
| 单 Session + 中性提示 | **0 次** | 29.5 min |
| 单 Session + 提示优先委派 | 4 次（1 次「觉得活小」自己干了） | 长尾 |
| 双层架构 | 不需要判断 | 15.7 min |

同一个模型、同一个任务，**换一句提示词，委派率从 0 跳到 80%，还剩一条长尾**。
这就是「靠提示词约束分工」的真实可靠性。

所以我们的做法是：Router 的工具表里**没有 `bash` / `read` / `edit`**。

```rust
fn build_router_tools(_p: Platform) -> Vec<ToolSpec> {
    vec![list_sessions, read_session, dispatch]  // 就这三个
}
```

模型想「顺手跑个命令看看」的时候，它手里没有那手。它不是**被要求**不做，是**做不了**。

`agent::tests::router_tools_cannot_execute` 守着这条：谁往 router 表里加了执行工具，
测试立刻红 —— 因为那时候双层已经悄悄退化成一层了，**在行为上完全看不出来**。

---

## 3. 四个工具

| 工具 | 干什么 | 关键决定 |
| --- | --- | --- |
| `list_sessions` | 有哪些会话、谁在跑 | **不列 Router 自己**：它不需要看见自己，也不该有「派活给 Router」的念头 |
| `read_session` | 读某个会话最近几条 | 摘要不拉全文；**读不到 Router**；按需从库读，读完不常驻 |
| `dispatch` | 把任务投进某个会话 | **立即返回**，不等 Worker 做完；不给 id 就新建一个 Worker |

**没有 `answer`**：一开始加过一个，去掉了。模型的正文本来就是给用户看的那句话
（`drive` 把 assistant 的 content 流给前端、也进历史），专门开一个工具为说一句话
只会白多一轮请求。提示词里直接说「**你回复的正文就是给用户看的那句话**」。

`read_session` 的长度是 DeepSeek 那篇 Inline & Spill 的最小版本：窗口里只放「最近 N 条 +
截断」，要看更多再调一次。默认 8 条、最多 30 条、每条 600 字。

---

## 4. `dispatch` 为什么不自己起回合

复用了 `AppState::enqueue` + `agent::continue_with_queued`：

```rust
state.enqueue(&sid, msg)?;                       // 投进队列
agent::continue_with_queued(core, &provider, &sid); // 空着就开，忙就等
```

三个理由：

1. **回合串行、跨会话并行**这条不变式已经在那儿了。`continue_with_queued` 先试
   `begin_turn` 占坑，占不到就把消息留在队列里，让正在跑的那个回合在步边界取走。
   自己起回合就得把这两条规则再写一遍，而且两处一定会走偏。
2. **立即返回**：`dispatch` 一返回就结束，Router 的回合不会被一个跑半小时的 Worker 卡住。
3. **不需要新概念**：没有「派工单」表、没有 Ticket。任务的归属就是「那条会话里的
   一条用户消息」，和工作台本来的模型完全一样。

代价：`dispatch` 的返回里拿不到 Worker 的**最终结果**。结果以那条会话里新消息的形式
存在，通过 `report_back` 送回 Router（见下）。这是有意的 —— **同步等待就是把双层塌成一层**。

---

## 5. 闭环：Worker 做完要回报

`dispatch` 是单向的话，Router 派出去就再也听不到回音。所以有 `router::report_back`：

```
Worker 的回合结束
      ↓
查 dispatched 表：这个会话是谁派的？（没有 → 什么都不做）
      ↓
往 Router 的队列里排一条：「<标题>做完了 / 失败了，说：<它最后说的话>」
      ↓
continue_with_queued：Router 空着就开一轮，忙就等
```

**落库而不是只放内存**：一个 Worker 回合可能跑很久、中间核心被杀过。只在内存里记
「谁派的」，重启后这条活就断了。`dispatched` 表：

```sql
CREATE TABLE dispatched (
  session_id  TEXT PRIMARY KEY,  -- Worker
  by_session  TEXT NOT NULL,     -- Router
  dispatch_id TEXT NOT NULL,
  created_at  INTEGER NOT NULL
);
```

同一个会话可以被派好几次，后来的覆盖先前的 —— Worker 做完时回报给**最近一次**派它的
那个 Router 就行。

三处有意为之：

- **用户自己开的会话不回报**：`dispatched` 表里没有它，所以什么都不做。用户自己开的会话
  不该凭空往 Router 里塞消息。
- **失败也回报**：失败正是 Router 最该知道的。不报的话 Router 会一直以为它还在干。
- **用户按停止不回报**：那是人不想让它继续了，不是「做完」也不是「失败」。

---

## 6. 角色是缓存前缀的一部分

`ChatSession.role` 是 `Worker | Router`，**建会话那一刻定下来、之后不改**（换角色就是换会话）。

它决定两样东西，两样都在可缓存前缀里：

| 按角色分的东西 | 落在哪 | 索引 |
| --- | --- | --- |
| 工具表 | `agent::tools(p, role)` | `平台 * 2 + 角色槽位` |
| 系统提示词 | `AppState::system_prompt_for(role)` | 见下 |

**Router 的提示词不吃覆盖、也不吃 skill / MCP 索引**：

```rust
pub fn system_prompt_for(&self, role: Role) -> Cow<'static, str> {
    if role == Role::Router {
        return Cow::Borrowed(crate::prompt::router_description());
    }
    ... // worker：基础提示词 + 覆盖 + MCP + skill 索引
}
```

理由：那两样说的是「这台机器上能干什么」，而 Router 的定义正是「不干什么」。把装上什么的
清单塞给它，只会诱导它觉得自己也能使唤 —— **工具表里没有、提示词里说有，是最坏的一种不一致**。

Router 的提示词也不分平台（400 字符）：它不执行、不碰文件，沙箱形态与它无关。

---

## 7. 落盘：空的 Router 也要存

`sessions` 表加了一列：

```sql
role TEXT NOT NULL DEFAULT 'worker'   -- 老库里的会话全是 worker
```

迁移是**幂等**的（`db::migrate`，查 `PRAGMA table_info` 看列在不在），不用版本号 ——
现在只有「这一列在不在」一种判断，引版本号进来就得为以后每种改动预先编号。

有一个坑值得记：

> **普通空会话不落盘**（「新对话」点了没说话，重启后不该冒出来），但 **Router 必须例外**。
>
> Router 的 id 记在 `meta.router_session` 里，是「那唯一一个协调层会话」的身份。它要是空着
> 不落盘，下次启动库里找不到 → `ensure_router` 只能重建一个、换一个新 id。表现是：
> **用户什么都没做，Router 每次重启都变成另一条会话**，`meta` 里那条永远指向不存在的 id。
>
> 三处一起改才算数：
> 1. `plan` 的 `keep` 过滤加 `|| role == Router`
> 2. `load` 那句清孤儿的 DELETE 加 `AND role <> 'router'`
> 3. `load` 的 `SELECT` 用 **LEFT JOIN**（`JOIN` 会把零消息的会话行漏掉）

`db::tests::an_empty_router_session_survives_a_restart` 和
`an_empty_worker_session_still_does_not_persist` 一对守这两头。

---

## 8. 接口

| 接口 | 干什么 |
| --- | --- |
| `POST /api/router` | 拿协调层会话的 id，没有就建。**只有一个**，幂等 |
| `GET /api/sessions` | 每行多了 `role` 字段（`worker` / `router`）。前端靠它把协调层那条画得不一样 |
| `GET /api/debug/tools?role=router` | 看协调层那张工具表 |
| `GET /api/debug/request?session=<router id>` | 看 Router 会收到什么（提示词 + 工具表） |

`cargo run -q --example dump_prompt -- --router` 把 Router 那份提示词和工具表打出来，
用于核算体量。worker 那份照旧（不带 `--router`）。

---

## 8.5 界面：Router 是入口，不是第 N 条历史

三处改动，都在 `web/`：

| 在哪 | 做什么 |
| --- | --- |
| **默认打开** | 地址是 `/`（没点名某条会话）时打开 Router，不是空对话 |
| **侧边栏** | Router **永远第一条**，四角星图标、加粗 |
| **不能动** | 长按不弹菜单（不能改名、不能删） |

### 默认打开的那段

```ts
const want = sessionFromPath();
if (want) { chat.show(want); }          // 地址点名了一条：就看它
else {                                   // 没点名 → 打开协调层
  chat.show("");                         // 先给 viewing 一个确定的值
  void (async () => {
    const id = await ensureRouter();     // POST /api/router（幂等）
    if (id && !chat.viewing && !sessionFromPath()) {
      chat.show(id);
      syncUrl(id, true);                 // replace：首屏不该在历史里留一格
    }
  })();
}
```

那个 `if (id && !chat.viewing && !sessionFromPath())` 的三重判断是必须的：
`await` 期间用户可能已经自己点了一条会话 / 按了后退。**只在「还停在空对话、
地址还是 `/`」时才切过去**，否则会把用户的选择覆盖掉。

拿不到 Router（没配模型、请求失败）就**留在空对话**：那时候双层本来就不成立，
空对话是唯一说得通的状态 —— App 不该因为一个附属功能开不了。

### 为什么排序在前端做

服务端 `/api/sessions` 按**创建时间倒序**排，是为了「位置稳定」（见 `SessionList`
的注释）。而「Router 钉在最上面」是**这一个界面**的呈现决定 —— 换个客户端可能想
放在别处。所以它在 `SessionList.pinRouter()` 里，两个赋值点（首屏 `load`、
翻页 `more`）都过一遍，第二页里哪怕混进一条 router 也会被顶回第一条。

不用 getter/setter 包 `items`：Svelte 5 的 `$state` 编译期只处理字段声明，
accessor 对里的 `$state` 语义容易出岔子。显式调一个纯函数，读的人一眼看得出
「排序在哪发生」，而且它**可单独测**（`tests/sessions.test.ts`）。

### 空 Router 要出现在列表里

`list_page` 以前只留 `has_content()` 的会话（「新对话」点了没说话不该占位）。
Router 必须例外 —— 它是入口、得能点回它，而它**通常就是空的**（活由 worker 干，
它自己不攒消息）。不例外的话界面显示「还没有对话」，而地址栏正指着它。

配上前面那节（空的 Router 也要**落盘**），两处是同一个理由的两个面：
**Router 的身份不靠「有内容」成立**。

---

## 9. 没做的（有意）

| 没做 | 为什么 |
| --- | --- |
| **同步等 Worker 结果** | 那就是把双层塌成一层。`dispatch` 必须立即返回 |
| **Router 之间通信 / 多个 Router** | 它是「全局序点」，多开一个就是两个都在跟用户说话 |
| **Worker 之间的点对点通信** | 现在没有这个需求；真要协同，让它们各自被派活、结果都回到 Router |
| **Task / Ticket / 派工单** | 任务的归属就是「那条会话里的一条用户消息」。多加一层没有收益 |
| **Router 的输出单独一条流** | 现在前端把 Router 当普通会话渲染（只是图标/位置/文案不同）。等 Worker 的输出真的要「不外露」时，再决定要不要单独一条信道 |
| **Router 用轻量模型** | 该做但还没做（现在它和 Worker 用同一个 provider）。接口留好了：`system_prompt_for(role)` / `tools(p, role)` 都是按角色分的，加 model 分派是同一个位置 |
| **权限 / 审批** | 铁律 7 没变。当前只有用户主动发起，没有事件驱动，所以不需要 |

---

## 10. 代价

- **多一层延迟**：用户 → Router → Worker，一句话要过一次 LLM 才轮到干活。
- **多一份 token**：Router 自己也要上下文。按 DeepSeek 那边的量级，这套是「用更多 token
  换更短更稳的完成时间」。手机上电和流量都有限，所以 **Router 用轻量模型**这件事要早做
  （现在还没做：Router 用和 Worker 同一个 provider）。
- **`dispatch` 的返回拿不到结果**：Router 和用户都是通过 `report_back` 那条消息才知道
  做完了 —— 这是设计，不是缺陷，但要习惯。

---

## 11. 测试守着什么

| 测试 | 守什么 |
| --- | --- |
| `agent::router_tools_cannot_execute` | Router 表里没有 `bash`/`read`/`edit`；worker 表里没有编排工具 |
| `agent::router_request_is_prefix_stable_and_uses_router_tools` | 前缀不变式对 Router 也成立，且两轮都用 router 那张表 |
| `agent::router_prompt_does_not_promise_execution` | 提示词不承诺执行能力 |
| `db::an_empty_router_session_survives_a_restart` | 空的 Router 落盘 + LEFT JOIN 读得回来 |
| `agent::an_empty_router_is_listed_but_an_empty_worker_is_not` | 空的 Router 要出现在侧边栏，空的 worker 不要 |
| `db::an_empty_worker_session_still_does_not_persist` | 上面的例外没扩大到所有空会话 |
| `router::dispatch_creates_a_worker_and_queues_the_task` | 派活建的是 worker、任务是排进去的 |
| `router::dispatch_without_a_model_creates_nothing` | 派不出去就不留空会话 |
| `router::dispatch_refuses_to_target_a_router_session` | 不能往协调层里派活 |
| `router::a_dispatched_worker_reports_back_to_its_router` | 闭环 |
| `router::an_undispatched_session_reports_nowhere` | 自己开的会话不回报 |
| `router::a_failed_turn_reports_back_as_a_failure` | 失败也回报 |
| `web: 协调层永远第一条 ×4` | 首屏 / 无 router / 翻页混入 / 纯函数不白拷 |
