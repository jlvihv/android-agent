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

/** 浏览器动作的中文名（折叠行和面板主行共用） */
export const BROWSER_ACTION: Record<string, string> = {
  navigate: '打开',
  get_backbone: '读结构',
  get_readable: '抽正文',
  get_text: '读文本',
  click: '点击',
  type: '输入',
  press: '按键',
  find_elements: '找元素',
  scroll: '滚动',
  screenshot: '截图',
  set_user_agent: '切UA',
  back: '后退',
  wait_for_dom_stable: '等页面安定',
  js: '执行 JS',
  new_tab: '开新标签',
  list_tabs: '列出标签',
  switch_tab: '切标签',
  close_tab: '关标签',
};

/** 浏览器面板主行：动作 + 它最主要的那个对象。返回用掉的参数名 */
function browserPrimary(a: Record<string, unknown>): { text: string; used: string[] } {
  const action = str(a.action);
  const label = BROWSER_ACTION[action] ?? action;
  const pick = (...keys: string[]) => {
    const k = keys.find((k) => a[k] !== undefined && a[k] !== '');
    return k ? { text: `${label} ${str(a[k]) || String(a[k])}`, used: ['action', k] } : null;
  };
  if (action === 'click' && a.selector === undefined && a.x !== undefined) {
    return { text: `${label} (${a.x}, ${a.y})`, used: ['action', 'x', 'y'] };
  }
  if (action === 'type') {
    return { text: `${label} ${str(a.selector)} ← ${str(a.text)}`, used: ['action', 'selector', 'text'] };
  }
  return (
    pick('url', 'script', 'selector', 'key', 'user_agent') ?? { text: label, used: ['action'] }
  );
}

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
    case 'search':
      // 老会话回放用：`search` 工具已删（唯一入口是沙箱里的 `agent search`）
      primary = take('query');
      break;
    case 'browser': {
      const b = browserPrimary(a);
      primary = b.text;
      b.used.forEach((k) => used.add(k));
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
