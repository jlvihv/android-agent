/**
 * `session.ts` 的测试：这几条规则的不变式（以前只靠手测 —— 连着栽了两次）。
 *
 *     bun test            # 或者 `just web-test`
 */

import { describe, expect, test } from 'bun:test';
import {
  applyPatch,
  applyWindow,
  prependPage,
  reconcile,
  spliceAt,
  trimOlder,
  type Window,
} from '../src/lib/session';
import type { Message, Part } from '../src/lib/sse';

/** 造一条消息：id 就是**完整视图里的下标**（和核心的 `u{i}` / `a{i}` 一个意思）。 */
function msg(i: number, parts?: Part[]): Message {
  const role = i % 2 === 0 ? 'user' : 'assistant';
  return {
    id: `${role === 'user' ? 'u' : 'a'}${i}`,
    role,
    steps: 1,
    done: true,
    parts: parts ?? [{ type: 'text', id: `t${i}`, text: `第 ${i} 条` }],
  };
}

/** 完整视图：n 条，下标即位置。 */
const full = (n: number): Message[] => Array.from({ length: n }, (_, i) => msg(i));

/**
 * 手上这一窗。注意**没有** `hasOlder` 字段：它恒等于 `windowFrom > 0`，由 `App`
 * 推导（存成状态就会陈旧 —— 收窗口之后正是这个坑，见 `lib/session.ts` 顶上那段）。
 */
const win = (messages: Message[], windowFrom: number): Window => ({ messages, windowFrom });

describe('reconcile', () => {
  test('按 id 复用，但**不复用最后一条**（那个回合可能还在长）', () => {
    const prev = full(4);
    const next = full(4).map((m) => ({ ...m }));
    const out = reconcile(prev, next);
    expect(out[0]).toBe(prev[0]); // 复用（同一个对象）
    expect(out[1]).toBe(prev[1]);
    expect(out[3]).not.toBe(prev[3]); // 上次的最后一条：不复用
  });

  test('变短了 / 第一条不是同一条：整份换（不能复用）', () => {
    const prev = full(6);
    expect(reconcile(prev, full(3))).toHaveLength(3);
    const other = full(6).map((m) => ({ ...m, id: `x${m.id}` }));
    expect(reconcile(prev, other)[0]).toBe(other[0]);
  });
});

describe('spliceAt', () => {
  test('落在手上这段里：位置对上就行', () => {
    const w = full(30).slice(10); // 手上是 [10, 30)
    expect(spliceAt(w, 20, 10, full(40).slice(20))).toBe(10);
  });

  test('正好接在尾巴后面：不需要对 id（下标已经说明是接着的）', () => {
    expect(spliceAt(full(30).slice(10), 30, 10, full(40).slice(30))).toBe(20);
  });

  test('落在这窗前面 / 后面（中间有缝）：接不上', () => {
    const w = full(30).slice(10);
    expect(spliceAt(w, 5, 10, full(40).slice(5))).toBe(-1);
    expect(spliceAt(w, 35, 10, full(40).slice(35))).toBe(-1);
  });

  test('位置对上但 id 对不上（换了一条会话）：接不上', () => {
    const w = full(30).slice(10);
    const other = full(40)
      .slice(20)
      .map((m) => ({ ...m, id: `x${m.id}` }));
    expect(spliceAt(w, 20, 10, other)).toBe(-1);
  });

  test('空的一份：没得接', () => {
    expect(spliceAt(full(30).slice(10), 20, 10, [])).toBe(-1);
  });
});

