/**
 * 会话窗口的**纯逻辑**：`/api/session` 的回包怎么套到手上那一窗上。
 *
 * 为什么要单独一个模块：这块的不变式最脆 —— 下标是**完整视图**的，而客户端手上
 * 只有尾巴那一窗，服务端是按 `from` 翻译过一次才发过来的；以前只靠手测（结果连着
 * 栽了两次）。抽出来就能用 `bun test` 盯住，不必上 DOM（见 `session.test.ts`）。
 *
 * 三条规则（对应服务端三种回包）：
 *
 *   - `unchanged`：什么都没变 —— 调用方自己处理，这里不管
 *   - `append`（补丁）：同一个会话的**当前回合**那一段，只换尾部
 *   - 整窗：首屏那一窗，或者服务端认为我们对不上了（`from` + `hasMore`）
 */

import type { Message } from './sse';

/**
 * 手上这一窗：`messages` 是完整视图里 `[windowFrom, windowFrom + len)` 那一段。
 *
 * **「更早的还有没有」不在这里** —— 它恒等于 `windowFrom > 0`，调用方自己推导
 * （`App` 的 `hasOlder`）。存成状态就会陈旧：`trimOlder` 一收窗口它就与事实不符，
 * 而「该往前要」正好看它（踩过：翻到底之后再往上滚，什么都不发生）。
 */
export type Window = { messages: Message[]; windowFrom: number };

/**
 * `/api/session` 的回包里我们在这里用得上的字段（排队、用量那些 App 自己处理）。
 *
 * 字段名和核心的 `session_body` 一一对应。
 */
export type SessionPayload = {
  messages?: Message[];
  /** 这一窗在**完整视图**里的起点 */
  from?: number;
  /** 更早的还有没有 */
  hasMore?: boolean;
  /** 是「只补当前回合那一段」的增量 */
  append?: boolean;
  /** 增量时从客户端数组的第几条开始换（服务端已按 `from` 翻译成本地下标） */
  replaceFrom?: number;
  /** 增量时 `messages[0]` 该接在本地那条 assistant 的前几个 part 后面 */
  keepParts?: number | null;
};

/** 补丁套不上的两种原因（调用方的处置不一样）。 */
export type PatchResult =
  | { ok: true; window: Window }
  /** 发请求之后窗口变过（用户往上翻了一页）：下标是按旧窗口翻译的，别硬套 */
  | { ok: false; why: 'stale' }
  /** 本地那条和服务端以为的对不上：老老实实重新要一份全量 */
  | { ok: false; why: 'mismatch' };

/**
 * 能复用的旧消息对象原样留着，Svelte 就不会重新渲染它们的 DOM。
 *
 * 服务端的会话是**只追加**的：一条消息 append 之后内容不再变。唯一的例外是最后一条
 * assistant —— 一个回合里它会把多步的 parts 合进来、越变越长。所以除了最后一条，
 * 能按 id 对上的消息直接复用；最后一条永远用新的。
 *
 * 不能比 `JSON.stringify(o) === JSON.stringify(m)`：轮询每 1.5 秒就要把整段历史
 * （包括每个 50KB 的 bash 输出）序列化两遍 —— 长任务轮询时这是最大的一笔 CPU 和 GC。
 * id 是位置标识，只追加的情况下对 id 就够了。
 */
export function reconcile(prev: Message[], next: Message[]): Message[] {
  // 切会话 / 清空 / 换了一条会话：不能复用
  if (next.length < prev.length) return next;
  if (prev.length > 0 && next.length > 0 && prev[0].id !== next[0].id) return next;

  const out = next.slice();
  const prevLast = prev.length - 1;
  for (let i = 0; i < Math.min(prev.length, next.length); i++) {
    // 只复用「上次就已经不是最后一条」的消息：上次的最后一条当时可能还在长
    if (i < prevLast && prev[i].id === next[i].id) out[i] = prev[i];
  }
  return out;
}

/**
 * 服务端这份「整窗」该从本地第几条接上（接不上 = -1）。
 *
 * 两边说的是同一套下标：`at` 是它在完整视图里的起点，而本地是从 `windowFrom`
 * 开始的，所以 `at - windowFrom` 就是它在本地数组里的位置。落在手上这段里还得
 * id 对得上才算（对不上说明这是另一条会话的内容 / 被重置过）；正好接在尾巴
 * 后面（`=== messages.length`）不用对 id，下标已经说明了它是接着的。
 */
