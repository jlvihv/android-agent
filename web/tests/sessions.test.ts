/**
 * `lib/sessions.svelte.ts`：侧边栏那份列表的合并 / 分页 / 错误态。
 *
 * 原来这块是 `App.svelte` 里四个裸变量 + 一个 `slot` 占位对象编码的状态机，
 * 没法单独测。抽出来之后这几条都能盯住。
 */

import { describe, expect, test } from "bun:test";
import { SessionList, type Session } from "../src/lib/sessions.svelte.ts";

const row = (id: string): Session => ({ id, title: id, count: 1 });

/** 假 `/api/sessions`：按顺序回 `replies`，并把请求 URL 记下来 */
function fakeApi(replies: unknown[]) {
  const calls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    calls.push(String(input));
    const body = replies.shift() ?? { sessions: [] };
    return new Response(JSON.stringify(body), { status: 200 });
  }) as typeof fetch;
  return calls;
}

describe("load", () => {
  test("在飞就复用：连发三次只出一个请求", async () => {
    const calls = fakeApi([{ sessions: [row("a")] }]);
    const list = new SessionList();
    const p = [list.load(), list.load(), list.load()];
    await Promise.all(p);
    expect(calls.length).toBe(1);
    expect(list.items.map((s) => s.id)).toEqual(["a"]);
  });

  test("刚落地 300ms 内复用（不再发第二份）", async () => {
    const calls = fakeApi([{ sessions: [row("a")] }, { sessions: [row("b")] }]);
    const list = new SessionList();
    await list.load();
    await list.load(); // 窗口内
    expect(calls.length).toBe(1);
    expect(list.items.map((s) => s.id)).toEqual(["a"]);
  });

  test("force 绕开合并，一定打一份新的", async () => {
    const calls = fakeApi([{ sessions: [row("a")] }, { sessions: [row("b")] }]);
    const list = new SessionList();
    await list.load();
    await list.load(true);
    expect(calls.length).toBe(2);
    expect(list.items.map((s) => s.id)).toEqual(["b"]);
  });

  test("重拉时带上已经翻出来的条数（别把翻过的缩回一页）", async () => {
    const calls = fakeApi([
      { sessions: [row("a"), row("b"), row("c")] },
      { sessions: [row("a"), row("b"), row("c")] },
    ]);
    const list = new SessionList();
    await list.load();
    await list.load(true);
    expect(calls[1]).toContain("tail=3");
  });

  test("拉失败：列表保持原样，不抛", async () => {
    globalThis.fetch = (async () => {
      throw new Error("boom");
    }) as unknown as typeof fetch;
    const list = new SessionList();
    list.items = [row("keep")];
    await list.load();
    expect(list.items.map((s) => s.id)).toEqual(["keep"]);
  });
});

describe("more（游标分页）", () => {
  test("按游标追加，并按 id 去重", async () => {
    const calls = fakeApi([
      { sessions: [row("a"), row("b")], next: "cur1" },
      { sessions: [row("b"), row("c")], next: null },
    ]);
    const list = new SessionList();
    await list.load();
    expect(list.next).toBe("cur1");

    await list.more();
    expect(calls[1]).toContain("before=cur1");
    expect(list.items.map((s) => s.id)).toEqual(["a", "b", "c"]); // b 没重复
    expect(list.next).toBeNull();
  });

  test("没有游标就不发请求", async () => {
    const calls = fakeApi([{ sessions: [] }]);
    const list = new SessionList();
    await list.load();
    await list.more();
    expect(calls.length).toBe(1);
  });

  test("滚到底会反复触发：别叠请求", async () => {
    const calls = fakeApi([
      { sessions: [], next: "c" },
      { sessions: [], next: null },
    ]);
    const list = new SessionList();
    await list.load();
    await Promise.all([list.more(), list.more(), list.more()]);
    expect(calls.length).toBe(2); // load + 一次 more
  });

  test("拉失败：记 moreError（不是「到底了」），游标留着能重试", async () => {
    let n = 0;
    globalThis.fetch = (async () => {
      n++;
      if (n === 1)
        return new Response(JSON.stringify({ sessions: [], next: "cur" }));
      throw new Error("boom");
    }) as unknown as typeof fetch;
    const list = new SessionList();
    await list.load();
    await list.more();
    expect(list.moreError).toBe(true);
    expect(list.next).toBe("cur"); // 还能重试
    expect(list.moreBusy).toBe(false);
  });

  test("翻页期间整份重来过：这一页作废", async () => {
    const calls = fakeApi([
      { sessions: [row("a")], next: "cur" },
      { sessions: [row("x"), row("y")] }, // more 的回包（会作废）
    ]);
    const list = new SessionList();
    await list.load();
    const pending = list.more();
    // 期间整份重来（游标变了）
    list.next = "other";
    await pending;
    expect(calls.length).toBe(2);
    expect(list.items.map((s) => s.id)).toEqual(["a"]);
  });
});

/**
 * 协调层（router）永远排第一。
 *
 * 这是**界面的呈现决定**，不是服务端的排序：服务端按创建时间倒序是为了位置稳定
 * （见 core 的 `list_page`），而「Router 钉在最上面」只有这个界面要。放在前端做，
 * 服务端不用为某一个客户端让步。
 */
describe("协调层永远第一条", () => {
  const r = (id: string): Session => ({
    id,
    title: id,
    count: 1,
    role: "router",
  });
  const w = (id: string): Session => ({
    id,
    title: id,
    count: 1,
    role: "worker",
  });

  test("首屏：router 在中间也会被挪到第一条", async () => {
    // 服务端按创建时间倒序给：worker 比 router 新，所以 router 在中间
    fakeApi([{ sessions: [w("a"), r("R"), w("b")] }]);
    const list = new SessionList();
    await list.load();
    expect(list.items.map((s) => s.id)).toEqual(["R", "a", "b"]);
  });

  test("没有 router 时原样不动（顺序完全交给服务端）", async () => {
    fakeApi([{ sessions: [w("a"), w("b")] }]);
    const list = new SessionList();
    await list.load();
    expect(list.items.map((s) => s.id)).toEqual(["a", "b"]);
  });

  test("翻页追加页里混进 router：仍然只有一条在最前，不会出现两个「第一条」", async () => {
    fakeApi([
      { sessions: [r("R"), w("a")], next: "cur" },
      // 第二页理论上不会有 router，但万一有也不该插在中间
      { sessions: [w("x"), r("R2")], next: null },
    ]);
    const list = new SessionList();
    await list.load();
    await list.more();
    expect(list.items.map((s) => s.id)).toEqual(["R", "R2", "a", "x"]);
  });

  test("纯函数本身：无 router 返回原数组（不白拷一份）", () => {
    const v = [w("a"), w("b")];
    expect(SessionList.pinRouter(v)).toBe(v);
  });
});
