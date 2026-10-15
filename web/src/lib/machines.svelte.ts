/**
 * 「选择机器」：连过的电脑、局域网里看到的、现在连的是哪台。
 *
 * **一份实现，两个宿主**（见 docs/12）：手机 App 的 WebView 和电脑上的浏览器问的是
 * 同一个接口 —— 经隧道时隧道把 `/api/machines*` 转回**本机核心**（「我能连哪些电脑」
 * 的答案在你手上这台机器里，不在正连着的那台）。平台只差两件薄的：
 *
 *   - **发现**：手机是壳的 `NsdManager`（面板开着时找，`watchDiscovery`），
 *     电脑是核心自己起的 `avahi-browse`（一直在找）；
 *   - **谁来导航**：App 里交给壳（`AgentHost.navigate`，它还要清 WebView 历史），
 *     浏览器里页面自己 `location.href`。
 *
 * 两份东西都只有几行，逻辑一行都不重复 —— 面板本身、配对、连接、选路由核心那边
 * 一份代码负责到底。
 */
import { backToLocalShell, hasShell, navigateShell, watchDiscovery } from './overlay';
import { store } from './store.svelte.ts';

/** 面板里的一行（`GET /api/machines` 的 `list`）。 */
export type Machine = {
  id: string;
  name: string;
  /** 局域网上那个地址；经中继才连得上的那行是空的 */
  host: string;
  port: number;
  /** 配对过（点一下直接连；没配对过要先输配对码） */
  paired: boolean;
  /** 局域网里找不到，得经服务器连 */
  relay: boolean;
  /** 现在连着的就是它 */
  current: boolean;
  /**
   * 经服务器的那几台在不在线（中继说的）。`null` / 没有 = 不知道（局域网里看得到的
   * 不用问；中继是老版本、或者问不到）—— 不知道就照常让人点。
   */
  online?: boolean | null;
};

/** 本机核心自己（面板第一行「这台手机 / 这台电脑」）。 */
export type LocalCore = {
  /** 本机核心的实例 id（和 `/api/health` 的 `id` 比，就知道这页是不是本机给的） */
  id?: string;
  /** 回到它要导航到的地址 */
  url: string;
  platform: string;
  name: string | null;
};

class Machines {
  list = $state<Machine[]>([]);
  local = $state<LocalCore | null>(null);
  /**
   * 本机核心记着的「正连着的那台」（null = 没连）。**不拿它打勾**：核心记的是隧道开着
   * 没有，不是你眼前这页是谁给的 —— 比如在电脑浏览器里连过别的电脑、又直接打开了本机
   * 的地址，隧道还开着，勾就会落在别人头上。打勾看 [here]。
   */
  current = $state<string | null>(null);
  err = $state('');
  /** 正在配对 / 正在连（按钮转圈，顺便防重复点） */
  busy = $state(false);
  /** 等上面那份数据（第一帧画什么） */
  loaded = $state(false);

  #timer: ReturnType<typeof setInterval> | undefined;

  /**
   * 眼前这页是哪台给的：`/api/health` 的 `id`（经隧道时是那台电脑答的，见 `store.instance`）。
   * 和本机核心的 id 一样 = 这页就是本机给的（null，勾第一行）。
   *
   * 两个 id 有一个还没拿到（页面刚起来、老核心不给）就退回核心记的 `current` —— 只有一边
   * 的话判断不了：拿 S 的 id 去列表里找，本机那一行不在列表里，就一行都勾不上。
   */
  get here(): string | null {
    const me = store.instance;
    const local = this.local?.id;
    if (!me || !local) return this.current;
    return me === local ? null : me;
  }

  /** 这一行是不是眼前这页（勾在它上面） */
  isHere(id: string): boolean {
    return this.here === id;
  }

  /** 问一次核心。 */
  async load(): Promise<void> {
    try {
      const r = await fetch('/api/machines');
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      const j = (await r.json()) as { list: Machine[]; local: LocalCore; current: string | null };
      this.list = j.list;
      this.local = j.local;
      this.current = j.current;
      this.err = '';
    } catch (e) {
      this.err = String(e);
    } finally {
      this.loaded = true;
    }
  }

