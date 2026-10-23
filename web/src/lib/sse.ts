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
  | { type: "start"; messageId: string }
  | { type: "start-step" }
  | { type: "finish-step" }
  | { type: "reset-step" }
  | { type: "finish" }
  | { type: "abort"; reason?: string }
  | { type: "error"; errorText: string }
  | { type: "text-start"; id: string }
  | { type: "text-delta"; id: string; delta: string }
  | { type: "text-end"; id: string }
  | { type: "reasoning-start"; id: string }
  | { type: "reasoning-delta"; id: string; delta: string }
  | { type: "reasoning-end"; id: string }
  | { type: "tool-input-start"; toolCallId: string; toolName: string }
  | { type: "tool-input-delta"; toolCallId: string; inputTextDelta: string }
  | {
      type: "tool-input-available";
      toolCallId: string;
      toolName: string;
      input: unknown;
    }
  | {
      type: "tool-output-available";
      toolCallId: string;
      output: string;
      /** 工具返回的图片（沙箱里的原图路径），只有读到图时才带 */
      images?: string[];
      /** 工具还在跑，这是阶段性结果（bash 的实时输出）。最终结果不带它 */
      preliminary?: boolean;
    }
  | { type: `data-${string}`; data: unknown };

const DONE = Symbol("done");

/**
 * 一条流最多能静默多久（ms）。
 *
 * 服务端每 15 秒一条 keep-alive 注释心跳（`sse_response` 的 `KeepAlive`，
 * 注释行对解析器是透明的）——所以「45 秒一个字节都没有」只可能是连接已经
 * 死了、而对端又没关连接（换 Wi-Fi、NAT 把空闲连接默默地丢了、手机从冻住
 * 里醒来）。
 *
 * 不盯这一点的话：`reader.read()` 会一直挂着，这边的回合就永远是「运行中」——
 * `loadMessages` 又认 `busy` 这个门，连切回前台的同步都进不来，界面就停在
 * 半截，只有刷新才看得见服务端那份完整的历史。
 *
 * 手机壳订阅回合状态那条流用的是同一个数、同一个理由
 * （`TurnStream.kt` 的 `READ_TIMEOUT_MS`）。
 */
const IDLE_TIMEOUT_MS = 45_000;

// ---------------------------------------------------------------------------
// SSE 帧解析
// ---------------------------------------------------------------------------

/** 解析单个 SSE 帧（不含结尾空行）。非 data 行忽略。 */
function parseFrame(raw: string): UiChunk | typeof DONE | null {
  const lines = raw.split("\n");
  const dataLines: string[] = [];

  for (const line of lines) {
    if (!line.startsWith("data:")) continue;
    // 规范：`data:` 后紧跟一个可选空格
    dataLines.push(line.slice(5).replace(/^ /, ""));
  }
  if (dataLines.length === 0) return null;

  const payload = dataLines.join("\n");
  if (payload === "[DONE]") return DONE;

  try {
    return JSON.parse(payload) as UiChunk;
  } catch {
    // 单帧坏掉不该拖垮整条流
    console.warn("[sse] 无法解析的帧:", payload.slice(0, 200));
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
  if (!res.body) throw new Error("响应没有 body");

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  const drain = function* (flush: boolean): Generator<UiChunk> {
    buf = buf.replace(/\r\n/g, "\n");
    for (;;) {
      const idx = buf.indexOf("\n\n");
      if (idx === -1) break;
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const parsed = parseFrame(frame);
      if (parsed === DONE) return;
      if (parsed) yield parsed;
    }
    if (flush && buf.trim()) {
      const parsed = parseFrame(buf);
      buf = "";
      if (parsed && parsed !== DONE) return yield parsed;
    }
  };

  for (;;) {
    // 静默超时：到点了就把读掐掉（`cancel` 让挂着的这次 read 以 done 收场）。
    // 生成器于是正常结束，调用方（`App` 的 `run`）会去拉一次服务端的历史 ——
    // 界面自己接上，最多晚 45 秒，不用人手刷新。
    const idle = setTimeout(() => void reader.cancel(), IDLE_TIMEOUT_MS);
    let done: boolean;
    let value: Uint8Array | undefined;
    try {
      ({ done, value } = await reader.read());
    } finally {
      clearTimeout(idle);
    }
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
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });

  if (!res.ok) {
    const text = await res.text().catch(() => "");
    throw new Error(
      `${res.status} ${res.statusText}${text ? `: ${text}` : ""}`,
    );
  }
  yield* parseUiStream(res);
}

