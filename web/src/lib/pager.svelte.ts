/**
 * 一条会话的**消息窗口**：翻页、预取、收窗口、跟服务端对账。
 *
 * 从 `chat.svelte.ts` 的 `Live` 里拆出来的。理由：
 *
 *   - 这半和**流**没关系。它只做「手上这一窗 = 完整视图的哪一段」的记账
 *     （`windowFrom` / `messages` / 预取的那一窗 / 在飞的那一份），是纯逻辑 ——
 *     纯逻辑就该能单独测（`session.ts` 已经是这么拆的，这里接着它往下走）。
 *   - `Live` 原来一手包办窗口 + 收流 + 排队 + 停止 + 用量，1052 行里有一半是
 *     这里的事，读的人要同时在脑子里放五个不变式。
 *
 * **仍然带 `.svelte.ts` 后缀**：`messages` 得是 `$state`（`applyChunk` 原地写
 * 代理，界面才动），`windowFrom` 也一样。
 *
 * 三个不变式（都有测试盯着，见 `tests/pager.test.ts`）：
 *
 *   1. `messages` = 完整视图的 `[windowFrom, windowFrom + len)`
 *   2. 补丁的 `replaceFrom` 是服务端按**发请求时**的 `windowFrom` 翻译的 ——
 *      路上翻过页就作废（`stale`）
 *   3. 本地发过消息（`sent` 变了）之后，路上那份回包整份作废（它算的是加之前）
 */

import { api } from "./api";
import type { Message } from "./sse";
import {
  applyPatch,
  applyWindow,
  prependPage,
  trimOlder,
  type SessionPayload,
  type Window,
} from "./session";

/**
 * 一窗多少条消息（视图消息的条数，**不是回合数**：一个回合的思考 / 工具 / 正文
 * 都在同一条助手消息里，所以 10 条 = 5 个回合）。
 *
 * 首屏就只跟服务端要这么多（`tail`），往上翻到头再要更早的一窗 —— 整段历史
 * 每打开一次就传一遍很亏（一个长会话几 MB，而一屏只看得到几条）。
 * 同一个数也给 `MessageList`（它管「挂多少条进 DOM」）—— 两处必须同一个数：
 * 挂得比要的多会露白，比要的少就白传。
 */
export const MSG_WINDOW = 10;

/**
 * 手上最多留几条更早的（回到底部时收回）。
 *
 * `MessageList` 那层只管**挂进 DOM** 的（一窗 10 条）；这里管**手里**的（往上翻
 * 多少就留多少）—— 一页 10 条在真实会话里约 300 KB（工具输出很胖），翻十页就是
 * 3 MB 常驻。留三窗：再往上翻一两屏不用重新拉，再早就丢掉重拉。
 */
const KEEP_MSGS = MSG_WINDOW * 3;

/** 预取好的、还没插进去的那一窗 */
type Prefetched = { base: number; page: Message[]; from: number };

/** `/api/session` 里这里用得上的全部字段（窗口那几件 + 对账那几件） */
export type SessionReply = SessionPayload & {
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
  usage?: unknown;
};

/**
 * 一条会话的消息窗口。
 *
 * **生命周期跟着 `Live` 走**，不跟组件走 —— 切走再切回来，手上这一窗原样还在。
 */
export class Pager {
  messages = $state<Message[]>([]);
  /**
   * 手上这窗在**完整视图**里的起点（服务端的 `from`）：
   * `messages` = 完整视图的 `[windowFrom, windowFrom + messages.length)`。
   *
   * 为什么要它：服务端算增量补丁时用的是完整视图的下标（`replaceFrom`），
   * 而我们手上只有尾巴这一窗 —— 把这个数带回去，它替我们翻译。
   */
  windowFrom = $state(0);

  /**
   * 消息列表被本地改过的版本号。**只在真往 `messages` 里加东西时才加一**。
   *
   * 发请求前记下它，回来时对不上就说明这份回包算的是加之前的样子 —— 拿它整份
   * 覆盖会把刚推进去的用户消息和正在流的那条冲掉。
   */
  sent = 0;
  /**
   * 上次拿到的会话版本号（服务端算的）。一致就不再重传整段历史。
   *
   * **切会话 / 新建 / 删除后必须清掉** —— 否则会拿旧会话的 rev 去比对新会话，
   * 服务端一旦碰巧返回 unchanged，界面就卡在上一个会话的内容上。
   */
  rev = "";

  /** 正在路上的那份 `load` 是哪一份（会话|窗口|rev）；空 = 没有 */
  #inflight = "";
  /** 路上这份还在跑的时候又有调用被合并掉了：回来后必须再补一次 */
  #inflightAgain = false;
  #pending: Prefetched | null = null;
  #loadingOlder = false;
  /**
   * 预取在路上（滚动中会反复叫它，别每帧都发一个请求）。
   *
   * 存的是那个 promise 而不是一个布尔：真要插的时候（`loadOlder`）预取要是还在
   * 路上，**等它**就行 —— 以前是另发一份一模一样的请求，等于白白多一个来回。
   */
  #prefetching: Promise<void> | null = null;