export function spliceAt(prev: Message[], at: number, windowFrom: number, fresh: Message[]): number {
  if (!fresh.length) return -1;
  const i = at - windowFrom;
  if (i < 0 || i > prev.length) return -1;
  if (i === prev.length) return i;
  return prev[i].id === fresh[0].id ? i : -1;
}

/**
 * 补丁（同一个会话的当前回合）：只换尾部那一段。
 *
 * `base` 是**发请求时**手上的窗口起点。补丁里的下标是服务端按 `base` 翻译的，而
 * 请求在路上时用户可能往上翻了一页（数组前面多了一段）—— 那时下标就不作数了，
 * 硬套的结果是消息重复或丢几段。这种（极少见的）情况宁可作废，下一次同步会重算。
 */
export function applyPatch(w: Window, d: SessionPayload, base: number): PatchResult {
  if (base !== w.windowFrom) return { ok: false, why: 'stale' };

  const from = Math.min(d.replaceFrom ?? 0, w.messages.length);
  const fresh = d.messages ?? [];
  const keep = d.keepParts;
  if (keep != null) {
    const old = w.messages[from];
    if (!old || old.role !== 'assistant' || old.parts.length < keep || !fresh[0]) {
      return { ok: false, why: 'mismatch' };
    }
    fresh[0] = { ...fresh[0], parts: [...old.parts.slice(0, keep), ...fresh[0].parts] };
  }
  return { ok: true, window: { ...w, messages: [...w.messages.slice(0, from), ...fresh] } };
}

/**
 * 整窗（首屏那一窗 / 服务端认为我们对不上了）。
 *
 * 本地可能还留着更早的那些（用户往上翻过）：能对上就只换后面那一段，别把界面
 * 拽回尾部；对不上（`at` 落在手上这窗之外，说明中间有缝）才整份换。
 */
export function applyWindow(w: Window, d: SessionPayload, same: boolean): Window {
  const fresh = d.messages ?? [];
  const at = d.from ?? 0;
  const cut = same ? spliceAt(w.messages, at, w.windowFrom, fresh) : -1;
  if (cut >= 0) {
    return {
      messages: [...w.messages.slice(0, cut), ...reconcile(w.messages.slice(cut), fresh)],
      windowFrom: w.windowFrom,
    };
  }
  return {
    messages: same && at === 0 ? reconcile(w.messages, fresh) : fresh,
    windowFrom: at,
  };
}

/**
 * 往前翻一页（`?before=<windowFrom>`）：这一窗接在手上的前面。
 *
 * 接不上就整页丢掉（返回 null）：宁可少显示一窗，也不能在中间留个洞 —— 洞会让
 * 之后所有下标都对不上，`replaceFrom` 那套全乱。空页面也返回 null（调用方什么都不
 * 用做，下次再要）。
 */
export function prependPage(w: Window, d: SessionPayload): Window | null {
  const page = d.messages ?? [];
  if (!page.length) return null;
  const from = d.from ?? 0;
  if (from + page.length !== w.windowFrom) return null;
  return { messages: [...page, ...w.messages], windowFrom: from };
}

/**
 * 回到底部时把视口之外的旧页丢掉（内存封顶）。
 *
 * DOM 那层本来就只挂一窗（`MessageList` 的 `limit`），但**手里**这一窗是往上翻
 * 多少就留多少 —— 一页 10 条在真实会话里约 300 KB，翻十页就是 3 MB 常驻。所以
 * 回到底部（不再看更早的了）时收到 `keep` 条：刚好够再往上翻一两屏不用重新拉。
 *
 * 返回 null = 没到该收的时候（不用动）。注意收完 `windowFrom` 就 > 0 了 ——
 * 调用方那个推导出来的「还有更早的」会自动跟着变真（存成状态时这里正是踩过的坑）。
 */
export function trimOlder(w: Window, keep: number): Window | null {
  const over = w.messages.length - keep;
  if (over <= 0) return null;
  return {
    messages: w.messages.slice(over),
    // 起点跟着走：删掉的是**前面**那几条，下标必须对齐，不然补丁全错位
    windowFrom: w.windowFrom + over,
  };
}
