/**
 * `lib/sse.ts` 的两条不变式：
 *
 *   1. `[DONE]` 一帧就把这条流**结束掉**（不能接着读 —— 服务端要是留着连接，
 *      这条流就永远挂着，回合永远是「运行中」）
 *   2. 一个帧被网络切在中间也能拼回来（必须自己缓冲）
 *
 * 不用真网络：给 `parseUiStream` 一个自己造 `Response`。
 */

import { describe, expect, test } from "bun:test";
import {
  applyChunk,
  parseUiStream,
  type Message,
  type UiChunk,
} from "../src/lib/sse";
import { applyPatch } from "../src/lib/session";

/** 把几段字节做成一个流。`hang` = 发完不关（学服务端的 keep-alive）。 */
function res(chunks: string[], hang = false): Response {
  let i = 0;
  const enc = new TextEncoder();
  return new Response(
    new ReadableStream({
      pull(c) {
        if (i < chunks.length) return void c.enqueue(enc.encode(chunks[i++]));
        if (hang) return new Promise(() => {}); // 永不结束
        c.close();
      },
    }),
  );
}

const frame = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;

async function collect(r: Response, ms = 200): Promise<UiChunk[]> {
  const out: UiChunk[] = [];
  const it = parseUiStream(r);
  // 到点就判失败（流本该已经结束）
  const timeout = new Promise<never>((_, no) =>
    setTimeout(() => no(new Error("流没结束")), ms),
  );
  await Promise.race([
    (async () => {
      for await (const c of it) out.push(c);
    })(),
    timeout,
  ]);
  return out;
}

describe("parseUiStream", () => {
  test("[DONE] 之后不再读：即使对端不关连接，这条流也结束", async () => {
    const got = await collect(
      res(
        [
          frame({ type: "start", messageId: "m" }),
          frame({ type: "text-delta", id: "t", delta: "你好" }),
          "data: [DONE]\n\n",
        ],
        true, // 对端不关（服务端 keep-alive / 代理不转 FIN）
      ),
    );
    expect(got).toHaveLength(2);
    expect(got[1]).toMatchObject({ type: "text-delta", delta: "你好" });
  });

  test("一帧被切在中间也能拼回来（含多字节字符）", async () => {
    const payload = frame({ type: "text-delta", id: "t", delta: "你好世界" });
    // 按字节切，故意切在 UTF-8 多字节字符中间
    const bytes = new TextEncoder().encode(payload);
    const dec = new TextDecoder();
    const parts: string[] = [];
    for (let i = 0; i < bytes.length; i += 3)
      parts.push(dec.decode(bytes.slice(i, i + 3), { stream: true }));
    const got = await collect(res([...parts, "data: [DONE]\n\n"]));
    expect(got).toHaveLength(1);
    expect(got[0]).toMatchObject({ delta: "你好世界" });
  });

  test("没闭合的最后一帧也吐出来（对端直接关掉的情况）", async () => {
    const got = await collect(
      res(['data: {"type":"finish"}']), // 没有结尾空行
    );
    expect(got).toEqual([{ type: "finish" }]);
  });

  test("帧分隔符正好被切在片边界上（`\n` | `\n`）", async () => {
    const got = await collect(
      res([
        'data: {"type":"text-start","id":"t"}\n',
        '\ndata: {"type":"text-delta","id":"t","delta":"hi"}\n\n',
      ]),
    );
    expect(got).toHaveLength(2);
    expect(got[1]).toMatchObject({ type: "text-delta", delta: "hi" });
  });

  test("`\r\n` 的流也能认（跨片的 `\r` | `\n\r\n`）", async () => {
    const got = await collect(
      res(['data: {"type":"text-delta","id":"t","delta":"a"}\r', "\n\r\n"]),
    );
    expect(got).toHaveLength(1);
    expect(got[0]).toMatchObject({ delta: "a" });
  });

  test("大帧被切成很多片：不重扫已经看过的部分（O(n) 而不是 O(n²)）", async () => {
    // 一个 200 KB 的帧切成 64 B 的片。以前每片都 `buf += x` 再从头 indexOf，
    // 实测 82 ms；现在攒成数组、只在真出现分隔符时 join 一次。
    const big = "x".repeat(200 * 1024);
    const one = `data: ${JSON.stringify({ type: "text-delta", id: "t", delta: big })}\n\n`;
    const pieces: string[] = [];
    for (let i = 0; i < one.length; i += 64) pieces.push(one.slice(i, i + 64));

    const t0 = performance.now();
    const got = await collect(res(pieces), 2000);
    const ms = performance.now() - t0;

    expect(got).toHaveLength(1);
    expect(ms).toBeLessThan(30); // 留足余量（实测 ~4 ms），只要别退回平方级
  });

  test("坏帧跳过，不拖垮整条流", async () => {
    const got = await collect(
      res([
        "data: 这不是 JSON\n\n",
        frame({ type: "text-delta", id: "t", delta: "好" }),
        "data: [DONE]\n\n",
      ]),
    );
    expect(got).toHaveLength(1);
    expect(got[0]).toMatchObject({ delta: "好" });
  });
});

describe("data-* 不是消息内容", () => {
  /**
   * `data-*` 帧是**事件**，不能进 `parts`：`parts` 的下标必须和服务端的
   * `view_parts_len`（只数 reasoning / text / tool）对齐 —— `session_patch` 的
   * `keepParts` 就是按那个数告诉客户端「前 k 个 part 留着、后面换掉」。
   *
   * 往 `parts` 里塞一个服务端不数的 part，多步回合的下一步补丁就会把它（以及
   * 它后面的一切）挤掉：实测第一步的「输出被截断」警告在第二步到达时消失。
   */
  const m = (): Message => ({
    id: "a1",
    role: "assistant",
    parts: [],
    done: false,
  });

  test("data-usage 不往 parts 里堆（一个回合几十步就是几十个 part）", () => {
    const msg = m();
    for (let i = 0; i < 50; i++)
      applyChunk(msg, {
        type: "data-usage",
        data: { input: i, session: null },
      } as UiChunk);
    expect(msg.parts).toHaveLength(0);
  });

  test("data-warning 落到 warnings 字段，界面看得见", () => {
    const msg = m();
    applyChunk(msg, {
      type: "data-warning",
      data: { message: "输出达到长度上限被截断" },
    } as UiChunk);
    expect(msg.parts).toHaveLength(0);
    expect(msg.warnings).toEqual(["输出达到长度上限被截断"]);
  });

  test("warning 跨下一步的增量补丁保住（服务端历史里没有它）", () => {
    const msg = m();
    applyChunk(msg, { type: "text-start", id: "t1" } as UiChunk);
    applyChunk(msg, {
      type: "text-delta",
      id: "t1",
      delta: "第一段",
    } as UiChunk);
    applyChunk(msg, {
      type: "data-warning",
      data: { message: "输出被截断" },
    } as UiChunk);

    // 服务端 `view_parts_len` 只数 text = 1
    const r = applyPatch(
      { messages: [msg], windowFrom: 0 },
      {
        append: true,
        replaceFrom: 0,
        keepParts: 1,
        messages: [
          {
            id: "a1",
            role: "assistant",
            done: false,
            parts: [{ type: "text", id: "t2", text: "第二段" }],
          },
        ],
      },
      0,
    );
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    const out = r.window.messages[0];
    expect(out.parts.map((p) => p.type)).toEqual(["text", "text"]);
    expect(out.warnings).toEqual(["输出被截断"]);
  });
});
