/**
 * `lib/partfull.ts` 的缓存 key 必须带会话。
 *
 * `r{i}` / `t{i}` 是 `ChatMessage` 的下标 —— 每条会话都有 `r3`，只按裸 id 缓存
 * 的话，在会话 A 点开过 `r3` 再切到 B 点开 `r3`，拿到的是 A 的那段思考，而且
 * 连请求都不发（`hit !== undefined` 直接返回）。
 */

import { afterEach, describe, expect, test } from "bun:test";

/** 每个用例用不同的会话 id：缓存是模块级的，跨用例共享（那正是它要防的事） */
let n = 0;
const sid = () => `s${++n}`;
import { partFull } from "../src/lib/partfull";

const realFetch = globalThis.fetch;
const calls: string[] = [];

function stub(): void {
  calls.length = 0;
  globalThis.fetch = (async (url: string) => {
    calls.push(String(url));
    const q = new URL(String(url), "http://x").searchParams;
    return new Response(
      JSON.stringify({ text: `${q.get("session")}:${q.get("id")}` }),
      {
        headers: { "content-type": "application/json" },
      },
    );
  }) as unknown as typeof fetch;
}

afterEach(() => {
  globalThis.fetch = realFetch;
});

describe("partFull", () => {
  test("同一个 id、不同会话：各拿各的，不串", async () => {
    stub();
    const a = await partFull("r3", sid());
    const b = await partFull("r3", sid());
    expect(a.text).toEndWith(":r3");
    expect(b.text).toEndWith(":r3");
    expect(a.text).not.toBe(b.text); // 两条不同的会话答的
    expect(calls).toHaveLength(2); // 第二条**真的发了**（没被 A 的缓存命中）
  });

  test("同一个会话同一个 id：第二次不再请求", async () => {
    stub();
    const s = sid();
    await partFull("r3", s);
    await partFull("r3", s);
    expect(calls).toHaveLength(1);
  });

  test("并发点两次：只发一条", async () => {
    stub();
    const s = sid();
    const [x, y] = await Promise.all([partFull("c1", s), partFull("c1", s)]);
    expect(x).toBe(y);
    expect(calls).toHaveLength(1);
  });

  test("空会话：根本不发请求（服务端没有「当前会话」可兜底了）", async () => {
    stub();
    const got = await partFull("r1", "");
    expect(got).toEqual({});
    expect(calls).toHaveLength(0);
  });
});
