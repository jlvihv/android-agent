/**
 * 每条会话的「活状态」：正在收的那条流、在不在跑、用量；外加所有会话的
 * 热集合、轮询、发消息。
 *
 * ## 为什么在组件外面
 *
 * 以前这些全是 `App.svelte` 的 `$state`。于是「切走」只能等于「把流断掉」——
 * 因为 `messages` 是**一个**数组，切会话时 `resetView()` 会清空它，旧流要是还
 * 活着，它 `consume` 出来的 delta 就会写进新会话的列表（串内容）。owner 和视图
 * 是同一个东西，所以只能一起死。
 *
 * 断流的代价是看得见的：切回来之后 `view` 落到 `remote`，靠 1.5 秒一次的
 * `/api/session` 轮询看进度 —— 而轮询的粒度是「一步」（核心在一步 LLM 调用
 * 结束、工具也跑完之后才把 assistant 写进历史），所以逐字流出来的东西全没了。
 *
 * 现在 owner 是这里，**视图只是它的一个读者**：切走不碰流（服务端本来就不看
 * 有没有人听，见 `http::sse_response`），切回来接着看，一个字都不少。
 *
 * ## 三个文件的分工
 *
 *   - `pager.svelte.ts` —— 消息**窗口**（翻页 / 预取 / 收窗口 / 跟服务端对账）
 *   - `queue.svelte.ts` —— 「排队中」那几条的**镜像**（服务端队列的画法）
 *   - 这里 —— **流**（收 / 停 / 身份）、在不在跑、用量、热集合、轮询
 *
 * 以前三样都在这个文件里（1052 行），读的时候要同时在脑子里放十几个不变式。
 *
 * ## 热集合
 *
 * 不能无限留：一条长会话手上那一窗就几百 KB（工具输出很胖）。所以只有
 * `HOT_MAX` 条是「热的」（可能连着流、可能还留着消息），超了就把**最久没看、
 * 而且不是正在看的**那条丢掉 —— 丢掉时如果它还在流，就 `abort` 本地的连接
 * （服务端照跑），下次进去按 `remote` 走轮询，和改造前一模一样。
 *
 * 轮询那套**留着**，而且现在归这里管（`syncPoll`）：WebView 被冻住 / 被回收、
 * 流真的断了的时候，它是唯一的兜底（见 docs/06）。
 */

import { post } from "./api";
import { MSG_WINDOW, Pager, type SessionReply } from "./pager.svelte.ts";
import { QueueMirror, type Queued } from "./queue.svelte.ts";
import {
  consume,
  isQueued,
  prepend,
  streamFrom,
  totalsOf,
  type Message,
  type Part,
  type SessionUsage,
  type UiChunk,
  type Upload,
  type Usage,
} from "./sse";
import { uid } from "./uid";

export { MSG_WINDOW };
export type { Queued };

/**
 * 同时「热」着的会话上限。
 *
 * 3 条：够「一边跑 A、一边看 B、顺手回 C」这种真实用法，而三条长会话手上那一窗
 * 加起来还在几 MB 这个量级。超了就丢最久没看的（见文件头）。
 */
const HOT_MAX = 3;

/** 服务端还在跑 → 轮询到结束。粒度是「一步」，1.5 秒完全够用 */
const POLL_MS = 1500;
/** 按下停止之后过多久拉一次（给服务端一点时间把「已停止」的工具结果补上） */
const AFTER_STOP_MS = 300;

/**
 * 「眼前这条会话此刻是什么状态」。**一个联合，不是四个布尔。**
 *
 *   idle    有会话（或空态），没有回合在跑
 *   fresh   点了「新对话」、第一句还没发（服务端**还没有**这条会话）
 *   live    本地正连着它的流（`#run` 是身份：`stop` / 收尾认它）
 *   remote  服务端在跑，本地没连着 —— 切到后台 / 从别的会话切回来 / 别人发起的
 */
export type Kind = "idle" | "fresh" | "live" | "remote";

