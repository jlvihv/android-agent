/**
 * 工具调用面板（消息列表里点开工具那一行的底部面板）该显示什么。
 *
 * **原则：模型看到的，我们都看得到。** 工具结果是一个 JSON 对象，原样（序列化成
 * 字符串）进了模型的上下文；面板要是只挑几个字段显示，模型和人看到的就不一致了
 * —— 模型据以推理的东西，人在界面上找不到。所以：
 *
 *   1. 认识的字段各有合适的样式（正文 / diff / 错误 / 药丸 / 图片 / 链接）
 *   2. **剩下的字段一个不藏**，按 key: value 列在「其他字段」里 —— 以后工具多返回
 *      一个字段，不改前端它也会出现
 *   3. 最底下还有一份「模型收到的原始 JSON」（折叠着），逐字段对得上
 *
 * 参数同理：主行只放最关键的那个（命令 / 路径 / 网址），其余参数列在下面。
 *
 * 纯函数，不碰 DOM —— MessageList 只管照着画。
 */

import type { ToolPart } from './sse';

export type Tone = 'ok' | 'bad' | 'warn' | 'plain';
export type Chip = { text: string; tone: Tone };

/** 没有专门样式的字段：原样列出来。`block` = 多行 / 很长，用代码块显示 */
export type Field = { key: string; text: string; block: boolean };

export type InputView = {
  /** 主行：命令 / 路径 / 网址 …（可能为空） */
  primary: string;
  /** 主行之外的参数 */
  params: Field[];
};

export type ResultView = {
  label: string;
  failed: boolean;
  chips: Chip[];
  /** 正文（output / error / 兜底），没有就是 null */
  body: string | null;
  /** body 是 unified diff（按行染色） */
  diff: boolean;
  /** bash 输出被截断时的完整输出文件 */
  path: string | null;
  /** 工具返回的图片在沙箱里的路径（read 读图、浏览器截图） */
  image: string | null;
  /** 输出里挑出来的网址（bash 的 `urls`） */
  urls: string[];
  /** 剩下的字段 */
  fields: Field[];
  /** 模型收到的原始结果（排好版的 JSON）。还在跑的阶段性输出没有 —— 模型还没收到 */
  raw: string | null;
};

const str = (v: unknown) => (typeof v === 'string' ? v : '');

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

/** 人类可读的字节数，跟核心 reader::fmt_size 一致 */
export function fmtBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return '?';
  for (const [unit, div] of [
    ['GB', 1024 ** 3],
    ['MB', 1024 ** 2],
    ['KB', 1024],
  ] as const) {
    if (n >= div) return `${(n / div).toFixed(1)}${unit}`;
  }
  return `${Math.round(n)}B`;
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
      // 改成功了 diff 里已经是同一份改动，不必再列一遍旧文 / 新文
      const out = t.output as Record<string, unknown> | undefined;
      if (out && typeof out === 'object' && typeof out.diff === 'string') used.add('edits');
      break;
    }
    case 'search':
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

export function describeResult(t: ToolPart): ResultView | null {
  const o = t.output;
  if (o === undefined) return null;

  const empty: ResultView = {
    label: '输出',
    failed: false,
    chips: [],
    body: null,
    diff: false,
    path: null,
    image: null,
    urls: [],
    fields: [],
    raw: null,
  };

  if (typeof o !== 'object' || o === null || Array.isArray(o)) {
    const text = typeof o === 'string' ? o : JSON.stringify(o, null, 2);
    return { ...empty, body: text, raw: JSON.stringify(o, null, 2) };
  }

  const r = o as Record<string, unknown>;
  // 取一个字段：**类型对得上才算用掉**。形状不对的（比如 exit_code 给成了字符串）
  // 不会被吞，自然落进下面的「其他字段」
  const used = new Set<string>();
  const num = (k: string) => (typeof r[k] === 'number' ? (used.add(k), r[k] as number) : undefined);
  const text = (k: string) => (typeof r[k] === 'string' ? (used.add(k), r[k] as string) : undefined);
  const flag = (k: string) => (typeof r[k] === 'boolean' ? (used.add(k), r[k] as boolean) : undefined);
  /** 认识的字段给了 null = 「没有」，不单独列（原始 JSON 里照样看得到） */
  const none = (...ks: string[]) => ks.forEach((k) => r[k] === null && used.add(k));

  const chips: Chip[] = [];
  const chip = (t: string, tone: Tone = 'plain') => chips.push({ text: t, tone });

  // 还在跑：服务端推的阶段性输出，模型还没收到最终结果
  const running = flag('running') === true;
  if (running) chip('运行中', 'warn');

  // —— 状态 ——（false 的开关没有信息量，不占药丸；原始 JSON 里有）
  if (flag('ok') === false) chip('失败', 'bad');
  const exit = num('exit_code');
  if (exit !== undefined) chip(`exit ${exit}`, exit === 0 ? 'plain' : 'bad');
  if (flag('timed_out')) chip('超时被终止', 'bad');
  const sig = num('signal');
  if (sig !== undefined) chip(`信号 ${sig}`, 'bad');
  const wall = num('wall_time_seconds');
  if (wall !== undefined) chip(`${wall}s`);
  if (flag('truncated')) chip('已截断', 'warn');

  // —— read ——
  const lines = num('totalLines');
  const bytes = num('totalBytes');
  if (lines !== undefined) chip(`${lines} 行${bytes !== undefined ? ` · ${fmtBytes(bytes)}` : ''}`);
  else if (bytes !== undefined) chip(fmtBytes(bytes));
  const w = num('imageWidth');
  const h = num('imageHeight');
  if (w !== undefined && h !== undefined) chip(`${w}×${h}`);
  else {
    // 只有一边的话别吞
    if (w !== undefined) used.delete('imageWidth');
    if (h !== undefined) used.delete('imageHeight');
  }
  const mime = text('imageMime');
  if (mime) chip(mime);

  // —— edit / search ——
  const message = text('message');
  if (message) chip(message);
  const first = num('firstChangedLine');
  if (first !== undefined) chip(`第 ${first} 行起`);
  const count = num('count');
  if (count !== undefined) chip(`${count} 条`);
  const provider = text('provider');
  if (provider) chip(provider);

  // —— 正文：error > diff > output，只用掉真显示出来的那一个 ——
  let label = '输出';
  let failed = exit !== undefined && exit !== 0;
  let body: string | null = null;
  let diff = false;
  const err = text('error');
  if (err !== undefined) {
    label = '错误';
    failed = true;
    body = err;
  } else {
    const d = text('diff');
    if (d !== undefined) {
      label = '改动';
      body = d;
      diff = true;
    } else {
      body = text('output') ?? null;
    }
  }

  const image = text('imagePath') ?? null;
  const path = text('full_output_path') ?? null;
  none('signal', 'full_output_path');
  let urls: string[] = [];
  if (Array.isArray(r.urls) && r.urls.every((u) => typeof u === 'string')) {
    used.add('urls');
    urls = r.urls as string[];
  }

  return {
    label,
    failed,
    chips,
    body,
    diff,
    path,
    image,
    urls,
    fields: rest(r, used),
    raw: running ? null : JSON.stringify(o, null, 2),
  };
}
