/**
 * `lib/queue.svelte.ts`：排队那条虚线气泡的**来源标记**。
 *
 * 真实问题（审查抓到的）：服务端 `queued_view` 已经给了 `fromWorker` /
 * `fromSchedule`，但前端这条链路有两处把它们抹掉了 ——
 *
 *   1. `QueueMirror#replace` 只取 `{id, parts}`，两个键直接丢
 *   2. `App` 把排队项拼成 `Message` 时只给 `{id, role, parts, done}`
 *
 * 后果：一条排队中的回执 / 定时任务消息在界面上是**用户气泡**，交出去之后才变成
 * 「子会话报告」/「定时任务」。同一个东西凭空换个样子。
 *
 * 第三处是 `Chat#onData` 里 `data-queued-delivered` 那一刻（`deliver` 返回的那几条
 * 要带着来源推进正文），见 `chat.svelte.ts`。
 */

import { describe, expect, test } from "bun:test";
import { QueueMirror } from "../src/lib/queue.svelte.ts";
import type { Message } from "../src/lib/sse.ts";

const msg = (id: string, extra: Partial<Message> = {}): Message => ({
  id,
  role: "user",
  done: false,
  parts: [{ type: "text", id: `t-${id}`, text: id }],
  ...extra,
});

describe("replace", () => {
  test("服务端给的 fromWorker / fromSchedule 要留下来", () => {
    const q = new QueueMirror();
    q.replace("s1", [
      msg("a"),
      msg("b", { fromWorker: "sess-9" }),
      msg("c", { fromSchedule: "task-7" }),
    ]);
    const got = q.of("s1");
    expect(got.length).toBe(3);
    expect(got[0].fromWorker).toBeUndefined();
    expect(got[0].fromSchedule).toBeUndefined();
    expect(got[1].fromWorker).toBe("sess-9");
    expect(got[2].fromSchedule).toBe("task-7");
  });

  test("只换这个会话的那几条，别的会话（含来源）不动", () => {
    const q = new QueueMirror();
    q.replace("s1", [msg("a", { fromSchedule: "t-1" })]);
    q.replace("s2", [msg("b", { fromWorker: "w-1" })]);
    q.replace("s1", [msg("c", { fromSchedule: "t-2" })]);
    expect(q.of("s1").map((x) => [x.id, x.fromSchedule])).toEqual([
      ["c", "t-2"],
    ]);
    expect(q.of("s2").map((x) => [x.id, x.fromWorker])).toEqual([["b", "w-1"]]);
  });

  test("刚发出去、还没回话的那条（pending）不会被服务端那份冲掉", () => {
    const q = new QueueMirror();
    q.add({ id: "hoping", sid: "s1", parts: [], pending: true });
    q.replace("s1", [msg("a")]);
    expect(q.of("s1").map((x) => x.id)).toEqual(["a", "hoping"]);
  });
});

describe("deliver", () => {
  test("交出去了那几条带着来源交还给调用方（推进正文时不能丢）", () => {
    const q = new QueueMirror();
    q.replace("s1", [
      msg("a", { fromSchedule: "t-1" }),
      msg("b"),
      msg("c", { fromWorker: "w-1" }),
    ]);
    const moved = q.deliver(["a", "c"]);
    expect(moved.map((x) => x.id)).toEqual(["a", "c"]);
    expect(moved[0].fromSchedule).toBe("t-1");
    expect(moved[1].fromWorker).toBe("w-1");
    // 剩下的还在排队
    expect(q.of("s1").map((x) => x.id)).toEqual(["b"]);
  });
});
