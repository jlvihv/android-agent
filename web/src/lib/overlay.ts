/**
 * 覆盖层接入 Android 返回键。
 *
 * 抽屉/面板是 JS 覆盖层，系统返回键不知道它们存在 —— 不处理的话按返回
 * 会直接退出 App。做法：打开时 push 一条 history，返回键触发 popstate 时
 * 关掉**最上面**那一层。
 *
 * 之前是 App 里统一管，但只覆盖了抽屉和模型设置；消息列表里的思考/工具
 * 面板没接，按返回直接退出。而且"新建会话"这类不走 history 的关闭会留下
 * 脏记录，下次返回要按两下。现在每个覆盖层自己登记：
 *
 *   - 返回键 → 弹出栈顶，调它的 close
 *   - 任何别的方式关掉（点遮罩、拖走、代码里直接卸载）→ 卸载时 release()，
 *     自己把多出来的那条 history 退掉；这次 popstate 要跳过，别误关下一层
 *
 * ## 预测性返回（Android 14+ 的「跟手返回」）
 *
 * 系统手势返回时，Android 会把**手势进度**交给我们（壳里注册
 * OnBackInvokedCallback，见 MainActivity.kt），我们再喂给栈顶覆盖层：
 *
 *   手指划到哪 → 覆盖层跟着挪到哪 → 松手完成就顺势关掉，撤回就弹回原位
 *
 * 覆盖层只要把自己的「拖拽三件套」当 `peek` 传进来即可 —— 跟手拖和手势
 * 返回本来就该是同一套位移，不用写两遍。桌面浏览器上没有这个钩子，
 * 手动调 `window.__predictiveBack('start'|'progress'|'cancel'|'commit', 0..1)`
 * 可以模拟。
 */

/**
 * 跟手位移：0 = 原位，1 = 完全退出。
 *
 * 字段名跟 `lib/drag.ts` 的拖拽回调对齐 —— 拖拽三件套和预测性返回本来就是
 * 同一套位移，覆盖层直接把 `drag` 对象传进来就行。
 */
export type Peek = {
  onStart: () => void;
  onMove: (p: number) => void;
  onEnd: (commit: boolean) => void;
};

const stack: Layer[] = [];

/**
 * 「我们自己正在退回去」的那几条 history。
 *
 * `release()`（点遮罩 / 拖走 / Esc 关掉一层）要把当初 `pushState` 的那一条退掉，
 * 而它引起的 `popstate` **不能**被当成「用户按了返回」—— 否则会顺手把下面一层也
 * 关了。所以退之前记一笔，`popstate` 来了先扣掉这一笔。
 *
 * 用计数器而不是「哪一层」：两边都是「少一层」，而我们的层已经从 `stack` 里摘掉了，
 * 所以无论两条 popstate 谁先到，最后剩下的栈顶都一样。
 *
 * **但计数器必须会自己过期**（以前不会，这是真的丢返回键的地方）：`history.back()`
 * 在栈底 / 被浏览器合并时可能**根本不发** popstate，那这笔账就永远挂着 —— 之后用户
 * 按的第一次返回被当成我们的、静默吞掉（表现就是「按一下没反应，得再按一下」）。
 * 所以每一笔都挂一个定时器：等这么久还没等到 popstate，就当它没发生。
 */
let skip = 0;
const SKIP_TTL_MS = 600;
let skipTimers: ReturnType<typeof setTimeout>[] = [];

function skipOnce(): void {
  skip++;
  const t = setTimeout(() => {
    skip = Math.max(0, skip - 1);
    skipTimers = skipTimers.filter((x) => x !== t);
  }, SKIP_TTL_MS);
  skipTimers.push(t);
}

/** 一个覆盖层。`peek` 可选（预测性返回的跟手位移）。 */
type Layer = { close: () => void; peek?: Peek };

