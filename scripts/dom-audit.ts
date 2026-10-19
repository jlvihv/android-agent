#!/usr/bin/env bun
/**
 * DOM 审计：用真实渲染树找「多余的 HTML 层级」。
 *
 * 判据分两道 —— 静态筛出候选，再**动态证明**它真的没用：
 *
 *   1. 静态候选：只有一个元素子节点、没有自有文字、盒子与子元素**完全重合**、
 *      不是滚动容器、计算样式里没有任何有视觉或层叠上下文副作用的属性。
 *   2. 拔掉测试：把候选的 `display` 改成 `contents`（这一层不再产生盒子），
 *      比对**整页每个元素**的盒子 —— 一个像素都没动，才算真多余。
 *
 * 为什么第二道必须做：静态判据看着「盒子完全重合」，但那一层可能是父级 flex 的
 * item（被拉伸）、或者子元素靠它的 `flex-1` 撑宽 —— 删掉就变了。实测过：Markdown
 * 列表项里那层 `div.flex.min-w-0.flex-1` 静态上完全重合，拔掉后段落宽度
 * 359 → 335.5px。只有动态测才能分辨。
 *
 * 拔掉测试**只对当前状态有效**：这一屏里惰性的层，换个内容可能就有用了
 * （`#app` 那个 div 就是时灵时不灵）。所以报告是「候选」，不是「判决书」。
 *
 * 覆盖不到的（要在源码里人工确认，别当成没问题）：
 *   - 只承载**可继承**样式的层（font-size / color / leading）—— `display:contents`
 *     不会丢继承，所以测试看不见它有用。
 *   - 只当 JS 钩子的层（`bind:this` / `bind:clientWidth`）—— 测试只看几何。
 *   - 语义必需的层（`li` / `tr` / `td`）—— 几何上透明，但删了 DOM 就不合法。
 *
 * 用法：
 *
 *     bun scripts/dom-audit.ts                      # 默认 http://127.0.0.1:5173
 *     bun scripts/dom-audit.ts --base http://127.0.0.1:8787
 *     bun scripts/dom-audit.ts --snapshot /tmp/a.json   # 只导几何快照
 *     bun scripts/dom-audit.ts --diff /tmp/a.json       # 和快照比对（改完前端验证「没动布局」）
 *
 * 通常不用直接跑它，用 `just dom-audit`（它会起核心 + vite + headless Chrome）。
 */
import { readFileSync, writeFileSync } from 'node:fs';

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

const argv = process.argv.slice(2);
const arg = (name: string, fallback: string) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};
const BASE = arg('base', process.env.AGENT_UI ?? 'http://127.0.0.1:5173');
const CDP = arg('cdp', process.env.AGENT_CDP ?? 'http://127.0.0.1:9222');
const SNAPSHOT = arg('snapshot', '');
const DIFF = arg('diff', '');
/** `--shot <目录>`：每屏存一张 png，用来肉眼对样式 */
const SHOT = arg('shot', '');
let shotN = 0;
const QUIET_PANELS = argv.includes('--no-panels');
const DEBUG = argv.includes('--debug');

// ---------------------------------------------------------------------------
// 在页面里跑的两个函数
//
// ⚠️ 它们是 `toString()` 之后送到页面里 eval 的，**必须保持纯 JS** ——
//    写 TS 类型标注会变成页面里的语法错误（踩过）。
// ---------------------------------------------------------------------------

