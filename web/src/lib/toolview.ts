/**
 * 工具调用面板（消息列表里点开工具那一行的底部面板）该显示什么。
 *
 * **原则：模型看到什么，这里就显示什么。** 工具结果是一段纯文本（退出码、截断、
 * 续读位置都写在正文里，见 core 的 `ToolOutcome`），面板原样显示，不另拆字段。
 *
 * 参数：主行只放最关键的那个（命令 / 路径 / 网址），其余参数列在下面。
 *
 * 纯函数，不碰 DOM —— MessageList 只管照着画。
 */

import type { ToolPart } from './sse';

/** 没有专门样式的参数：原样列出来。`block` = 多行 / 很长，用代码块显示 */
export type Field = { key: string; text: string; block: boolean };

export type InputView = {
  /** 主行：命令 / 路径 / 网址 …（可能为空） */
  primary: string;
  /** 主行之外的参数 */
  params: Field[];
};

export type ResultView = {
  /** 模型收到的原文 */
  text: string;
  failed: boolean;
  /** 原文是 unified diff（edit 的结果）：按行染色 */
  diff: boolean;
  /** 工具返回的图片（沙箱里的原图路径；模型看到的是缩放后的那份） */
  images: string[];
};

const str = (v: unknown) => (typeof v === 'string' ? v : '');

/**
 * 这次工具调用是不是**失败**了 —— 只看正文里模型也看得见的信号：
 *   - 以 `错误：` 开头：核心出错时的唯一写法（read / bash / edit 都这样）
 *   - 末尾的 `[exit N]` 不是 0（超时被杀也是非 0）
 *
 * 折叠行染红和面板用**同一个**判定。
 */
export function failed(t: ToolPart): boolean {
  const text = t.output;
  if (text === undefined) return false;
  if (text.startsWith('错误：')) return true;
  const exit = /\[exit (-?\d+)\]\s*$/.exec(text);
  return exit !== null && exit[1] !== '0';
}

/** 一个值怎么显示：字符串原样，别的排成 JSON */
function field(key: string, v: unknown): Field {
  const text = typeof v === 'string' ? v : JSON.stringify(v, null, 2) ?? String(v);
  const block = typeof v === 'object' && v !== null ? true : text.includes('\n') || text.length > 80;
  return { key, text, block };
}

/** 没被消费掉的字段 → Field[]（保持原来的顺序） */
function rest(o: Record<string, unknown>, used: Set<string>): Field[] {
  return Object.entries(o)
    .filter(([k]) => !used.has(k))
    .map(([k, v]) => field(k, v));
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

/** edit 的 edits：每处一段，旧文 `-`、新文 `+`（比转义过的 JSON 好读得多） */
function formatEdits(edits: unknown): string {
  if (!Array.isArray(edits)) return JSON.stringify(edits, null, 2);
  return edits
    .map((e, i) => {
      const o = (e ?? {}) as Record<string, unknown>;
      const lines = (s: unknown, mark: string) =>
        str(s)
          .split('\n')
          .map((l) => `${mark} ${l}`)
          .join('\n');
      return `── 第 ${i + 1} 处 ──\n${lines(o.oldText, '-')}\n${lines(o.newText, '+')}`;
    })
    .join('\n');
}

export function describeInput(t: ToolPart, args: Record<string, unknown>): InputView {
  const a = args;
  const used = new Set<string>();
  let primary = '';
  const take = (k: string) => {
    used.add(k);
    return str(a[k]);
  };

  switch (t.toolName) {
    case 'read':
      primary = take('path');
      break;
    case 'bash':
      primary = take('command');
      break;
    case 'edit': {
      primary = take('path');
      // 改成功了，结果里的 diff 就是同一份改动，不必再列一遍旧文 / 新文
      if (t.output !== undefined && isDiff(t.output)) used.add('edits');
      break;
    }
  }

  const params = rest(a, used).map((f) =>
    f.key === 'edits' && t.toolName === 'edit'
      ? { key: f.key, text: formatEdits(a.edits), block: true }
      : f,
  );
  return { primary, params };
}

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

/** edit 成功时的结果就是一段 unified diff（`--- 路径` / `+++ 路径` 开头） */
function isDiff(text: string): boolean {
  return text.startsWith('--- ') && text.includes('\n+++ ');
}

export function describeResult(t: ToolPart): ResultView | null {
  if (t.output === undefined) return null;
  return { text: t.output, failed: failed(t), diff: isDiff(t.output), images: t.images ?? [] };
}