/** 壳暴露给页面的钩子（桌面浏览器上没有）。见 MainActivity.kt 的 BackHost */
type BackHost = {
  setBackHandled?: (handled: boolean) => void;
  /** 开 agent 的浏览器看板（没开过页面时壳会自己提示） */
  openBrowser?: () => void;
  /** agent 现在有没有开着的浏览器页面（头部的地球按钮据此显隐）。同步返回 */
  hasBrowser?: () => boolean;
  /**
   * 换页面（切换机器）：壳加载核心给的那个地址，顺手清掉 WebView 的历史 ——
   * 返回键不该退回另一台的页面。
   *
   * 壳自己问核心那个地址算不算数（它只认核心当前的隧道地址和本机地址），页面传不了
   * 任意地址：`AgentHost` 每个 frame 都看得见（包括查看器里 AI 写的网页），能传任意
   * 地址的话，一句 JS 就能把界面带到别人的服务器上（见 docs/11）。
   */
  navigate?: (url: string) => void;
  /** 回到本机自己的核心（壳那边要重新种 cookie，所以不只是一个导航） */
  useLocal?: () => void;
  /** 开始 / 停止在局域网里找电脑（电脑上没有壳：核心自己一直在找） */
  startDiscovery?: () => void;
  stopDiscovery?: () => void;
};

const host = (): BackHost | undefined =>
  (globalThis as { AgentHost?: BackHost }).AgentHost;

/**
 * 页面上这一份跑在壳里吗（手机 App 的 WebView）。
 *
 * 决定两件事：第一行写「这台手机」还是「这台电脑」；换机器时是让壳导航（它还要清
 * WebView 历史、重新种 cookie）还是页面自己 `location.href`。电脑浏览器、手机浏览器
 * 里都没有壳。
 */
export function hasShell(): boolean {
  return typeof host()?.useLocal === "function";
}

/** 让壳开始 / 停止在局域网里找电脑（面板开着时才找）。桌面：什么都不做。 */
export function watchDiscovery(on: boolean): void {
  const h = host();
  if (on) h?.startDiscovery?.();
  else h?.stopDiscovery?.();
}

/** 回到本机自己的核心（有壳就走壳那条路）。返回 false = 没有壳，调用方自己导航。 */
export function backToLocalShell(): boolean {
  const h = host();
  if (!h?.useLocal) return false;
  h.useLocal();
  return true;
}

/** 让壳换到核心给的那个地址。返回 false = 没有壳，调用方自己 `location.href`。 */
export function navigateShell(url: string): boolean {
  const h = host();
  if (!h?.navigate) return false;
  h.navigate(url);
  return true;
}

/** 头部的地球按钮调它。桌面（没有壳）上什么都不做。 */
export function openAgentBrowser(): void {
  host()?.openBrowser?.();
}

/**
 * agent 现在有没有开着的浏览器页面（决定头部那个地球按钮显不显示）。
 *
 * 桌面浏览器上没有壳，返回 false —— 按钮不出现，本来也没东西可看。
 */
export function agentHasBrowser(): boolean {
  return host()?.hasBrowser?.() === true;
}

/**
 * 告诉壳「现在有没有覆盖层要接返回键」。
 *
 * 有才注册系统的返回回调 —— 没有覆盖层时不注册，返回就还是系统自己处理
 * （退出 App 时系统会给它自己的预测性动画）。注册了就得自己画动画，
 * 那样退出反而更生硬。
 */
function syncHost(): void {
  host()?.setBackHandled?.(stack.length > 0);
}

export type PredictivePhase = "start" | "progress" | "cancel" | "commit";

declare global {
  interface Window {
    __predictiveBack?: (phase: PredictivePhase, progress?: number) => void;
    /**
     * 壳推过来的「agent 有没有开着浏览器」。见 MainActivity.kt 的
     * `pushBrowserAvailable` —— agent 第一次开页面、或者把标签关光时各推一次。
     * 页面加载时自己还会用 `AgentHost.hasBrowser()` 拉一次。
     */
    __browserAvailable?: (on: boolean) => void;
  }
}