// ---------------------------------------------------------------------------
// Reducer：chunk 流 → 可渲染的消息
// ---------------------------------------------------------------------------

/**
 * 一次 LLM 调用的 token 用量。**形状就是核心归一化之后的 `UsageView`**
 * （见 `core/src/openai.rs`）—— 互斥的桶 + 核心算好的命中率。
 *
 * `input` = 这一次请求发出去的整段上下文（整段历史 + 系统提示词 + 工具），
 * 所以它同时就是「现在上下文有多大」；`uncached_input` + `cache_read` +
 * `cache_write` = `input`，加起来不会重复。
 *
 * provider 的两套字段名、`cached_tokens: 0` 这些坑全在核心处理完了，
 * 前端只读这里的数，**不再自己解析**（以前两边各算一份，容易跑偏）。
 */
export type Usage = {
  input: number;
  uncached_input: number;
  cache_read: number;
  cache_write: number;
  output: number;
  reasoning: number;
  total: number;
  /** 命中率 0..1；provider 没报缓存字段是 null（和「报了 0」不是一回事） */
  cache_hit_ratio: number | null;
  /**
   * 记上这一次之后的**会话累计**（流里的 `data-usage` 才带；核心算好的，
   * 前端直接换上，不自己往上加）。形状同 `/api/session` 的 `usage`。
   */
  session?: (UsageTotals & { last?: Usage | null }) | null;
};

/** 本会话累计的用量（同一个互斥桶口径）。 */
export type UsageTotals = {
  input: number;
  uncached_input: number;
  cache_read: number;
  cache_write: number;
  output: number;
  reasoning: number;
  calls: number;
  /** 报了缓存字段的那些调用的输入 —— 累计命中率的分母（核心那边用） */
  cache_reported_input: number;
  /** 累计命中率 0..1（核心算好）；一次都没报过缓存是 null */
  cache_hit_ratio: number | null;
};

/**
 * 核心给的会话累计（`/api/session` 的 `usage`、`data-usage` 的 `session`）→
 * `UsageTotals`。只是补缺省值、去掉 `last`，**不算任何东西**。
 */
export function totalsOf(u: Partial<UsageTotals>): UsageTotals {
  return {
    input: u.input ?? 0,
    uncached_input: u.uncached_input ?? 0,
    cache_read: u.cache_read ?? 0,
    cache_write: u.cache_write ?? 0,
    output: u.output ?? 0,
    reasoning: u.reasoning ?? 0,
    calls: u.calls ?? 0,
    cache_reported_input: u.cache_reported_input ?? 0,
    cache_hit_ratio: u.cache_hit_ratio ?? null,
  };
}

/**
 * 一个会话的用量：最近一次调用 + 累计。输入框上那个圆环读它，
 * 形状和 `/api/session` 的 `usage` 字段一致（那边 `last` 可能缺）。
 */
export type SessionUsage = { last: Usage; total: UsageTotals };

/**
 * 用量来了就喊一声。
 *
 * 输入框上那个上下文圆环靠它实时更新。为什么不让 UI 自己去扫 parts：
 * `onUpdate` 是每个 delta 都调一次的，而 parts 里可能有几十条工具输出 ——
 * 每来一个字都扫一遍太贵（见 `summarize` 的注释）。
 * 由 `applyChunk` 直接报一声是 O(1) 的。
 */
