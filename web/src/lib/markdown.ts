/**
 * 极简 Markdown 解析。
 *
 * 故意不引 marked / markdown-it —— 依赖越少越好，而 LLM 实际吐出来的
 * 就那几种结构。解析成结构化的块交给 Svelte 渲染，**不用 `{@html}`**：
 * 既没有 XSS 面，也能让 Tailwind 直接管样式。
 *
 * 块级：段落、标题、分隔线、围栏代码块、表格、引用（可嵌套）、
 *       有序/无序/任务列表（可嵌套、可续行、项之间可空行）
 * 行内：粗体、斜体、粗斜体、删除线、行内代码、链接、图片、裸链接、转义
 *
 * 几个刻意的行为：
 *   - **没闭合的围栏也当代码块收**。流式输出时经常只有前半截，这时候
 *     不该把 ``` 当普通文字闪一下。
 *   - 有序列表项之间空一行仍是同一个列表，编号接着数。LLM 很爱这样写，
 *     按严格 CommonMark 拆开的话每一项都是"1."。
 *   - `snake_case` 里的下划线不算斜体。
 *   - 链接只放行 http/https/mailto/preview，其余（javascript: 之类）当普通文字。
 *   - `preview:/root/x.html` 指向沙箱里的文件，渲染时改写成 `/sandbox/root/x.html`
 *     （漏写 `preview:` 的裸绝对路径 `/root/x.png` 也按它算，见 `safeHref`）
 *     （服务端按路径直接映射），图片同理。
 */

import { store } from './store.svelte.ts';
import { isLoopbackHost } from './host';

export type Span =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'strong'; c: Span[] }
  | { t: 'em'; c: Span[] }
  | { t: 'del'; c: Span[] }
  | { t: 'link'; href: string; c: Span[] }
  | { t: 'img'; src: string; alt: string };

export type Align = 'left' | 'center' | 'right' | null;

export type Item = { task: boolean | null; blocks: Block[] };

export type Block =
  | { t: 'p'; spans: Span[] }
  | { t: 'h'; level: number; spans: Span[] }
  | { t: 'hr' }
  | { t: 'code'; lang: string; v: string }
  | { t: 'table'; align: Align[]; head: Span[][]; rows: Span[][][] }
  | { t: 'quote'; blocks: Block[] }
  | { t: 'list'; ordered: boolean; start: number; tight: boolean; items: Item[] };