  /**
   * 面板开着的时候轮询 + 让壳去找。
   *
   * 发现是**推进来**的（壳的 mDNS 回调、核心那边的 avahi-browse），页面这边只管问；
   * 不推一条 SSE 是因为这份数据只在面板开着那几秒有人看，1.5 秒问一次够了，
   * 也省掉一整条流。
   */
  watch(): () => void {
    void this.load();
    watchDiscovery(true);
    this.#timer = setInterval(() => void this.load(), 1500);
    return () => {
      clearInterval(this.#timer);
      this.#timer = undefined;
      watchDiscovery(false);
    };
  }

  /** 连上这台（配对过的直接连）。 */
  async connect(id: string): Promise<void> {
    const j = await this.#post('connect', { id });
    const url = j?.url;
    if (typeof url === 'string') this.#go(url);
  }

  /** 和「本机在局域网里看到过的那台」配对（输它的 6 位码），然后直接连上。 */
  async pair(id: string, code: string): Promise<void> {
    await this.#pairWith({ id, code });
  }

  /**
   * 手输地址配对（只有电脑上有这个入口）。
   *
   * 手机上页面给不了任意地址：`AgentHost` 每个 frame 都看得见（包括查看器里 AI 写的
   * 网页），核心那边也直接拒了带 host/port 的请求（见 docs/11）。电脑上页面就是本机
   * 核心自己发的，多这一条 —— 广播工具不在、`avahi-daemon` 没跑时还能用。
   */
  async pairManual(host: string, port: number, code: string): Promise<void> {
    await this.#pairWith({ host, port, code });
  }

  /** 配对完就接上：人输那个码就是为了用它，中间多一步「再来点一下」没道理。 */
  async #pairWith(body: Record<string, unknown>): Promise<void> {
    const j = await this.#post('pair', body);
    const id = j?.id;
    if (typeof id === 'string') await this.connect(id);
  }

  /** 回到本机自己的核心。 */
  async useLocal(): Promise<void> {
    // App 里交给壳：它还得重新种 cookie（本机核心的凭据是 /auth 种上的）
    if (backToLocalShell()) return;
    this.busy = true;
    try {
      const r = await fetch('/api/machines/local', { method: 'POST' });
      if (!r.ok) throw new Error((await r.text()) || `HTTP ${r.status}`);
      const j = (await r.json()) as { url: string };
      this.#go(j.url);
    } catch (e) {
      this.err = String(e);
    } finally {
      this.busy = false;
    }
  }

  /** 忘掉一台（它不认我们的凭据了：电脑上换过库）。 */
  async forget(id: string): Promise<void> {
    await this.#post('forget', { id });
    await this.load();
  }

  /** 打一个 `POST /api/machines/<what>`，返回核心给的 JSON（出错就写进 `err`）。 */
  async #post(what: string, body: unknown): Promise<Record<string, unknown> | null> {
    this.busy = true;
    try {
      const r = await fetch(`/api/machines/${what}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
      });
      const text = await r.text();
      let j: Record<string, unknown> = {};
      try {
        j = JSON.parse(text) as Record<string, unknown>;
      } catch {
        // 不是 JSON（核心没起来 / 老核心）：下面那行把原文带上
      }
      if (!r.ok) throw new Error(String(j.error ?? '').trim() || text.trim() || `HTTP ${r.status}`);
      this.err = '';
      return j;
    } catch (e) {
      this.err = String(e instanceof Error ? e.message : e);
      return null;
    } finally {
      this.busy = false;
    }
  }

  /**
   * 换到核心给的那个地址。
   *
   * App 里交给壳（WebView 的历史要清、返回键不该退回另一台的页面）；浏览器里
   * 页面自己走。两边都是「导航到那个地址」，地址里只有一把隧道钥匙，**没有任何
   * 机器的凭据**（见 docs/12）。
   */
  #go(url: string): void {
    if (navigateShell(url)) return;
    location.href = url;
  }
}

export const machines = new Machines();

/** 这一份跑在壳里吗（决定第一行写「这台手机」还是「这台电脑」）。 */
export { hasShell };