/**
 * 本地正连着的那条流。
 *
 * **它不放在 `$state` 里**（`Live.#run` 是个普通私有字段），这一条不是风格问题：
 * `$state` 是深代理，`live.run` 读回来会是个新的 Proxy，`=== me` 永远为假 ——
 * 于是流正常收尾时那个「还是我这条吗」的判断恒不成立，`kind` 停在 `live`，
 * 停止键永远亮着（真实踩过，见 `tests/view.test.ts`）。
 */
export type Run = { controller: AbortController };

/**
 * 一条空的助手消息（聊天流从这里开始写）。
 *
 * **返回的必须是 `$state` 代理**，不能是普通对象 —— 这一条不是风格问题：
 * `applyChunk` 是**原地**往 `parts` 里追加 delta 的，而流式期间没有任何东西会
 * 重新给 `messages[i]` 赋值。所以「改的是不是代理」直接决定界面动不动：
 *
 *   - 返回普通对象 → push 进 `messages` 之后，流写的是**原始对象**，
 *     Svelte 的代理感知不到（实测：effect 不重跑），界面上只有用户那条气泡，
 *     模型的回复永远不出现（踩过，而且看起来就像「SSE 没推送」）。
 *   - 先写进一个 `$state`、再**读回来** → 拿到代理，写它才发信号。
 *
 * 回归测试见 `tests/streaming.test.ts`（它盯的就是「写进去能不能被看见」）。
 */
function beginStreaming(): Message {
  const box = $state<{ msg: Message }>({
    msg: { id: uid(), role: "assistant", parts: [], done: false },
  });
  return box.msg;
}

/** 一条用户消息的 parts：正文 + 附件。排队那条和发出去那条用的是它 */
export function userParts(text: string, attachments: Upload[]): Part[] {
  return [
    { type: "text", id: "u", text },
    ...attachments.map((a, i): Part =>
      a.kind === "image"
        ? { type: "image", id: `a${i}`, url: a.url, name: a.name, mime: a.mime }
        : { type: "file", id: `a${i}`, url: a.url, name: a.name, mime: a.mime },
    ),
  ];
}

/**
 * 一条会话的活状态。
 *
 * 生命周期跟着 `Chat` 的热集合走（见文件头），不跟着组件走。
 */
export class Live {
  /** 会话 id。`""` = 还没在服务端建出来的草稿（点了「新对话」、第一句还没发） */
  sid: string;
  /** 消息窗口（翻页 / 对账 / 收窗口）—— 见 `pager.svelte.ts` */
  readonly page: Pager;
  kind = $state<Kind>("idle");
  /** 本会话的 token 用量（输入框里那个上下文圆环） */
  usage = $state<SessionUsage | null>(null);
  /**
   * 「用户自己发了一条」的次数（`MessageList` 的 `follow`）。
   *
   * 与 `page.sent` 分开：那个只管和服务端的竞态，这个只管界面要不要把视口带回尾部。
   * **只有发消息会加** —— 排队那条被服务端交给模型（`data-queued-delivered`）
   * 不算：那是服务端在动，用户可能正往上翻着看历史。
   */
  ownSend = $state(0);
  /** 最近一次被「看」的时刻（热集合淘汰用，单调递增的计数器） */
  lastUsed = 0;

  /** 本地正连着的那条流（身份，**不在 `$state` 里**，见 `Run` 的注释） */
  #run: Run | null = null;

  constructor(
    readonly chat: Chat,
    sid: string,
  ) {
    this.sid = sid;
    this.page = new Pager(
      () => this.sid,
      () => this.chat.get(this.sid) === this,
    );
  }

  /** 消息数组（`App` / `MessageList` 直接读它） */
  get messages(): Message[] {
    return this.page.messages;
  }
  /** 本地正连着流 */
  get busy(): boolean {
    return this.kind === "live";
  }
  /** 点了新对话还没建 */
  get fresh(): boolean {
    return this.sid === "";
  }
  /** 有没有回合在跑（本地或远端）—— 输入框的停止键看它 */
  get running(): boolean {
    return this.kind === "live" || this.kind === "remote";
  }
  /** 服务端还有没有更早的（推导的：手上这窗的起点 > 0）*/
  get hasOlder(): boolean {
    return this.page.hasOlder;
  }
  get run(): Run | null {
    return this.#run;
  }

