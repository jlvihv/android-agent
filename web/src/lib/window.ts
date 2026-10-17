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
  'measured' | 'hasOlder' | 'start' | 'pinned' | 'scrollTop' | 'contentH' | 'viewH'
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
 *   - 没贴底（用户自己往上翻了）：**离顶两屏就**要（见 `nearTop`）
 *   - 还贴着底：只有**整屏都装不下**才要（根本滚不动的那种）
 *
 * 后者不能用「`scrollTop` 是 0」来判：首屏刚渲染、还没滚到底的那一两帧也是 0，
 * 那样会误拉一页（而且是在首帧乱序的时机插进去的，DOM 一抖 `scrollTop` 被夹成 0）。
 */
export function wantOlder(f: OlderFacts): boolean {
  if (!f.measured) return false;
  if (!(f.start > 0 || f.hasOlder)) return false;
  return f.pinned ? f.contentH <= f.viewH : f.scrollTop < nearTop(f.viewH);
}

/**
 * 「离顶多近就该往前要」：**两屏**（视口高的两倍）。
 *
 * 这段余量是留给**预取**的：滚到这里就把下一窗拉回来（不动界面），用户滑到顶时
 * 内容已经到手，停稳直接插。
 *
 * 为什么是两屏：余量要盖住「用户滑到顶所需的时间 ≥ 一个网络往返」。一屏够快网络
 * （实测本地 1ms）但不够慢的（手机经中继一页 300 KB 是几百毫秒），而滑得快的时候
 * 一屏也就是几百毫秒。两屏把常见情况都盖住了，代价只是「提前一点开始拿」。
 *
 * 也不能写成一个很小的死数（以前是 400px）：那等于「滑到顶才开始拉」，用户到顶
 * 要干等一个来回 —— 实测的观感就是这个。
 */
function nearTop(viewH: number): number {
  return viewH * 2;
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

/** 「现在能不能插」要的那几个数 */
export type InsertFacts = {
  /** 这一轮滚动停稳了 */
  settled: boolean;
  scrollTop: number;
  /** 浏览器有原生 scroll anchoring（`CSS.supports('overflow-anchor', 'auto')`） */
  anchoring: boolean;
};

/**
 * 现在能不能把更早的一窗插到上面：**停稳了**，或者**浏览器会替我们钉住视口**。
 *
 * 插入本身不难，难在插完视口不能动：要么浏览器的 scroll anchoring 自己把 `scrollTop`
 * 补上，要么我们写 `scrollTop` —— 而后者会把手机上正在进行的惯性滑动掐掉。所以：
 *
 *   - 有 anchoring、而且**不在最顶上**（`scrollTop > 0`）：滑动中也直接插，一个字都
 *     不用写。这是常态 —— 离顶两屏就开始要，内容一到就插，用户滑到那里时上面早就
 *     有东西了，根本碰不到顶。
 *   - 已经贴在 0 上：**anchoring 在 0 不生效**（实测：`scrollTop` 0 时往前面插
 *     1000px，参照那条被推下去 1000px；1 / 50 时漂移都是 0）—— 只能自己写
 *     `scrollTop`，那就得等停稳。没有 anchoring 的内核（老 Safari）一律这样。
 *
 * 以前一律「等停稳」：那等于一直滑就一直不插，用户必然撞到顶、停手、再等 150ms
 * 才看见东西 —— 局域网里也卡一下，就是这个。（当初得出「anchoring 在用户滚动时
 * 不参与」的那次实测，插入点恰好在 0 上。）
 */
export function insertNow(f: InsertFacts): boolean {
  return f.settled || (f.anchoring && f.scrollTop > 0);
}
