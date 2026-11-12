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
import { parseUiStream, type UiChunk } from "../src/lib/sse";

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
