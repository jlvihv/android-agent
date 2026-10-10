/**
 * 极简 Markdown 解析。
 *
 * 故意不引 marked / markdown-it —— 依赖越少越好，而 LLM 实际吐出来的
 * 就那几种结构。解析成结构化的块交给 Svelte 渲染，**不用 `{@html}`**：
 * 既没有 XSS 面，也能让 Tailwind 直接管样式。
 *
 * 块级：段落、标题、围栏代码块、有序/无序列表、引用
 * 行内：粗体、斜体、行内代码、链接
 *
 * 一个刻意的行为：**没闭合的围栏也当代码块收**。流式输出时经常只有
 * 前半截，这时候不该把 ``` 当普通文字闪一下。
 */

export type Span =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'strong'; v: string }
  | { t: 'em'; v: string }
  | { t: 'link'; href: string; v: string };

export type Block =
  | { t: 'p'; spans: Span[] }
  | { t: 'h'; level: number; spans: Span[] }
  | { t: 'code'; lang: string; v: string }
  | { t: 'ul'; items: Span[][] }
  | { t: 'ol'; items: Span[][] }
  | { t: 'quote'; spans: Span[] };

const INLINE =
  /(`[^`\n]+`|\*\*[^*\n]+\*\*|__[^_\n]+__|\*[^*\n]+\*|_[^_\n]+_|\[[^\]\n]*\]\([^)\s]+\))/g;

const FENCE = /^\s*```(\S*)\s*$/;
const FENCE_END = /^\s*```\s*$/;
const HEADING = /^(#{1,6})\s+(.*)$/;
const QUOTE = /^>\s?(.*)$/;
const BULLET = /^\s*[-*+]\s+/;
const NUMBER = /^\s*\d+[.)]\s+/;

function isBlockStart(l: string): boolean {
  return (
    FENCE.test(l) || HEADING.test(l) || QUOTE.test(l) || BULLET.test(l) || NUMBER.test(l)
  );
}

function inline(src: string): Span[] {
  const out: Span[] = [];
  let last = 0;

  for (const m of src.matchAll(INLINE)) {
    const i = m.index;
    if (i > last) out.push({ t: 'text', v: src.slice(last, i) });

    const s = m[0];
    if (s.startsWith('`')) out.push({ t: 'code', v: s.slice(1, -1) });
    else if (s.startsWith('**') || s.startsWith('__')) out.push({ t: 'strong', v: s.slice(2, -2) });
    else if (s.startsWith('*') || s.startsWith('_')) out.push({ t: 'em', v: s.slice(1, -1) });
    else {
      const link = /^\[([^\]]*)\]\(([^)\s]+)\)$/.exec(s);
      if (link) out.push({ t: 'link', href: link[2], v: link[1] });
      else out.push({ t: 'text', v: s });
    }

    last = i + s.length;
  }

  if (last < src.length) out.push({ t: 'text', v: src.slice(last) });
  return out;
}

export function parse(src: string): Block[] {
  const lines = src.replace(/\r\n?/g, '\n').split('\n');
  const out: Block[] = [];
  let i = 0;

  while (i < lines.length) {
    // 围栏代码块
    const fence = FENCE.exec(lines[i]);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !FENCE_END.test(lines[i])) body.push(lines[i++]);
      i++; // 吃掉收尾的 ```
      out.push({ t: 'code', lang: fence[1], v: body.join('\n') });
      continue;
    }

    if (!lines[i].trim()) {
      i++;
      continue;
    }

    const h = HEADING.exec(lines[i]);
    if (h) {
      out.push({ t: 'h', level: h[1].length, spans: inline(h[2]) });
      i++;
      continue;
    }

    const q = QUOTE.exec(lines[i]);
    if (q) {
      out.push({ t: 'quote', spans: inline(q[1]) });
      i++;
      continue;
    }

    if (BULLET.test(lines[i])) {
      const items: Span[][] = [];
      while (i < lines.length && BULLET.test(lines[i])) {
        items.push(inline(lines[i++].replace(BULLET, '')));
      }
      out.push({ t: 'ul', items });
      continue;
    }

    if (NUMBER.test(lines[i])) {
      const items: Span[][] = [];
      while (i < lines.length && NUMBER.test(lines[i])) {
        items.push(inline(lines[i++].replace(NUMBER, '')));
      }
      out.push({ t: 'ol', items });
      continue;
    }

    // 段落。走到这里说明当前行不是块起始，所以至少收一行，不会死循环。
    const para: string[] = [];
    while (i < lines.length && lines[i].trim() && !isBlockStart(lines[i])) {
      para.push(lines[i++]);
    }
    out.push({ t: 'p', spans: inline(para.join('\n')) });
  }

  return out;
}
