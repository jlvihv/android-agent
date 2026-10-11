/**
 * AI SDK UI Message Stream 解析器（决策 D7 + W2）。
 *
 * 我们采用 AI SDK 的**线上协议**，但不装它的 npm 包（W2），所以这里自己实现
 * 解析 + reducer。协议本身很简单，就是 SSE + JSON：
 *
 *   data: {"type":"text-delta","id":"t1","delta":"你"}
 *   data: {"type":"text-delta","id":"t1","delta":"好"}
 *   data: [DONE]
 *
 * 规范：https://ai-sdk.dev/docs/ai-sdk-ui/stream-protocol
 * 好处是以后想换回官方 `@ai-sdk/svelte` 客户端，Rust 侧一行都不用改。
 */

// ---------------------------------------------------------------------------
// 线上 chunk 类型
// ---------------------------------------------------------------------------

export type UiChunk =
  | { type: 'start'; messageId: string }
  | { type: 'start-step' }
  | { type: 'finish-step' }
  | { type: 'reset-step' }
  | { type: 'finish' }
  | { type: 'abort'; reason?: string }
  | { type: 'error'; errorText: string }
  | { type: 'text-start'; id: string }
  | { type: 'text-delta'; id: string; delta: string }
  | { type: 'text-end'; id: string }
  | { type: 'reasoning-start'; id: string }
  | { type: 'reasoning-delta'; id: string; delta: string }
  | { type: 'reasoning-end'; id: string }
  | { type: 'tool-input-start'; toolCallId: string; toolName: string }
  | { type: 'tool-input-delta'; toolCallId: string; inputTextDelta: string }
  | {
      type: 'tool-input-available';
      toolCallId: string;
      toolName: string;
      input: unknown;
    }
  | {
      type: 'tool-output-available';
      toolCallId: string;
      output: unknown;
      /** 工具还在跑，这是阶段性结果（bash 的实时输出）。最终结果不带它 */
      preliminary?: boolean;
    }
  | { type: `data-${string}`; data: unknown };

const DONE = Symbol('done');

// ---------------------------------------------------------------------------
// SSE 帧解析
// ---------------------------------------------------------------------------

/** 解析单个 SSE 帧（不含结尾空行）。非 data 行忽略。 */
function parseFrame(raw: string): UiChunk | typeof DONE | null {
  const lines = raw.split('\n');
  const dataLines: string[] = [];

  for (const line of lines) {
    if (!line.startsWith('data:')) continue;
    // 规范：`data:` 后紧跟一个可选空格
    dataLines.push(line.slice(5).replace(/^ /, ''));
  }
  if (dataLines.length === 0) return null;

  const payload = dataLines.join('\n');
  if (payload === '[DONE]') return DONE;

  try {
    return JSON.parse(payload) as UiChunk;
  } catch {
    // 单帧坏掉不该拖垮整条流
    console.warn('[sse] 无法解析的帧:', payload.slice(0, 200));
    return null;
  }
}

/**
 * 把 `Response` 的 body 变成 chunk 异步迭代器。
 *
 * 必须自己缓冲：网络分片会把一个 SSE 帧切在中间。
 */
export async function* parseUiStream(
  res: Response,
): AsyncGenerator<UiChunk, void, undefined> {
  if (!res.body) throw new Error('响应没有 body');

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';

  const drain = function* (flush: boolean): Generator<UiChunk> {
    buf = buf.replace(/\r\n/g, '\n');
    for (;;) {
      const idx = buf.indexOf('\n\n');
      if (idx === -1) break;
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const parsed = parseFrame(frame);
      if (parsed === DONE) return;
      if (parsed) yield parsed;
    }
    if (flush && buf.trim()) {
      const parsed = parseFrame(buf);
      buf = '';
      if (parsed && parsed !== DONE) return yield parsed;
    }
  };

  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    yield* drain(false);
  }
  buf += decoder.decode();
  yield* drain(true);
}

/** 从一个 URL 起流。带 `x-vercel-ai-ui-message-stream: v1` 是服务端的事。 */
export async function* streamFrom(
  url: string,
  body: unknown,
  signal?: AbortSignal,
): AsyncGenerator<UiChunk, void, undefined> {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
    signal,
  });

  if (!res.ok) {
    const text = await res.text().catch(() => '');
    throw new Error(`${res.status} ${res.statusText}${text ? `: ${text}` : ''}`);
  }
  yield* parseUiStream(res);
}

// ---------------------------------------------------------------------------
// Reducer：chunk 流 → 可渲染的消息
// ---------------------------------------------------------------------------