/** 遍历渲染树，找候选层并对每个做拔掉测试。 */
function pageAudit() {
  const SKIP = new Set(['SCRIPT', 'STYLE', 'LINK', 'META', 'TITLE']);
  // 「有副作用」的属性。注意**不要**看 border*Style / outlineWidth：
  // Tailwind preflight 把所有元素的 border-style 设成 solid（宽度 0，不渲染），
  // Chrome 的 outline-width 恒为 3px（outline-style 是 none，也不渲染）——
  // 带上这两个，**所有**元素都会被判成「脏」，报告直接归零（踩过）。
  const VISUAL = [
    'position', 'overflowX', 'overflowY', 'transform', 'filter', 'opacity', 'mixBlendMode',
    'isolation', 'contain', 'contentVisibility', 'zIndex', 'clipPath', 'maskImage', 'mask',
    'perspective', 'willChange', 'backdropFilter', 'borderTopWidth', 'borderLeftWidth',
    'borderRightWidth', 'borderBottomWidth', 'backgroundImage', 'outlineStyle',
  ];
  const OK = new Set(['none', 'normal', 'visible', 'auto', 'static', '0px', '1', '0', '', 'rgba(0, 0, 0, 0)']);
  const short = (el: any) =>
    el.tagName.toLowerCase() + (el.getAttribute('class') ? '.' + el.getAttribute('class').split(/\s+/)[0] : '');
  const path = (el: any) => {
    const parts = [];
    for (let n = el; n && n !== document.body; n = n.parentElement) {
      parts.unshift(short(n));
      if (parts.length >= 4) break;
    }
    return parts.join(' > ');
  };
  const kidsOf = (el: any) => [...el.children].filter((k: any) => !SKIP.has(k.tagName));
  const list = [...document.querySelectorAll('body *')].filter(
    (el: any) => !SKIP.has(el.tagName) && !(el.parentElement && el.parentElement.closest('svg')),
  );
  const boxes = () =>
    list.map((el: any) => {
      const r = el.getBoundingClientRect();
      const h = (n: number) => Math.round(n * 2) / 2;
      return `${h(r.x)},${h(r.y)},${h(r.width)},${h(r.height)}`;
    });

  const funnel = { total: list.length, oneKid: 0, oneKidNoText: 0, sameBox: 0, notScroll: 0, clean: 0 };
  const A = [], C = [], D = [], E = [];
  for (const el of list) {
    const cs = getComputedStyle(el);
    const r = el.getBoundingClientRect();
    if (cs.display === 'contents') {
      D.push({ p: path(el), kids: kidsOf(el).length });
      continue;
    }
    const kids = kidsOf(el);
    const dirty = VISUAL.filter((p) => !OK.has(cs[p]));
    const scrolls = el.scrollHeight > el.clientHeight + 1 || el.scrollWidth > el.clientWidth + 1;
    const hasText = [...el.childNodes].some((n: any) => n.nodeType === 3 && n.textContent.trim());
    if (kids.length === 0 && !hasText && !scrolls && !dirty.length && r.width < 0.6 && r.height < 0.6) {
      E.push({ p: path(el) });
      continue;
    }
    if (kids.length !== 1) continue;
    funnel.oneKid++;
    if (hasText) continue;
    funnel.oneKidNoText++;
    const one = kids[0];
    const kr = one.getBoundingClientRect();
    const same =
      Math.abs(r.x - kr.x) < 0.6 && Math.abs(r.y - kr.y) < 0.6 &&
      Math.abs(r.width - kr.width) < 0.6 && Math.abs(r.height - kr.height) < 0.6;
    if (same) funnel.sameBox++;
    if (dirty.length) {
      // 唯一「脏」的原因是 position:relative —— 那是给绝对定位的子元素当锚点，合理
      if (dirty.length === 1 && cs.position === 'relative' && same && !scrolls)
        C.push({ p: path(el), box: Math.round(r.width) + 'x' + Math.round(r.height), child: short(one) });
      continue;
    }
    if (scrolls) continue;
    funnel.notScroll++;
    if (!same) continue;
    funnel.clean++;
    A.push({
      el, p: path(el), tag: short(el), display: cs.display,
      box: Math.round(r.width) + 'x' + Math.round(r.height), child: short(one),
      ptag: short(el.parentElement), pdisplay: getComputedStyle(el.parentElement).display,
      pkids: kidsOf(el.parentElement).length,
      pseudo: ['::before', '::after'].filter((x) => getComputedStyle(el, x).content !== 'none'),
    });
  }

  // —— 拔掉测试 ——
  const b0 = boxes();
  const b1 = boxes();
  const stable = b0.join(';') === b1.join(';');
  const idx = new Map(list.map((el: any, i: number) => [el, i]));
  for (const c of A) {
    const i = idx.get(c.el);
    const before = boxes();
    c.el.style.display = 'contents';
    void c.el.offsetHeight;
    const after = boxes();
    c.el.style.removeProperty('display');
    void c.el.offsetHeight;
    let moved = 0, sample = '';
    for (let k = 0; k < before.length; k++) {
      if (k === i) continue;
      if (before[k] !== after[k]) {
        moved++;
        if (!sample) sample = short(list[k]) + ' ' + before[k] + ' -> ' + after[k];
      }
    }
    c.moved = moved;
    c.sample = sample;
  }

  // 按「父 > 自己 > 子」去重
  const groups = new Map();
  for (const c of A) {
    const key = `${c.ptag}(${c.pdisplay}) > ${c.tag}(${c.display}) > ${c.child}`;
    const g = groups.get(key) || { key, n: 0, moved0: 0, first: c.p, pseudo: c.pseudo, sample: '' };
    g.n++;
    if (c.moved === 0) g.moved0++;
    if (!g.sample && c.sample) g.sample = c.sample;
    groups.set(key, g);
  }
  return { funnel, stable, A: A.length, groups: [...groups.values()].sort((a, b) => b.n - a.n), C, D, E };
}