  /**
   * 换状态。**所有对 `kind` 的写都走这里** —— 轮询的开关挂在上面
   *（`Chat#syncPoll`），漏一处就是「服务端还在跑、界面再也不刷新」。
   */
  setKind(k: Kind): void {
    if (this.kind === k) return;
    this.kind = k;
    this.chat.syncPoll();
  }

  /**
   * 被热集合淘汰了 / 被删掉了：断流、**把状态清掉**（留着的那一份只会占内存）。
   *
   * 从 `live` 退到 `remote` 再 abort：`kind` 落在 `remote` 是「服务端还在跑」
   * 的正确说法，而这条已经不在热集合里了，轮询不会轮到它（`syncPoll` 只看
   * 表里那些）。下次进去按 `remote` 从服务端重拉 —— 和改造前切会话的行为一样。
   */
  evict(): void {
    if (this.kind === "live") {
      this.setKind("remote");
      this.#run?.controller.abort();
    }
    this.page.reset();
    this.usage = null;
  }

  // —— 跟服务端对账 ——

  /**
   * 跟服务端对一次账：把这条会话的最新状态（消息、排队、用量、在不在跑）拉回来。
   *
   * 窗口那半（合并 / 作废 / 套补丁）在 `Pager#load` 里；这里只负责**该不该发**
   * 和**回包里跟流有关的那几件**。
   */
  async load(): Promise<void> {
    // 本地正连着流（本地才是最新的）/ 还没建会话（服务端那边没有）
    if (this.busy || this.fresh) return;
    await this.page.load((d: SessionReply) => {
      // 服务端说这个会话在不在跑。**只在本地没连着流时才认**：连着流的时候
      // 本地那份更新（`run` 收完流会自己落到 `idle`），拿一个路上算好的旧值
      // 盖上去只会让停止键闪一下。
      if (!this.busy) this.setKind(d.busy ? "remote" : "idle");
      // 排队那几条按服务端**整份重来**（`unchanged` 时也要）：它们的变化不进
      // `rev`（那是消息的版本号），光看 rev 看不出「排队里少了一条」。
      const sid = d.session ?? this.sid;
      if (d.queued) this.chat.queue.replace(sid, d.queued);
      // 服务端说没变：连 JSON 里都不带 messages，窗口不用动
      if (d.unchanged) return false;
      // 用量整份换成服务端的：它数的是这个会话的**全部**调用，而我们手上
      // 那份只从页面加载后开始算。`last` 没有 = 这个会话还没跑过
      const u = d.usage as (Partial<Usage> & { last?: Usage | null }) | null;
      this.usage = u?.last ? { last: u.last, total: totalsOf(u) } : null;
      if (d.turnError) this.showTurnError(d.turnError);
    });
  }

  /**
   * 上一个回合失败了：在末尾补一条错误（服务端历史里没有这条，它只给人看）。
   *
   * 两种来路：排队消息接着开的回合（没有人连着它的流，错误只能从这里来），以及
   * 刷新 / 回到前台之后的前台回合（本地那条错误随着整份换掉的历史没了）。
   * 末尾已经有同一句错误就不再补。
   */
  showTurnError(text: string): void {
    const last = this.messages.at(-1);
    if (last?.parts.some((p) => p.type === "error" && p.text === text)) return;
    this.messages.push({
      id: uid(),
      role: "assistant",
      done: true,
      parts: [{ type: "error", text }],
    });
  }

  // —— 收流 ——

