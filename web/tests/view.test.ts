/**
 * 「回合跑完了，停止键还在」—— 这条测试盯的是流的**身份**。
 *
 * 真实 bug（2026-11）：流身份和状态挤在同一个 `$state` 里（`{k:"live", run}`）。
 * `$state` 是**深代理** —— 存进去的对象读回来是新的 Proxy，`run === me` 永远为假，
 * 于是收尾时那句
 *
 *     if (view.run === me) view = { k: "idle" };
 *
 * 恒不成立：停止键永远亮着，`load` 的 `if (!busy)` 门也一直关着（轮询白转）。
 *
 * 现在身份活在 `Live` 的普通私有字段里，`kind` 那个 `$state` 只装字符串。
 * 这里同时钉两件事：
 *
 *   1. 深代理确实会把这个判断弄坏（反例：别把身份塞回 `$state`）
 *   2. `Live` 真的能一边收流一边被切走，切回来接着收完
 */

import { describe, expect, test } from "bun:test";
import { derived, get, proxy, set, state } from "svelte/internal/client";
import { Chat } from "../src/lib/chat.svelte.ts";

/** 复刻旧写法：`deep` 决定身份是不是塞在深代理里 */
function oneTurn(deep: boolean): boolean {
  type V = { k: "idle" | "live"; run?: { controller: AbortController } };
  const make = (v: V) => state(deep ? proxy(v) : v);
  const view = make({ k: "idle" });
  const running = derived(() => get(view).k === "live");

  const me = { controller: new AbortController() };
  set(view, { k: "live", run: me }, deep);

  const cur = get(view);
  if (cur.k === "live" && cur.run === me) set(view, { k: "idle" }, deep);

  return get(running);
}

// ---------------------------------------------------------------------------
// 假网络：一个由测试自己往里推帧的 SSE 流
// ---------------------------------------------------------------------------

type Pusher = {
  push: (o: unknown) => void;
  done: () => void;
  closed: () => boolean;
};

/** 装一个 fetch：`/api/chat` 给可控的流，别的接口给空 JSON */
function fakeFetch() {
  const pushers: Record<string, Pusher> = {};
  const encoder = new TextEncoder();
  let aborted = 0;

  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (!url.startsWith("/api/chat")) {
      // `/api/session` 之类：一个空会话（`load` 只在 `messages` 空时才发）
      return new Response(
        JSON.stringify({
          messages: [],
          from: 0,
          rev: "r1",
          busy: false,
          queued: [],
          session: "",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      );
    }
    const sid = (JSON.parse(String(init?.body)) as { session: string }).session;
    let ctrl: ReadableStreamDefaultController<Uint8Array> | null = null;
    let isClosed = false;
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        ctrl = c;
      },
      cancel() {
        isClosed = true;
        aborted++;
      },
    });
    // 真的 `fetch` 会在 signal abort 时把 body 掐掉（我们的假实现得自己接上，
    // 不然「淘汰」那条路测不出「流真的断了」）
    init?.signal?.addEventListener("abort", () => {
      if (isClosed) return;
      isClosed = true;
      aborted++;
      ctrl?.error(new DOMException("aborted", "AbortError"));
    });
    const send = (s: string) => {
      if (isClosed) return;
      ctrl?.enqueue(encoder.encode(`data: ${s}\n\n`));
    };
    pushers[sid] = {
      push: (o) => send(JSON.stringify(o)),
      done: () => {
        isClosed = true;
        ctrl?.close();
      },
      closed: () => isClosed,
    };
    return new Response(body, { status: 200 });
  }) as typeof fetch;

  return { pushers, aborted: () => aborted };
}

/** 让微任务 / 宏任务跑一轮（流是异步的） */
const settle = () => new Promise((r) => setTimeout(r, 0));

const start = (messageId = "m1") => ({ type: "start", messageId });

describe("流的身份", () => {
  test("深代理（$state）：run !== me，停止键收不掉（旧 bug 的样子）", () => {
    expect(oneTurn(true)).toBe(true);
  });

  test("普通字段：身份保得住，流收完就回到 idle", () => {
    expect(oneTurn(false)).toBe(false);
  });
});