/**
 * 整页几何快照：给**有文字**的元素（以及带 id/aria-label 的骨架元素）记下盒子。
 *
 * 为什么按「文字」做锚点：改完前端要验证「布局一个像素都没动」。按元素序号对不行的 ——
 * 删掉一层冗余 div，后面所有元素的下标都会移一位，全变成假阳性。文字是稳的：
 * 冗余层自己没有文字（进不了快照），而真正该在位的段落、列表项、按钮都带文字。
 */
function pageSnapshot() {
  const SKIP = new Set(['SCRIPT', 'STYLE', 'LINK', 'META', 'TITLE']);
  const out = [];
  const seen = new Map();
  for (const el of document.querySelectorAll('body *')) {
    if (SKIP.has(el.tagName)) continue;
    if (el.parentElement && el.parentElement.closest('svg')) continue;
    const own = [...el.childNodes]
      .filter((n) => n.nodeType === 3)
      .map((n) => n.textContent.trim())
      .join(' ')
      .trim();
    const anchor = el.id || el.getAttribute('aria-label') || '';
    if (!own && !anchor) continue;
    const r = el.getBoundingClientRect();
    const h = (n) => Math.round(n * 2) / 2;
    const cls = (el.getAttribute('class') || '').split(/\s+/).filter(Boolean).slice(0, 2).join('.');
    const key = `${el.tagName.toLowerCase()}${cls ? '.' + cls : ''}${anchor ? '#' + anchor : ''}|${own.slice(0, 48)}`;
    const n = (seen.get(key) ?? 0) + 1;
    seen.set(key, n);
    out.push(`${key}#${n}=${h(r.x)},${h(r.y)},${h(r.width)},${h(r.height)}`);
  }
  return out;
}

// ---------------------------------------------------------------------------
// CDP 客户端
// ---------------------------------------------------------------------------

const targets = (await (await fetch(`${CDP}/json/list`)).json()) as any[];
const target = targets.find((t) => t.type === 'page');
if (!target) throw new Error(`CDP ${CDP} 上没有 page target —— headless Chrome 起来了吗？`);

const ws = new WebSocket(target.webSocketDebuggerUrl);
await new Promise<void>((r) => (ws.onopen = () => r()));
let seq = 0;
const pending = new Map<number, (v: any) => void>();

ws.onmessage = async (e) => {
  const m = JSON.parse(e.data as string);
  if (m.id && pending.has(m.id)) pending.get(m.id)!(m);
};

function send(method: string, params?: unknown): Promise<any> {
  const id = ++seq;
  return new Promise((res, rej) => {
    pending.set(id, (m) => {
      pending.delete(id);
      if (m.error) rej(new Error(`${method}: ${JSON.stringify(m.error)}`));
      else res(m.result);
    });
    ws.send(JSON.stringify({ id, method, params }));
  });
}

async function evaluate(expression: string): Promise<any> {
  const r = await send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval 失败');
  return r.result.value;
}
const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------------------
// 报告
// ---------------------------------------------------------------------------

let problems = 0;

async function audit(label: string) {
  if (SHOT) {
    shotN++;
    const s = await send('Page.captureScreenshot', { format: 'png' });
    const name = `${String(shotN).padStart(2, '0')}-${label.replace(/[^\w\u4e00-\u9fa5]+/g, '-')}.png`;
    writeFileSync(`${SHOT}/${name}`, Buffer.from(s.data, 'base64'));
  }
  const r = await evaluate(`${pageAudit.toString()}\npageAudit()`);
  const f = r.funnel;
  console.log(`\n===== ${label} =====`);
  console.log(
    `  漏斗：元素 ${f.total} → 只有一个子元素 ${f.oneKid} → 且无自有文字 ${f.oneKidNoText} → ` +
      `盒子与子元素重合 ${f.sameBox} → 无视觉副作用 ${f.notScroll} → 完全透明 ${f.clean}`,
  );
  if (!r.stable) console.log('  ⚠️ 页面在动（流式输出/动画），这次结果不可靠');
  for (const g of r.groups) {
    const zero = g.moved0 === g.n;
    if (zero) problems++;
    const verdict = zero ? '✅ 拔掉后 0 像素变化' : `❌ 拔掉后动了：${g.sample}`;
    console.log(`  ${String(g.n).padStart(3)} 个  ${g.key}`);
    console.log(`          ${verdict}`);
    console.log(`          例子: ${g.first}${g.pseudo.length ? '  伪元素:' + g.pseudo : ''}`);
  }
  if (r.C.length) console.log(`  （${r.C.length} 个只因 position:relative 保留 —— 给绝对定位子元素当锚点，合理）`);
  if (r.D.length) console.log(`  （${r.D.length} 个 display:contents：${JSON.stringify(r.D)}）`);
  if (r.E.length) console.log(`  （${r.E.length} 个 0×0 空元素：${JSON.stringify(r.E)}）`);
}