  /**
   * 收一条流：把 chunk 一个个喂进 `msg`，直到流结束。
   *
   * `me` 是这条流的身份（`stop` / 收尾认它）；`msg` 是这条流往里写的消息 ——
   * 服务端把排队的消息交给模型时会再来一个 `start`，那时换**新的一条**写
   *（`onRestart`），和服务端历史的形状一致。
   */
  async #runStream(
    me: Run,
    msg: Message,
    stream: AsyncGenerator<UiChunk, void, undefined>,
  ): Promise<void> {
    this.#run = me;
    this.setKind("live");
    let cur = msg;
    try {
      await consume(msg, stream, {
        onRestart: (prev) => {
          prev.done = true;
          cur = beginStreaming();
          this.messages.push(cur);
          return cur;
        },
        // 需要「这是哪条会话」的那些 data-* 帧（用量、排队交付）。以前是
        // `lib/sse.ts` 里的全局 sink —— 没有 sid，两条会话同时跑就会串。
        onChunk: (c) => this.#onData(c),
      });
    } catch (e) {
      if (!me.controller.signal.aborted) {
        cur.parts.push({ type: "error", text: String(e) });
      }
    } finally {
      cur.done = true;
      // 已经被淘汰 / 被删掉了、或者后面又开了一条：别把别人的状态清掉
      if (this.#run === me) {
        this.#run = null;
        if (this.kind === "live") {
          this.setKind("idle");
          // 流收完了：换成服务端那份（id、合并方式以它为准；流里断了的也补回来）
          void this.load();
        }
      }
      // 这一趟可能刚建出新会话（第一句才建）、标题也是服务端起的：要新的那份
      this.chat.ended(this);
    }
  }

  /** 流里的 `data-*` 帧：用量的圆环、排队那条「交出去了」 */
  #onData(c: UiChunk): void {
    if (c.type === "data-usage") {
      const { session, ...last } = c.data as Usage;
      if (session) this.usage = { last, total: totalsOf(session) };
      return;
    }
    if (c.type === "data-queued-delivered") {
      const ids = (c.data as { ids: string[] }).ids ?? [];
      if (!ids.length) return;
      // 挪进**这条会话**的正文（以前是一个没有 sid 的全局回调，两条会话同时
      // 跑就会把 A 的排队项挪进 B）
      for (const q of this.chat.queue.deliver(ids)) {
        this.messages.push({
          id: q.id,
          role: "user",
          parts: q.parts,
          done: true,
        });
      }
    }
  }

  /**
   * 真的把一条消息交给服务端（首帧决定它是「开跑」还是「排队」）。
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
  async deliver(
    id: string,
    text: string,
    attachments: Upload[],
    parts: Part[],
  ): Promise<void> {
    const sid = this.sid;
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
      this.chat.queue.remove(id);
      this.page.sent++;
      this.messages.push({ id, role: "user", parts, done: true });
      this.messages.push({
        id: uid(),
        role: "assistant",
        done: true,
        parts: [{ type: "error", text: String(e) }],
      });
      return;
    }

    if (isQueued(first)) {
      // 排上了：服务端那边已经有它了，这条流到此为止（读完就扔，别挂着连接）
      this.chat.queue.settle(id);
      me.controller.abort();
      // 服务端可能正是因为这条才开了一个回合（上个回合出错后留下的队列）——
      // 那个回合没有连着我们，拉一次才知道它在跑、开始轮询
      void this.load();
      // 本地这条流结束了 ≠ 服务端那个回合结束了：此刻还在跑的是**另一条**流
      // （或者根本没连着）。所以这里 `kind` 一点都不动 —— 紧接着那次
      // `load` 会按服务端的 `busy` 把它置成 `remote`，轮询就开起来了。
      return;
    }

    // 开跑了：那条虚线气泡翻进正文（换了个 id 的影子都没有 —— 就是同一个 id）
    this.chat.queue.remove(id);
    this.page.sent++;
    // 第一帧到了 = 服务端那边这条用户消息和标题已经落库 → 它现在才**真的**
    // 出现在 `/api/sessions` 里（那条接口按 `has_content()` 过滤空会话）。
    // 侧边栏要在这儿刷新，不然新会话要等这一轮整个跑完（`ended`）才冒出来。
    this.chat.started(this);
    this.messages.push({ id, role: "user", parts, done: true });
    const msg = beginStreaming();
    this.messages.push(msg);
    await this.#runStream(me, msg, prepend(first, stream));
  }

  /** 点了停止：告诉服务端，断掉本地的流，300ms 后拉一次 */
  stop(): void {
    // 服务端循环看不到 SSE 断连（tx.send 失败会被忽略），得显式告诉它停。
    // 否则前端显示停了，agent 还在后台一轮轮跑工具、烧 token。
    // 只停**这个**会话的回合：别的会话在跑的不受影响。
    // 没有会话 id 就没有回合可停（新对话、还没发第一句）——
    // 服务端**要求**带 session，所以别发那个必然 400 的请求。
    if (this.sid)
      void post("/api/stop", { session: this.sid }).catch(() => {
        /* 停不下来也得把本地的流断掉（下面照样 abort） */
      });
    const r = this.#run;
    // 点了停止：先按「没在跑」画出来（服务端那边 300ms 内会确认），
    // 拉回来的那份要是还在跑，`load` 会再翻成 `remote`
    this.setKind("idle");
    r?.controller.abort();
    // 本地流断了，服务端收尾（补上「已停止」的工具结果）的那几条要拉回来；
    // 没连着流时按的停止也一样：马上拉一次，「运行中」尽快消失。
    this.chat.afterStop(this);
  }
}

