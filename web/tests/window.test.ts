/**
 * 窗口化两条策略的测试。
 *
 * 这两条各自都简单，但**互相关联**（一收一要），而且都在真机上栽过：
 *
 *   - 「翻到底之后再往上滚，什么都不发生」（`wantOlder` 漏了 `start > 0` 那一半）
 *   - 「一收一要地空转」（`wantCollapse` 的判据跟着 `extra` 变）
 *
 * 所以它们是纯函数、有测试 —— 组件里只剩「把观察层的数喂进来」。
 */

import { describe, expect, test } from 'bun:test';
import { wantCollapse, wantOlder, type WindowFacts } from '../src/lib/window';

/** 一份「什么都正常」的事实，各用例只改自己关心的那几个 */
const facts = (over: Partial<WindowFacts> = {}): WindowFacts => ({
  measured: true,
  hasOlder: false,
  start: 0,
  pinned: false,
  scrollTop: 0,
  contentH: 9000,
  viewH: 800,
  extra: 0,
  loadingEarlier: false,
  tailH: 3000,
  ...over,
});

describe('wantOlder', () => {
  test('没量过尺寸：一个决定都不做（`contentH`/`viewH` 都是 0，会白白成立）', () => {
    expect(wantOlder(facts({ measured: false, hasOlder: true, start: 0, scrollTop: 0, contentH: 0, viewH: 0 }))).toBe(false);
  });

  test('手上还有没挂出来的 → 该往前放（**服务端翻完了也算**）', () => {
    // 这就是那个线上 bug：翻到底（hasOlder=false）→ 回到底部收窗口 → 再往上滚，
    // 手上那几十条还压在上面，但只认 hasOlder 的话什么都不发生。
    expect(wantOlder(facts({ hasOlder: false, start: 20 }))).toBe(true);
  });

  test('服务端还有更早的、用户往上翻 → 离顶**两屏**就该要（留给预取）', () => {
    // 视口 800：1599 还算「两屏之内」（该提前拉），1601 就不该了
    expect(wantOlder(facts({ hasOlder: true, scrollTop: 1599, viewH: 800 }))).toBe(true);
    expect(wantOlder(facts({ hasOlder: true, scrollTop: 1601, viewH: 800 }))).toBe(false);
    // 视口越高，提前得越多（桌面大窗口）
    expect(wantOlder(facts({ hasOlder: true, scrollTop: 2600, viewH: 1400 }))).toBe(true);
  });

  test('贴底时「在顶部」说明不了什么：够长就不动（首屏那两帧 scrollTop 也是 0）', () => {
    expect(wantOlder(facts({ hasOlder: true, pinned: true, scrollTop: 0, contentH: 9000, viewH: 800 }))).toBe(false);
  });

  test('贴底 + 整屏都装不下（根本滚不动）→ 该要（不然那一截历史见不到）', () => {
    expect(wantOlder(facts({ hasOlder: true, pinned: true, scrollTop: 0, contentH: 700, viewH: 800 }))).toBe(true);
  });

  test('两边都没有更早的了 → 不要', () => {
    expect(wantOlder(facts({ hasOlder: false, start: 0, scrollTop: 0 }))).toBe(false);
  });
});

describe('wantCollapse', () => {
  test('贴底、放过更早的、收完还装得满一屏 → 收', () => {
    expect(wantCollapse(facts({ pinned: true, extra: 10, tailH: 3000, viewH: 800 }))).toBe(true);
  });

  test('收完装不满一屏 → 不收（否则「不够一屏就接着要」立刻要回来 → 空转）', () => {
    expect(wantCollapse(facts({ pinned: true, extra: 10, tailH: 700, viewH: 800 }))).toBe(false);
  });

  test('没贴底 / 没什么可收 / 正在加载 → 不收', () => {
    expect(wantCollapse(facts({ pinned: false, extra: 10 }))).toBe(false);
    expect(wantCollapse(facts({ pinned: true, extra: 0 }))).toBe(false);
    expect(wantCollapse(facts({ pinned: true, extra: 10, loadingEarlier: true }))).toBe(false);
  });
});

describe('两条策略不许同时成立（一收一要就是空转的源头）', () => {
  test('把各个维度都扫一遍', () => {
    for (const measured of [true, false])
      for (const hasOlder of [true, false])
        for (const start of [0, 5, 30])
          for (const pinned of [true, false])
            for (const scrollTop of [0, 200, 5000])
              for (const extra of [0, 10, 30])
                for (const tailH of [100, 700, 3000])
                  for (const contentH of [100, 700, 9000]) {
                    // `tailH` 是 `contentH` 里最后那几条 —— 物理上不可能比整段还高
                    //（不变量正是靠这个：两条判据都拿它们跟视口比）
                    if (tailH > contentH) continue;
                    const f = facts({ measured, hasOlder, start, pinned, scrollTop, extra, tailH, contentH, viewH: 800 });
                    expect(wantOlder(f) && wantCollapse(f)).toBe(false);
                  }
  });
});
