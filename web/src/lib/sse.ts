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
 *
 * **`[DONE]` 一帧就把流结束掉**，不再接着读。以前 `drain` 遇到它就 `return`，
 * 而上层那个 `for(;;)` 仍然继续 `reader.read()` —— 于是后面每来一帧都要等一次
 * 读才被丢掉，服务端要是留着连接（keep-alive 心跳、代理不转 FIN），这条流就
 * 一直挂着，`consume` 也就一直不返回（回合永远是「运行中」）。现在 `drain` 用
 * 返回值告诉上层「收了终止帧」，上层 `break` + `cancel()` 释放连接。
 */
export async function* parseUiStream(
  res: Response,
): AsyncGenerator<UiChunk, void, undefined> {
  if (!res.body) throw new Error("响应没有 body");

  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  /**
   * 还没消费的原始片（**不**每来一片就拼成一个大字符串）。
   *
   * 为什么是数组：一个帧可能很大（阶段性工具输出每 120ms 推 32 KB 尾部快照，
   * 实测见过 200 KB 的单帧），而网络分片能把它切成几百上千片。以前每片都
   * `buf += ...` —— 那是 O(n²) 的拷贝（200 KB / 3202 片实测 82 ms）。
   * 攒成数组、**只在真出现帧分隔符时 join 一次**，总拷贝量回到 O(n)。
   */
  let parts: string[] = [];
  /** `parts` 里已知含分隔符（或跨片边界构成分隔符）的第一片下标；-1 = 没有 */
  let sepAt = -1;

  const push = (s: string) => {
    if (!s) return;
    const i = parts.length;
    parts.push(s);
    if (sepAt !== -1) return;
    if (s.includes("\n\n")) sepAt = i;
    // 分隔符被切在片边界上（`\n` 在上一片末尾、`\n` 在这一片开头）
    else if (i > 0 && parts[i - 1].endsWith("\n") && s.startsWith("\n"))
      sepAt = i;
  };

  /** 把缓冲里完整的帧都吐出去；返回 true = 撞上终止帧（后面的一律不要了） */
  const drain = function* (flush: boolean): Generator<UiChunk, boolean> {
    if (sepAt === -1 && !flush) return false;

    let buf = parts.join("");
    parts = [];
    sepAt = -1;
    // 只在真出现 `\r` 时才归一化：绝大多数流（我们自己的核心就是）一个 `\r`
    // 都没有，而 `replace` 每次都要拷一遍整个 `buf`。
    if (buf.includes("\r")) buf = buf.replace(/\r\n/g, "\n");

    for (;;) {
      const idx = buf.indexOf("\n\n");
      if (idx === -1) break;
      const frame = buf.slice(0, idx);
      buf = buf.slice(idx + 2);
      const parsed = parseFrame(frame);
      if (parsed === DONE) return true;
      if (parsed) yield parsed;
    }
    if (!buf) return false;
    if (flush) {
      // 对端直接关了：没闭合的最后一帧也吐出来
      const parsed = buf.trim() ? parseFrame(buf) : null;
      if (parsed === DONE) return true;
      if (parsed) yield parsed;
      return false;
    }
    // 剩下的半帧留着，等下一片接上
    push(buf);
    return false;
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
    push(decoder.decode(value, { stream: true }));
    if (yield* drain(false)) {
      // 收到 `[DONE]`：剩下的字节不用读，连接也主动放掉
      void reader.cancel();
      return;
    }
  }
  push(decoder.decode());
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
  /**
   * 服务端算好的「这次调用失败了吗」。
   *
   * 首屏**不再传 output 正文**（它在界面上从不显示，只在面板里看），而工具行
   * 要不要标红就看这个。服务端按全文判定（开头 `错误：` 或末尾 `[exit N]`，N≠0），
   * 比前端拿截断的片段自己判可靠 —— 截断会把末尾的 `[exit 1]` 切掉。
   */
  failed?: boolean;
  /**
   * `output` 的总字符数。**有它就说明首屏把正文扣下了**（`output` 是空串），
   * 点开面板时要按 `toolCallId` 走 `/api/part` 取。
   * 面板里也用它显示「原文 N 字」。
   */
  chars?: number;
  /**
   * `input`（工具参数）也被截过。首屏只给一段预览 —— 实测 `bash` 的参数合计
   * 能到 57 KB，而屏幕上工具行只显示 `command` 的第一行。
   * 点开面板时按 `input=1` 走 `/api/part` 取完整 JSON。
   */
  inputTruncated?: boolean;
  /** 截断前 `input` 序列化后的总字符数 */
  inputChars?: number;
};