/** `Chat` 往外发的事件（见 `Chat#subs`） */
export type ChatEvents = {
  /** 草稿在服务端建出来了（`sid` 变了） */
  created: ((sid: string) => void)[];
  /** 一条流开跑了（第一帧到了，服务端那边消息和标题已落库） */
  started: ((live: Live) => void)[];
  /** 一条流收尾了 */
  ended: ((live: Live) => void)[];
};

/**
 * 所有会话的活状态 + 排队 + 轮询。
 *
 * 一个实例（`chat`）。组件只读它，不自己管连接 —— 这正是「切页面不断流」的前提：
 * 流的所有者是模块，不是某一次挂载。
 */
export class Chat {
  /**
   * 会话 id → 活状态。
   *
   * **`$state<Map>` 在这里是错的**（实测）：Svelte 的 `proxy()` 只代理 plain
   * object / array，`Map` 的原型不是 `object_prototype`，`proxy()` 直接原样返回
   * （见 `svelte/src/internal/client/proxy.js` 开头那个判断）。于是 `set` /
   * `delete` **一个信号都不发** —— `current` 只在 `viewing` 变时才重算。
   *
   * 平时看不出来，因为 `show()` 总是先改 `viewing`；但「只动 map 不动 viewing」
   * 就露馅：冷启动（`viewing` 本来就是 `""`）时建会话失败，那条错误气泡永远
   * 不显示，用户发的字凭空消失（实测复现过，见 `tests/chat.test.ts`）。
   *
   * 所以 map 是普通 `Map`，另外用一个 `$state` 计数器当信号：**每次增删都
   * `#bump()`**，读它的地方（`current` / `get` / `#all`）都 `void this.#rev`。
   * 比换 `SvelteMap` 稳：`svelte/reactivity` 在 `bun test` 里解析到 server 版
   * （非响应式桩），浏览器里才是 client 版 —— 那种「测试和线上不是同一个实现」
   * 的坑不值得为一个 Map 踩。
   */
  #map = new Map<string, Live>();
  /** `#map` 的变更计数（见上面那段）。只为了给读它的人一个依赖。 */
  #rev = $state(0);
  #bump(): void {
    this.#rev++;
  }

  /** 排队等着交给模型的消息（服务端那份队列的镜像，见 `queue.svelte.ts`） */
  readonly queue = new QueueMirror();
  /** 界面现在看着哪条（`""` = 草稿 / 新对话）*/
  viewing = $state("");

  #clock = 0;
  #poll: ReturnType<typeof setInterval> | null = null;
  /**
   * `stop()` 之后那次补拉：**每条会话一个定时器**。
   *
   * 以前只有一个字段 —— 两条会话各停一次时，后一个 `clearTimeout` 把前一个
   * 抹掉了（实测：`a.stop(); b.stop()` 只有 `b` 的 load 跑）。而 `stop()` 已经
   * 把那条的 `kind` 置成 `idle`，轮询不会轮到它 → 那条会话的「运行中」要等到
   * 用户切前台才消失。
   */
  #afterStop = new Map<string, ReturnType<typeof setTimeout>>();
  /**
   * 事件订阅表。
   *
   * 以前是 `#ended` / `#created` 两个各自独立的回调数组 —— 加第三个事件就得再开
   * 一个数组、再写一对 `onXxx()` / `xxx()`，而「订阅 / 退订 / 分发」那三行每次
   * 抄一遍。这里收成一张表，事件名是联合类型（写错名字编译期就报）。
   *
   * 三个事件都只在**状态真的变了**的时候发（不是每次轮询都发）：
   *
   *   - `created`：草稿在服务端建出来了（`sid` 变了）—— `App` 拿它换地址栏
   *   - `started`：**一条流开跑了**（第一帧到了，服务端那边用户消息和标题已经落库）
   *     —— `App` 拿它刷新抽屉列表。**这是「新会话出现在侧边栏」的时机**：
   *     `POST /api/sessions` 刚建出来时那条会话还没有内容，服务端的 `list_page`
   *     按 `has_content()` 过滤，这时候重拉列表是拿不到它的。
   *   - `ended`：一条流收尾了 —— 标题 / 顺序都定下来了，再刷一次列表
   */
  #subs: ChatEvents = { created: [], started: [], ended: [] };

  /** 一条会话的活状态 */
  get(sid: string): Live | undefined {
    void this.#rev;
    return this.#map.get(sid);
  }

  /** 表里所有活状态（`syncAll` / 淘汰 / 轮询用） */
  #all(): IterableIterator<Live> {
    void this.#rev;
    return this.#map.values();
  }

  /**
   * 取（或建）一条会话的活状态，并把它标成「刚用过」。
   *
   * 淘汰在这一步做（而不是在 `show` 里）：只有「又打开一条」才会让内存变紧。
   */
  live(sid: string): Live {
    let live = this.#map.get(sid);
    if (!live) {
      live = new Live(this, sid);
      this.#map.set(sid, live);
      this.#bump();
      // 淘汰时**显式跳过这一条**：它刚建、`lastUsed` 还是 0（全表最小），而且
      // 未必已经在 `viewing` 里（`live()` 是公开的，调用方可能先建后 show）——
      // 靠 LRU 顺序保证「不淘汰自己」是运气，不是约束。
      this.#evict(live);
    }
    live.lastUsed = ++this.#clock;
    return live;
  }

  /** 界面现在看的那条（`null` = 还没初始化 / 刚被删掉）*/
  get current(): Live | null {
    void this.#rev;
    return this.#map.get(this.viewing) ?? null;
  }

  /**
   * 切到某条会话（`""` = 新对话）。
   *
   * **不碰流**：正在跑的那条继续在后台收（服务端本来就不看有没有人听），
   * 切回来接着看 —— 这就是这次改造的全部意义。
   */
  show(sid: string): void {
    this.viewing = sid;
    if (!sid) {
      // 空对话：没有草稿就建一个（`/` 冷启动走这条）。**不清空**已经有的那个
      // —— 用户可能在草稿里写了一半又去翻历史，翻回来字还在。
      this.#draft(false);
      return;
    }
    const live = this.live(sid);
    // 已经有内容的不再对账：热会话手上的就是最新的（别人在动的话，WS 的
    // `turnActive` 和回前台那两条路会叫 `syncAll`）。以前切一次会话就重传一整窗
    // 历史（实测 190 KB），而它一个字都没变。
    if (live.messages.length === 0) void live.load();
  }

  /**
   * 草稿（`sid === ""`）那条：没有就建，`reset` = 清空它。
   *
   * `reset` 只给「点了新对话」用 —— 地址退回到 `/`（后退）时**不能**清：那一下
   * 只是回到空对话，用户可能刚从草稿切走，回来该接着写。
   */
  #draft(reset: boolean): Live {
    let live = this.#map.get("");
    if (!live) {
      live = new Live(this, "");
      this.#map.set("", live);
      this.#bump();
      reset = true;
      this.#evict(live);
    }
    if (reset) {
      live.evict();
      live.setKind("fresh");
    }
    live.lastUsed = ++this.#clock;
    return live;
  }

  /**
   * 点了「新对话」：把草稿清空，界面看它。
   *
   * **不请求服务端**：新会话到第一句话才建（`#ensure`），这里只把本地视图清空
   * —— 点了不发就不会攒出一堆空会话。
   */
  newDraft(): Live {
    this.viewing = "";
    return this.#draft(true);
  }

  /**
   * 草稿在服务端建出来了：把它换成真 id（同一个对象，消息不用搬）。
   *
   * 换 key 之后 `get(sid)` 才找得到它 —— `Pager` 里那些「还是这条吗」的判断
   * 全靠这个（对不上就当作废）。
   */
  #adopt(live: Live, sid: string): void {
    // 建会话要一个来回：这期间用户可能已经切走了（去看历史 / 另一条会话）。
    // 那种情况下**别把他拽回来**，也别改地址栏 —— 消息照样发进这条会话
    //（`deliver` 认的是对象，不是 `viewing`）。
    const stillHere = this.viewing === live.sid;
    this.#map.delete(live.sid);
    live.sid = sid;
    this.#map.set(sid, live);
    this.#bump();
    live.page.rev = "";
    // 哪怕这会儿已经在跑了也用 `idle`：`deliver` 紧接着会把它换成 `live`
    //（或者服务端说排队了，由 `load` 认成 `remote`）。
    live.setKind("idle");
    if (!stillHere) return;
    this.viewing = sid;
    this.#emit("created", sid);
  }

  /**
   * 订阅一个事件。返回退订函数（放进 `$effect` 的清理里正好）。
   *
   * 事件名见 `#subs` 上面那段。返回值那个 `() => void` 是 `$effect` 的清理约定。
   */
  on<E extends keyof ChatEvents>(
    event: E,
    fn: ChatEvents[E][number],
  ): () => void {
    const list: ((...a: never[]) => void)[] = this.#subs[event];
    list.push(fn as (...a: never[]) => void);
    return () => {
      const i = list.indexOf(fn as (...a: never[]) => void);
      if (i >= 0) list.splice(i, 1);
    };
  }

  #emit<E extends keyof ChatEvents>(
    event: E,
    ...args: Parameters<ChatEvents[E][number]>
  ): void {
    for (const f of this.#subs[event] as ((...a: unknown[]) => void)[])
      f(...args);
  }

  /** `Live#runStream` 收尾时叫它 */
  ended(live: Live): void {
    this.#emit("ended", live);
  }

  /** `Live#deliver` 在第一帧到达（确认开跑了）时叫它 */
  started(live: Live): void {
    this.#emit("started", live);
  }

  /**
   * 一条会话没了（删掉了 / 清空了）：连消息带流一起丢掉。
   *
   * 地址收回根路径是 `App` 的事（那是路由），这里只管状态。
   */
  forget(sid: string): void {
    const live = this.#map.get(sid);
    if (!live) return;
    live.evict();
    this.#map.delete(sid);
    this.#bump();
    this.queue.dropSession(sid);
    // 正在看的那条没了：退回空对话（**建一个草稿**，不然 `current` 是 null，
    // 界面成了空态、发消息也没地方去）
    if (this.viewing === sid) this.show("");
    this.syncPoll();
  }

  // —— 轮询 ——

  /**
   * 按「有没有会话在远端跑」开关轮询。
   *
   * **不能删掉轮询**：WebView 被冻住 / 被回收、流真的断了的时候，它是唯一的
   * 兜底（见 docs/06）。
   */
  syncPoll(): void {
    let need = false;
    for (const live of this.#all()) {
      if (live.kind === "remote") {
        need = true;
        break;
      }
    }
    if (need && !this.#poll) {
      this.#poll = setInterval(() => this.#tick(), POLL_MS);
    } else if (!need && this.#poll) {
      clearInterval(this.#poll);
      this.#poll = null;
    }
  }

  #tick(): void {
    // 直接遍历：`load()` 不会增删表（只会改那条自己的状态），不需要防御性拷贝
    for (const live of this.#all()) {
      if (live.kind === "remote") void live.load();
    }
  }

  /**
   * `stop()` 之后那次拉取。
   *
   * **同一条会话**只有一个定时器（300ms 内连按两下停止，前一个该作废 —— 它会在
   * 新回合开跑之后才到，那次拉取和 `/api/chat` 赛跑）；不同会话各算各的。
   */
  afterStop(live: Live): void {
    const prev = this.#afterStop.get(live.sid);
    if (prev !== undefined) clearTimeout(prev);
    this.#afterStop.set(
      live.sid,
      setTimeout(() => {
        this.#afterStop.delete(live.sid);
        void live.load();
      }, AFTER_STOP_MS),
    );
  }

  /**
   * 跟所有**热**会话对一次账（WS 的 `turnActive`、回前台、重连）。
   *
   * 以前只有「眼前这条」，所以别的会话在跑时界面完全不知道 —— 现在热集合里的
   * 每条都补一次（它们各自的门会挡掉不该发的，见 `Live#load`）。
   */
  syncAll(): void {
    for (const live of this.#all()) void live.load();
  }

  /**
   * 淘汰：超过 `HOT_MAX` 就把最久没看、**而且不是正在看的**那条丢掉。
   *
   * 正在看的那条永远不淘汰（用户正盯着它）；`keep` 是刚建出来的那条（理由见
   * `live()`）。没有别的可淘汰时宁可超一点 —— 少留一条的代价是用户切回去要重拉，
   * 比把眼前的流断掉好。
   */
  #evict(keep?: Live): void {
    while (this.#map.size > HOT_MAX) {
      let victim: Live | null = null;
      for (const live of this.#map.values()) {
        if (live.sid === this.viewing || live === keep) continue;
        if (!victim || live.lastUsed < victim.lastUsed) victim = live;
      }
      if (!victim) return;
      victim.evict();
      this.#map.delete(victim.sid);
      this.#bump();
    }
    this.syncPoll();
  }

  // —— 发消息 ——

  /**
   * 新会话到第一句话才在服务端建（点了「新对话」不发就不留痕）。
   *
   * 返回 `null` = 建不出来（网络 / 服务端），调用方补一条错误。
   */
  async #ensure(live: Live): Promise<string | null> {
    if (live.sid) return live.sid;
    try {
      const { id } = await post<{ id: string }>("/api/sessions");
      this.#adopt(live, id);
      return id;
    } catch {
      return null;
    }
  }

  /**
   * 发一条消息（发到界面正看着的那条会话）。
   *
   * 先按「排队中」画出来（`queue` + `pending`），再看 `/api/chat` 的首帧 ——
   * 见 `Live#deliver`。
   */
  async send(text: string, attachments: Upload[]): Promise<void> {
    const live = this.current;
    if (!live) return;
    live.ownSend++;

    const id = uid();
    const parts = userParts(text, attachments);
    const sid = await this.#ensure(live);
    if (!sid) {
      live.page.sent++;
      live.messages.push({ id, role: "user", parts, done: true });
      live.messages.push({
        id: uid(),
        role: "assistant",
        done: true,
        parts: [{ type: "error", text: "建会话失败，再试一次" }],
      });
      return;
    }
    this.queue.add({ id, sid, parts, pending: true });
    await live.deliver(id, text, attachments, parts);
  }

  /** 当前会话里排队的那几条（其它会话的排着队，切回去才看得见）*/
  queuedOf(sid: string): Queued[] {
    return this.queue.of(sid);
  }

  /** 撤掉一条还在排队的消息（虚线气泡左边那个 ×）*/
  dropQueued(id: string): void {
    this.queue.drop(id);
  }

  /** 测试 / 卸载用：停掉所有定时器 */
  close(): void {
    if (this.#poll) clearInterval(this.#poll);
    this.#poll = null;
    for (const t of this.#afterStop.values()) clearTimeout(t);
    this.#afterStop.clear();
    for (const live of this.#map.values()) live.evict();
    this.#map.clear();
    this.#bump();
  }
}

export const chat = new Chat();