// ---------------------------------------------------------------------------
// 块级
// ---------------------------------------------------------------------------

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^`\s]*)/;
const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const HR = /^ {0,3}([-*_])(?:\s*\1){2,}\s*$/;
const QUOTE = /^ {0,3}>\s?/;
const ITEM = /^(\s*)([-*+]|(\d{1,9})[.)])(\s+|$)/;
const TABLE_DELIM = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

const indentOf = (l: string) => l.match(/^\s*/)![0].replace(/\t/g, '    ').length;
const blank = (l: string) => !l.trim();

function isTableStart(lines: string[], i: number): boolean {
  return (
    i + 1 < lines.length &&
    lines[i].includes('|') &&
    TABLE_DELIM.test(lines[i + 1]) &&
    lines[i + 1].includes('-')
  );
}

function isBlockStart(lines: string[], i: number): boolean {
  const l = lines[i];
  return (
    FENCE.test(l) ||
    HEADING.test(l) ||
    HR.test(l) ||
    QUOTE.test(l) ||
    ITEM.test(l) ||
    isTableStart(lines, i)
  );
}

/** 去掉至多 n 个前导空格 */
function dedent(l: string, n: number): string {
  let k = 0;
  while (k < n && k < l.length && l[k] === ' ') k++;
  return l.slice(k);
}

/** 表格的一行 → 单元格。`\|` 是单元格里的竖线 */
function cells(row: string): string[] {
  let s = row.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  const out: string[] = [];
  let cur = '';
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '\\' && s[i + 1] === '|') {
      cur += '|';
      i++;
    } else if (s[i] === '|') {
      out.push(cur.trim());
      cur = '';
    } else cur += s[i];
  }
  out.push(cur.trim());
  return out;
}

export function parse(src: string): Block[] {
  return blocks(src.replace(/\r\n?/g, '\n').split('\n'));
}

function blocks(lines: string[]): Block[] {
  const out: Block[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i];

    if (blank(line)) {
      i++;
      continue;
    }

    // 围栏代码块。没闭合就收到结尾
    const fence = FENCE.exec(line);
    if (fence) {
      const mark = fence[1];
      const pad = indentOf(line);
      const body: string[] = [];
      i++;
      while (i < lines.length) {
        const l = lines[i];
        if (l.trim().startsWith(mark[0].repeat(mark.length)) && !l.trim().slice(mark.length).trim()) {
          i++;
          break;
        }
        body.push(dedent(l, pad));
        i++;
      }
      out.push({ t: 'code', lang: fence[2], v: body.join('\n') });
      continue;
    }

    const h = HEADING.exec(line);
    if (h) {
      out.push({ t: 'h', level: h[1].length, spans: inline(h[2]) });
      i++;
      continue;
    }

    // 分隔线要在列表之前判断：`* * *` 和 `- - -` 也能匹配列表
    if (HR.test(line)) {
      out.push({ t: 'hr' });
      i++;
      continue;
    }

    if (isTableStart(lines, i)) {
      const head = cells(line);
      const align: Align[] = cells(lines[i + 1]).map((c) => {
        const l = c.startsWith(':');
        const r = c.endsWith(':');
        return l && r ? 'center' : r ? 'right' : l ? 'left' : null;
      });
      i += 2;
      const rows: Span[][][] = [];
      while (i < lines.length && !blank(lines[i]) && lines[i].includes('|')) {
        const c = cells(lines[i]);
        // 列数对齐表头：少的补空，多的丢掉
        rows.push(head.map((_, k) => inline(c[k] ?? '')));
        i++;
      }
      out.push({ t: 'table', align, head: head.map((c) => inline(c)), rows });
      continue;
    }

    // 引用：连续的 > 行（中间可以有"懒"续行）收成一块，内部递归解析
    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length && !blank(lines[i])) {
        if (QUOTE.test(lines[i])) inner.push(lines[i].replace(QUOTE, ''));
        else if (isBlockStart(lines, i)) break;
        else inner.push(lines[i]);
        i++;
      }
      out.push({ t: 'quote', blocks: blocks(inner) });
      continue;
    }

    const m = ITEM.exec(line);
    if (m) {
      i = list(lines, i, out);
      continue;
    }

    // 段落。走到这里说明当前行不是块起始，所以至少收一行，不会死循环。
    const para: string[] = [];
    while (i < lines.length && !blank(lines[i]) && (para.length === 0 || !isBlockStart(lines, i))) {
      para.push(lines[i].trim());
      i++;
    }
    out.push({ t: 'p', spans: inline(para.join('\n')) });
  }

  return out;
}

/** 从第 i 行开始收一个列表，推进 out，返回下一个未消费的行号 */
function list(lines: string[], i: number, out: Block[]): number {
  const first = ITEM.exec(lines[i])!;
  const base = indentOf(lines[i]);
  const ordered = first[3] !== undefined;
  const start = ordered ? parseInt(first[3], 10) : 1;
  const items: Item[] = [];
  let tight = true;

  while (i < lines.length) {
    const m = ITEM.exec(lines[i]);
    if (!m || indentOf(lines[i]) !== base || (m[3] !== undefined) !== ordered) break;

    // 内容缩进：标记之后的位置。续行和子列表要缩进到至少这里（宽松一点：base+2）
    const content = Math.min(base + m[0].length, base + 4);
    const body: string[] = [lines[i].slice(m[0].length)];
    i++;

    let sawBlank = false;
    while (i < lines.length) {
      const l = lines[i];
      if (blank(l)) {
        sawBlank = true;
        body.push('');
        i++;
        continue;
      }
      const ind = indentOf(l);
      if (ind >= base + 2) {
        // 缩进的行属于这一项（续行或子块）
        body.push(dedent(l, content));
        sawBlank = false;
        i++;
        continue;
      }
      if (!sawBlank && ind <= base && !isBlockStart(lines, i)) {
        // 懒续行：紧跟着的、没缩进的普通文字
        body.push(l.trim());
        i++;
        continue;
      }
      break;
    }

    // 尾部空行：如果后面紧接着同级的下一项，这个列表是"松"的，但仍是同一个列表
    while (body.length && blank(body[body.length - 1])) body.pop();
    if (sawBlank) {
      const next = ITEM.exec(lines[i] ?? '');
      if (next && indentOf(lines[i]) === base && (next[3] !== undefined) === ordered) tight = false;
    }

    let task: boolean | null = null;
    const tm = /^\[([ xX])\]\s+/.exec(body[0]);
    if (tm) {
      task = tm[1] !== ' ';
      body[0] = body[0].slice(tm[0].length);
    }
    if (body.some((b, k) => k > 0 && blank(b))) tight = false;
    items.push({ task, blocks: blocks(body) });

    // 项之间的空行已经被上面的内层循环吃掉了，这里直接看下一行
  }

  out.push({ t: 'list', ordered, start, tight, items });
  return i;
}

// ---------------------------------------------------------------------------
// 行内
// ---------------------------------------------------------------------------

const PUNCT = /[!-/:-@[-`{-~]/;
const WORD = /[\p{L}\p{N}]/u;
const URL_RE = /^https?:\/\/[^\s<>()（）「」，。；：！？、]+/;

