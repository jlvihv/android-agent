<script lang="ts">
  /**
   * 外壳。结构对齐 Claude 移动端：
   *   header（菜单 + 状态）/ main（空态标题 或 消息列表）/ 输入卡片
   *   抽屉 = Claude 侧边栏（导航项 + 最近对话 + 新建会话）
   *
   * 没有魔法数字：尺寸走 Tailwind 刻度，顶部/底部由安全区给。
   */
  import { untrack } from "svelte";
  import { store } from "./lib/store.svelte.ts";
  import {
    consume,
    isQueued,
    prepend,
    streamFrom,
    onQueuedDelivered,
    onUsage,
    type Message,
    type Part,
    type UiChunk,
    type Upload,
    type Usage,
    type SessionUsage,
    type UsageTotals,
    totalsOf,
  } from "./lib/sse";
  import { uid } from "./lib/uid";
  import { api, post } from "./lib/api";
  import { clearUrl, onRouteChange, sessionFromPath, syncUrl } from "./lib/route";
  import {
    applyPatch,
    applyWindow,
    prependPage,
    trimOlder,
    type SessionPayload,
    type Window,
  } from "./lib/session";

  import Icon from "./components/Icon.svelte";
  import Composer from "./components/Composer.svelte";
  import MessageList from "./components/MessageList.svelte";
  import Drawer, { type Session } from "./components/Drawer.svelte";
  import MachineSheet from "./components/MachineSheet.svelte";
  import ModelSheet from "./components/ModelSheet.svelte";
  import Settings from "./components/Settings.svelte";
  import Viewer from "./components/Viewer.svelte";
  import { viewer } from "./lib/viewer.svelte.ts";
  import { models } from "./lib/models.svelte.ts";
  import { agentHasBrowser, openAgentBrowser } from "./lib/overlay";
  import { wide } from "./lib/wide.svelte.ts";
  import { dismissable } from "./lib/drag";

  let input = $state("");

  /**
   * 「眼前这个会话此刻是什么状态」。**一个联合，不是四个布尔。**
   *
   * 以前是 `busy` / `serverBusy` / `live` / `fresh` 四个独立变量，合法组合只有
   * 五种，而约束全靠记性和注释维系（`newSession` 里那句「不清 `serverBusy` 的话
   * 输入框要等一个轮询周期才解冻」就是约束被外部记住的样子）。写成联合之后，
   * 非法组合根本表达不出来：
   *
   *   idle    有会话（或空态），没有回合在跑
   *   fresh   点了「新对话」、第一句还没发（服务端**还没有**这条会话）
   *   live    本地正连着它的流（`run` 是身份：`stop` / `detach` 认它）
   *   remote  服务端在跑，本地没连着 —— 切到后台 / 从别的会话切回来 / 别人发起的
   *
   * 几个推导量：`running` = 输入框该不该显示停止键；`busy` = 本地连没连着流
   *（`loadMessages` 用这个门）；`fresh` = 还没建。
   */
  type View =
    { k: "idle" } | { k: "fresh" } | { k: "live"; run: Run } | { k: "remote" };

  /**
   * 本地正连着的那条流。**只属于当前看着的会话**。
   *
   * 可以同时在几个会话里各跑一个回合（服务端按会话记）。切走的时候只是把
   * 本地这条流断开（`detach`），服务端那个回合照跑；切回来靠轮询看进度。
   */
  type Run = { controller: AbortController };

  let view = $state<View>({ k: "idle" });
  /** 本地正连着流 */
  const busy = $derived(view.k === "live");
  /** 点了新对话还没建 */
  const fresh = $derived(view.k === "fresh");
  /** 有没有回合在跑（本地或远端）—— 输入框的停止键看它 */
  const running = $derived(view.k === "live" || view.k === "remote");
  let messages = $state<Message[]>([]);
  let sessions = $state<Session[]>([]);

  /**
   * 一窗多少条消息（视图消息的条数，**不是回合数**：一个回合的思考 / 工具 / 正文
   * 都在同一条助手消息里，所以 10 条 = 5 个回合）。
   *
   * 首屏就只跟服务端要这么多（`tail`），往上翻到头再要更早的一窗 —— 整段历史
   * 每打开一次就传一遍很亏（一个长会话几 MB，而一屏只看得到几条）。
   * 同一个数也给 `MessageList`（它管「挂多少条进 DOM」）—— 两处必须同一个数：
   * 挂得比要的多会露白，比要的少就白传。
   */
  const MSG_WINDOW = 10;

  /**
   * 手上这窗消息在**完整视图**里的起点（服务端的 `from`）：
   * `messages` = 完整视图的 `[windowFrom, windowFrom + messages.length)`。
   *
   * 为什么要它：服务端算增量补丁时用的是完整视图的下标（`replaceFrom`），
   * 而我们手上只有尾巴这一窗 —— 把这个数带回去，它替我们翻译。
   */
  let windowFrom = $state(0);
  /**
   * 服务端那边还有没有更早的。**推导的**，不是状态：它恒等于「手上这窗的起点 > 0」。
   *
   * 存成状态会陈旧（踩过）：`trimOldPages` 一收窗口，手上就少了几十条 —— 那些
   * 服务端还有，可标志还是上次翻完时的 `false`，于是「该往前要」再也不成立，
   * 用户看到的是「第一次能翻，滚回底部再往上翻就翻不动了」。
   */
  let hasOlder = $derived(windowFrom > 0);

  /**
   * 手上最多留几条更早的（回到底部时收回）。
   *
   * `MessageList` 那层只管**挂进 DOM** 的（一窗 10 条）；这里管**手里**的（往上翻
   * 多少就留多少）—— 一页 10 条在真实会话里约 300 KB（工具输出很胖），翻十页就是
   * 3 MB 常驻。留三窗：再往上翻一两屏不用重新拉，再早就丢掉重拉。
   */
  const KEEP_MSGS = MSG_WINDOW * 3;

  /**
   * 预取好的、还没插进去的那一窗（见 `prefetchOlder` / `loadOlder`）。
   *
   * 为什么要预取：有时插入得等用户滑停（贴在 0 上 / 内核没有 scroll anchoring，得
   * 自己写 `scrollTop`，而那会把手机上正在进行的惯性滑动掐掉，见 `lib/window.ts` 的
   * `insertNow`），那就不应该连网络也一起等 —— 滚动中先把内容拿到手，停下的那一刻
   * 直接插。常态（有 anchoring、没贴顶）根本不等停：拿到就插。
   */
  let pending: {
    sid: string;
    base: number;
    page: Message[];
    from: number;
  } | null = null;

  /** 正在路上的那份 `loadMessages` 是哪一份（会话|窗口|rev）；空 = 没有 */
  let inflight = "";
  /** 路上这份还在跑的时候又有调用被合并掉了：回来后必须再补一次（见下面的注释） */
  let inflightAgain = false;
  /** 正在拉更早的一窗（插入那一次），别重入 */
  let loadingOlder = false;
  /**
   * 预取在路上（滚动中会反复叫它，别每帧都发一个请求）。
   *
   * 存的是那个 promise 而不是一个布尔：真要插的时候（`loadOlder`）预取要是还在路上，
   * **等它**就行 —— 以前是另发一份一模一样的请求，等于白白多一个来回。
   */
  let prefetching: Promise<void> | null = null;

  /**
   * 排队等着交给模型的消息 —— 它是**服务端**那份队列的本地镜子。
   *
   * 队列本身在核心（`AppState::queued`）：`POST /api/chat` 碰上这个会话正跑着
   * 就把它排进去，正在跑的那个回合**一空下来**（步边界）就把它交给模型 —— 不用
   * 等整个回合结束。为什么不在前端排：前端那个队列活在 WebView 里，App 切到
   * 后台、WebView 被冻住或回收，这条消息就永远发不出去了（见 docs/05 的「排队」）。
   *
   * 这一份只负责**画**（虚线气泡 + 左边那个 ×），三个来源：
   *
   * 1. `send` 里刚发出去、还没听到回话的那条（`pending: true`）—— 服务端说
   *    「排队了」就坐实，说「开跑了」就翻进正文（见 `deliver`）。
   * 2. 服务端把排队的消息交给模型时推的 `data-queued-delivered`：从这里挪进正文。
   * 3. `loadMessages` 每次按 `/api/session` 的 `queued` **整份重来** —— App 被
   *    回收过 / 换了设备 / 界面刚刷新时，那几条就是从这儿回来的。
   *
   * 每条记着自己属于哪个会话：回合在后台跑着时用户可能已经切走了。
   */
  type Queued = { id: string; sid: string; parts: Part[]; pending?: boolean };
  let queue = $state<Queued[]>([]);

  /**
   * 本会话的 token 用量（输入框里那个上下文圆环）。
   *
   * 两个来源：流里的 `data-usage`（每步结束一条，实时）+ `/api/session` 的
   * `usage`（刷新 / 切会话回来时的基线）。累计值以服务端为准 —— 流里那些
   * 增量只是两次拉取之间的临时账，下一次响应整份换掉（两边数的是同一批
   * 步骤，对得上）。
   */
  let usage = $state<SessionUsage | null>(null);

  // 圆环的分母：模型窗口。清单是启动时就拉好的（见下面的 models.load）
  let ctxWindow = $derived(models.contextWindow(store.llm?.model));

  /**
   * 流里每跑完一步就报一次用量 —— 圆环实时跟着涨（一轮多步时也能看出来
   * 上下文在长）。事件里带着核心记上这一步之后的会话累计（`session`），
   * 直接换上：前端不自己往上加、也不自己算命中率。
   */
  $effect(() =>
    onUsage((u) => {
      const { session, ...last } = u;
      if (session) usage = { last, total: totalsOf(session) };
    }),
  );

  /**
   * 不看这个会话了：断开本地的流，**不**停服务端的回合。
   *
   * 从 `live` 退到 `remote`（不是 `idle`）：我们只是不听了，服务端那个回合还在跑
   * —— 这样轮询会接着把进度捞回来（以前只把 `busy` 置假、`serverBusy` 保持原值，
   * 恰好也是这个效果，但没人说得清为什么）。
   */
  function detach() {
    const v = view;
    if (v.k !== "live") return;
    view = { k: "remote" };
    v.run.controller.abort();
  }

  let drawer = $state(false);
  /**
   * 宽屏下侧边栏要不要展开（窄屏不看它 —— 那里侧边栏是抽屉，看 `drawer`）。
   *
   * 默认**收起**：刷新之后永远是同一个样子（不会有“从关闭弹到打开”那一下），
   * 要用再点左上的菜单。不持久化 —— 这个 App 什么都不往 localStorage 里存。
   */
  let sidebar = $state(false);
  let sheet = $state(false);
  /** 「选择机器」面板（侧栏底栏那个机器标签点开的，见 MachineSheet） */
  let machineSheet = $state(false);
  /** 壳在不在（能在手机核心 / 电脑核心之间切）。桌面浏览器、手机浏览器里没有壳 */
  /**
   * 底栏那个机器标签点开「选择机器」（面板那份数据是本机核心答的，见 docs/05 的 4.5）。
   *
   * 这里不再看着有没有壳：电脑浏览器、手机 App、手机浏览器都问得到那份数据 ——
   * 以前只在 App 里是「页面上没有壳，换不了」，现在换机器整条路都在核心里（docs/12）。
   */
  const openMachines = () => (machineSheet = true);
  /** 设置页。可以从抽屉的齿轮进，也可以从模型面板「模型提供商」页的「管理模型提供商」进 ——
   *  后者是叠在模型面板上面的，关掉之后回到模型面板。每层各自在
   *  lib/overlay.ts 里登记，返回键先关最上面那层。 */
  let settings = $state(false);
  /** 设置页每关一次加一：Key 可能改了，模型面板据此重拉清单 */
  let settingsRev = $state(0);
  const closeSettings = () => {
    settings = false;
    settingsRev++;
  };

  /**
   * 开机就把模型清单拉回来。
   *
   * 「选择模型」面板是「第一帧就得有数据」才不跳（见 `lib/models.svelte.ts`）——
   * 等用户点开才拉的话，第一次打开还是会先渲染「载入中…」再长高。这里提前
   * 拉一次，之后 10 分钟内打开面板都是瞬间的，一个请求也不发。
   *
   * 没配服务商就不拉（`/api/models` 也没东西可回）。
   */
  $effect(() => {
    const on = !!store.llm;
    const r = settingsRev;
    if (!on) return;
    // `untrack`：同 ModelSheet 里的理由，别把 `load()` 内部读的 `$state` 也变成依赖
    void untrack(() => models.load(r));
  });

  /**
   * agent 有没有开着浏览器 —— 有才显示头部那个地球按钮。
   *
   * 状态在**壳**那边（`AgentApp.browser.hasPage()`，见 BrowserController）：
   * agent 调 `browser` 工具会懒建一只 WebView，把标签关光又回到「没有」。
   * 所以两条腿：挂载时同步拉一次（`AgentHost.hasBrowser`），之后壳推
   * `window.__browserAvailable`（见 MainActivity 的 `pushBrowserAvailable`）。
   */
  let browserAvailable = $state(false);
  $effect(() => {
    // 先挂上推送口，再拉一次初始值：同一个 tick 里不会漏掉中间那一下
    window.__browserAvailable = (on) => {
      browserAvailable = !!on;
    };
    browserAvailable = agentHasBrowser();
    return () => {
      delete window.__browserAvailable;
    };
  });

  /** 输入区实际高度。消息列表的底部留白和底部渐隐都跟着它走。 */
  let composerH = $state(0);

  // —— 会话 ——
  //
  // 消息本来只活在 JS 内存里，Activity 重建 / 刷新 / 从后台回来就空了。
  // 服务端一直留着，所以进来先拉一次。
  async function loadMessages() {
    // 本地流正在跑：本地才是最新的，服务端那份是旧的。拉回来只会白传
    // 一整段历史再丢掉 —— 干脆不发这个请求。
    if (busy) return;
    // 刚点了「新对话」还没发第一句：服务端那边还是旧会话（新的根本还没建），
    // 拉回来会把刚清空的界面又填满旧内容。
    if (fresh) return;
    // 同一份（会话 + 窗口 + rev）已经在路上了就别再发：触发源有好几个
    //（挂载、WS 连上、回合起停、切回前台），它们常常挤在同一瞬间 —— 实测首屏
    // 发了三份一模一样的窗口（每次一窗，长会话里就是白花几份流量）。
    // 被合并掉的那一次不能就这么算了（它可能正是「回合刚跑完，去拿最后那几条」），
    // 所以下面会在这一份回来之后再补一次 —— 见 `inflightAgain`。
    const key = `${sessionId}|${windowFrom}|${rev}`;
    if (key === inflight) {
      // 已经在路上了：**不能直接不管** —— 被合并掉的往往是「这个回合刚跑完，去把
      // 最后那几条拿回来」那一次，而它要的东西可能比路上这份更新（路上那份可能在
      // 回合结束之前就在服务端算好了）。记一笔，等它回来再补一次。
      inflightAgain = true;
      return;
    }
    inflight = key;
    inflightAgain = false;
    const at = sent;
    // 发请求时手上那一窗的起点。补丁的 `replaceFrom` 是服务端按**这个**翻译过来的，
    // 而请求在路上时用户可能往上翻了一页（数组前面多了一段）—— 下标就不作数了。
    const winAt = windowFrom;
    try {
      const sid0 = sessionId;
      if (!sid0) return; // 没 id 就没有「当前会话」可拉（服务端也不猜了）
      const d = await api<
        SessionPayload & {
          // 窗口那几件（`messages` / `from` / `hasMore` / `append` / `replaceFrom` /
          // `keepParts`）是 `SessionPayload`，在 `lib/session.ts` 里定义、有测试盯着 ——
          // 以前这里手抄了一份，两份字段名一旦漂就靠 `as` 强转糊过去。
          busy: boolean;
          unchanged?: boolean;
          rev?: string;
          /** 服务端队列里等着交给模型的那几条（`Message` 的形状，`done: false`） */
          queued?: Message[];
          /** 这份是哪个会话的 */
          session?: string;
          /** 上一个回合失败的原因（没在跑时才有） */
          turnError?: string;
          /** token 用量（输入框那个圆环）。服务端那份是权威 */
          usage?: (Partial<UsageTotals> & { last?: Usage | null }) | null;
        }
      >(
        // **显式带 id**：「现在看的是哪条会话」是前端的事（地址栏说了算），
        // 不能靠服务端那个 `active` —— 别的客户端 / 调试脚本随时会把它改掉。
        // 手上还没有 id 时才不带（首屏、刚点新对话），那时服务端回的是它认为的
        // 当前会话，我们随后把它写进地址栏（见下面的 `syncUrl`）。
        `/api/session?id=${encodeURIComponent(sid0)}` +
          `&rev=${encodeURIComponent(rev)}&tail=${MSG_WINDOW}&from=${windowFrom}`,
      );
      // 请求在路上的时候本地发了一条（`send`）：这份是发之前的样子，拿它整份
      // 覆盖会把刚推进去的用户消息和正在流的那条助手消息冲掉 —— 流接着往一个
      // 已经不在列表里的对象里写，界面上就是「没回复」。丢掉，下一次再拉。
      if (busy || at !== sent) return;
      // 服务端说这个会话在不在跑。**只在本地没连着流时才认**：连着流的时候
      // 本地那份更新（`run` 收完流会自己落到 `idle`），拿一个路上算好的旧值
      // 盖上去只会让停止键闪一下。
      if (!busy) view = d.busy ? { k: "remote" } : { k: "idle" };
      rev = d.rev ?? "";
      // 排队那几条按服务端**整份重来**（`unchanged` 时也要）：它们的变化不进
      // `rev`（那是消息的版本号），光看 rev 看不出「排队里少了一条」。
      // `pending` 的那几条是刚发出去、还没听到回话的，留着 —— 服务端下一份就会
      // 报上它们（或者它们已经开跑了）。
      const sid = d.session ?? sid0;
      if (d.queued) {
        // 只换**这个**会话的那几条：别的会话的排队项留着（切回去之前不会再拉它们）。
        // 名字别叫 `inflight` —— 那是「这一份 /api/session 在不在路上」的门。
        const hoping = queue.filter((q) => q.pending && q.sid === sid);
        queue = [
          ...queue.filter((q) => q.sid !== sid),
          ...d.queued.map((m): Queued => ({ id: m.id, sid, parts: m.parts })),
          ...hoping,
        ];
      }
      // 服务端说没变：连 JSON 里都不带 messages，什么都不用做
      if (d.unchanged) return;
      // 用量整份换成服务端的：它数的是这个会话的**全部**调用，而我们手上
      // 那份只从页面加载后开始算。`last` 没有 = 这个会话还没跑过
      usage = d.usage?.last
        ? { last: d.usage.last, total: totalsOf(d.usage) }
        : null;
      // 会话 id 变了（切会话 / 重置）就不能按 id 复用旧消息 ——
      // `u1` 这种 id 每条会话都会有。`sid` 在上面算过了。
      //
      // 补丁 / 整窗两条路的规则在 `lib/session.ts` 里（有测试盯着，见 `bun test`）。
      const same = sid === sessionId;
      if (d.append && same) {
        const r = applyPatch({ messages, windowFrom }, d, winAt);
        if (!r.ok) {
          if (r.why === "mismatch") {
            // 本地那条和服务端以为的对不上：老老实实重新要一份全量
            //（`rev = ''` → 下一份是尾部窗口，回来时会按 `from` 接在旧的那段后面）
            rev = "";
            return loadMessages();
          }
          // 'stale'：这一路上窗口变过（用户往上翻了一页），补丁的下标是按旧窗口
          // 算的 —— 作废。下一次同步（轮询 / 聚焦 / 回合结束）会重算。
          return;
        }
        setWindow(r.window);
      } else {
        setWindow(applyWindow({ messages, windowFrom }, d, same));
      }
      if (d.turnError) showTurnError(d.turnError);
      sessionId = sid;
    } catch {
      /* 拉不到就算了 */
    } finally {
      if (inflight === key) {
        inflight = "";
        // 被合并掉的那次在这里补上（新的一份 key 不一样，不会自己把自己的门堵上）
        if (inflightAgain) {
          inflightAgain = false;
          void loadMessages();
        }
      }
    }
  }

  /**
   * 上一个回合失败了：在末尾补一条错误（服务端历史里没有这条，它只给人看）。
   *
   * 两种来路：排队消息接着开的回合（没有人连着它的流，错误只能从这里来），以及
   * 刷新 / 回到前台之后的前台回合（本地那条错误随着整份换掉的历史没了）。
   * 末尾已经有同一句错误就不再补。
   */
  function showTurnError(text: string) {
    const last = messages.at(-1);
    if (last?.parts.some((p) => p.type === "error" && p.text === text)) return;
    messages.push({
      id: uid(),
      role: "assistant",
      done: true,
      parts: [{ type: "error", text }],
    });
  }

  /**
   * 上次拿到的会话版本号（服务端算的）。一致就不再重传整段历史。
   *
   * **切会话 / 新建 / 删除后必须清掉** —— 否则会拿旧会话的 rev 去比对新会话，
   * 服务端一旦碰巧返回 unchanged，界面就卡在上一个会话的内容上。
   */
  let rev = "";

  /**
   * 消息列表被本地改过的版本号。**只在真往 `messages` 里加东西时才加一**。
   *
   * `loadMessages` 发请求前记下它，回来时对不上就说明这份回包算的是加之前的样子
   * —— 拿它整份覆盖会把刚推进去的用户消息和正在流的那条冲掉（见那里的注释）。
   *
   * 以前在 `send()` 里就 +1 了（那时消息还没进列表），等于把一个「还没发生」的
   * 改动提前记账：`ensureSession` 那一路要是慢，中间回来的那份回包会被无理由丢掉。
   * 现在只有真正 `messages.push` 的两条路（建会话失败、开跑）各加一次。
   */
  let sent = 0;

  /**
   * 「用户自己发了一条」的次数。与 `sent` 分开：那个只管和服务端的竞态，
   * 这个只管界面要不要把视口带回尾部（MessageList 的 `follow`）。
   *
   * **只有这里会加** —— 排队那条被服务端交给模型（`onQueuedDelivered`）不算：
   * 那是服务端在动，用户可能正往上翻着看历史。
   */
  let ownSend = $state(0);

  /**
   * 当前会话 id（从 rev 里解出来）。切会话时清掉，避免跨会话复用消息对象。
   *
   * `$state`：**队列要跟着它走**（输入框上面那几条「排队中」只看当前会话的，
   * 切回去才轮到它发）。
   */
  let sessionId = $state("");

  /**
   * 把算好的三个值写回去（`$state` 得逐个赋值才触发更新）。
   *
   * 手上这一窗 = 完整视图的 `[windowFrom, windowFrom + messages.length)`。
   */
  function setWindow(w: Window) {
    messages = w.messages;
    windowFrom = w.windowFrom;
  }

  /**
   * 从 `MessageList` 自己那个窗口状态里取一份（三件套）—— 读的是当前 `$state`。
   */
  function currentWindow(): Window {
    return { messages, windowFrom };
  }

  /**
   * 把更早的一页插到手上（预取的、或刚拉到的）。返回 `true` = 真插上了。
   *
   * 接不上就整页丢掉（`prependPage` 给 null）：宁可少一窗，也不能在中间留个洞。
   */
  function insertOlder(p: { page: Message[]; from: number }): boolean {
    if (!p.page.length) return false; // 空页：什么都不用做，下次再要
    const next = prependPage(currentWindow(), {
      messages: p.page,
      from: p.from,
    });
    if (!next) return false;
    setWindow(next);
    // 插进去之后窗口起点变了：手上那份预取（如果有）的基准点也跟着过时 ——
    // 丢掉，下一次贴到顶时按新的起点重新预取（不然会拿着一份对不上的干等）。
    pending = null;
    return true;
  }

  /**
   * 翻页请求本身（预取和插都用它）。返回 null = 这一份不能用了。
   */
  async function fetchOlder(
    base: number,
  ): Promise<Omit<NonNullable<typeof pending>, "sid" | "base"> | null> {
    const sid = sessionId;
    try {
      const d = await api<SessionPayload>(
        `/api/session?id=${encodeURIComponent(sid)}&before=${base}&tail=${MSG_WINDOW}`,
      );
      // 这一路走的过程中会话切走了 / 窗口又变了一页 / 已经没有更早的了：这份不要了
      if (sid !== sessionId || base !== windowFrom || windowFrom === 0)
        return null;
      return { page: d.messages ?? [], from: d.from ?? 0 };
    } catch {
      return null;
    }
  }

  /**
   * 滚动中先把下一窗拿到手（不动界面）：等用户滑停了直接插，不再等一个来回。
   *
   * 失败不算事 —— 真要的时候 `loadOlder` 会重拉一次。
   */
  function prefetchOlder(): void {
    if (!hasOlder || !sessionId || pending || prefetching || loadingOlder)
      return;
    const base = windowFrom;
    const p = (async () => {
      const got = await fetchOlder(base);
      if (!got || base !== windowFrom || pending) return;
      pending = { sid: sessionId, base, ...got };
    })().finally(() => {
      if (prefetching === p) prefetching = null;
    });
    prefetching = p;
  }

  /**
   * 真的插一窗更早的（`MessageList` 判停之后叫它）。
   *
   * 只跟服务端要**这一窗** —— 历史是只追加的，这一窗要回来就永远是对的，不用管
   * `rev`（那套是给「当前回合在长」用的）。返回 `true` = 真拿到了新东西（调用方
   * 据此补偿滚动位置，不补偿的话视口会跳）。
   */
  async function loadOlder(): Promise<boolean> {
    if (!hasOlder || !sessionId) return false; // hasOlder = 手上这窗的起点 > 0
    // 预取还在路上：等它（它要的就是这一窗），别另发一份
    if (prefetching) await prefetching;
    if (!hasOlder || !sessionId) return false;
    // 预取的还在手上、又正好接得上 → 直接用
    if (pending && pending.sid === sessionId && pending.base === windowFrom) {
      const p = pending;
      pending = null;
      return insertOlder(p);
    }
    pending = null;
    loadingOlder = true;
    try {
      const got = await fetchOlder(windowFrom);
      return got ? insertOlder(got) : false;
    } finally {
      loadingOlder = false;
    }
  }

  /**
   * 回到底部了（`MessageList` 说）：把视口之外的旧页丢掉，给内存封顶。
   *
   * 这一步只动**前面**那些（下标跟着 `windowFrom` 平移，所以补丁的下标依旧对得上），
   * 界面不动：被丢掉的全在视口上方，`MessageList` 那层本来就只挂着最后 10 条。
   */
  function trimOldPages(): void {
    const next = trimOlder(currentWindow(), KEEP_MSGS);
    if (next) setWindow(next);
    // 预取的基准点可能已经不在了
    if (pending && pending.base !== windowFrom) pending = null;
  }

  /**
   * 拉会话列表。**重复调用并成一次**（两档）：
   *
   *   1. 已经有一份在路上 → 直接复用它的 promise。**不看发起时刻**（只看它是不是
   *      卡了太久，`SESSIONS_INFLIGHT_MAX_MS`）。真机上 22 KB 在路上要几十上百毫秒，
   *      「切回前台（focus）+ 点菜单」这种最常见的组合刚好落在旧的 300ms 窗口之外，
   *      于是同一份列表在路上发了两遍（实测：第一份还没落地，第二份已经出去了）。
   *   2. 刚落地那份 → `SESSIONS_MERGE_MS` 内复用。
   *
   * 首屏那几个入口（挂载、WS 连上、`pageshow`）以前就是三份 21 KB 同时发出去。
   * 判断收敛到函数自己身上，不在各个入口各写一遍 —— 那种写法漏一个就又回来了。
   *
   * 会不会漏掉更新？会晚 `SESSIONS_MERGE_MS`，而且真正的「必须看到最新」那几处
   * （重命名 / 删除 / 切会话 / 回合收尾）走 `loadSessions(true)`，绕开合并。
   */
  const SESSIONS_MERGE_MS = 300;
  /**
   * 在飞的那一份最多认这么久（ms）。
   *
   * 「在飞就复用」不能没有上限：隧道断着 / 核心卡住时那份请求可能永远不落地，
   * 认它一辈子等于列表再也不刷新了。
   */
  const SESSIONS_INFLIGHT_MAX_MS = 10_000;
  /** 上一份**发起**的时刻：算「在飞了多久」用 */
  let sessionsAt = 0;
  /** 上一份**落地**的时刻：算「刚拉过」的安静窗口用 */
  let sessionsDoneAt = 0;
  let sessionsInflight: Promise<void> | null = null;
  /** 上手那份（不管落地没有）。安静窗口内复用它，省掉「刚拉过又拉一次」 */
  let sessionsLast: Promise<void> | null = null;
  /**
   * 第一页之后的游标（`/api/sessions` 的 `next`）。`null` = 已经到底 / 还没拉过。
   *
   * 见 `loadMoreSessions`：抽屉滚到底才按它要下一页。一千条会话不该在打开的那一帧
   * 就传 150 KB 过来。
   */
  let sessionsNext = $state<string | null>(null);
  /** 正在拉下一页（滚到底会反复触发，别叠请求；抽屉底部那行「载入中」也看它） */
  let sessionsMoreBusy = $state(false);
  /**
   * 上一页**没拉成**（网络 / 后端）。
   *
   * 和「到底了」必须分开：`finally` 里 `sessionsMoreBusy` 一落，抽屉那边就只剩
   * 「还有没有下一页」这一个信号 —— 拉失败时 `sessionsNext` 还是原值（还有更多），
   * 但界面不能装作一切正常接着自动要（会死循环），也不能说「没有更早的了」
   *（那是撒谎）。所以这里记一笔，抽屉改口「加载失败，点这里重试」。
   *
   * 一旦用户重试成功或者整份列表重来过，就清掉。
   */
  let sessionsMoreError = $state(false);

  /**
   * `force`：绕开合并，**一定**打一份新的（重命名 / 删除 / 切换会话 / 回合收尾）。
   *
   * **只拉第一页**（服务端默认 20 条）。翻页走 `loadMoreSessions`。已经翻出来不止
   * 一页的，按现有条数重拉（见下面 `keep`）。
   */
  async function loadSessions(force = false) {
    const now = Date.now();
    if (!force) {
      // **在飞就无条件复用。** 以前这个条件还带着「发起不到 300ms」，可真机上 22 KB
      // 在路上就要几十上百毫秒，而「切回前台（focus）+ 点菜单」这种最常见的组合正好
      // 落在窗口之外 —— 同一份列表就在路上发了两遍（实测：第一份还没落地，第二份已出）。
      if (sessionsInflight && now - sessionsAt < SESSIONS_INFLIGHT_MAX_MS) {
        return sessionsInflight;
      }
      // 刚落地那份：窗口内也不再问
      if (sessionsLast && now - sessionsDoneAt < SESSIONS_MERGE_MS) {
        return sessionsLast;
      }
    }
    sessionsAt = now;
    // 先占位：`finally` 里要靠它认出「现在在飞的是不是我自己」—— 赋值在本函数
    // 尾部，所以用会一个先建出来的对象，而不引用那个 `const p`。
    const slot: { p: Promise<void> | null } = { p: null };
    // 重拉时把**已经翻出来的条数**带上（0 = 服务端默认一页）。
    // 不带的话，回一次前台 / 重命名一次就会把用户翻到第 6 页的列表缩回 20 条，
    // 滚动位置也跟着跳回顶部 —— 明明什么都没变。代价是翻得越多重拉越重，
    // 但那正是用户自己翻出来的量（以前每一次都是全量）。
    const keep = sessions.length;
    const p = (async () => {
      try {
        const d = await api<{ sessions?: Session[]; next?: string | null }>(
          `/api/sessions${keep ? `?tail=${keep}` : ""}`,
        );
        sessions = d.sessions ?? [];
        // 整份重来：游标也跟着回到第一页之后（之前翻出来的那些没了）
        sessionsNext = d.next ?? null;
        sessionsMoreError = false;
      } catch {
        /* 同上 */
      } finally {
        sessionsDoneAt = Date.now();
        // 只清自己那一份：`force` 可能在它落地之前又发了一份，那份不能被抹掉
        // （抹了的话下一次调用就以为「没人在飞」，又发一份）
        if (sessionsInflight === slot.p) sessionsInflight = null;
      }
    })();
    slot.p = p;
    sessionsInflight = p;
    sessionsLast = p;
    return p;
  }

  /**
   * 抽屉滚到底了：按游标再要一页，追加到 `sessions` 后面。
   *
   * 不能和 `loadSessions` 合并（那份是「整份重来」，会把翻出来的页面冲掉）——
   * 这里只在自己身上防重入。`force` 重拉第一页时游标会重置，这一页作废。
   */
  async function loadMoreSessions() {
    const cursor = sessionsNext;
    if (!cursor || sessionsMoreBusy) return;
    sessionsMoreBusy = true;
    sessionsMoreError = false;
    const at = sessions.length;
    try {
      const d = await api<{ sessions?: Session[]; next?: string | null }>(
        `/api/sessions?before=${encodeURIComponent(cursor)}`,
      );
      // 这期间整份重来过（切会话 / 回合收尾 / 回前台）：游标和工作都换了，这份丢掉
      if (sessionsNext !== cursor || sessions.length !== at) return;
      // 按 id 去重：翻页期间新建的会话会顶到最前面，理论上不该重复，但宁可少一行
      const have = new Set(sessions.map((s) => s.id));
      sessions = [
        ...sessions,
        ...(d.sessions ?? []).filter((s) => !have.has(s.id)),
      ];
      sessionsNext = d.next ?? null;
    } catch {
      // 记下来，抽屉里那行改成「加载失败，点这里重试」——
      // **不要**静默：以前这里什么都不做，界面上那行「载入中…」既没变成错误、
      // 也没人去要下一页（自动要的那条路被 `sessionsMoreBusy` 挡住过一轮），
      // 看起来就是「一直在载入、永远载不完」。
      sessionsMoreError = true;
    } finally {
      sessionsMoreBusy = false;
    }
  }

  function newSession() {
    detach();
    resetView();
    // **不请求服务端**：新会话到第一句话才建（`ensureSession`），
    // 这里只把本地视图清空 —— 点了不发就不会攒出一堆空会话。
    view = { k: "fresh" };
    // 新会话还没有 id：地址收回根路径（不带 id 的 `/` 就是「新对话」的意思）
    clearUrl();
    drawer = false;
  }

  /**
   * 把「手上这一个会话的视图」清空。**切会话三条路共用**（新建 / 切换 / 删除）。
   *
   * 以前这段在三个函数里各抄了一遍，而且已经抄漂了：`newSession` 清了 `usage` 和
   * `serverBusy`，另外两个没清 —— 删掉当前会话之后，输入框上那个圆环还在报被删
   * 那个会话的用量，`serverBusy` 还是 true 的话输入框会冻到下一次响应回来。
   *
   * `fresh` 不在这里：它表示「还没在服务端建」，只有新建和保证建过之后才说得清。
   */
  function resetView() {
    rev = "";
    sessionId = "";
    messages = [];
    // 手上这一窗的起点和「还有没有更早的」都从头算
    windowFrom = 0;
    pending = null;
    // 用量是**每个会话**的，跟着一起清
    usage = null;
    // 服务端在不在跑也是每个会话的：不清就会留着上一条会话的「还在跑」。
    // **`fresh` 除外**（见 `newSession`）：它是「还没建」，由调用方决定。
    view = { k: "idle" };
  }

  /**
   * 切到另一条会话。
   *
   * **只动前端**：不问服务端「把 active 设成这条」—— 当前看的是哪条会话是界面
   * 的事，服务端那个 `active` 只是「不指定 id 时的默认值」（给 `agentctl` 这类
   * 脚本用）。之前每次点会话都要打一次 `activate`，等于把前端状态写进后端，
   * 多端同时开着时两边还会互相抢。
   *
   * 现在的顺序：地址先变（`push`，这样后退能回上一条）→ 本地视图清空 →
   * 按**地址里那个 id** 拉消息（`loadMessages` 显式带 id）。
   */
  async function switchSession(id: string) {
    if (id === sessionId) {
      drawer = false;
      return;
    }
    detach();
    syncUrl(id);
    resetView();
    sessionId = id;
    await Promise.all([loadSessions(true), loadMessages()]);
    drawer = false;
  }

  async function renameSession(id: string, title: string) {
    try {
      await post("/api/sessions/rename", { id, title });
    } catch {
      /* 名字没改成就还是旧名字，下一次拉列表会纠正 */
    }
    await loadSessions(true);
  }

  async function deleteSession(id: string) {
    if (id === sessionId) detach();
    // 这个会话没了，它排着的队也没地方发了
    queue = queue.filter((q) => q.sid !== id);
    try {
      await post("/api/sessions/delete", { id });
    } catch {
      /* 删不掉的话下一次拉列表它还在，界面不会撒谎 */
    }
    resetView();
    await Promise.all([loadSessions(true), loadMessages()]);
  }

  // 只在挂载时跑一次。`untrack` 不能省：`loadMessages` 在第一个 await 之前同步读了
  // `busy` / `fresh`，不包的话它们都成了这个 effect 的依赖 —— 发第一句时 `fresh`
  // 一变就多拉一次，和 `/api/chat` 赛跑。
  //
  // **首屏那一轮要记账**（`booted`）：下面还有三处会在挂载后紧接着触发同步
  // （WS 连上、`pageshow`、切回前台），它们各自都是「用户回来了 / 连接恢复了，
  // 补一次」，而首屏这一轮刚拉过 —— 不挡住就是同一份 190 KB 的历史立刻再发两遍。
  // 实测（Lighthouse 的 network-requests）：`/api/sessions` 发两次、
  // `/api/session` 一次在 37ms、一次在 1612ms，两次都把整窗历史拉回来。
  let bootedAt = 0;
  $effect(() =>
    untrack(() => {
      bootedAt = Date.now();
      // 地址栏里带了 `/s/<id>` 就认它（刷新 / 分享 / 从历史里回来都走这条）。
      //
      // **没带（`/`）= 空对话**：冷启动就该是「来思考些什么?」，不是把上次那条
      // 翻出来。「上次看的是哪条」这个东西服务端根本不存（见 core 的
      // `SessionStore` 注释），前端也不往 localStorage 里放 —— 这个 App 一直
      // 守着「不往本地存东西」那条线。
      const fromUrl = sessionFromPath();
      if (fromUrl) {
        sessionId = fromUrl;
        void loadMessages();
      }
      void loadSessions();
    }),
  );

  /**
   * 后退 / 前进 → 换会话。
   *
   * 只处理「地址变了」这一种：覆盖层的 popstate 由 `lib/overlay.ts` 自己接
   *（它看栈、不看地址）。两者压进 history 的顺序天然正确 —— 覆盖层是后压的，
   * 后退时它先弹出去，再轮到换会话。
   *
   * `id` 为 `null`（退回到 `/`）等于「新对话」：和点头部的「新对话」同一个语义
   *（`fresh` = 服务端还没有这条）。
   */
  $effect(() => {
    return onRouteChange((id) => {
      if (id === (sessionId || null)) return; // 地址没实质变化（覆盖层弹栈会走到这儿）
      if (!id) {
        newSession();
        return;
      }
      // 不重复 `syncUrl`：地址已经是它了，`switchSession` 里那次会自己认出没变
      void switchSession(id);
    });
  });

  /**
   * 「刚起来这一会儿不算数」的窗口（ms）。
   *
   * 首屏那几个同步入口（WS 连上 / `pageshow` / `visibilitychange`）和挂载几乎是
   * 同时到的；隔这么久之内的，认定为首屏那一轮已经覆盖，不再发。
   * 取 1500：够覆盖连接建立 + 首屏渲染，又不会把「用户真去后台转了一圈马上回来」
   * 这种挡掉（那本来就有各自的 `SYNC_GAP_MS` 兜着）。
   */
  const BOOT_GRACE_MS = 1500;
  const justBooted = () => Date.now() - bootedAt < BOOT_GRACE_MS;

  // 有人在**服务端**动这个会话 → 立即对一次账。
  //
  // 为什么必须有这一条：页面自己发起的回合它连着那条流（`live`），可别人发起的它
  // 一无所知 —— 手机上按的、另一台电脑上那个页面按的、排队消息接着开的那个回合，
  // 都是在别处动。这时页面闲着、`serverBusy` 还是 false，轮询根本没开，于是它一直
  // 停在旧内容上，直到人去切窗口（focus）/ 刷新。实测就是这样：服务端整段历史都写好
  // 了（`busy=false`），页面还是空态，`focus` 一响立刻全出来。
  //
  // 核心本来就把这件事放在总线上了（`Event::TurnActive` / `Event::Turn`，手机壳的
  // 通知栏就是听它），只是页面以前只处理 hello/device/log/ack，把这两个白收了。
  //
  // 这一次对账之后 `serverBusy` 就是 true，1.5 秒的轮询接着往下走；收尾那一下（false）
  // 也补一次，把最后一步的正文捞回来（轮询可能在最后一条落进会话之前就停了）。
  $effect(() => {
    // 只为了盯住这个位：回合起 / 止各变一次，一变就跑一遍
    void store.turnActive;
    void untrack(() => loadMessages());
  });

  // WS 重新连上 → 也同步一次。
  //
  // 连接可能断很久（换网络、核心重启、WebView 刚从系统冻住里醒来），这期间服务端
  // 发生的事这条页面一概不知。`reloadHealth` 在 store 里替我们做了（模型 / 能力 /
  // 沙箱钥匙），消息列表只能这里补。
  $effect(() => {
    if (!store.connected) return;
    void untrack(() => {
      // 首屏那一轮刚拉过就别再来一遍（这条在挂载后几毫秒就会响）
      if (justBooted()) return;
      void loadMessages();
    });
  });

  // 切回前台 / 重新聚焦 → 同步一次。
  //
  // 为什么需要：App 切到后台时 WebView 会挂起 JS，SSE 流可能就断了。
  // 但**服务端的 agent 不会停**（前台服务保着进程，Rust 线程照跑，
  // SSE 客户端断开只是让 tx.send 失败然后被忽略）。
  // 所以切回来时从服务端拉最新状态，而不是相信本地的旧视图。
  //
  // 三个事件常常一起来（手机切回前台：visibilitychange + focus + pageshow），
  // 只认第一个：不然同样的两个请求一下发三遍 —— 经隧道 / 中继时每个都是一条新连接。
  /** 两次同步至少隔这么久（ms） */
  const SYNC_GAP_MS = 1000;
  let lastSync = 0;
  $effect(() => {
    const sync = () => {
      if (document.visibilityState !== "visible") return;
      // `pageshow` 在**首次加载**时也会触发（不只是从 bfcache 恢复），而
      // `lastSync` 初值是 0 —— 以前这条等于「挂载后立刻再拉一轮」。首屏那轮
      // 刚拉过就跳过。
      if (justBooted()) return;
      const now = Date.now();
      if (now - lastSync < SYNC_GAP_MS) return;
      lastSync = now;
      void loadMessages();
      void loadSessions();
    };
    document.addEventListener("visibilitychange", sync);
    window.addEventListener("focus", sync);
    window.addEventListener("pageshow", sync);
    return () => {
      document.removeEventListener("visibilitychange", sync);
      window.removeEventListener("focus", sync);
      window.removeEventListener("pageshow", sync);
    };
  });

  // 服务端还在跑 → 轮询到结束。
  //
  // 比"重连 SSE"简单得多，而且跨 WebView 挂起也可靠。
  // 服务端每个步骤完成都会把 assistant 消息写进会话，所以轮询能
  // 看到逐步骤推进。
  /** 轮询间隔（ms）。服务端的粒度是「一步」，1.5 秒完全够用；再密就是白转发。 */
  const POLL_MS = 1500;
  /** `stop()` 之后那次拉取的定时器（见那里） */
  let afterStop: ReturnType<typeof setTimeout> | undefined;
  /** 按下停止之后过多久拉一次（给服务端一点时间把「已停止」的工具结果补上） */
  const AFTER_STOP_MS = 300;
  $effect(() => {
    if (!running) return;
    const t = setInterval(() => void loadMessages(), POLL_MS);
    return () => clearInterval(t);
  });

  /**
   * 服务端把排队的消息交给模型了 → 把那几条从「排队中」挪进正文。
   *
   * 挪进去就成了本地历史的一部分（`done: true`）：服务端那边它们也确实是历史了
   * —— 紧接着流里会来一个 `start`，模型的回答进**新的一条**助手消息（`consume`
   * 的 `onRestart`），和服务端历史的形状一致（`session_view` 里 User 会打断
   * 助手消息的合并）。
   */
  $effect(() =>
    onQueuedDelivered((ids) => {
      if (!ids.length) return;
      const moved = queue.filter((q) => ids.includes(q.id));
      if (moved.length) queue = queue.filter((q) => !ids.includes(q.id));
      for (const q of moved) {
        messages.push({
          id: q.id,
          role: "user",
          parts: q.parts,
          done: true,
        });
      }
    }),
  );

  // 当前会话里排队的那几条（其它会话的排着队，切回去才看得见）。
  let queued = $derived(queue.filter((q) => q.sid === sessionId));

  // 它们按「**还没发出去**的用户消息」渲染（`done: false`）—— 消息列表里就是
  // 普通用户气泡的样子，只多一圈虚线和左边一个 ×（详见上面 Queued 的注释）。
  let queuedMsgs = $derived(
    queued.map((q): Message => ({
      id: q.id,
      role: "user",
      parts: q.parts,
      done: false,
    })),
  );

  // 左上角那个菜单按钮：窄屏是开抽屉；宽屏是**收 / 展**侧边栏
  // （宽屏侧边栏一直在，只是可以收起来把地方让给对话）。
  //
  // **打开不发请求**。抽屉里只有三样东西：会话列表 / 收藏 / 机器标签，前两样都是
  // 模块级状态（`sessions` / `favorites`），不需要「打开时去确认一下」：
  //
  //   - 本页改过列表（新建 / 重命名 / 删除 / 切会话 / 回合收尾）→ 那些地方自己已经拉了；
  //   - 用户从别处回来（切前台 / 聚焦 / 连接恢复）→ 那三条入口已经拉了；
  //   - 首屏 → 挂载时拉了。
  //
  // 以前这里会多发一份 `/api/sessions`（22 KB，手机 WiFi 上几百毫秒），为的是盖住
  // 「别的客户端在服务端改了、本页一直在前台没动」这一种情况 —— 那一下的代价和它
  // 出现的频率不成比例，而真碰上了切一下前台就是最新的。
  function toggleSidebar() {
    if (wide.on) sidebar = !sidebar;
    else drawer = true;
  }

  // 返回键关覆盖层：每个覆盖层自己登记（lib/overlay.ts），这里不管。
  const close = () => {
    drawer = false;
    sheet = false;
  };

  // —— 在对话区往右滑，打开抽屉（窄屏）——
  //
  // 跟手：拖多少抽屉就出来多少，松手看速度 / 位置决定展开还是收回 —— 和往左拖关抽屉
  // 是同一套手势（lib/drag.ts），只是方向反过来、拖的是还没出来的抽屉。
  //
  // 从屏幕**最左边**起手的那一下归系统（Android 的返回手势），轮不到页面；从对话中间
  // 往右划就行。

  /** 拖着打开时抽屉拉出来的比例（交给 Drawer 的 `peek`）；null = 不在跟手 */
  let peek = $state<number | null>(null);
  /** 触屏才开：桌面上用鼠标往右拖是在选字，不是想开抽屉 */
  const touch =
    typeof matchMedia === "function" && matchMedia("(pointer: coarse)").matches;

  /** 这一下能不能当成「开抽屉」：输入框里是拖光标；能横着滚、又没在最左的内容（代码块、
   *  宽表格）是想把它往回滚 */
  function swipeable(t: EventTarget | null): boolean {
    for (
      let el = t instanceof Element ? t : null;
      el && el !== document.body;
      el = el.parentElement
    ) {
      if (el.matches('input, textarea, [contenteditable="true"]')) return false;
      if (el.scrollLeft > 0) return false;
    }
    return true;
  }

  const openDrag = {
    axis: "x" as const,
    dir: 1 as const,
    // 抽屉宽 92%（app.css 的 --drawer-w）：手指走完这么远就是整个拉出来了
    size: () => innerWidth * 0.92,
    canStart: (t: EventTarget | null) =>
      touch && !wide.on && !drawer && swipeable(t),
    onStart: () => {
      peek = 0;
      drawer = true;
    },
    onMove: (p: number) => (peek = p),
    onEnd: (open: boolean) => {
      if (open) peek = null;
      else drawer = false; // 出场动画从当前位置接着收（Drawer 的 slideOut）
    },
  };
  // 收回之后把跟手状态清掉，下次点菜单打开就是正常的进场动画
  $effect(() => {
    if (!drawer) peek = null;
  });

  // —— 对话 ——

  /**
   * 收一条流：把 chunk 一个个喂进 `msg`，直到流结束。
   *
   * `me` 是这条流的身份（`live` 和停止键认它）；`msg` 是这条流往里写的消息 ——
   * 服务端把排队的消息交给模型时会再来一个 `start`，那时换**新的一条**写
   * （见下面 `onRestart`），和服务端历史的形状一致。
   */
  async function run(
    me: Run,
    msg: Message,
    stream: AsyncGenerator<UiChunk, void, undefined>,
  ) {
    view = { k: "live", run: me };
    let cur = msg;
    try {
      await consume(msg, stream, (prev) => {
        prev.done = true;
        cur = store.beginStreaming();
        messages.push(cur);
        return cur;
      });
    } catch (e) {
      if (!me.controller.signal.aborted) {
        cur.parts.push({ type: "error", text: String(e) });
      }
    } finally {
      cur.done = true;
      // 已经被 detach（切走了）、或者后面又开了一条：别把别人的状态清掉
      if (view.k === "live" && view.run === me) {
        view = { k: "idle" };
        // 流收完了：换成服务端那份（id、合并方式以它为准；流里断了的也补回来）
        void loadMessages();
      }
      // 这一趟可能刚建出新会话（第一句才建）、标题也是服务端起的：要新的那份
      void loadSessions(true);
    }
  }

  /** 新会话到第一句话才在服务端建（点了「新对话」不发就不留痕）。 */
  async function ensureSession(): Promise<string | null> {
    if (!fresh && sessionId) return sessionId;
    try {
      const { id } = await post<{ id: string }>("/api/sessions");
      // 哪怕这会儿已经在跑了也用 `idle`：`deliver` 紧接着会把 `view` 换成
      // `live`（或者服务端说排队了，由 `loadMessages` 认成 `remote`）。
      view = { k: "idle" };
      sessionId = id;
      rev = "";
      // 服务端刚建出来的会话：**换地址**（`replace`，不是 `push`）——
      // 用户点「新对话」那一下不该在历史里留一格「空对话」，否则从新会话
      // 按一次后退会退到另一个空对话，看着像没反应。
      // 之后在抽屉里切会话才 `push`（见 `switchSession`）。
      syncUrl(id, true);
      return id;
    } catch {
      return null;
    }
  }

  /**
   * 发一条消息。
   *
   * **谁跑谁排队由服务端说了算**，前端不猜：先按「排队中」画出来，然后看
   * `POST /api/chat` 的**首帧** ——
   *
   *   - 正常流（`start` 开头）= 这个回合开跑了 → 那条气泡翻进正文，接着收流；
   *   - `data-queued` = 这个会话正跑着，消息进了**服务端的**队列 → 虚线气泡留着，
   *     等回合跑到步边界时服务端会把它交给模型（`data-queued-delivered`）。
   *
   * 为什么不能由前端判断「正跑着 → 排队」：那样队列就活在 WebView 里，App 切到
   * 后台、WebView 被冻住或回收，这条消息永远发不出去；而且只有等整个回合结束
   * 才发得出去，模型中途空下来也拿不到它。
   */
  async function send(attachments: Upload[]) {
    const text = input.trim();
    if (!text && attachments.length === 0) return;
    input = "";
    ownSend++;

    const id = uid();
    const parts = userParts(text, attachments);
    const sid = await ensureSession();
    if (!sid) {
      sent++;
      messages.push({ id, role: "user", parts, done: true });
      messages.push({
        id: uid(),
        role: "assistant",
        done: true,
        parts: [{ type: "error", text: "建会话失败，再试一次" }],
      });
      return;
    }
    queue = [...queue, { id, sid, parts, pending: true }];
    await deliver(id, sid, text, attachments, parts);
  }

  /**
   * 一条用户消息的 parts：正文 + 附件。
   *
   * 发出去的那条（`deliver`）和排队的那条（`queuedMsgs`）用的都是它 ——
   * 所以排队中那条能直接当用户消息渲染，长得和发出去的一模一样。
   */
  function userParts(text: string, attachments: Upload[]): Part[] {
    return [
      { type: "text", id: "u", text },
      ...attachments.map((a, i): Part =>
        a.kind === "image"
          ? {
              type: "image",
              id: `a${i}`,
              url: a.url,
              name: a.name,
              mime: a.mime,
            }
          : {
              type: "file",
              id: `a${i}`,
              url: a.url,
              name: a.name,
              mime: a.mime,
            },
      ),
    ];
  }

  /**
   * 真的把一条消息交给服务端（首帧决定它是「开跑」还是「排队」）。
   */
  async function deliver(
    id: string,
    sid: string,
    text: string,
    attachments: Upload[],
    parts: Part[],
  ) {
    const me: Run = { controller: new AbortController() };
    const stream = streamFrom(
      "/api/chat",
      { text, id, session: sid, attachments: attachments.map((a) => a.id) },
      me.controller.signal,
    );
    let first: UiChunk | null = null;
    try {
      first = (await stream.next()).value ?? null;
    } catch (e) {
      // 连请求都没发出去：把那条虚线气泡撤了，用一条错误消息说清楚（字不能丢）
      queue = queue.filter((q) => q.id !== id);
      messages.push({ id, role: "user", parts, done: true });
      messages.push({
        id: uid(),
        role: "assistant",
        done: true,
        parts: [{ type: "error", text: String(e) }],
      });
      return;
    }

    if (isQueued(first)) {
      // 排上了：服务端那边已经有它了，这条流到此为止（读完就扔，别挂着连接）
      queue = queue.map((q) => (q.id === id ? { ...q, pending: false } : q));
      me.controller.abort();
      // 服务端可能正是因为这条才开了一个回合（上个回合出错后留下的队列，
      // 见 core 的 `http::chat`）—— 那个回合没有连着我们，拉一次才知道它在跑、开始轮询
      void loadMessages();
      // 本地这条流结束了 ≠ 服务端那个回合结束了：此刻还在跑的是**另一条**流
      // （或者根本没连着）。所以这里 `view` 一点都不动 —— 紧接着那次
      // `loadMessages` 会按服务端的 `busy` 把它置成 `remote`，轮询就开起来了。
      return;
    }

    // 开跑了：那条虚线气泡翻进正文（换了个 id 的影子都没有 —— 就是同一个 id）
    queue = queue.filter((q) => q.id !== id);
    sent++;
    messages.push({ id, role: "user", parts, done: true });
    const msg = store.beginStreaming();
    messages.push(msg);
    await run(me, msg, prepend(first, stream));
  }

  /**
   * 撤掉一条**还在排队**的消息（虚线气泡左边那个 ×）。
   *
   * 队列在服务端，所以得告诉它 —— 不然那条照样会在下一个步边界交给模型。界面上
   * 立刻撤掉（不等回话）：真撤不掉（刚好在这一刻被交出去了）也没关系，下一次
   * `loadMessages` 就看到它已经落进正文了。
   */
  function dropQueued(id: string) {
    const item = queue.find((q) => q.id === id);
    queue = queue.filter((q) => q.id !== id);
    if (!item) return;
    void post("/api/sessions/unqueue", { session: item.sid, id }).catch(() => {
      /* 撤不掉就算了：它已经落进正文，或者下次轮询会纠正 */
    });
  }

  const stop = () => {
    // 服务端循环看不到 SSE 断连（tx.send 失败会被忽略），得显式告诉它停。
    // 否则前端显示停了，agent 还在后台一轮轮跑工具、烧 token。
    // 只停**这个**会话的回合：别的会话在跑的不受影响。
    // 没有会话 id 就没有回合可停（新对话、还没发第一句）——
    // 服务端现在**要求**带 session，所以别发那个必然 400 的请求。
    if (sessionId)
      void post("/api/stop", { session: sessionId }).catch(() => {
        /* 停不下来也得把本地的流断掉（下面照样 abort） */
      });
    const r = view.k === "live" ? view.run : null;
    // 点了停止：先按「没在跑」画出来（服务端那边 300ms 内会确认），
    // 拉回来的那份要是还在跑，`loadMessages` 会再翻成 `remote`
    view = { k: "idle" };
    r?.controller.abort();
    // 本地流断了，服务端收尾（补上「已停止」的工具结果）的那几条要拉回来；
    // 没连着流时按的停止也一样：马上拉一次，「运行中」尽快消失。
    //
    // 句柄留着、下一次按下先清掉：300ms 内连按两下停止（或者按了停止又马上发下
    // 一句），前一个定时器会在新回合开跑之后才到，那次拉取和 `/api/chat` 赛跑 ——
    // `at !== sent` 那道门会把它丢掉，但白传一整窗历史。
    clearTimeout(afterStop);
    afterStop = setTimeout(() => void loadMessages(), AFTER_STOP_MS);
  };