async function click(desc: string, expr: string) {
  const ok = await evaluate(expr);
  if (!ok) console.log(`  · ${desc} -> 没找到（这一屏可能没这个元素）`);
  await wait(500);
}

const BY_TEXT = (t: string) =>
  `(() => { const b = [...document.querySelectorAll('button')].find(x => x.textContent.trim().startsWith(${JSON.stringify(t)})); if(!b) return 0; b.click(); return 1; })()`;
const PILL = `(() => { const b = [...document.querySelectorAll('button')].find(x => (x.className||'').includes('max-w-[60%]')); if(!b) return 0; b.click(); return 1; })()`;
const CLOSE = `document.querySelector('button[aria-label="关闭"]')?.click(), 1`;

/** 走真 UI 的路径发一条消息：填 textarea（触发 Svelte 的 bind:value）+ 点发送。 */
async function sendMessage(text: string) {
  await evaluate(`(() => {
    const ta = document.querySelector('textarea');
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
    setter.call(ta, ${JSON.stringify(text)});
    ta.dispatchEvent(new Event('input', { bubbles: true }));
    return 1;
  })()`);
  await wait(200);
  await click('发送', `document.querySelector('button[aria-label="发送"]')?.click(), 1`);
  for (let i = 0; i < 40; i++) {
    const busy = await evaluate(`!!document.querySelector('button[aria-label="停止"]')`);
    if (DEBUG) console.log(`    [debug] t=${(i * 0.3).toFixed(1)}s busy=${busy} 元素=${await evaluate('document.querySelectorAll("body *").length')}`);
    if (!busy) break;
    await wait(300);
  }
  await wait(900);
  if (DEBUG) console.log(`    [debug] 发完之后正文: ${JSON.stringify(String(await evaluate('document.body.innerText')).slice(0, 160))}`);
}

// ---------------------------------------------------------------------------
// 跑
// ---------------------------------------------------------------------------

await send('Page.enable');
await send('Runtime.enable');
await send('Emulation.setDeviceMetricsOverride', { width: 411, height: 923, deviceScaleFactor: 2, mobile: true });
await send('Page.navigate', { url: BASE });
await wait(3000);

// 只导快照 / 只比对
if (SNAPSHOT || DIFF) {
  await sendMessage('渲染一下夹具内容');
  await wait(500);
  const snap = (await evaluate(`${pageSnapshot.toString()}\npageSnapshot()`)) as string[];
  if (SNAPSHOT) {
    writeFileSync(SNAPSHOT, JSON.stringify(snap, null, 0));
    console.log(`几何快照已写入 ${SNAPSHOT}（${snap.length} 个元素）`);
  }
  if (DIFF) {
    const key = (line: string) => line.slice(0, line.lastIndexOf('='));
    const before = new Map(
      (JSON.parse(readFileSync(DIFF, 'utf8')) as string[]).map((l) => [key(l), l.slice(l.lastIndexOf('=') + 1)]),
    );
    const after = new Map(snap.map((l) => [key(l), l.slice(l.lastIndexOf('=') + 1)]));
    const moved: string[] = [];
    for (const [k, v] of after) {
      if (!before.has(k)) moved.push(`新增  ${k} = ${v}`);
      else if (before.get(k) !== v) moved.push(`移动  ${k}  ${before.get(k)} -> ${v}`);
    }
    for (const [k, v] of before) if (!after.has(k)) moved.push(`消失  ${k} = ${v}`);
    console.log(`对比 ${DIFF}：锚点 ${before.size} → ${after.size}，${moved.length} 处不同`);
    for (const m of moved.slice(0, 40)) console.log('  ' + m);
    if (!moved.length) console.log('  ✅ 所有带文字的元素都在原位（布局没动）');
    ws.close();
    process.exit(moved.length ? 1 : 0);
  }
  ws.close();
  process.exit(0);
}