export type Part =
  | { type: "text"; id: string; text: string }
  | {
      type: "reasoning";
      id: string;
      text: string;
      /** 被截断过（首屏只给前 400 字）。面板打开时按 `id` 取全文 */
      truncated?: boolean;
      /** 截断前的总字符数 */
      chars?: number;
    }
  | ToolPart
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
  /**
   * 这条 user 消息其实是**子会话推回来的回执**（服务端 `ChatMessage::User.origin`
   * 是 `WorkerReport`）。有值就是那条子会话的 id。
   *
   * 服务端在历史里只能把它存成 `user`（协议要求，见 docs/16），但没有这个标记的话
   * 用户会在 Router 会话里看到一堆自己没说过的话、还显示成自己说的样子。
   */
  fromWorker?: string;
  /**
   * 这条 user 消息是**定时器**投进来的（服务端 `ChatMessage::User.origin` 是
   * `Schedule`）。有值就是那条定时任务的 id。
   *
   * 和 `fromWorker` 同一个理由：它也是 `user` 角色，但用户没说过 —— 不标出来就会在
   * Router 会话里显示成用户自己说的话。**排队中的那一条也带**（服务端 `queued_view`
   * 一起给），否则同一个东西在「排队中」和「交出去了」两个状态之间会换个样子。
   */
  fromSchedule?: string;
  /**
   * 流里的 `data-warning`（「输出被长度上限截断」「LLM 重试中」…）。
   *
   * **不是 part**，故意：`parts` 的下标要和**服务端**的 `view_parts_len` 对齐
   * （它只数 reasoning / text / tool）—— `session_patch` 的 `keepParts` 就是按
   * 那个数告诉客户端「前 k 个 part 留着、后面换掉」。往 `parts` 里塞一个服务端
   * 不数的 part，多步回合的下一步补丁就会把它（以及它后面的一切）挤掉：实测
   * 第一步的「输出被截断」警告在第二步到达时消失。
   *
   * 顺带也是内存：`data-usage` 每步一条、永不清理，塞进 `parts` 就是一个回合
   * 几十个只增不减的 part（每条还带着整份 `session` 累计）。
   */
  warnings?: string[];
  /**
   * 这条消息说完了没有。**两个用途别混**：
   *   - 助手消息：还在流式（`false`）还是结束了（`true`）
   *   - 用户消息：`false` = 还排在服务端队列里、没交给模型（虚线圈 + ×），
   *     `true` = 已经进正文了
   * 服务端 `/api/session` 给的排队项就是 `done: false`，所以不是本地约定。
   */
  done: boolean;
};

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
    // 步边界只影响服务端记账（一次 LLM 调用算一步），界面不画它 ——
    // 以前这里还维护 `msg.steps`，全项目没有一处读，删了。
    case "start-step":
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
      // `data-*` 是**事件**，不是消息内容 —— 不进 `parts`（理由见 `Message.warnings`）。
      // 消费方（`Live#onData`）通过 `consume` 的 `onChunk` 自己收。
      if (c.type === "data-warning") {
        const w = (c.data as { message?: string }).message;
        if (w) (msg.warnings ??= []).push(w);
      }
      break;
    }
  }
}

/**
 * 消费整条流。返回最终那条消息（调用方手上那个对象，或者最后一个新开的）。
 *
 * 以前还有一个 `onUpdate` 回调（每个 delta 调一次）：唯一的调用点传的是
 * `undefined`，测试也不用。消息是**原地**改的响应式对象，界面自己会更新 ——
 * 那个回调是个没人付钱的每帧函数调用，删了。
 */
export async function consume(
  msg: Message,
  stream: AsyncGenerator<UiChunk, void, undefined>,
  /**
   * 两个回调。**为什么是选项对象而不是两个位置参数**：`onRestart` 老早就有，
   * 再加一个 `onChunk` 就变成「第三个参数是对象、第二个是函数」那种谁都得回去
   * 数一遍的签名。
   */
  opts: {
    /**
     * 流里又出现一个 `start` —— 服务端把排队的消息交给模型了，接下来的输出属于
     * **新的一条**助手消息（服务端的历史里也是新的一条：`session_view` 的合并
     * 规则里，User 会打断合并）。返回新消息接着写，`null` = 还是写这一条。
     *
     * 不给这个回调的调用方（脚本 / 测试）一直写同一条。
     */
    onRestart?: (prev: Message) => Message | null;
    /**
     * 每一帧（`start` 之外）都叫一次，**在 `applyChunk` 之后**。
     *
     * 给的是「这条流属于哪条会话」的那个消费方（`lib/chat.svelte.ts`）——
     * 用量圆环和排队交付都要 sid，而这里正好知道。以前是 `lib/sse.ts` 里的两个
     * 全局 sink（`onUsage` / `onQueuedDelivered`）：**没有 sid**，两条会话同时
     * 跑的时候会把 A 的用量写到 B 的圆环上。
     */
    onChunk?: (c: UiChunk) => void;
  } = {},
): Promise<Message> {
  let cur = msg;
  // 第一条 `start` 对应的消息调用方已经建好了（`beginStreaming`），不换
  let started = false;
  for await (const c of stream) {
    if (c.type === "start") {
      if (started && opts.onRestart) {
        const next = opts.onRestart(cur);
        if (next) {
          cur = next;
          continue;
        }
      }
      started = true;
      continue;
    }
    applyChunk(cur, c);
    opts.onChunk?.(c);
  }
  cur.done = true;
  return cur;
}