</script>

<!-- 抽屉那堆 props 只有「并排 / 盖在上面」这一处不同，两个宽度下各挂一次。
     以前是把同一份 11 个 prop 抄了两遍，加一个 prop 要记得改两处。 -->
{#snippet drawerBody(pinned: boolean)}
  <Drawer
    {sessions}
    {peek}
    onclose={close}
    onswitch={switchSession}
    ondelete={deleteSession}
    onrename={renameSession}
    onsettings={() => (settings = true)}
    machine={store.machine}
    onmachine={openMachines}
    onmore={loadMoreSessions}
    more={sessionsNext !== null}
    moreBusy={sessionsMoreBusy}
    moreError={sessionsMoreError}
    {pinned}
  />
{/snippet}

<div class="flex h-full overflow-hidden bg-ink">
  <!-- 宽屏：侧边栏**并排**在左边（不是抽屉），默认收着，点左上角的菜单展开。
       窄屏：它还是盖在对话上的抽屉，同样是那个菜单按钮控制（见下面）。 -->
  {#if wide.on && sidebar}
    {@render drawerBody(true)}
  {/if}

  <!-- 对话区：把侧边栏剩下的宽度**全部**拿走（不限宽）。 -->
  <div
    class="shell relative h-full w-full min-w-0 flex-1 overflow-hidden"
    style:--composer-h="{composerH}px"
    use:dismissable={openDrag}
  >
    <!-- 消息列表铺满整个高度：内容从头部和输入卡片下面穿过去。
         顶部/底部的渐隐是 `.shell` 的伪元素（只为了装饰，不值得占两个 div）。 -->
    <!-- 不要给 main 加 z-index：`z-0` 会形成层叠上下文，把里面
         弹出面板的 z-50 困住，压不过同级 z-20 的输入框（踩过）。
         z-auto 的定位元素不建层叠上下文，浮层的 z-10 依旧在它上面。 -->
    <main class="absolute inset-0">
      {#if messages.length === 0 && queuedMsgs.length === 0}
        <h1 class="grid h-full place-items-center text-2xl font-normal">
          来思考些什么?
        </h1>
      {:else}
        <MessageList
          {messages}
          pending={queuedMsgs}
          ondrop={dropQueued}
          follow={ownSend}
          session={sessionId}
          hasolder={hasOlder}
          onloadearlier={loadOlder}
          onprefetch={prefetchOlder}
          ontrim={trimOldPages}
          step={MSG_WINDOW}
        />
      {/if}
    </main>

    <!--
      渐隐用浮层而不是滚动容器上的 mask。
      mask 会让滚动容器每帧重新合成 —— 实测手机上划动明显卡顿。
    -->

    <!-- 头部浮在内容上 -->
    <header
      class="pointer-events-none absolute inset-x-0 top-0 z-20 flex items-center justify-between
             px-4 pb-1 pt-[calc(0.25rem+env(safe-area-inset-top))]"
    >
      <!-- 窄屏：开抽屉；宽屏：收 / 展侧边栏（两个宽度下都在） -->
      <button
        onclick={toggleSidebar}
        aria-label="菜单"
        title={wide.on ? "收起 / 展开侧边栏" : "菜单"}
        class="tap-y pointer-events-auto p-1 text-fg"
      >
        <Icon name="menu" size={20} />
      </button>
      <!-- 右上角 = 新对话（对齐 Claude 移动端：左边菜单，右边开新会话）。 -->
      <div class="pointer-events-auto ml-auto flex items-center gap-1">
        <!-- agent 的浏览器：它自己在后台开了网页时，用户可以看看它在干什么、
             也能自己接着操作（壳里那块是**同一只** WebView）。
             **只在它真开着页面时才出现** —— 没开过页面时点它只会弹一句
             「还没打开过网页」，那就不该显示（也少一个常驻图标）。 -->
        {#if browserAvailable}
          <button
            onclick={openAgentBrowser}
            aria-label="看 agent 的浏览器"
            title="看 agent 的浏览器"
            class="tap-y p-1 text-fg"
          >
            <Icon name="globe" size={20} />
          </button>
        {/if}
        <button
          onclick={newSession}
          aria-label="新对话"
          title="新对话"
          class="tap-y p-1 text-fg"
        >
          <Icon name="newchat" size={20} />
        </button>
      </div>
    </header>

    <!-- 输入卡片也浮在内容上 -->
    <div class="absolute inset-x-0 bottom-0 z-20" bind:offsetHeight={composerH}>
      <!-- 排队中那几条不在这里（输入框上方）—— 它们画在**消息列表末尾**，
           和已发送的用户气泡同一个样子（虚线 + 左边一个 ×），见 MessageList。 -->

      <Composer
        bind:value={input}
        busy={running}
        {usage}
        {ctxWindow}
        onsend={send}
        onstop={stop}
        onmodel={() => (sheet = true)}
      />
    </div>
  </div>

  <!-- 窄屏：抽屉盖在对话上（宽屏它已经并排在左边了） -->
  {#if !wide.on && drawer}
    {@render drawerBody(false)}
  {/if}
</div>

{#if sheet}<ModelSheet
    onclose={close}
    onconfig={() => (settings = true)}
    rev={settingsRev}
  />{/if}

{#if machineSheet}<MachineSheet onclose={() => (machineSheet = false)} />{/if}

{#if settings}<Settings onclose={closeSettings} />{/if}

{#if viewer.url}<Viewer />{/if}
