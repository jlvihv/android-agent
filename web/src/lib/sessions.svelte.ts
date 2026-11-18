/**
 * 侧边栏那份**会话列表**（`/api/sessions`）+ 它的两档合并、游标分页、错误态。
 *
 * 从 `App.svelte` 抽出来的。原来那块是四个裸变量（`sessionsAt` / `sessionsDoneAt`
 * / `sessionsInflight` / `sessionsLast`）加一个 `slot` 占位对象，编码了「没在飞 /
 * 在飞 / 刚落地 / force 覆盖」四种状态 —— 状态机藏在变量的组合里，读的人得自己
 * 把注释拼成一张图。
 *
 * 现在是一个类，状态是**字段**，规则在**方法**里。和 `pager.svelte.ts` /
 * `queue.svelte.ts` 同一个思路：能单独测的东西就单独一个文件。
 *
 * 为什么 `.svelte.ts`：`items` / `next` / `moreBusy` / `moreError` 都直接喂模板。
 */

import { api } from "./api";

/**
 * 抽屉里的一行（`/api/sessions` 的 `sessions[]`）。
 *
 * 形状和 `Drawer.svelte` 导出的 `Session` 一致 —— 那份是给组件用的，这份是给
 * 这个模块用的；两处都只读字段，谁也别加自己独有的必填项（加了这里立刻编译报错，
 * 这是好事）。
 */
export type Session = {
  id: string;
  title: string;
  /** 这条会话有几条消息（抽屉里那行小字） */
  count: number;
  /** 这条会话此刻有没有回合在跑（那个闪烁的小圆点） */
  busy?: boolean;
};

/** 刚落地那份的安静窗口：窗口内再来要就直接复用它 */
const MERGE_MS = 300;
/**
 * 在飞的那一份最多认这么久（ms）。
 *
 * 「在飞就复用」不能没有上限：隧道断着 / 核心卡住时那份请求可能永远不落地，
 * 认它一辈子等于列表再也不刷新了。
 */
const INFLIGHT_MAX_MS = 10_000;

export class SessionList {
  items = $state<Session[]>([]);
  /**
   * 第一页之后的游标（`/api/sessions` 的 `next`）。`null` = 已经到底 / 还没拉过。
   *
   * 抽屉滚到底才按它要下一页：一千条会话不该在打开的那一帧就传 150 KB 过来。
   */
  next = $state<string | null>(null);
  /** 正在拉下一页（滚到底会反复触发，别叠请求；抽屉底部那行「载入中」也看它） */
  moreBusy = $state(false);
  /**
   * 上一页**没拉成**（网络 / 后端）。
   *
   * 和「到底了」必须分开：`finally` 里 `moreBusy` 一落，抽屉那边就只剩「还有没有
   * 下一页」这一个信号 —— 拉失败时 `next` 还是原值（还有更多），但界面不能装作
   * 一切正常接着自动要（会死循环），也不能说「没有更早的了」（那是撒谎）。
   * 所以这里记一笔，抽屉改口「加载失败，点这里重试」。
   *
   * 一旦用户重试成功或者整份列表重来过，就清掉。
   */
  moreError = $state(false);

  /** 上一份**发起**的时刻：算「在飞了多久」用 */
  #at = 0;
  /** 上一份**落地**的时刻：算「刚拉过」的安静窗口用 */
  #doneAt = 0;
  #inflight: Promise<void> | null = null;
  /** 手上那份（不管落地没有）。安静窗口内复用它，省掉「刚拉过又拉一次」 */
  #last: Promise<void> | null = null;

