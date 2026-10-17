/**
 * 窗口化的两条**策略**（纯函数，有测试盯着 —— `web/tests/window.test.ts`）。
 *
 * 抽出来的理由：两条规则各自都简单，但它们**互相关联**（一收一要）、都只看
 * 「观察层」的几个数，而且都栽过 —— 埋在组件里的时候，改错一次就是
 * 「翻到底之后再往上滚，什么都不发生」这种只有用过才知道的 bug。
 */

/** 「还有没有更早的」的一半事实：`start` 是手上没挂出来的，`hasOlder` 是服务端的 */
export type OlderFacts = Pick<
  WindowFacts,
  'measured' | 'hasOlder' | 'start' | 'pinned' | 'scrollTop' | 'contentH' | 'viewH' | 'nearTop'
>;

/** 收窗口要的那几个数 */
export type CollapseFacts = Pick<
  WindowFacts,
  'pinned' | 'extra' | 'loadingEarlier' | 'tailH' | 'viewH'
>;

/** 观察层交给策略的那些数（`MessageList` 里的同名 `$state`） */
export type WindowFacts = {
  /** 尺寸量过没有。没量过（都是 0）时一个决定都不做 */
  measured: boolean;
  /** 服务端还有更早的（= 手上这窗的起点 > 0） */
  hasOlder: boolean;
  /** 手上还有几条没挂出来（`messages.length - limit`） */
  start: number;
  /** 贴底（跟着最新内容走） */
  pinned: boolean;
  scrollTop: number;
  contentH: number;
  viewH: number;
  /** 往上多放出来的条数 */
  extra: number;
  /** `loadEarlier` 正在跑 */
  loadingEarlier: boolean;
  /** 最后 `step` 条挂出来有多高 */
  tailH: number;
  /** 滚到离顶这么近就该往前要（px） */
  nearTop: number;
};

/**
 * 该往前要了：**手上还有没挂出来的**（`start > 0`）**或者服务端还有更早的**
 * （`hasOlder`），而且用户正看着顶那一带。
 *
 * `start > 0` 那一半不能少（踩过）：服务端那边翻完之后 `hasOlder` 就是 false 了，
 * 而手上那几十条一收窗口就全压在窗口上面 —— 只认 `hasOlder` 的话，再往上滚什么
 * 都不发生。用户看到的是「第一次能翻，滚回底部再往上翻就翻不动了」。
 *
 * 贴不贴底看的是**两件事**，不能混：
 *
 *   - 没贴底（用户自己往上翻了）：贴到顶就要
 *   - 还贴着底：只有**整屏都装不下**才要（根本滚不动的那种）
 *
 * 后者不能用「`scrollTop` 是 0」来判：首屏刚渲染、还没滚到底的那一两帧也是 0，
 * 那样会误拉一页（而且是在首帧乱序的时机插进去的，DOM 一抖 `scrollTop` 被夹成 0）。
 */
export function wantOlder(f: OlderFacts): boolean {
  if (!f.measured) return false;
  if (!(f.start > 0 || f.hasOlder)) return false;
  return f.pinned ? f.contentH <= f.viewH : f.scrollTop < f.nearTop;
}

/**
 * 该把往上的窗口收回去（封顶 DOM）：贴底、放过更早的、**收完之后还装得满一屏**、
 * 而且没有正在跑的加载。
 *
 * 「收完装得满」的判据是 `tailH`（最后 `step` 条的高度），**不是** `contentH > viewH`：
 * 后者跟着 `extra` 变，会和 `wantOlder` 互相触发 —— 实测内容高度在视口附近时
 * 一收一要地空转（落地层 5 秒跑了 139 次、`extra` 在 0/10 之间来回跳）。
 * `tailH` 只跟最后那几条自己的高度有关，不会因为 `extra` 变而变。
 *
 * `loadingEarlier` 也不能少：收窗口会把刚放出来的那几窗又压回去。
 */
export function wantCollapse(f: CollapseFacts): boolean {
  return f.pinned && f.extra > 0 && !f.loadingEarlier && f.tailH > f.viewH;
}