const usageSinks = new Set<(u: Usage) => void>();
/** 返回退订函数（放进 `$effect` 的清理里正好） */
export function onUsage(fn: (u: Usage) => void): () => void {
  usageSinks.add(fn);
  return () => {
    usageSinks.delete(fn);
  };
}

/**
 * 服务端把排队的那几条用户消息**交给模型了**（`data-queued-delivered`）。
 *
 * UI 靠它把那几条从「排队中」（虚线圈 + ×）挪进正文 —— 交出去之后它们就在
 * 服务端的历史里了，不是「还没发出去」的东西了。
 *
 * 和 `onUsage` 同一个理由放在这里：一个个 delta 都往 UI 里推太吵，由 reducer
 * 直接报一声。
 */
const queuedSinks = new Set<(ids: string[]) => void>();
/** 返回退订函数（放进 `$effect` 的清理里正好） */
export function onQueuedDelivered(fn: (ids: string[]) => void): () => void {
  queuedSinks.add(fn);
  return () => {
    queuedSinks.delete(fn);
  };
}

/** 这条消息排进服务端的队列了（`POST /api/chat` 的全部回应就这一帧）。 */
export function isQueued(
  c: UiChunk | null,
): c is { type: "data-queued"; data: { id: string } } {
  return c?.type === "data-queued";
}

/**
 * 把已经读过的一帧塞回流的最前面。
 *
 * 发消息时要先看首帧才知道服务端是「开跑了」还是「排队了」（`send` 里那段），
 * 那帧不能丢 —— 后面的 reducer 还要吃它。
 */
export async function* prepend(
  first: UiChunk | null,
  rest: AsyncGenerator<UiChunk, void, undefined>,
): AsyncGenerator<UiChunk, void, undefined> {
  if (first) yield first;
  yield* rest;
}

export type ToolPart = {
  type: "tool";
  toolCallId: string;
  toolName: string;
  /** 流式到达的原始 JSON 文本 */
  inputText: string;
  input: unknown;
  state: "streaming" | "ready" | "output";
  /** 模型收到的结果原文（还在跑时是到目前为止的输出） */
  output?: string;
  /** 工具返回的图片（read 读到图）：沙箱里的原图路径 */
  images?: string[];
};

export type Part =
  | { type: "text"; id: string; text: string }
  | { type: "reasoning"; id: string; text: string }
  | ToolPart
  | { type: "data"; name: string; data: unknown }
  | { type: "error"; text: string }
  /** 用户在输入框里附的图片 / PDF。服务端 `session_view` 恢复历史时才有；
   *  刚发出去的那条是本地直接拼的（同一个形状）。 */
  | { type: "image"; id: string; url: string; name: string; mime: string }
  | { type: "file"; id: string; url: string; name: string; mime: string };

/** 已上传、还没发出去的附件（输入卡片里的一个小胶囊）。 */
export type Upload = {
  id: string;
  name: string;
  mime: string;
  kind: "image" | "pdf";
  /** 取回本体给缩略图/下载用的 URL */
  url: string;
};

export type Message = {
  id: string;
  role: "user" | "assistant";
  parts: Part[];
  /** 已完成一个 LLM 调用（finish-step）的次数 */
  steps: number;
  done: boolean;
};

export function newAssistantMessage(id: string): Message {
  return { id, role: "assistant", parts: [], steps: 0, done: false };
}

/**
 * 把一个 chunk 应用到消息上（原地修改）。
 *
 * 这是整个流式 UI 的核心：所有状态变更都收敛到这一个函数，
 * 不需要在组件里散落 if/else。
 */
