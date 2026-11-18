/**
 * `lib/chat.svelte.ts` 的**状态机**：热集合、响应性、定时器、队列镜像。
 *
 * 这几条都是实测踩到过的（不是假想的）：
 *
 *   1. `$state<Map>` **不是**响应式的 —— Svelte 只代理 plain object / array，
 *      `Map` 的原型不匹配就直接原样返回（`svelte/src/internal/client/proxy.js`
 *      开头）。于是 `map.set()` 一个信号都不发，`current` 只在 `viewing` 变时
 *      才重算。冷启动（`viewing` 本来就是 `""`）时建会话失败，那条错误气泡永远
 *      不显示，用户发的字凭空消失。
 *   2. `stop()` 之后的补拉只有一个定时器 —— 两条会话各停一次，前一个被
 *      `clearTimeout` 抹掉，那条的「运行中」要等切前台才消失。
 *   3. `live()` 里「先 set 再淘汰」时，新建的那条 `lastUsed` 还是 0（全表最小），
 *      能不能保住全看调用顺序 —— 淘汰必须**显式跳过**它。
 *   4. `replaceQueued` 每次轮询都换新数组 —— 依赖 `queue` 的 `$derived` 每次
 *      轮询都白重算一遍。
 */

import { describe, expect, test } from "bun:test";
import { effect_root, flush, render_effect } from "svelte/internal/client";
import { Chat } from "../src/lib/chat.svelte.ts";

/** 把一条异步流走完的等待（定时器是 300ms，留余量） */
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

describe("热集合的响应性", () => {
  test("只动 map、不动 viewing：current 也要跟着更新", async () => {
    const chat = new Chat();
    const seen: (string | null)[] = [];
    const stop = effect_root(() => {
      render_effect(() => {
        seen.push(chat.current?.sid ?? null);
      });
    });
    await flush();

    // 冷启动：`viewing` 本来就是 `""`，`show("")` 不会让它变 ——
    // 这一步以前什么都读不到（`current` 恒为 null）
    chat.show("");
    await flush();
    const live = chat.current;
    expect(live).not.toBeNull();

    // 建会话失败的那条路：直接往 live.messages 里推错误消息，界面要看得见
    live!.messages.push({
      id: "e",
      role: "assistant",
      parts: [{ type: "error", text: "建会话失败" }],
      done: true,
    });
    await flush();
    expect(seen.at(-1)).toBe("");

    stop();
    chat.close();
  });
});

describe("淘汰（LRU）", () => {
  test("刚建出来的那条不会被自己挤掉（哪怕它 lastUsed 最小）", () => {
    const chat = new Chat();
    chat.show("a");
    chat.show("b");
    chat.show("c");
    // 直接调 `live()`：`viewing` 还是 "c"，d 是第 4 条、`lastUsed` 还是 0
    const d = chat.live("d");
    expect(chat.get("d")).toBe(d);
    // 被挤掉的是最久没看的 a
    expect(chat.get("a")).toBeUndefined();
    chat.close();
  });

  test("正在看的那条永远不淘汰", () => {
    const chat = new Chat();
    chat.show("a");
    chat.show("b");
    chat.show("c");
    chat.show("a"); // a 变成正在看的
    chat.live("d");
    expect(chat.get("a")).toBeTruthy();
    chat.close();
  });
});

describe("stop() 之后的补拉", () => {
  test("两条会话各停一次：两次都要拉回来", async () => {
    const chat = new Chat();
    const a = chat.live("a");
    const b = chat.live("b");
    const hits: string[] = [];
    (a as unknown as { load: () => Promise<void> }).load = async () => {
      hits.push("a");
    };
    (b as unknown as { load: () => Promise<void> }).load = async () => {
      hits.push("b");
    };
    a.stop();
    b.stop();
    await wait(400);
    expect(hits.sort()).toEqual(["a", "b"]);
    chat.close();
  });

  test("同一条会话连按两次停止：只跑最后一次（前一个该作废）", async () => {
    const chat = new Chat();
    const a = chat.live("a");
    let n = 0;
    (a as unknown as { load: () => Promise<void> }).load = async () => {
      n++;
    };
    a.stop();
    a.stop();
    await wait(400);
    expect(n).toBe(1);
    chat.close();
  });
});

describe("队列镜像", () => {
  test("服务端回同一份空队列：不换数组（依赖它的 derived 不该重算）", async () => {
    const chat = new Chat();
    let runs = 0;
    const stop = effect_root(() => {
      render_effect(() => {
        void chat.queuedOf(chat.viewing).length;
        runs++;
      });
    });
    await flush();
    const base = runs;

    for (let i = 0; i < 3; i++) {
      chat.queue.replace("s1", []);
      await flush();
    }
    expect(runs - base).toBe(0);

    // 真变了就要换
    chat.queue.replace("s1", [
      { id: "q1", role: "user", parts: [], done: false },
    ]);
    await flush();
    expect(runs - base).toBe(1);

    stop();
    chat.close();
  });
});

describe("事件：新会话什么时候进侧边栏", () => {
  /**
   * `POST /api/sessions` 建出来的会话**还没有内容**，而 `/api/sessions` 的
   * `list_page` 按 `has_content()` 过滤 —— 那一刻重拉列表是拿不到它的。
   *
   * 所以「新会话出现在侧边栏」的时机是**第一帧到达**（`started`），不是 `created`。
   * 实测踩过：点新对话、发第一句，抽屉里一直是空的，要等整个回合跑完才冒出来。
   */
  test("created 只带 sid；started 在确认开跑时才发", async () => {
    const chat = new Chat();
    const events: string[] = [];
    chat.on("created", (sid) => events.push(`created:${sid.slice(0, 4)}`));
    chat.on("started", () => events.push("started"));
    chat.on("ended", () => events.push("ended"));

    // 让建会话和流都可控
    globalThis.fetch = (async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.endsWith("/api/sessions"))
        return new Response(JSON.stringify({ id: "s-real" }), { status: 200 });
      // `/api/chat`：给一个只含 `start` 的流，然后结束
      const enc = new TextEncoder();
      return new Response(
        new ReadableStream({
          start(c) {
            c.enqueue(enc.encode('data: {"type":"start","messageId":"m"}\n\n'));
            c.close();
          },
        }),
        { status: 200 },
      );
    }) as typeof fetch;

    chat.show("");
    await chat.send("第一句", []);
    await new Promise((r) => setTimeout(r, 50));

    // created 在建会话之后立刻发（地址栏要换），started 在第一帧之后
    expect(events[0]).toBe("created:s-re");
    expect(events).toContain("started");
    expect(events).toContain("ended");
    // 顺序：created → started → ended
    expect(events.indexOf("created:s-re")).toBeLessThan(events.indexOf("started"));
    expect(events.indexOf("started")).toBeLessThan(events.indexOf("ended"));
    chat.close();
  });

  test("退订之后不再收到", async () => {
    const chat = new Chat();
    let n = 0;
    const off = chat.on("ended", () => n++);
    chat.ended(chat.live("a"));
    off();
    chat.ended(chat.live("a"));
    expect(n).toBe(1);
    chat.close();
  });
});
