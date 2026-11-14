/**
 * 「回合跑完了，停止键还在」—— 这条测试盯的是 `App.svelte` 的 `view` 那个联合。
 *
 * 真实 bug（2026-11）：`view` 写成 `$state<View>(…)`。`$state` 是**深代理**，
 * `view = { k: "live", run: me }` 之后读回来的 `view.run` 是一个新的 Proxy，
 * 不是 `me`。于是 `run()` 收尾时那句
 *
 *     if (view.k === "live" && view.run === me) view = { k: "idle" };
 *
 * 恒不成立：`view` 留在 `live`，`running` 一直是真 —— 前端就一直画着停止键；
 * 顺带 `loadMessages` 的 `if (!busy)` 门也关着，服务端那份 `busy: false` 再也
 * 盖不上去（轮询白转，只有刷新才恢复）。
 *
 * 两份复刻：`deep` = `$state`（坏的那个），`raw` = `$state.raw`（现在的写法）。
 * 第一条**故意**断言旧写法的坏结果 —— 以后谁把 `.raw` 去掉，两条会一起亮。
 */

import { describe, expect, test } from "bun:test";
import { derived, get, proxy, set, state } from "svelte/internal/client";

type View = { k: "idle" } | { k: "live"; run: { controller: AbortController } };

/**
 * 复刻一轮：开跑 → 流收尾。`deep` 决定 `view` 是深代理还是 raw。
 *
 * 返回收尾之后「输入框该不该显示停止键」。
 */
function oneTurn(deep: boolean): boolean {
  const make = (v: View) => state(deep ? proxy(v) : v);
  const view = make({ k: "idle" });
  const running = derived(() => {
    const v = get(view);
    return v.k === "live";
  });

  // `run()`：流握在手上
  const me = { controller: new AbortController() };
  set(view, { k: "live", run: me }, deep);

  // 流收完了（`run()` 的 finally）：还是我那条吗？
  const cur = get(view);
  if (cur.k === "live" && cur.run === me) set(view, { k: "idle" }, deep);

  return get(running);
}

describe("view 的流身份", () => {
  test("$state（深代理）：view.run !== me，停止键收不掉（旧 bug 的样子）", () => {
    expect(oneTurn(true)).toBe(true);
  });

  test("$state.raw：身份保得住，流收完就回到 idle", () => {
    expect(oneTurn(false)).toBe(false);
  });

  test("App.svelte 的 view 是 $state.raw（不是深代理）—— 否则上面那条就复发", async () => {
    const src = await Bun.file(
      new URL("../src/App.svelte", import.meta.url).pathname,
    ).text();
    const decl = src.split("\n").find((l) => l.includes("let view = "));
    expect(decl).toContain("$state.raw<View>");
  });
});
