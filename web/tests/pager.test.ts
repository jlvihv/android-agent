/**
 * `lib/pager.svelte.ts`：一条会话的消息**窗口**。
 *
 * 从 `Live` 里拆出来的那半 —— 它和流没关系，只做「手上这一窗是完整视图的哪一段」
 * 的记账。`session.ts` 的纯函数（`applyPatch` / `applyWindow` / `prependPage` /
 * `trimOlder`）已经有测试；这里盯的是**编排**：合并、作废、预取、收窗口。
 *
 * 不碰网络：给 `globalThis.fetch` 装一个假的 `/api/session`。
 */

import { describe, expect, test } from "bun:test";
import { Pager } from "../src/lib/pager.svelte.ts";
import type { Message } from "../src/lib/sse";

const msg = (id: string, role: "user" | "assistant" = "user"): Message => ({
  id,
  role,
  parts: [{ type: "text", id: "t", text: id }],
  done: true,
});

/** 一个受控的 `/api/session`：测试自己决定下一次回什么 */
function fakeApi() {
  const replies: unknown[] = [];
  const calls: string[] = [];
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    calls.push(String(input));
    const body = replies.shift() ?? { messages: [], from: 0, busy: false };
    return new Response(JSON.stringify(body), { status: 200 });
  }) as typeof fetch;
  return {
    /** 下一次请求回这个 */
    next(body: unknown) {
      replies.push(body);
    },
    calls,
  };
}

/** 一个「活着」的 pager（`alive()` 恒真，`sid()` 固定） */
function pager(sid = "s1") {
  return new Pager(
    () => sid,
    () => true,
  );
}

describe("整窗", () => {
  test("首屏：整份换上，起点用服务端的 from", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u1"), msg("a1", "assistant")], from: 0, busy: false });
    await p.load(() => {});
    expect(p.messages.map((m) => m.id)).toEqual(["u1", "a1"]);
    expect(p.windowFrom).toBe(0);
    expect(p.hasOlder).toBe(false);
  });

  test("窗口从中间开始（服务端还有更早的）：hasOlder 跟着为真", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u5")], from: 5, busy: false });
    await p.load(() => {});
    expect(p.windowFrom).toBe(5);
    expect(p.hasOlder).toBe(true);
  });

  test("unchanged：连 messages 都不带，窗口原样不动", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u1")], from: 0, busy: false, rev: "r1" });
    await p.load(() => {});
    const before = p.messages;

    api.next({ unchanged: true, busy: false, rev: "r1" });
    await p.load(() => {});
    expect(p.messages).toBe(before); // 同一个数组对象（没白换）
  });

  test("onReply 返回 false：不套补丁（unchanged 那一路）", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u1")], from: 0, busy: false });
    await p.load(() => {});
    const before = p.messages;

    api.next({ messages: [msg("u2")], from: 0, busy: false, unchanged: true });
    await p.load(() => false);
    expect(p.messages).toBe(before);
  });
});

describe("作废（stale）", () => {
  test("路上本地发过消息：这份回包丢掉", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u1")], from: 0, busy: false });
    const pending = p.load(() => {});
    p.sent++; // 发消息了
    await pending;
    expect(p.messages).toEqual([]); // 回包被丢掉
  });

  test("路上往上翻了一页：这份回包丢掉", async () => {
    const api = fakeApi();
    const p = pager();
    p.messages = [msg("u3"), msg("u4")];
    p.windowFrom = 3;
    api.next({ messages: [msg("u5")], from: 3, busy: false });
    const pending = p.load(() => {});
    p.windowFrom = 1; // 用户往上翻
    await pending;
    expect(p.messages.map((m) => m.id)).toEqual(["u3", "u4"]);
  });

  test("会话被淘汰了（alive 为假）：回包丢掉", async () => {
    let alive = true;
    const api = fakeApi();
    const p = new Pager(
      () => "s1",
      () => alive,
    );
    api.next({ messages: [msg("u1")], from: 0, busy: false });
    const pending = p.load(() => {});
    alive = false;
    await pending;
    expect(p.messages).toEqual([]);
  });
});

describe("合并（inflight）", () => {
  test("同一份已经在路上：只发一次请求，但回来时补一次", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u1")], from: 0, busy: false });
    api.next({ messages: [msg("u2")], from: 0, busy: false });

    const a = p.load(() => {});
    const b = p.load(() => {}); // 合并掉，但记一笔
    await Promise.all([a, b]);
    // 补的那一次是 `finally` 里 fire-and-forget 发出去的，等它落地
    await new Promise((r) => setTimeout(r, 0));
    // 第一次 + 补的那一次 = 2
    expect(api.calls.length).toBe(2);
    expect(p.messages.map((m) => m.id)).toEqual(["u2"]);
  });
});

describe("往前翻 / 收窗口", () => {
  test("prependPage 接得上：插到前面，windowFrom 跟着走", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u5"), msg("u6")], from: 5, busy: false });
    await p.load(() => {});
    expect(p.windowFrom).toBe(5);

    api.next({ messages: [msg("u3"), msg("u4")], from: 3 });
    const ok = await p.loadOlder();
    expect(ok).toBe(true);
    expect(p.messages.map((m) => m.id)).toEqual(["u3", "u4", "u5", "u6"]);
    expect(p.windowFrom).toBe(3);
  });

  test("接不上（中间有洞）：整页丢掉，宁可少一窗", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u5")], from: 5, busy: false });
    await p.load(() => {});

    api.next({ messages: [msg("u0")], from: 0 }); // 应该是 from 4
    const ok = await p.loadOlder();
    expect(ok).toBe(false);
    expect(p.messages.map((m) => m.id)).toEqual(["u5"]);
  });

  test("预取之后 loadOlder 直接用预取的那一份，不再发请求", async () => {
    const api = fakeApi();
    const p = pager();
    api.next({ messages: [msg("u5")], from: 5, busy: false });
    await p.load(() => {});

    api.next({ messages: [msg("u4")], from: 4 });
    p.prefetchOlder();
    await new Promise((r) => setTimeout(r, 0)); // 让预取落地
    const n = api.calls.length;

    const ok = await p.loadOlder();
    expect(ok).toBe(true);
    expect(api.calls.length).toBe(n); // 没有新请求
  });

  test("trimOldPages：回到底部时收掉前面的（windowFrom 平移）", () => {
    const p = pager();
    const many = Array.from({ length: 40 }, (_, i) => msg(`u${i}`));
    p.messages = many;
    p.windowFrom = 100;
    p.trimOldPages();
    expect(p.messages.length).toBeLessThan(40);
    // 删的是前面那几条，下标必须对齐
    expect(p.windowFrom).toBe(100 + (40 - p.messages.length));
    expect(p.messages.at(-1)!.id).toBe("u39");
  });
});