if (typeof window !== "undefined") {
  addEventListener("popstate", (e) => {
    const st = e.state as { overlay?: number; route?: boolean } | null;
    // 判据：**带 `overlay` 且不带 `route`**。后者是防御性的 —— 两个字段同时出现
    // 说明有 bug（谁都不该压这样的条目），那种时候两边都别动，比抢着处理安全。
    const isOurs = !!st && typeof st.overlay === "number" && !st.route;

    // 自己退的那一条回来了：吃掉，别关下一层。
    //
    // **只在「退到的是我们自己那条」时消耗 `skip`**：`release()` 里
    // `history.back()` 退到的可能是**前一条**，而那一条未必是覆盖层（比如盖在
    // 会话页上时，下面是路由条目 `state: null`）。以前无条件先扣 `skip`，那种
    // 情况下这笔账就白扣了 —— 紧接着用户按的返回会被当成我们自己的、静默吞掉。
    if (skip > 0 && isOurs) {
      skip--;
      return;
    }

    // **只处理「覆盖层那一类」的 popstate**。
    //
    // history 里现在同时住着两种条目：
    //   - 覆盖层：`pushState({ overlay: n })` —— **地址不变**
    //   - 换会话：`pushState(null, "", "/s/<id>")` —— **地址变**（见 `lib/route.ts`）
    //
    // 以前这里不区分，靠的是一条隐含约定「覆盖层的条目地址不变，所以路由那边的
    // 监听者不会误响」—— 反过来呢？换会话引起的 popstate 会走到这里，把**栈顶
    // 那一层覆盖层关掉**。现在恰好没暴露，是因为换会话时抽屉自己先关了；但那是
    // 巧合，不是约定。
    //
    // 现在按 `history.state` 认领（更硬的判据）：覆盖层压进去的条目一定带
    // `overlay` 字段，路由压的是 `null`。谁也认领不到的（老条目 / 手改的）
    // 按「不是覆盖层的」处理，交给别人 —— 这里**不**乱关东西。
    if (!isOurs) return;

    const top = stack.pop();
    syncHost();
    top?.close();
  });

  /**
   * Esc 关闭栈顶那一层（桌面）。
   *
   * 安卓有返回键、触屏有点遮罩 / 拖走，但**桌面浏览器里 Esc 是唯一的习惯** ——
   * 以前按 Esc 什么都不发生。
   *
   * 统一在这里接，而不是每个覆盖层各挂一个 `onkeydown`：那些层盖满屏幕，
   * 焦点往往还在原来的触发按钮上（没有焦点圈的面层 —— 抽屉、查看器、设置页），
   * 事件根本不会冒泡到它们身上。挂在 window 上就不用先给每层抢焦点。
   *
   * 只给**栈顶**：`stopImmediatePropagation` 之后不会再有人拿到这个 Esc，
   * 叠两层时按一下只关最上面那层（和返回键一致）。
   *
   * `isComposing`：中文输入法展开着候选框时 Esc 是给输入法的（收候选），不抢。
   */
  addEventListener("keydown", (e) => {
    if (e.key !== "Escape" || e.isComposing) return;
    const top = stack.at(-1);
    if (!top) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    // 走栈顶的 close()：history 那一条由它卸载时的 release 退回（和点遮罩同一条路）
    top.close();
  });

  // 系统手势返回：进度驱动栈顶那一层
  window.__predictiveBack = (phase, progress = 0) => {
    const top = stack.at(-1);
    if (!top) return;
    if (phase === "commit") {
      // 有跟手位移的从当前位置接着收尾；没给 peek 的（万一以后有）也要能关掉
      if (top.peek) top.peek.onEnd(true);
      else top.close();
      return;
    }
    if (!top.peek) return;
    if (phase === "start") top.peek.onStart();
    else if (phase === "progress")
      top.peek.onMove(Math.min(1, Math.max(0, progress)));
    else if (phase === "cancel") top.peek.onEnd(false);
  };
}

/**
 * 登记一层覆盖层。返回 release，在卸载时调（放进 $effect 的清理函数里正好）。
 *
 * `peek` 可选：给了就支持预测性返回的跟手位移（一般直接把 lib/drag.ts 那套
 * 拖拽三件套传进来）。
 */
export function onBack(close: () => void, peek?: Peek): () => void {
  history.pushState({ overlay: stack.length + 1 }, "");
  stack.push({ close, peek });
  syncHost();
  return () => {
    const i = stack.findIndex((l) => l.close === close);
    if (i === -1) return; // 已经被返回键弹掉了
    stack.splice(i, 1);
    syncHost();
    skipOnce();
    history.back();
  };
}