describe('applyPatch', () => {
  test('同一个回合的延续：前 keepParts 个 part 留着，接上新的', () => {
    const last = msg(29, [
      { type: 'reasoning', id: 'r29', text: '想' },
      { type: 'text', id: 't29', text: '先说一半' },
      { type: 'tool', toolCallId: 'c1', toolName: 'bash', inputText: '', input: {}, state: 'output', output: 'x' },
    ]);
    const w = win([...full(29), last], 0);
    const fresh = msg(29, [{ type: 'text', id: 't-new', text: '接着说' }]);
    const r = applyPatch(w, { append: true, replaceFrom: 29, keepParts: 2, messages: [fresh] }, 0);
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    const parts = r.window.messages[29].parts;
    expect(parts).toHaveLength(3); // 前 2 个留着 + 新的 1 个
    expect(parts[0].type).toBe('reasoning');
    expect(parts[1].type).toBe('text');
    expect(parts[2]).toMatchObject({ text: '接着说' });
    expect(r.window.messages).toHaveLength(30);
  });

  test('下标是**本地的**：服务端按 from 翻译过（这里再减一次就错了）', () => {
    // 手上是 [20, 30)，服务端说从本地第 9 条开始换
    const w = win(full(30).slice(20), 20);
    const r = applyPatch(w, { append: true, replaceFrom: 9, messages: [msg(29, [{ type: 'text', id: 'n', text: '新' }])] }, 20);
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.window.messages).toHaveLength(10); // 前 9 条留着 + 1 条新的
    expect(r.window.windowFrom).toBe(20); // 起点不动
  });

  test('请求在路上时窗口变过（用户往上翻了一页）：作废，不硬套', () => {
    const r = applyPatch(win(full(30).slice(10), 10), { append: true, replaceFrom: 5, messages: [msg(0)] }, 20);
    expect(r).toEqual({ ok: false, why: 'stale' });
  });

  test('本地那条对不上（不是 assistant / parts 太少）：要一份全量', () => {
    const w = win([...full(29), msg(28, [{ type: 'text', id: 'a', text: 'x' }])], 0);
    const r = applyPatch(w, { append: true, replaceFrom: 29, keepParts: 5, messages: [msg(29)] }, 0);
    expect(r).toEqual({ ok: false, why: 'mismatch' });

    const w2 = win(full(30), 0); // 第 29 条是 assistant 但只有 1 个 part
    const r2 = applyPatch(w2, { append: true, replaceFrom: 29, keepParts: 3, messages: [msg(29)] }, 0);
    expect(r2).toEqual({ ok: false, why: 'mismatch' });
  });

  test('普通追加（keepParts 为 null）：整段接在尾巴后面', () => {
    const w = win(full(30), 0);
    const r = applyPatch(w, { append: true, replaceFrom: 30, messages: [msg(30), msg(31)] }, 0);
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.window.messages).toHaveLength(32);
    expect(r.window.messages[31].id).toBe('a31');
  });
});

describe('applyWindow', () => {
  test('本地空的（首屏）：整份换上，起点用服务端的', () => {
    const w = applyWindow(win([], 0), { messages: full(40).slice(35), from: 35, hasMore: true }, false);
    expect(w.messages).toHaveLength(5);
    expect(w.windowFrom).toBe(35);
  });

  test('本地还有更早的、能接上：只换后面那段，起点不动', () => {
    // 手上是 [20, 40)，服务端发来尾部那一窗 [90, 100) —— 中间有洞，接不上
    const w = applyWindow(win(full(40).slice(20), 20), { messages: full(100).slice(90), from: 90, hasMore: true }, true);
    expect(w.windowFrom).toBe(90); // 整份换
    // 能接上的（服务端这窗正好从手上某条开始）
    const w2 = applyWindow(win(full(40).slice(20), 20), { messages: full(100).slice(30), from: 30, hasMore: true }, true);
    expect(w2.windowFrom).toBe(20); // 起点不动
    expect(w2.messages).toHaveLength(80); // 手上前 10 条（下标 20..29）+ 服务端那 70 条
    expect(w2.messages[0].id).toBe('u20');
    expect(w2.messages[79].id).toBe('a99');
  });

  test('不是同一个会话 / 首窗从 0 开始：整份换上', () => {
    const w = applyWindow(win(full(20).slice(0), 0), { messages: full(20), from: 0 }, true);
    expect(w.messages).toHaveLength(20);
    expect(w.windowFrom).toBe(0);
  });
});