// 夹具走**真实 agent loop**：假 LLM 先发一个 bash 工具调用（核心真的去沙箱跑），
// 再回 Markdown 正文。所以这一屏同时有：思考行 + 工具行 + 列表/嵌套/任务项/表格/引用/代码块。
await sendMessage('渲染一下夹具内容');
await audit('主界面 · 夹具（思考行 + 工具行 + Markdown 各种块级结构）');

await click('工具行', `(() => { const b = [...document.querySelectorAll('button')].find(x => (x.className||'').includes('px-2.5 py-0.5')); if(!b) return 0; b.click(); return 1; })()`);
await audit('工具面板（Sheet）');
await click('遮罩关闭', CLOSE);
await wait(600);

if (!QUIET_PANELS) {
  await click('模型药丸', PILL);
  await audit('模型面板 · 选择模型');
  await click('模型提供商行', BY_TEXT('模型提供商'));
  await audit('模型面板 · 模型提供商页');
  await click('遮罩关闭', CLOSE);
  await wait(600);
  // 抽屉里「收藏的页面」那一段要有内容才审得到（临时核心的库里是空的）。
  // 直接打接口造两行 —— 只是路径 + 标题，不碰沙箱、不花 token。
  const seeded = await evaluate(`fetch('/api/favorites', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ path: '/workspace/report/index.html', title: '季度销售报表' }),
  }).then(r => r.text()).then(async (a) => {
    const b = await fetch('/api/favorites', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ path: '/workspace/demo/index.html', title: '一个名字特别长的页面用来看看抽屉里会不会把行撑开' }),
    }).then(r => r.text());
    return a + ' / ' + b;
  })`);
  console.log(`  · 收藏种子：${String(seeded).slice(0, 200)}`);
  await click('菜单', `document.querySelector('button[aria-label="菜单"]')?.click(), 1`);
  await audit('抽屉');
  // 点第一行收藏 → 查看器。看的是**网页那种形态**（右上角那颗星 / iframe 铺满），
  // 顺便验证「收藏的路径 + 当前的 sandbox 钥匙」能拼成一个能开的地址。
  await click('收藏行', BY_TEXT('季度销售报表'));
  await wait(1200);
  await audit('查看器 · 网页（收藏星）');
  // 星是双向的：先取消、确认服务端真删了，再收回来（顺便走一遍「取页面标题」那条路）。
  // 列表在界面之外，只能问接口 —— 这也是收藏真落库了的凭据。
  const favCount = () =>
    evaluate(`fetch('/api/favorites').then(r => r.json()).then(j => j.favorites.length)`);
  await click('取消收藏', `document.querySelector('button[aria-label="取消收藏"]')?.click(), 1`);
  await wait(800);
  console.log(`  · 收藏数（取消后）：${await favCount()}`);
  await click('重新收藏', `document.querySelector('button[aria-label="收藏页面"]')?.click(), 1`);
  await wait(800);
  console.log(`  · 收藏数（重新收藏后）：${await favCount()}`);
  await click('关查看器', `document.querySelector('button[aria-label="返回"]')?.click(), 1`);
  await wait(600);
  await click('设置', `document.querySelector('button[aria-label="设置"]')?.click(), 1`);
  await audit('设置页');
  // 设置是全屏页（z-60），附件面板 z-50 在它**下面** —— 不先关掉它，
  // 下一屏拍到的还是设置页（踩过）。关完设置再关抽屉，才看得到附件面板。
  await click('关设置', `document.querySelector('button[aria-label="返回"]')?.click(), 1`);
  await wait(600);
  await click('关抽屉', `(() => { const b = [...document.querySelectorAll('button[aria-label="关闭"]')].find(x => (x.className||'').includes('bg-black/50')); if(!b) return 0; b.click(); return 1; })()`);
  await wait(600);
  await click('附件 +', `document.querySelector('button[aria-label="添加文件"]')?.click(), 1`);
  await audit('附件面板');
}

console.log(`\n共 ${problems} 处「拔掉后 0 像素变化」的候选层。`);
console.log('别忘了：拔掉测试只对当前状态有效；只承载可继承样式 / 只当 JS 钩子 / 语义必需的层，');
console.log('测试看不见它们有用 —— 动手删之前回源码确认一遍。');

ws.close();