  /**
   * 拉第一页。**重复调用并成一次**（两档）：
   *
   *   1. 已经有一份在路上 → 直接复用它的 promise。**不看发起时刻**（只看它是不是
   *      卡了太久）。真机上 22 KB 在路上要几十上百毫秒，「切回前台（focus）+ 点菜单」
   *      这种最常见的组合刚好落在旧的 300ms 窗口之外，于是同一份列表在路上发了两遍
   *      （实测：第一份还没落地，第二份已经出去了）。
   *   2. 刚落地那份 → `MERGE_MS` 内复用。
   *
   * 首屏那几个入口（挂载、WS 连上、`pageshow`）以前就是三份 21 KB 同时发出去。
   * 判断收敛到方法自己身上，不在各个入口各写一遍 —— 那种写法漏一个就又回来了。
   *
   * 会不会漏掉更新？会晚 `MERGE_MS`，而且真正的「必须看到最新」那几处
   *（重命名 / 删除 / 回合收尾）走 `load(true)`，绕开合并。
   *
   * `force`：绕开合并，**一定**打一份新的。**只拉第一页**（服务端默认 20 条），
   * 翻页走 `more()`；已经翻出来不止一页的，按现有条数重拉（见 `keep`）。
   */
  load(force = false): Promise<void> {
    const now = Date.now();
    if (!force) {
      // **在飞就无条件复用**
      if (this.#inflight && now - this.#at < INFLIGHT_MAX_MS) {
        return this.#inflight;
      }
      // 刚落地那份：窗口内也不再问
      if (this.#last && now - this.#doneAt < MERGE_MS) return this.#last;
    }
    this.#at = now;
    // 先占位：`finally` 里要靠它认出「现在在飞的是不是我自己」—— 赋值在函数
    // 尾部，所以用一个先建出来的槽，而不引用那个 `const p`。
    const slot: { p: Promise<void> | null } = { p: null };
    // 重拉时把**已经翻出来的条数**带上（0 = 服务端默认一页）。
    // 不带的话，回一次前台 / 重命名一次就会把用户翻到第 6 页的列表缩回 20 条，
    // 滚动位置也跟着跳回顶部 —— 明明什么都没变。代价是翻得越多重拉越重，
    // 但那正是用户自己翻出来的量（以前每一次都是全量）。
    const keep = this.items.length;
    const p = (async () => {
      try {
        const d = await api<{ sessions?: Session[]; next?: string | null }>(
          `/api/sessions${keep ? `?tail=${keep}` : ""}`,
        );
        this.items = d.sessions ?? [];
        // 整份重来：游标也跟着回到第一页之后（之前翻出来的那些没了）
        this.next = d.next ?? null;
        this.moreError = false;
      } catch {
        /* 拉不到就保持原样，下一次入口会再试 */
      } finally {
        this.#doneAt = Date.now();
        // 只清自己那一份：`force` 可能在它落地之前又发了一份，那份不能被抹掉
        //（抹了的话下一次调用就以为「没人在飞」，又发一份）
        if (this.#inflight === slot.p) this.#inflight = null;
      }
    })();
    slot.p = p;
    this.#inflight = p;
    this.#last = p;
    return p;
  }

  /**
   * 抽屉滚到底了：按游标再要一页，追加到 `items` 后面。
   *
   * 不能和 `load()` 合并（那份是「整份重来」，会把翻出来的页面冲掉）—— 这里只在
   * 自己身上防重入。`force` 重拉第一页时游标会重置，这一页作废。
   */
  async more(): Promise<void> {
    const cursor = this.next;
    if (!cursor || this.moreBusy) return;
    this.moreBusy = true;
    this.moreError = false;
    const at = this.items.length;
    try {
      const d = await api<{ sessions?: Session[]; next?: string | null }>(
        `/api/sessions?before=${encodeURIComponent(cursor)}`,
      );
      // 这期间整份重来过（切会话 / 回合收尾 / 回前台）：游标和工作都换了，这份丢掉
      if (this.next !== cursor || this.items.length !== at) return;
      // 按 id 去重：翻页期间新建的会话会顶到最前面，理论上不该重复，但宁可少一行
      const have = new Set(this.items.map((s) => s.id));
      this.items = [
        ...this.items,
        ...(d.sessions ?? []).filter((s) => !have.has(s.id)),
      ];
      this.next = d.next ?? null;
    } catch {
      // **不要**静默：以前这里什么都不做，界面上那行「载入中…」既没变成错误、
      // 也没人去要下一页，看起来就是「一直在载入、永远载不完」。
      this.moreError = true;
    } finally {
      this.moreBusy = false;
    }
  }
}
