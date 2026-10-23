/**
 * 猜一页 HTML 的底色。
 *
 * 为什么需要：查看器把网页放进 `<iframe sandbox>`，而 Chromium 会把宿主页的
 * `color-scheme: dark` 传下去 —— 内层文档的 canvas 是**透明**的（把
 * iframe 自己的背景涂成红色，一个完全没写样式的页面也会透出红色）。
 * 所以窗口露出来的就是 iframe 元素自己的背景色。
 *
 * 写死白色的话，深色网页会「先白 → 后黑」闪一下，在深色 App 里很扎眼。
 * 开之前先把底色探出来，上屏就是对的颜色。
 *
 * 怎么探：这份文档在另一个源上（`sandbox.localhost`，见 `sandboxPageUrl`），
 * 读不到 `contentDocument`，所以只能自己取一份源码、从文本里找。取的是
 * 同源（127.0.0.1）那份地址 —— 内容一样，走 loopback，不用配 CORS。
 *
 * 取值顺序（后面的覆盖前面的）：
 *   1. `<style>` 里 html / body / :root 规则上的 `background(-color)`
 *   2. `<html style=…>` / `<body style=…>` 内联的
 *   3. 还没找到 → 顺着 `<link rel=stylesheet>` 把 CSS 也取回来找一遍
 *      （底色写在单独 .css 里的页面很常见）
 *   4. 都没有 → `<meta name="color-scheme" content="dark">` 兜底成 #121212
 *      （深色体系下底色就是深色，写死白反而看不清字）
 *
 * 全都找不到就返回 null，调用方按白底处理 —— 那正是浏览器给「什么都没写」
 * 的文档的默认底色。
 */