/** 只放行这些协议。javascript: 等一律当文字 */
export function safeHref(href: string): string | null {
  const h = href.trim();
  // 这几个不是「打开一个地址」，是拿页面的权限执行/读东西 —— 一律不渲染成链接
  if (/^(javascript|data|file|content|blob|about):/i.test(h)) return null;
  // 裸的绝对路径（`![](/Users/x/shot.jpg)`）就是沙箱里的文件：模型常漏写
  // `preview:`，尤其是桌面宿主模式下截图不在 /workspace 里的时候 —— 不兜的话
  // 图片直接消失，只剩 alt。这个 App 里没有别的「站内相对路径」会出现在回复里。
  // `//host/…` 是协议相对的网址，不算。
  if (/^\/(?!\/)/.test(h)) return `preview:${h}`;
  // 其余都放行：**用户点一个链接是明确的动作**，App 自定义 scheme
  // （`weixin://`、`androidamap://`、`tel:`…）就该交给系统去挑对应的 App。
  // 这条也是「模型把网址转述在回复里、用户自己点开」的前提 ——
  // 沙箱里没有浏览器，我们也不会替用户打开任何东西。
  return /^[a-z][a-z0-9+.-]*:/i.test(h) ? h : null;
}

/** 按扩展名判断媒体类型（图片语法 `![]()` 也用来嵌音视频） */
export function mediaKind(url: string): 'image' | 'audio' | 'video' {
  const ext = url.split(/[?#]/)[0].split('.').pop()?.toLowerCase() ?? '';
  if (['mp4', 'webm', 'mov', 'm4v', 'ogv', 'mkv'].includes(ext)) return 'video';
  if (['mp3', 'm4a', 'aac', 'wav', 'ogg', 'oga', 'opus', 'flac'].includes(ext)) return 'audio';
  return 'image';
}

/**
 * 沙箱里的绝对路径 → 服务端的 URL（`/sandbox/<钥匙>/…`，见 `store.sandboxKey`）。
 *
 * 在模板里调它会跟踪 `store.sandboxKey`：钥匙到了，链接自己会更新。
 */
export function sandboxPath(guestPath: string): string {
  return `/sandbox/${store.sandboxKey}${encodeURI(guestPath)}`;
}

/**
 * 沙箱网页要在**另一个源**上打开：`sandbox.localhost:<同一个端口>`。
 *
 * 和 App（`127.0.0.1`）同源的话，页面里的脚本能带着 App 的凭据调 `/api`；
 * 所以同源那份被服务端关进 opaque origin，localStorage 之类全不能用。换个源
 * 就两头都好：页面有自己正常的源（存储能用），又碰不到 App 的凭据。见 core
 * `http.rs` 的 `SANDBOX_HOST`。
 *
 * 只给**网页**用（查看器的 iframe）。图片 / 音视频 / 文本是 App 自己取的，
 * 留在同源（拉文本要读响应，跨源还得配 CORS）。
 */
export function sandboxPageUrl(url: string): string {
  if (!url.startsWith('/sandbox/')) return url;
  // 局域网里用手机浏览器开电脑上的核心（`just dev-host`）时，`sandbox.localhost`
  // 解析到的是**手机自己** —— 打不开。那就留在同源：服务端给同源的沙箱文件
  // 加了 CSP sandbox（opaque origin），照样碰不到 App 的凭据，只是页面用不了
  // localStorage 之类。
  if (!isLoopbackHost(location.hostname)) return url;
  const port = location.port ? `:${location.port}` : '';
  return `${location.protocol}//sandbox.localhost${port}${url}`;
}

/**
 * `preview:` → 服务端的 `/sandbox/<钥匙>/…` 路径。不是这种链接返回 null。
 *
 * 宽松接受几种写法：`preview:/root/a.html`、`preview:///root/a.html`、
 * `preview:a.html`（相对路径按 /workspace 算，和沙箱里的 cwd 一致）。
 */
export function sandboxUrl(href: string): string | null {
  const m = /^preview:(.*)$/i.exec(href.trim());
  if (!m) return null;
  let rest = m[1].replace(/^\/\/+/, '/');
  if (!rest.startsWith('/')) rest = '/workspace/' + rest;
  const cut = rest.search(/[?#]/);
  const path = cut === -1 ? rest : rest.slice(0, cut);
  const tail = cut === -1 ? '' : rest.slice(cut);
  let decoded = path;
  try {
    decoded = decodeURI(path);
  } catch {
    /* 已经是编码好的或者有坏的 %，原样用 */
  }
  return sandboxPath(decoded) + tail;
}

export function inline(src: string): Span[] {
  const out: Span[] = [];
  let text = '';
  const flush = () => {
    if (text) out.push({ t: 'text', v: text });
    text = '';
  };

  let i = 0;
  while (i < src.length) {
    const c = src[i];

    // 转义
    if (c === '\\' && i + 1 < src.length && PUNCT.test(src[i + 1])) {
      text += src[i + 1];
      i += 2;
      continue;
    }

    // 行内代码：n 个反引号开，n 个反引号关
    if (c === '`') {
      let n = 1;
      while (src[i + n] === '`') n++;
      const fence = '`'.repeat(n);
      const end = src.indexOf(fence, i + n);
      if (end !== -1) {
        flush();
        let v = src.slice(i + n, end);
        if (v.length > 2 && v.startsWith(' ') && v.endsWith(' ')) v = v.slice(1, -1);
        out.push({ t: 'code', v });
        i = end + n;
        continue;
      }
      text += fence;
      i += n;
      continue;
    }

    // 强调 / 删除线
    if (c === '*' || c === '_' || c === '~') {
      const span = emphasis(src, i);
      if (span) {
        flush();
        out.push(span.node);
        i = span.end;
        continue;
      }
    }

    // 图片 ![alt](src)：只认沙箱文件和 http(s)
    if (c === '!' && src[i + 1] === '[') {
      const l = link(src, i + 1);
      if (l) {
        const href = safeHref(l.href);
        flush();
        if (href && !/^mailto:/i.test(href)) out.push({ t: 'img', src: href, alt: l.text });
        else text += l.text;
        i = l.end;
        continue;
      }
    }

    // 链接 [text](url)
    if (c === '[') {
      const l = link(src, i);
      if (l) {
        flush();
        const href = safeHref(l.href);
        const c2 = inline(l.text);
        if (href) out.push({ t: 'link', href, c: c2 });
        else out.push(...c2);
        i = l.end;
        continue;
      }
    }

    // 自动链接 <https://…>
    if (c === '<') {
      const m = /^<(https?:\/\/[^\s>]+)>/.exec(src.slice(i));
      if (m) {
        flush();
        out.push({ t: 'link', href: m[1], c: [{ t: 'text', v: m[1] }] });
        i += m[0].length;
        continue;
      }
    }

    // 裸链接：词首的 http(s)://，去掉结尾的标点
    if ((c === 'h' || c === 'H') && (i === 0 || !WORD.test(src[i - 1]))) {
      const m = URL_RE.exec(src.slice(i));
      if (m) {
        let url = m[0];
        while (/[.,:;!?'"*_~]$/.test(url)) url = url.slice(0, -1);
        flush();
        out.push({ t: 'link', href: url, c: [{ t: 'text', v: url }] });
        i += url.length;
        continue;
      }
    }

    text += c;
    i++;
  }
  flush();
  return out;
}

/**
 * 从 i 开始尝试匹配一段强调。失败返回 null（调用方把字符当普通文字）。
 *
 * 规则（CommonMark 的简化版，够 LLM 用）：
 *   - 开标记后面不能是空白，闭标记前面不能是空白
 *   - `_` 两侧是字母数字时不算（snake_case）
 *   - 内容递归解析，所以 `**粗体里的 *斜体***` 能嵌套
 */
function emphasis(src: string, i: number): { node: Span; end: number } | null {
  const c = src[i];
  let n = 1;
  while (src[i + n] === c) n++;

  if (c === '~') {
    if (n !== 2) return null;
  } else if (n > 3) return null;

  const after = src[i + n];
  if (after === undefined || /\s/.test(after)) return null;
  if (c === '_' && i > 0 && WORD.test(src[i - 1])) return null;

  const mark = c.repeat(n);
  let from = i + n;
  for (;;) {
    const run = src.indexOf(mark, from);
    if (run === -1) return null;
    // 闭标记所在的整串。比 n 长时取**最后** n 个 —— 前面多出来的属于
    // 里层：`**粗体 *斜体***` 的 `***` = 里层的 `*` + 外层的 `**`
    let r = n;
    while (src[run + r] === c) r++;
    const end = run + r - n;
    const before = src[run - 1];
    const wordAfter = c === '_' && WORD.test(src[end + n] ?? '');
    if (end > i + n && !/\s/.test(before) && !wordAfter && (c !== '~' || r === 2)) {
      const inner = inline(src.slice(i + n, end));
      const node: Span =
        c === '~'
          ? { t: 'del', c: inner }
          : n === 3
            ? { t: 'strong', c: [{ t: 'em', c: inner }] }
            : n === 2
              ? { t: 'strong', c: inner }
              : { t: 'em', c: inner };
      return { node, end: end + n };
    }
    from = run + r;
  }
}

/** `[文字](地址)`，文字里允许嵌套方括号，地址里允许成对的圆括号 */
function link(src: string, i: number): { text: string; href: string; end: number } | null {
  let depth = 0;
  let j = i;
  for (; j < src.length; j++) {
    if (src[j] === '\\') {
      j++;
      continue;
    }
    if (src[j] === '[') depth++;
    else if (src[j] === ']' && --depth === 0) break;
    else if (src[j] === '\n' && src[j - 1] === '\n') return null;
  }
  if (j >= src.length || src[j + 1] !== '(') return null;

  let paren = 0;
  let k = j + 2;
  for (; k < src.length; k++) {
    const ch = src[k];
    if (ch === '(') paren++;
    else if (ch === ')') {
      if (paren === 0) break;
      paren--;
    } else if (/\s/.test(ch)) return null;
  }
  if (k >= src.length) return null;
  return { text: src.slice(i + 1, j), href: src.slice(j + 2, k), end: k + 1 };
}