describe("切走再切回，流不断", () => {
  test("切到别的会话：流不 abort，消息接着长；切回来还是 live，最后自己收尾", async () => {
    const net = fakeFetch();
    const chat = new Chat();

    chat.show("s1");
    void chat.send("你好", []);
    await settle();
    const p = net.pushers["s1"]!;
    p.push(start());
    p.push({ type: "text-start", id: "t1" });
    p.push({ type: "text-delta", id: "t1", delta: "你" });
    await settle();

    const s1 = chat.get("s1")!;
    expect(s1.kind).toBe("live");
    expect(s1.messages.at(-1)!.parts).toEqual([
      { type: "text", id: "t1", text: "你" },
    ]);
    // 收尾之后 `load()` 会用服务端那份（这里是空的）换掉数组，所以这里先抓住
    // 这条消息本身 —— 要验的是「它被收尾了」，不是「数组还在」
    const assistant = s1.messages.at(-1)!;

    // —— 切走 ——
    chat.show("s2");
    await settle();
    expect(net.aborted()).toBe(0); // **没有**断流：这是这次改造的全部意义
    expect(s1.kind).toBe("live");
    expect(chat.viewing).toBe("s2");
    expect(chat.get("s2")).toBeTruthy();

    // 切走期间模型继续说话（服务端本来就不看有没有人听）
    p.push({ type: "text-delta", id: "t1", delta: "好，世界" });
    await settle();
    expect(s1.messages.at(-1)!.parts).toEqual([
      { type: "text", id: "t1", text: "你好，世界" },
    ]);

    // —— 切回来 ——
    chat.show("s1");
    expect(chat.current).toBe(s1);
    expect(s1.kind).toBe("live"); // 逐字还在流，不是轮询出来的快照

    p.done();
    await settle();
    await settle();
    expect(s1.kind).toBe("idle");
    expect(s1.run).toBeNull();
    expect(assistant.done).toBe(true);
    chat.close();
  });

  test("正在看的会话永远不淘汰；超了上限丢最久没看的那条（并断它的流）", async () => {
    const net = fakeFetch();
    const chat = new Chat();

    chat.show("a");
    void chat.send("a", []);
    await settle();
    net.pushers["a"]!.push(start());
    net.pushers["a"]!.push({ type: "text-start", id: "t" });
    await settle();
    const a = chat.get("a")!;
    expect(a.kind).toBe("live");

    // 再看三条：a 是最久没看的（而且不是正在看的）
    chat.show("b");
    chat.show("c");
    chat.show("d");
    expect(chat.get("a")).toBeUndefined();
    expect(net.aborted()).toBe(1); // 断的是本地那条流，服务端照跑
    expect(chat.get("b")).toBeTruthy();
    expect(chat.get("c")).toBeTruthy();
    expect(chat.get("d")).toBeTruthy();
    chat.close();
  });

  test("切回一条不在热集合里的会话：按 remote 走轮询（兜底路径还在）", async () => {
    const net = fakeFetch();
    const chat = new Chat();
    chat.show("x");
    await settle();
    // 服务端说它在跑
    globalThis.fetch = (async () =>
      new Response(
        JSON.stringify({
          messages: [],
          from: 0,
          rev: "r2",
          busy: true,
          queued: [],
          session: "x",
        }),
        { status: 200 },
      )) as unknown as typeof fetch;
    await chat.get("x")!.load();
    expect(chat.get("x")!.kind).toBe("remote");
    expect(chat.get("x")!.running).toBe(true);
    chat.close();
    void net;
  });
});

describe("两路并发：用量 / 排队不串会话", () => {
  /**
   * 以前用量和「排队交出去了」走的是 `lib/sse.ts` 里的**全局** sink
   *（`onUsage` / `onQueuedDelivered`）—— 没有 sid。两条会话同时跑的时候，
   * B 的 `data-usage` 会写到 A 的圆环上，A 的排队项会被挪进 B 的正文。
   * 现在由 `Live` 自己收（`consume` 的 `onChunk`），它知道自己是哪条。
   */
  test("A、B 交错到达：各自只涨自己的用量", async () => {
    const net = fakeFetch();
    const chat = new Chat();
    const usage = (n: number) => ({
      type: "data-usage",
      data: {
        input: n,
        uncached_input: n,
        cache_read: 0,
        cache_write: 0,
        output: n,
        reasoning: 0,
        total: n * 2,
        cache_hit_ratio: null,
        session: {
          input: n,
          uncached_input: n,
          cache_read: 0,
          cache_write: 0,
          output: n,
          reasoning: 0,
          calls: 1,
          cache_reported_input: 0,
          cache_hit_ratio: null,
        },
      },
    });

    chat.show("a");
    void chat.send("a", []);
    await settle();
    chat.show("b");
    void chat.send("b", []);
    await settle();

    net.pushers["a"]!.push(start());
    net.pushers["b"]!.push(start());
    // 交错：A 的、B 的、B 的、A 的
    net.pushers["a"]!.push(usage(10));
    net.pushers["b"]!.push(usage(99));
    net.pushers["b"]!.push(usage(98));
    net.pushers["a"]!.push(usage(11));
    await settle();

    expect(chat.get("a")!.usage!.last.input).toBe(11);
    expect(chat.get("b")!.usage!.last.input).toBe(98);
    chat.close();
  });

  test("A 的排队项被交付：挪进 A 的正文，B 的一根头发都不动", async () => {
    const net = fakeFetch();
    const chat = new Chat();

    // 两条会话各有一个「排队中」
    chat.queue.items = [
      { id: "q-a", sid: "a", parts: [{ type: "text", id: "u", text: "qa" }] },
      { id: "q-b", sid: "b", parts: [{ type: "text", id: "u", text: "qb" }] },
    ];
    chat.show("a");
    const b = chat.live("b"); // 让 B 也活在表里（下面要验它没被动过）
    void chat.send("x", []);
    await settle();
    net.pushers["a"]!.push(start());
    // 服务端说：A 的 q-a 交给模型了
    net.pushers["a"]!.push({ type: "data-queued-delivered", data: { ids: ["q-a"] } });
    await settle();

    const a = chat.get("a")!;
    expect(a.messages.some((m) => m.id === "q-a")).toBe(true);
    expect(b.messages.some((m) => m.id === "q-b")).toBe(false);
    expect(chat.queue.items.map((q) => q.id)).toEqual(["q-b"]);
    chat.close();
  });
});
