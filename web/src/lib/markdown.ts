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
 *   - 链接只放行 http/https/mailto/sandbox，其余（javascript: 之类）当普通文字。
 *   - `sandbox:/root/x.html` 指向沙箱里的文件，渲染时改写成 `/sandbox/root/x.html`
 *     （服务端按路径直接映射），图片同理。
 */

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
  return /^(https?:|mailto:|sandbox:)/i.test(href.trim()) ? href.trim() : null;
}

/**
 * `sandbox:` → 服务端的 `/sandbox/…` 路径。不是 sandbox 链接返回 null。
 *
 * 宽松接受几种写法：`sandbox:/root/a.html`、`sandbox:///root/a.html`、
 * `sandbox:a.html`（相对路径按 /root 算，和沙箱里的 cwd 一致）。
 */
export function sandboxUrl(href: string): string | null {
  const m = /^sandbox:(.*)$/i.exec(href.trim());
  if (!m) return null;
  let rest = m[1].replace(/^\/\/+/, '/');
  if (!rest.startsWith('/')) rest = '/root/' + rest;
  const cut = rest.search(/[?#]/);
  const path = cut === -1 ? rest : rest.slice(0, cut);
  const tail = cut === -1 ? '' : rest.slice(cut);
  let decoded = path;
  try {
    decoded = decodeURI(path);
  } catch {
    /* 已经是编码好的或者有坏的 %，原样用 */
  }
  return '/sandbox' + encodeURI(decoded) + tail;
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