export function applyChunk(msg: Message, c: UiChunk): void {
  const findText = (id: string) =>
    msg.parts.find((p) => p.type === "text" && p.id === id) as
      Extract<Part, { type: "text" }> | undefined;
  const findReasoning = (id: string) =>
    msg.parts.find((p) => p.type === "reasoning" && p.id === id) as
      Extract<Part, { type: "reasoning" }> | undefined;
  const findTool = (toolCallId: string) =>
    msg.parts.find((p) => p.type === "tool" && p.toolCallId === toolCallId) as
      ToolPart | undefined;

  switch (c.type) {
    case "text-start":
      if (!findText(c.id)) msg.parts.push({ type: "text", id: c.id, text: "" });
      break;
    case "text-delta": {
      let p = findText(c.id);
      if (!p) {
        p = { type: "text", id: c.id, text: "" };
        msg.parts.push(p);
      }
      p.text += c.delta;
      break;
    }
    case "text-end":
      break;

    case "reasoning-start":
      if (!findReasoning(c.id))
        msg.parts.push({ type: "reasoning", id: c.id, text: "" });
      break;
    case "reasoning-delta": {
      let p = findReasoning(c.id);
      if (!p) {
        p = { type: "reasoning", id: c.id, text: "" };
        msg.parts.push(p);
      }
      p.text += c.delta;
      break;
    }
    case "reasoning-end":
      break;

    case "tool-input-start":
      if (!findTool(c.toolCallId)) {
        msg.parts.push({
          type: "tool",
          toolCallId: c.toolCallId,
          toolName: c.toolName,
          inputText: "",
          input: undefined,
          state: "streaming",
        });
      }
      break;
    case "tool-input-delta": {
      const p = findTool(c.toolCallId);
      if (p) p.inputText += c.inputTextDelta;
      break;
    }
    case "tool-input-available": {
      let p = findTool(c.toolCallId);
      if (!p) {
        p = {
          type: "tool",
          toolCallId: c.toolCallId,
          toolName: c.toolName,
          inputText: "",
          input: undefined,
          state: "ready",
        };
        msg.parts.push(p);
      }
      p.toolName = c.toolName;
      p.input = c.input;
      p.state = "ready";
      break;
    }
    case "tool-output-available": {
      const p = findTool(c.toolCallId);
      if (p) {
        p.output = c.output;
        if (c.images) p.images = c.images;
        // 阶段性结果只刷新输出，状态留在"执行中"
        if (!c.preliminary) p.state = "output";
      }
      break;
    }
    case "start-step":
      msg.steps += 1;
      break;
    case "finish-step":
    case "start":
    case "reset-step":
      break;

    case "error":
      msg.parts.push({ type: "error", text: c.errorText });
      break;
    case "abort":
      msg.done = true;
      break;
    case "finish":
      msg.done = true;
      break;

    default: {
      if (c.type.startsWith("data-")) {
        msg.parts.push({
          type: "data",
          name: c.type.slice(5),
          data: (c as { data: unknown }).data,
        });
        // 用量单独报一声（圆环要实时的，不想让 UI 去扫 parts）
        if (c.type === "data-usage")
          for (const f of usageSinks) f(c.data as Usage);
        // 排队的消息交出去了：界面把那几条从「排队中」挪进正文
        if (c.type === "data-queued-delivered") {
          const ids = (c.data as { ids: string[] }).ids ?? [];
          for (const f of queuedSinks) f(ids);
        }
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
  /**
   * 流里又出现一个 `start` —— 服务端把排队的消息交给模型了，接下来的输出属于
   * **新的一条**助手消息（服务端的历史里也是新的一条：`session_view` 的合并
   * 规则里，User 会打断合并）。返回新消息接着写，`null` = 还是写这一条。
   *
   * 不给这个回调的调用方（脚本 / 测试）一直写同一条。
   */
  onRestart?: (prev: Message) => Message | null,
): Promise<Message> {
  let cur = msg;
  // 第一条 `start` 对应的消息调用方已经建好了（`beginStreaming`），不换
  let started = false;
  for await (const c of stream) {
    if (c.type === "start") {
      if (started && onRestart) {
        const next = onRestart(cur);
        if (next) {
          cur = next;
          continue;
        }
      }
      started = true;
      continue;
    }
    applyChunk(cur, c);
    onUpdate?.(cur);
  }
  cur.done = true;
  onUpdate?.(cur);
  return cur;
}
