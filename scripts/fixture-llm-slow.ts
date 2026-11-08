#!/usr/bin/env bun
/**
 * 压测用的假 LLM：和 `fixture-llm.ts` 同一个协议，但**节奏可控**。
 *
 * 为什么不复用那个：它把整段回复拼成一个字符串一次返回。压测要的是「服务端
 * 慢慢地、一片一片地吐」—— 只有那样才会真的经历「SSE 帧被切在网络分片中间」、
 * 「流开着好几十秒」、「并发几十路同时挂着」这些路径。
 *
 * 环境变量：
 *   FIXTURE_PORT     监听端口（默认 8791，避开 fixture-llm 的 8790）
 *   LLM_FIRST_MS     首个字节前等多久（模拟长上下文预填充，默认 0）
 *   LLM_GAP_MS       每个 chunk 之间等多久（默认 8）
 *   LLM_CHUNKS       正文分多少片（默认 40）
 *   LLM_TOOL         1 = 第一轮发一个 bash 工具调用（默认 0，只回正文）
 *   LLM_FAIL_RATE    0~1，按这个概率直接回 429/500（压测重试路径，默认 0）
 *   LLM_FAIL_STATUS  失败时用的状态码（默认 429）
 *   LLM_SILENT       1 = 回 200 + 一个事件都不发（压测「静默流」超时，默认 0）
 *
 *     bun scripts/fixture-llm-slow.ts
 */
const PORT = Number(process.env.FIXTURE_PORT ?? 8791);
const FIRST_MS = Number(process.env.LLM_FIRST_MS ?? 0);
const GAP_MS = Number(process.env.LLM_GAP_MS ?? 8);
const CHUNKS = Number(process.env.LLM_CHUNKS ?? 40);
const WITH_TOOL = process.env.LLM_TOOL === "1";
const FAIL_RATE = Number(process.env.LLM_FAIL_RATE ?? 0);
const FAIL_STATUS = Number(process.env.LLM_FAIL_STATUS ?? 429);
const SILENT = process.env.LLM_SILENT === "1";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const frame = (delta: object, extra: object = {}) =>
  `data: ${JSON.stringify({
    id: "chatcmpl-slow",
    object: "chat.completion.chunk",
    created: 0,
    model: "fixture-slow",
    choices: [{ index: 0, delta, ...extra }],
  })}\n\n`;

/** 一段足够长的正文，切片后每片都是一个完整的多字节字符边界。 */
const BODY =
  "这是一段用于压测的受控正文。" +
  "它会被切成很多片，一片一片地通过网络发出去，" +
  "用来验证流式链路上的切帧、累积和落盘。" +
  "每一片之间都有真实的延迟。".repeat(8);

let inFlight = 0;
let totalReq = 0;

Bun.serve({
  port: PORT,
  hostname: "127.0.0.1",
  async fetch(req) {
    const { pathname } = new URL(req.url);
    if (pathname.endsWith("/models")) {
      return Response.json({
        object: "list",
        data: [{ id: "fixture-slow", object: "model" }],
      });
    }
    if (!pathname.endsWith("/chat/completions")) {
      return new Response("只有 /v1/models 和 /v1/chat/completions", { status: 404 });
    }
    totalReq += 1;
    inFlight += 1;
    const body = (await req.json()) as { messages?: { role?: string }[] };
    const hasToolResult = (body.messages ?? []).some((m) => m.role === "tool");

    // 失败注入：直接回一个可重试的状态码
    if (FAIL_RATE > 0 && Math.random() < FAIL_RATE) {
      inFlight -= 1;
      return new Response(`{"error":{"message":"injected ${FAIL_STATUS}"}}`, {
        status: FAIL_STATUS,
        headers: { "content-type": "application/json" },
      });
    }
    if (SILENT) {
      inFlight -= 1;
      // 200 + 一个事件都不发：核心应当按 DATA_IDLE 判超时，而不是永远挂着
      return new Response(new ReadableStream({ start() {} }), {
        headers: { "content-type": "text/event-stream" },
      });
    }

    const stream = new ReadableStream({
      async start(ctrl) {
        const enc = new TextEncoder();
        const push = (s: string) => ctrl.enqueue(enc.encode(s));
        try {
          if (FIRST_MS > 0) await sleep(FIRST_MS);
          push(frame({ role: "assistant" }));

          if (WITH_TOOL && !hasToolResult) {
            push(frame({ reasoning_content: "先看一眼环境。" }));
            push(
              frame({
                tool_calls: [
                  {
                    index: 0,
                    id: "call_slow",
                    type: "function",
                    function: {
                      name: "bash",
                      arguments: '{"command":"echo stress"}',
                    },
                  },
                ],
              }),
            );
            push(frame({}, { finish_reason: "tool_calls" }));
            push("data: [DONE]\n\n");
            return;
          }

          // 切成 CHUNKS 片，片片之间真等一等
          const per = Math.max(1, Math.ceil(BODY.length / CHUNKS));
          for (let i = 0; i < BODY.length; i += per) {
            push(frame({ content: BODY.slice(i, i + per) }));
            if (GAP_MS > 0) await sleep(GAP_MS);
          }
          push(
            `data: ${JSON.stringify({
              id: "chatcmpl-slow",
              object: "chat.completion.chunk",
              created: 0,
              model: "fixture-slow",
              choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
              usage: { prompt_tokens: 100, completion_tokens: 200, total_tokens: 300 },
            })}\n\n`,
          );
          push("data: [DONE]\n\n");
        } finally {
          inFlight -= 1;
          ctrl.close();
        }
      },
    });
    return new Response(stream, {
      headers: { "content-type": "text/event-stream", "cache-control": "no-cache" },
    });
  },
});

console.log(
  `压测假 LLM: http://127.0.0.1:${PORT}/v1  ` +
    `(first=${FIRST_MS}ms gap=${GAP_MS}ms chunks=${CHUNKS} tool=${WITH_TOOL} ` +
    `fail=${FAIL_RATE}@${FAIL_STATUS} silent=${SILENT})`,
);
// 每 5 秒报一次在飞请求数（判断是不是压住了）
setInterval(() => console.log(`[stats] 累计 ${totalReq}，在飞 ${inFlight}`), 5000);
