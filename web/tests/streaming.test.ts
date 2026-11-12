/**
 * 流式推送**能不能被看见**。
 *
 * 这条测试是为了一个真实踩过的 bug：把 `beginStreaming()` 简化成「直接返回一个字面
 * 量对象」之后，界面上只有用户那条气泡，模型的回复永远不出现 —— 看起来活像「SSE
 * 没推送」，其实是写进了原始对象、Svelte 的代理感知不到。
 *
 * 机制：`applyChunk` 是**原地**往 `parts` 里追加 delta，而流式期间没有任何东西会
 * 重新给 `messages[i]` 赋值。所以「写的是代理还是原始对象」是唯一决定因素。
 *
 * 这里用 `svelte/internal/client` 的 `proxy` / `effect_root` / `render_effect`
 * 复刻一个 `$state` 数组 + 一个读它的 effect，行为与模板里的 `{#each}` 一致。
 */

import { describe, expect, test } from "bun:test";
import {
  effect_root,
  flush,
  proxy,
  render_effect,
} from "svelte/internal/client";
import { applyChunk, type Message } from "../src/lib/sse";

/** 复刻 `let messages = $state<Message[]>([])` + 一个渲染 effect */
function setup() {
  const messages = proxy([] as Message[]);
  const state = { runs: 0, text: "" };
  const stop = effect_root(() => {
    render_effect(() => {
      state.text = messages
        .flatMap((m) => m.parts)
        .map((p) => (p.type === "text" ? p.text : ""))
        .join("");
      state.runs += 1;
    });
  });
  return { messages, state, stop };
}

const empty = (): Message => ({
  id: "m1",
  role: "assistant",
  parts: [],
  done: false,
});

describe("流式推送", () => {
  test("写进 push 进去的那条消息，界面能看见（delta 必须落在代理上）", async () => {
    const { messages, state, stop } = setup();
    await flush();

    // 等价于 `store.beginStreaming()` 的返回值
    const msg = proxy(empty());
    messages.push(msg);
    await flush();
    const afterPush = state.runs;

    applyChunk(msg, { type: "text-start", id: "t1" });
    applyChunk(msg, { type: "text-delta", id: "t1", delta: "你好" });
    applyChunk(msg, { type: "text-delta", id: "t1", delta: "，世界" });
    await flush();

    expect(state.runs).toBeGreaterThan(afterPush); // 真的重跑了
    expect(state.text).toBe("你好，世界");
    stop();
  });

  test("反例：写普通对象没有信号 —— 所以 beginStreaming 不能返回字面量", async () => {
    const { messages, state, stop } = setup();
    await flush();

    const plain = empty(); // 普通对象（就是我改坏的那版）
    messages.push(plain);
    await flush();
    const afterPush = state.runs;

    applyChunk(plain, { type: "text-start", id: "t1" });
    applyChunk(plain, { type: "text-delta", id: "t1", delta: "看不见的字" });
    await flush();

    // 数字**没有**变：这正是那个 bug 的样子
    expect(state.runs).toBe(afterPush);
    expect(state.text).toBe("");
    // 但对象自己确实被改了 —— 所以不报错、不抛异常，只是界面不动
    expect(plain.parts).toHaveLength(1);
    stop();
  });

  /**
   * `beginStreaming()` 的真实实现（`store.svelte.ts`）没法在这里 import 跑：
   * `$state` 只在 `.svelte` / `.svelte.ts` 里可用，而 `bun test` 走的是普通 TS
   * 加载（不编译 runes）。所以这里复刻它的**那两行**，并盯住编译产物长什么样：
   *
   *     const box = $state({ msg: {...} });  →  F({ msg: {...} })
   *     return box.msg;                      →  .msg
   *
   * `F` 就是 `proxy()`。下面这条断言的是「同一个写法」的性质，配合 `vite build`
   * 的输出确认线上跑的就是它。
   */
  test("先写进 $state 再读回来 = 代理（beginStreaming 靠的就是这个）", async () => {
    const { messages, state, stop } = setup();
    await flush();

    // 复刻 beginStreaming：
    const box = proxy({ msg: empty() });
    const msg = box.msg; // ← 读回来，才是代理
    messages.push(msg);
    await flush();
    const afterPush = state.runs;

    applyChunk(msg, { type: "text-start", id: "t1" });
    applyChunk(msg, { type: "text-delta", id: "t1", delta: "模型的回复" });
    await flush();

    expect(state.runs).toBeGreaterThan(afterPush);
    expect(state.text).toBe("模型的回复");
    // 同一个对象（不是拷贝）：`run()` 和 `stop()` 认的就是这个身份
    expect(box.msg).toBe(msg);
    stop();
  });
});