/** 形状上像颜色的值（关键字交给浏览器判断） */
const COLOR = /^(#[0-9a-f]{3,8}|rgba?\([^)]*\)|hsla?\([^)]*\)|[a-z]+)$/i;

/** 认得出是颜色的值。关键字（white / rebeccapurple…）交给浏览器判断，
    否则 `background:var(--bg)` 里的 `var` 会被当成颜色名（踩过）。 */
function isColor(v: string): boolean {
  if (!COLOR.test(v)) return false;
  if (v.startsWith("#")) return true;
  return typeof CSS !== "undefined" && CSS.supports("color", v);
}

/** shorthand 里混着的、不是颜色的关键字 */
const NOT_COLOR = new Set([
  "none",
  "no-repeat",
  "repeat",
  "center",
  "top",
  "bottom",
  "left",
  "right",
  "fixed",
  "local",
  "scroll",
  "cover",
  "contain",
  "auto",
  "border-box",
  "padding-box",
  "content-box",
  "inherit",
  "initial",
  "unset",
  "revert",
  "transparent",
]);

/** 从一条声明的值里抠出颜色：shorthand 取第一个像颜色的记号，渐变取第一个色标 */
function pickColor(value: string): string | null {
  const v = value.trim().replace(/\s*!important\s*$/i, "");
  if (!v) return null;
  if (isColor(v) && !NOT_COLOR.has(v.toLowerCase())) return v;

  // `url(x.png) center/cover #333` / `linear-gradient(#111, #222)` …
  for (const tok of v.match(
    /#[0-9a-f]{3,8}|rgba?\([^)]*\)|hsla?\([^)]*\)|[a-z-]+/gi,
  ) ?? []) {
    const t = tok.toLowerCase();
    if (NOT_COLOR.has(t)) continue;
    if (isColor(tok)) return tok;
  }
  return null;
}

/** 从一个声明块（`a:b; c:d`）里取 background / background-color（后面的赢） */
function bgFromBlock(block: string): string | null {
  let found: string | null = null;
  for (const decl of block.split(";")) {
    const i = decl.indexOf(":");
    if (i < 0) continue;
    const prop = decl.slice(0, i).trim().toLowerCase();
    if (prop !== "background" && prop !== "background-color") continue;
    const c = pickColor(decl.slice(i + 1));
    if (c) found = c;
  }
  return found;
}

/** 选择器里有没有 html / body / :root（整词，不是 .html5 这种） */
function targetsRoot(sel: string): boolean {
  return /(^|[\s,>+~])(html|body|:root)([\s,>+~.:#[.]|$)/i.test(sel);
}

/** 从一段 CSS 里找根元素的底色（按作者顺序，最后一个赢） */
function bgFromCss(css: string): string | null {
  const clean = css.replace(/\/\*[\s\S]*?\*\//g, "");
  let bg: string | null = null;
  // 粗分规则 `选择器 { 声明 }`；@media 里的也一并扫到，够用（不判断条件真假）
  for (const r of clean.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (!targetsRoot(r[1])) continue;
    const c = bgFromBlock(r[2]);
    if (c) bg = c;
  }
  return bg;
}

/** 只认「显式写了 background」的情况 */
function sniffExplicit(html: string): string | null {
  const src = html.replace(/<!--[\s\S]*?-->/g, "");
  let bg: string | null = null;
  for (const m of src.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/gi)) {
    bg = bgFromCss(m[1]) ?? bg;
  }
  // 内联 style 比 <style> 更晚，覆盖它
  for (const m of src.matchAll(
    /<(?:html|body)\b[^>]*\bstyle=["']([^"']*)["']/gi,
  )) {
    bg = bgFromBlock(m[1]) ?? bg;
  }
  return bg;
}

/** `<meta name="color-scheme">` / `html{color-scheme:…}` —— 没写底色时用它兜底 */
function schemeOf(html: string): "dark" | "light" | null {
  const meta = /<meta[^>]+name=["']?color-scheme["']?[^>]*>/i.exec(html);
  if (meta) {
    const c = /content=["']?([^"'>]*)/i.exec(meta[0]);
    const first = c?.[1]
      .trim()
      .split(/[\s,]+/)[0]
      ?.toLowerCase();
    if (first === "dark" || first === "light") return first;
  }
  const css = /color-scheme\s*:\s*([a-z\s,]+)/i.exec(html);
  const first = css?.[1]
    .trim()
    .split(/[\s,]+/)[0]
    ?.toLowerCase();
  return first === "dark" || first === "light" ? first : null;
}

/** 外链样式表的绝对地址（同源才要；最多 3 个，够覆盖「html + 主题 css」） */
function stylesheets(html: string, base: string): string[] {
  const out: string[] = [];
  for (const m of html.matchAll(/<link\b[^>]*>/gi)) {
    const tag = m[0];
    if (!/rel=["']?stylesheet/i.test(tag)) continue;
    const href = /href=["']([^"']+)["']/i.exec(tag)?.[1];
    if (!href) continue;
    try {
      const u = new URL(href, base);
      if (u.origin === location.origin) out.push(u.href);
    } catch {
      /* 相对路径解不出来就算了 */
    }
    if (out.length >= 3) break;
  }
  return out;
}

export function sniffBackground(html: string): string | null {
  const bg = sniffExplicit(html);
  if (bg) return bg;
  return schemeOf(html) === "dark" ? "#121212" : null;
}

const cache = new Map<string, string | null>();

async function getText(url: string): Promise<string | null> {
  try {
    // 不看状态码：沙箱 404/403 的**响应体就是**查看器里要显示的那个
    // 错误页（服务端自己生成的深色页），不取就只能按白底处理
    return await (await fetch(url, { credentials: "omit" })).text();
  } catch {
    return null;
  }
}

/**
 * 取源码、猜底色。任何失败都当成「没探到」（调用方按白底处理）。
 *
 * 结果按 url 缓存：同一个文件反复打开（点进点出）不该反复请求。
 */
export async function probeBackground(url: string): Promise<string | null> {
  // 相对路径要补成绝对：下面用 URL 解析 <link href> 时需要一个带 origin 的基准
  const abs = new URL(url, location.href).href;
  if (cache.has(abs)) return cache.get(abs) ?? null;

  const html = await getText(abs);
  let bg = html ? sniffExplicit(html) : null;

  // 底色写在单独 .css 里的页面很常见，顺着 link 再找一遍
  if (!bg && html) {
    for (const href of stylesheets(html, abs)) {
      const css = await getText(href);
      if (css) bg = bgFromCss(css);
      if (bg) break;
    }
  }

  if (!bg && html && schemeOf(html) === "dark") bg = "#121212";

  cache.set(abs, bg);
  return bg;
}
