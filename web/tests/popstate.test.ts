/**
 * 两个 popstate 监听者怎么**认领**各自的 history 条目。
 *
 * 背景：history 里同时住着两种条目 ——
 *   - 覆盖层：`pushState({ overlay: n })`，**地址不变**
 *   - 换会话：`pushState({ route: true }, "", "/s/<id>")`，**地址变**
 *
 * 以前两边都不认领，靠「覆盖层那条地址不变所以路由监听者算出来的 id 等于当前
 * 会话、被早退挡住」这种巧合。这组测试盯的是「谁也不误伤谁」。
 *
 * 这里不跑真实 DOM，只把两条判据抽出来验（判据本身就是一行 if）：
 *   覆盖层要 `state.overlay` 是数字；路由要 `state.route === true`。
 */

import { describe, expect, test } from "bun:test";

type State = { overlay?: number; route?: boolean } | null;

/**
 * `lib/overlay.ts` 里那条判据：带 `overlay` 且**不带** `route`
 * （两者同时出现说明有 bug，两边都不动）。
 */
const overlayOwns = (st: State) =>
  !!st && typeof st.overlay === "number" && !st.route;

/** `lib/route.ts` 里那条判据：带 `route` 且**不带** `overlay` */
const routeOwns = (st: State) => st?.route === true && st.overlay === undefined;

describe("popstate 认领", () => {
  test("覆盖层压的条目，只有覆盖层认领", () => {
    const st: State = { overlay: 1 };
    expect(overlayOwns(st)).toBe(true);
    expect(routeOwns(st)).toBe(false);
  });

  test("路由压的条目，只有路由认领", () => {
    const st: State = { route: true };
    expect(routeOwns(st)).toBe(true);
    expect(overlayOwns(st)).toBe(false);
  });

  test("浏览器的原生条目（首屏、外链进来）：两边都不认领", () => {
    // 首屏那条是浏览器自己的，`state` 就是 null。谁也不该乱动 ——
    // 首屏的会话是从 `sessionFromPath()` 读的，不靠 popstate。
    expect(overlayOwns(null)).toBe(false);
    expect(routeOwns(null)).toBe(false);
  });

  test("老条目 / 手改的 state：两边都不认领（不乱关东西）", () => {
    for (const st of [
      {} as State,
      { foo: 1 } as State,
      { overlay: "1" } as State,
    ]) {
      expect(overlayOwns(st)).toBe(false);
      expect(routeOwns(st)).toBe(false);
    }
  });

  test("两者互斥：同一条 popstate 不可能被两边同时认领", () => {
    for (const st of [
      null,
      {} as State,
      { overlay: 1 } as State,
      { route: true } as State,
      { overlay: 2, route: true } as State, // 理论上不会出现，但出现了也不该双认
    ]) {
      expect(overlayOwns(st) && routeOwns(st)).toBe(false);
    }
  });
});
