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

type Layer = { close: () => void; peek?: Peek };

const stack: Layer[] = [];
let skip = 0;

/** 壳暴露给页面的钩子（桌面浏览器上没有）。见 MainActivity.kt 的 BackHost */
type BackHost = {
  setBackHandled?: (handled: boolean) => void;
  /** 开 agent 的浏览器看板（没开过页面时壳会自己提示） */
  openBrowser?: () => void;
  /** agent 现在有没有开着的浏览器页面（头部的地球按钮据此显隐）。同步返回 */
  hasBrowser?: () => boolean;
  /** 改连电脑上的核心（壳会先弹原生确认框）。见 MainActivity.confirmConnect */
  connectRemote?: (url: string) => void;
  /** 回到手机自己的核心 */
  useLocal?: () => void;
  /** 开始 / 停止在局域网里找电脑（找到的推给 `window.__coresFound`） */
  startDiscovery?: () => void;
  stopDiscovery?: () => void;
  /** 点了找到的某台电脑（壳只认找到过的 id；配对 / 确认都是原生对话框） */
  connectDiscovered?: (id: string) => void;
};

/** 壳推过来的一台电脑（见 MainActivity.pushCoresFound） */
export type FoundCore = {
  id: string;
  name: string;
  host: string;
  port: number;
  /** 手机记着它的钥匙（点了直接连，不用再输配对码） */
  paired: boolean;
  /** 现在连着的就是它 */
  current: boolean;
};

/** 现在有谁在「找电脑」（见 `discoverCores`） */
const listeners = new Set<(cores: FoundCore[]) => void>();

/**
 * 在局域网里找电脑：`onFound` 每次拿到完整的列表。返回停止函数 ——
 * 「选择机器」面板 / 设置里「连接电脑」那一栏挂上时开始找、卸掉时停，不常驻。
 *
 * **可以同时有多个订阅者**：壳那边只有一个推口（`window.__coresFound`），
 * 这里自己分发给所有订阅者，最后一个人走了才真停。
 *
 * 为什么不能只管一个：两个地方会同时开着 —— 从「选择机器」面板点「手动输入地址」
 * 进设置页时，设置里那一拦会**叠在面板上面**（面板还在，没卸）。只认最后一个的话，
 * 面板那张清单当场就冻在那儿不再更新（而且先走的那个人会把发现整个停掉）。
 */
export function discoverCores(onFound: (cores: FoundCore[]) => void): () => void {
  const w = globalThis as { __coresFound?: (c: FoundCore[]) => void };
  listeners.add(onFound);
  // 壳推给单个全局口，这里转发给每一个人
  w.__coresFound = (c) => {
    for (const l of listeners) l(c);
  };
  if (listeners.size === 1) host()?.startDiscovery?.();
  return () => {
    listeners.delete(onFound);
    if (listeners.size) return;
    host()?.stopDiscovery?.();
    delete w.__coresFound;
  };
}

export function connectDiscoveredCore(id: string): void {
  host()?.connectDiscovered?.(id);
}
const host = (): BackHost | undefined =>
  (globalThis as { AgentHost?: BackHost }).AgentHost;

/** 在 App 里吗（能在手机核心 / 电脑核心之间切）。桌面浏览器、手机浏览器上都没有壳 */
export function canSwitchCore(): boolean {
  return typeof host()?.connectRemote === 'function';
}

export function connectRemoteCore(url: string): void {
  host()?.connectRemote?.(url);
}

export function useLocalCore(): void {
  host()?.useLocal?.();
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

export type PredictivePhase = 'start' | 'progress' | 'cancel' | 'commit';

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

if (typeof window !== 'undefined') {
  addEventListener('popstate', () => {
    if (skip > 0) {
      skip--;
      return;
    }
    const top = stack.pop();
    syncHost();
    top?.close();
  });

  // 系统手势返回：进度驱动栈顶那一层
  window.__predictiveBack = (phase, progress = 0) => {
    const top = stack.at(-1);
    if (!top) return;
    if (phase === 'commit') {
      // 有跟手位移的从当前位置接着收尾；没给 peek 的（万一以后有）也要能关掉
      if (top.peek) top.peek.onEnd(true);
      else top.close();
      return;
    }
    if (!top.peek) return;
    if (phase === 'start') top.peek.onStart();
    else if (phase === 'progress') top.peek.onMove(Math.min(1, Math.max(0, progress)));
    else if (phase === 'cancel') top.peek.onEnd(false);
  };
}

/**
 * 登记一层覆盖层。返回 release，在卸载时调（放进 $effect 的清理函数里正好）。
 *
 * `peek` 可选：给了就支持预测性返回的跟手位移（一般直接把 lib/drag.ts 那套
 * 拖拽三件套传进来）。
 */
export function onBack(close: () => void, peek?: Peek): () => void {
  history.pushState({ overlay: stack.length + 1 }, '');
  stack.push({ close, peek });
  syncHost();
  return () => {
    const i = stack.findIndex((l) => l.close === close);
    if (i === -1) return; // 已经被返回键弹掉了
    stack.splice(i, 1);
    syncHost();
    skip++;
    history.back();
  };
}