describe('prependPage', () => {
  test('接得上：放到前面，起点前移', () => {
    const w = prependPage(win(full(40).slice(30), 30), { messages: full(40).slice(20, 30), from: 20, hasMore: true });
    expect(w?.windowFrom).toBe(20);
    expect(w?.messages).toHaveLength(20);
    expect(w?.messages[0].id).toBe('u20');
  });

  test('接不上（有缝）：整页丢掉 —— 宁可少一窗，也不能留洞', () => {
    expect(prependPage(win(full(40).slice(30), 30), { messages: full(40).slice(10, 20), from: 10 })).toBeNull();
  });

  test('空页面：返回 null（调用方只管把 hasMore 记下来）', () => {
    expect(prependPage(win(full(40).slice(30), 30), { messages: [], from: 30, hasMore: false })).toBeNull();
  });
});

describe('trimOlder', () => {
  test('回到底部时收回到 keep 条，起点跟着走', () => {
    const w = trimOlder(win(full(100).slice(20), 20), 30);
    expect(w?.messages).toHaveLength(30);
    // 留下的是【完整视图】的最后 30 条（70..99），起点就跟到 70
    expect(w?.windowFrom).toBe(70);
    expect(w?.messages[0].id).toBe('u70');
  });

  test('没到该收的时候：不动', () => {
    expect(trimOlder(win(full(100).slice(80), 80), 30)).toBeNull();
  });
});

// —— 不变式（比样例更值钱的那几条）——
//
// 一页页往前翻、中途还夹着当前回合的补丁：手上的内容必须**始终**等于完整视图的
// 一段（起点 = windowFrom），而且每一步的起点都单调往前。这条盯住的是「下标对齐」
// 这个最容易错的东西。
describe('不变式：翻页 + 补丁之后手上始终是完整视图的一段', () => {
  test('翻到底', () => {
    const all = full(45);
    let w = win(all.slice(35), 35); // 首屏：尾部那一窗

    let guard = 0;
    while (w.windowFrom > 0) {
      expect(++guard).toBeLessThan(20);
      const from = Math.max(0, w.windowFrom - 10);
      const page = prependPage(w, { messages: all.slice(from, w.windowFrom), from, hasMore: from > 0 });
      expect(page).not.toBeNull();
      w = page!;
      expect(w.messages).toEqual(all.slice(w.windowFrom));
    }
    expect(w.windowFrom).toBe(0);
    expect(w.messages).toEqual(all);
  });

  test('翻到一半时来一个补丁（当前回合长了一步）', () => {
    const all = full(44); // 最后一条（43，奇数）是 assistant —— 当前回合就在它上面长
    let w = win(all.slice(34), 34);
    w = prependPage(w, { messages: all.slice(24, 34), from: 24, hasMore: true })!;
    expect(w.messages).toEqual(all.slice(24));

    // 协议：`messages[0]` 只装**新**的 part，`keepParts` 说是「前面留着几个」
    const merged = msg(43, [
      { type: 'text', id: 't43', text: '第 43 条' },
      { type: 'text', id: 't43b', text: '又写了一句' },
    ]);
    const fresh = msg(43, [{ type: 'text', id: 't43b', text: '又写了一句' }]);
    // 本地下标 19 = 完整视图的 43（起点 24 + 19）
    const r = applyPatch(w, { append: true, replaceFrom: 19, keepParts: 1, messages: [fresh] }, 24);
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    w = r.window;
    const expectFull = all.slice(24).map((m) => (m.id === 'a43' ? merged : m));
    expect(w.messages).toEqual(expectFull);
    expect(w.windowFrom).toBe(24);
  });

  test('收窗口之后仍然是完整视图的一段（下标没漂）', () => {
    const all = full(60);
    let w = win(all.slice(10), 10);
    w = trimOlder(w, 30)!;
    expect(w.messages).toEqual(all.slice(w.windowFrom));
    // 收完还能接着往前翻
    const page = prependPage(w, { messages: all.slice(w.windowFrom - 10, w.windowFrom), from: w.windowFrom - 10, hasMore: true });
    expect(page?.messages).toEqual(all.slice(w.windowFrom - 10));
  });
});
