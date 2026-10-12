#!/usr/bin/env bun
/**
 * 假 LLM：一个 OpenAI **Chat Completions** 兼容的本地服务。
 *
 * 为什么需要它：界面上很多结构只在**有内容**时才出现（思考行、工具行、Markdown 的
 * 列表 / 嵌套 / 任务项 / 表格 / 引用 / 代码块）。拿真机上的会话去审，内容不可控 ——
 * 想审任务项就得先凑出一个任务项；而且那条路要 adb、要真机、每轮几十秒。
 * 内容自己造反而更全、可复现、不花 token、不依赖网络。
 *
 * 两阶段回复（走的是**真实 agent loop**，所以会落库、界面留得住）：
 *
 *   1. 第一次请求（还没有工具结果）：流一段思考 + 发一个真的 `bash` 工具调用
 *      → 核心真的去沙箱里执行它，界面因此有「思考行 + 工具行 + 工具面板」
 *   2. 第二次请求（请求里带了 role=tool 的结果）：流一段 Markdown 正文
 *
 * ⚠️ 别指望用核心自带的脚本化演示流（`POST /api/chat {"demo":true}`）去审界面：
 * 那条路**不落库**，而前端在流结束后会用服务端的会话视图覆盖本地消息
 * （`loadMessages`）—— 演示内容会被立刻冲掉，界面回到空态。它只验协议链路。
 *
 * 用法（`just dom-audit` 会自动起；单独起也行）：
 *
 *     bun scripts/fixture-llm.ts                       # 监听 8790
 *     FIXTURE_MD=./my.md bun scripts/fixture-llm.ts    # 换一份 Markdown
 *     FIXTURE_NO_TOOL=1 bun scripts/fixture-llm.ts     # 不发工具调用，只回正文
 */
import { readFileSync } from 'node:fs';

const PORT = Number(process.env.FIXTURE_PORT ?? 8790);
const MODELS = (process.env.FIXTURE_MODELS ?? 'fixture,fixture-mini').split(',');
const WITH_TOOL = !process.env.FIXTURE_NO_TOOL;

/** 默认夹具：把 Markdown 渲染器里每一种块级结构都过一遍。 */
const DEFAULT_MD = `# 假模型回复

这是一个**受控内容**夹具：不调用任何真实 LLM，用来在浏览器里审 DOM。

## 无序列表

- 第一项
- 第二项，带 \`inline code\`
- 第三项
  - 嵌套一项
  - 嵌套二项

## 有序列表

1. 规则①：先看环境
2. 规则②：再决定跑什么
3. 规则③：最后收尾

## 任务列表

- [x] 已完成的项
- [ ] 未完成的项

## 表格

| 列 A | 列 B |
| --- | --- |
| 1 | 2 |
| 3 | 4 |

> 引用块里也放一段话。
>
> 引用里再来一段。

\`\`\`python
print("代码块")
\`\`\`

---

普通段落结尾，带 *斜体*、**粗体**、~~删除线~~ 和 [外链](https://example.com)。
`;

const REPLY = process.env.FIXTURE_MD ? readFileSync(process.env.FIXTURE_MD, 'utf8') : DEFAULT_MD;
const REASONING = ['用户想让我渲染夹具。', '先跑个命令看看环境，', '再把内容写出来。'];
const TOOL = { name: 'bash', arguments: JSON.stringify({ command: 'echo fixture-tool-ok && uname -m', timeout: 10 }) };

const CHUNK = 24;
const frame = (delta: unknown, extra: Record<string, unknown> = {}) =>
  `data: ${JSON.stringify({
    id: 'chatcmpl-fixture',
    object: 'chat.completion.chunk',
    created: 0,
    model: MODELS[0],
    choices: [{ index: 0, delta, ...extra }],
  })}\n\n`;

/** 第一阶段：思考 + 一个工具调用。 */
function toolTurn(): string {
  const out = [frame({ role: 'assistant' })];
  for (const piece of REASONING) out.push(frame({ reasoning_content: piece }));
  out.push(frame({ tool_calls: [{ index: 0, id: 'call_fixture', type: 'function', function: TOOL }] }));
  out.push(frame({}, { finish_reason: 'tool_calls' }));
  out.push('data: [DONE]\n\n');
  return out.join('');
}

/** 第二阶段：正文（Markdown 夹具）。 */
function textTurn(): string {
  const out = [frame({ role: 'assistant' })];
  for (let i = 0; i < REPLY.length; i += CHUNK) out.push(frame({ content: REPLY.slice(i, i + CHUNK) }));
  out.push(
    `data: ${JSON.stringify({
      id: 'chatcmpl-fixture',
      object: 'chat.completion.chunk',
      created: 0,
      model: MODELS[0],
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    })}\n\n`,
  );
  out.push('data: [DONE]\n\n');
  return out.join('');
}

Bun.serve({
  port: PORT,
  hostname: '127.0.0.1',
  async fetch(req) {
    const { pathname } = new URL(req.url);
    if (pathname.endsWith('/models')) {
      return Response.json({ object: 'list', data: MODELS.map((id) => ({ id, object: 'model' })) });
    }
    if (!pathname.endsWith('/chat/completions')) {
      return new Response('假 LLM 只有 /v1/models 和 /v1/chat/completions', { status: 404 });
    }
    const body = (await req.json()) as { stream?: boolean; messages?: { role?: string }[] };
    // 请求里已经有工具结果 → 这是第二轮，该出正文了
    const hasToolResult = (body.messages ?? []).some((m) => m.role === 'tool');
    const turn = hasToolResult || !WITH_TOOL ? textTurn() : toolTurn();
    if (!body.stream) {
      return Response.json({
        id: 'chatcmpl-fixture',
        object: 'chat.completion',
        choices: [
          {
            index: 0,
            message: hasToolResult
              ? { role: 'assistant', content: REPLY }
              : { role: 'assistant', content: '', tool_calls: [{ id: 'call_fixture', type: 'function', function: TOOL }] },
            finish_reason: hasToolResult ? 'stop' : 'tool_calls',
          },
        ],
      });
    }
    return new Response(turn, { headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' } });
  },
});

console.log(
  `假 LLM: http://127.0.0.1:${PORT}/v1（模型 ${MODELS.join(' / ')}，` +
    `${WITH_TOOL ? '第一轮发 bash 工具调用、第二轮回' : '直接回'} ${REPLY.length} 字符 Markdown）`,
);