export type ToolPart = {
  type: 'tool';
  toolCallId: string;
  toolName: string;
  /** 流式到达的原始 JSON 文本 */
  inputText: string;
  input: unknown;
  state: 'streaming' | 'ready' | 'output';
  output?: unknown;
};

export type Part =
  | { type: 'text'; id: string; text: string }
  | { type: 'reasoning'; id: string; text: string }
  | ToolPart
  | { type: 'data'; name: string; data: unknown }
  | { type: 'error'; text: string };

export type Message = {
  id: string;
  role: 'user' | 'assistant';
  parts: Part[];
  /** 已完成一个 LLM 调用（finish-step）的次数 */
  steps: number;
  done: boolean;
};

export function newAssistantMessage(id: string): Message {
  return { id, role: 'assistant', parts: [], steps: 0, done: false };
}

/**
 * 把一个 chunk 应用到消息上（原地修改）。
 *
 * 这是整个流式 UI 的核心：所有状态变更都收敛到这一个函数，
 * 不需要在组件里散落 if/else。
 */
export function applyChunk(msg: Message, c: UiChunk): void {
  const findText = (id: string) =>
    msg.parts.find((p) => p.type === 'text' && p.id === id) as
      | Extract<Part, { type: 'text' }>
      | undefined;
  const findReasoning = (id: string) =>
    msg.parts.find((p) => p.type === 'reasoning' && p.id === id) as
      | Extract<Part, { type: 'reasoning' }>
      | undefined;
  const findTool = (toolCallId: string) =>
    msg.parts.find(
      (p) => p.type === 'tool' && p.toolCallId === toolCallId,
    ) as ToolPart | undefined;

  switch (c.type) {
    case 'text-start':
      if (!findText(c.id)) msg.parts.push({ type: 'text', id: c.id, text: '' });
      break;
    case 'text-delta': {
      let p = findText(c.id);
      if (!p) {
        p = { type: 'text', id: c.id, text: '' };
        msg.parts.push(p);
      }
      p.text += c.delta;
      break;
    }
    case 'text-end':
      break;

    case 'reasoning-start':
      if (!findReasoning(c.id))
        msg.parts.push({ type: 'reasoning', id: c.id, text: '' });
      break;
    case 'reasoning-delta': {
      let p = findReasoning(c.id);
      if (!p) {
        p = { type: 'reasoning', id: c.id, text: '' };
        msg.parts.push(p);
      }
      p.text += c.delta;
      break;
    }
    case 'reasoning-end':
      break;

    case 'tool-input-start':
      if (!findTool(c.toolCallId)) {
        msg.parts.push({
          type: 'tool',
          toolCallId: c.toolCallId,
          toolName: c.toolName,
          inputText: '',
          input: undefined,
          state: 'streaming',
        });
      }
      break;
    case 'tool-input-delta': {
      const p = findTool(c.toolCallId);
      if (p) p.inputText += c.inputTextDelta;
      break;
    }
    case 'tool-input-available': {
      let p = findTool(c.toolCallId);
      if (!p) {
        p = {
          type: 'tool',
          toolCallId: c.toolCallId,
          toolName: c.toolName,
          inputText: '',
          input: undefined,
          state: 'ready',
        };
        msg.parts.push(p);
      }
      p.toolName = c.toolName;
      p.input = c.input;
      p.state = 'ready';
      break;
    }
    case 'tool-output-available': {
      const p = findTool(c.toolCallId);
      if (p) {
        p.output = c.output;
        // 阶段性结果只刷新输出，状态留在"执行中"
        if (!c.preliminary) p.state = 'output';
      }
      break;
    }
    case 'start-step':
      msg.steps += 1;
      break;
    case 'finish-step':
    case 'start':
    case 'reset-step':
      break;

    case 'error':
      msg.parts.push({ type: 'error', text: c.errorText });
      break;
    case 'abort':
      msg.done = true;
      break;
    case 'finish':
      msg.done = true;
      break;

    default: {
      if (c.type.startsWith('data-')) {
        msg.parts.push({
          type: 'data',
          name: c.type.slice(5),
          data: (c as { data: unknown }).data,
        });
      }
      break;
    }
  }
}

/** 消费整条流并实时回调。返回最终消息。 */
export async function consume(
  msg: Message,
  stream: AsyncGenerator<UiChunk, void, undefined>,
  onUpdate?: (m: Message) => void,
): Promise<Message> {
  for await (const c of stream) {
    applyChunk(msg, c);
    onUpdate?.(msg);
  }
  msg.done = true;
  onUpdate?.(msg);
  return msg;
}