  /**
   * `sid` 要能跟着变（草稿在服务端建出来之后 `""` → 真 id）。用取值函数而不是
   * 一个字段：`Live` 换 id 时不用记着来同步这里。
   *
   * `alive` 回答「这条会话还在表里吗」—— 请求在路上时它可能已经被淘汰 / 删掉，
   * 那份回包就不该再往一个已经没人看的地方写。
   */
  constructor(
    private readonly sid: () => string,
    private readonly alive: () => boolean,
  ) {}

  /** 服务端还有没有更早的。**推导的**，不是状态（存成状态会陈旧，见 `trimOlder`） */
  get hasOlder(): boolean {
    return this.windowFrom > 0;
  }

  /** 清空（切会话 / 被淘汰 / 被删掉） */
  reset(): void {
    this.messages = [];
    this.windowFrom = 0;
    this.rev = "";
    this.#pending = null;
  }

  /** 把算好的两个值写回去（`$state` 得逐个赋值才触发更新） */
  #setWindow(w: Window): void {
    this.messages = w.messages;
    this.windowFrom = w.windowFrom;
  }

  #window(): Window {
    return { messages: this.messages, windowFrom: this.windowFrom };
  }

  // —— 往前翻 ——

  /**
   * 把更早的一页插到手上（预取的、或刚拉到的）。返回 `true` = 真插上了。
   *
   * 接不上就整页丢掉（`prependPage` 给 null）：宁可少一窗，也不能在中间留个洞。
   */
  #insertOlder(p: { page: Message[]; from: number }): boolean {
    if (!p.page.length) return false; // 空页：什么都不用做，下次再要
    const next = prependPage(this.#window(), {
      messages: p.page,
      from: p.from,
    });
    if (!next) return false;
    this.#setWindow(next);
    // 插进去之后窗口起点变了：手上那份预取（如果有）的基准点也跟着过时 ——
    // 丢掉，下一次贴到顶时按新的起点重新预取（不然会拿着一份对不上的干等）。
    this.#pending = null;
    return true;
  }

  /** 翻页请求本身（预取和插都用它）。返回 null = 这一份不能用了 */
  async #fetchOlder(
    base: number,
  ): Promise<{ page: Message[]; from: number } | null> {
    const sid = this.sid();
    try {
      const d = await api<SessionPayload>(
        `/api/session?id=${encodeURIComponent(sid)}&before=${base}&tail=${MSG_WINDOW}`,
      );
      // 这一路走的过程中会话切走了 / 窗口又变了一页 / 已经没有更早的了：这份不要了
      if (!this.alive() || this.sid() !== sid) return null;
      if (base !== this.windowFrom || this.windowFrom === 0) return null;
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
  prefetchOlder(): void {
    if (!this.hasOlder || this.#pending || this.#prefetching) return;
    if (this.#loadingOlder) return;
    const base = this.windowFrom;
    const p = (async () => {
      const got = await this.#fetchOlder(base);
      if (!got || base !== this.windowFrom || this.#pending) return;
      this.#pending = { base, ...got };
    })().finally(() => {
      if (this.#prefetching === p) this.#prefetching = null;
    });
    this.#prefetching = p;
  }

  /**
   * 真的插一窗更早的（`MessageList` 判停之后叫它）。
   *
   * 只跟服务端要**这一窗** —— 历史是只追加的，这一窗要回来就永远是对的，不用管
   * `rev`（那套是给「当前回合在长」用的）。返回 `true` = 真拿到了新东西（调用方
   * 据此补偿滚动位置，不补偿的话视口会跳）。
   */
  async loadOlder(): Promise<boolean> {
    if (!this.hasOlder) return false; // hasOlder = 手上这窗的起点 > 0
    // 预取还在路上：等它（它要的就是这一窗），别另发一份
    if (this.#prefetching) await this.#prefetching;
    if (!this.hasOlder) return false;
    // 预取的还在手上、又正好接得上 → 直接用
    if (this.#pending && this.#pending.base === this.windowFrom) {
      const p = this.#pending;
      this.#pending = null;
      return this.#insertOlder(p);
    }
    this.#pending = null;
    this.#loadingOlder = true;
    try {
      const got = await this.#fetchOlder(this.windowFrom);
      return got ? this.#insertOlder(got) : false;
    } finally {
      this.#loadingOlder = false;
    }
  }

  /**
   * 回到底部了（`MessageList` 说）：把视口之外的旧页丢掉，给内存封顶。
   *
   * 这一步只动**前面**那些（下标跟着 `windowFrom` 平移，所以补丁的下标依旧对得上），
   * 界面不动：被丢掉的全在视口上方，`MessageList` 那层本来就只挂着最后 10 条。
   */
  trimOldPages(): void {
    const next = trimOlder(this.#window(), KEEP_MSGS);
    if (next) this.#setWindow(next);
    // 预取的基准点可能已经不在了
    if (this.#pending && this.#pending.base !== this.windowFrom)
      this.#pending = null;
  }

  // —— 跟服务端对账 ——

  /**
   * 这份回包**已经作废**了？三件事任何一个变过，就说明它算的是旧状态：
   *
   *   - 会话没了 / 换了（`alive` / `sid`）：`u1` 这种 id 每条会话都有，套上去
   *     就是串内容
   *   - 本地发过消息了（`at !== sent`）：回包是发之前的样子，拿它整份覆盖会把刚推
   *     进去的用户消息和正在流的那条助手消息冲掉
   *   - 用户往上翻了一页（`winAt !== windowFrom`）：补丁的 `replaceFrom` 是按旧窗口
   *     翻译的，下标不作数
   */
  #stale(sid: string, at: number, winAt: number): boolean {
    return !this.alive() || this.sid() !== sid || at !== this.sent ||
      winAt !== this.windowFrom;
  }

  /**
   * 跟服务端对一次账：把这条会话的最新窗口拉回来。
   *
   * 触发源有六七个（挂载、WS 重连、回合起停、切回前台、轮询、发完消息、停止之后），
   * 它们常常挤在同一瞬间，所以这里要处理三件竞态：
   *
   *   1. **该不该发** —— 由调用方判（本地正在流 / 还没建会话时，服务端那份是旧的）
   *   2. **能不能合并**（`#inflight`）—— 同一份（会话|窗口|rev）已经在路上就不发
   *      第二份，但被合并掉的那次记一笔，等它回来再补（那条往往是「回合刚跑完，
   *      去拿最后几条」）
   *   3. **回来时还作不作数**（`#stale`）—— 请求在路上时用户可能已经发了消息、
   *      往上翻了一页、或者切了会话
   *
   * `onReply` 在回包被判定「还算数」之后调一次（调用方拿它去更新在不在跑、排队、
   * 用量、错误提示 —— 那些不归窗口管）。返回 `false` = 让这里别再往下套补丁
   *（`unchanged`）。
   */
  async load(
    onReply: (d: SessionReply) => boolean | void,
  ): Promise<void> {
    const sid0 = this.sid();
    const at = this.sent;
    // 发请求时手上那一窗的起点。补丁的 `replaceFrom` 是服务端按**这个**翻译过来的。
    const winAt = this.windowFrom;

    // 同一份已经在路上：合并掉，但记一笔
    const key = `${sid0}|${winAt}|${this.rev}`;
    if (key === this.#inflight) {
      this.#inflightAgain = true;
      return;
    }
    this.#inflight = key;
    this.#inflightAgain = false;

    try {
      const d = await api<SessionReply>(
        // **显式带 id**：「现在看的是哪条会话」是界面的事（地址栏说了算）。
        // 服务端没有「当前会话」这个概念，不带 id 会直接 400。
        `/api/session?id=${encodeURIComponent(sid0)}` +
          `&rev=${encodeURIComponent(this.rev)}&tail=${MSG_WINDOW}&from=${winAt}`,
      );
      // 这份回包还作不作数（路上发过消息、翻了页、会话被丢掉了）
      if (this.#stale(sid0, at, winAt)) return;
      this.rev = d.rev ?? "";
      // 调用方先处理它关心的（busy / 排队 / 用量 / 错误），再由这里套窗口
      if (onReply(d) === false) return;
      // 服务端说没变：连 JSON 里都不带 messages，什么都不用做
      if (d.unchanged) return;
      // 到这儿 `#stale()` 已经确认过「还是这条会话、没发过消息、窗口没动」，
      // 所以 `same` 恒为真、`applyPatch` 的 `stale` 那一支**不可能**再返回。
      //
      // 补丁 / 整窗两条路的规则在 `lib/session.ts` 里（有测试盯着，见 `bun test`）。
      if (d.append) {
        const r = applyPatch(this.#window(), d, winAt);
        if (!r.ok) {
          if (r.why === "mismatch") {
            // 本地那条和服务端以为的对不上：老老实实重新要一份全量
            //（`rev = ''` → 下一份是尾部窗口，回来时会按 `from` 接在旧的那段后面）
            this.rev = "";
            return this.load(onReply);
          }
          return; // 理论上到不了（见上面那段）
        }
        this.#setWindow(r.window);
      } else {
        // `reset`：整段换过了（`/api/reset`）。这时 id 会和旧消息撞上，不能合并。
        this.#setWindow(applyWindow(this.#window(), d, !d.reset));
      }
    } catch {
      /* 拉不到就算了 */
    } finally {
      if (this.#inflight === key) {
        this.#inflight = "";
        // 被合并掉的那次在这里补上（新的一份 key 不一样，不会自己把自己的门堵上）
        if (this.#inflightAgain) {
          this.#inflightAgain = false;
          void this.load(onReply);
        }
      }
    }
  }
}
