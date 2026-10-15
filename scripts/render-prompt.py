#!/usr/bin/env python3
"""把 dump_prompt 的几份输出渲染成一个 HTML，方便肉眼比对「最终拼出来的成品」。

版式：**按内容类型分组**（系统提示词 / read 的 description / read 的 schema / …），
每组里用标签切换形态 —— 同一个位置换内容，不用上下滚动找差异。
每份里和别的形态**对不上的字句**标黄（那就是平台差异点）。

用法（在仓库根）：

    cd core
    cargo run -q --example dump_prompt android          > /tmp/prompt-preview/prompt-preview-android.txt
    cargo run -q --example dump_prompt android --phone  > /tmp/prompt-preview/prompt-preview-android-phone.txt
    cargo run -q --example dump_prompt linux            > /tmp/prompt-preview/prompt-preview-linux.txt
    cargo run -q --example dump_prompt macos            > /tmp/prompt-preview/prompt-preview-macos.txt
    cd .. && python3 scripts/render-prompt.py

产出 `/tmp/prompt-preview/index.html`（用浏览器打开）。

**全部从代码渲染**，不碰设备：`--phone` 是给「手机形态」用的（那段设备清单只有
真手机上有，而判据是编译目标，电脑上默认渲染不出来）。
"""
import difflib
import html
import json
import os
import re

DIR = os.environ.get("PROMPT_PREVIEW_DIR", "/tmp/prompt-preview")

PLATFORMS = [
    ("android-phone", "Android 手机"),
    ("android", "Android 桌面形态"),
    ("linux", "Linux"),
    ("macos", "macOS"),
]

# 标签上的悬停说明
HINTS = {
    "android-phone": "proot + Alpine 沙箱，带设备能力（`agent sms` 那些）",
    "android": "桌面 `just dev`：形态和手机一样，但没有设备能力",
    "linux": "桌面宿主模式，没有沙箱",
    "macos": "桌面宿主模式，BSD 用户态",
}

SYS_RE = re.compile(r"^=== SYSTEM_PROMPT · (\d+) 字符（其中汉字 (\d+)）(?: ?· .*)? ===$")
TOOL_RE = re.compile(r"^=== (\w+) · desc (\d+) 字符（汉字 (\d+)）· schema 紧凑 (\d+) 字符 ===$")
TOTAL_RE = re.compile(r"^=== 合计约 (\d+) 字符")


def parse(path):
    lines = open(path).read().split("\n")
    out = {"system": None, "tools": [], "total": None}
    cur = None
    for ln in lines:
        m = SYS_RE.match(ln)
        if m:
            cur = {"kind": "system", "chars": int(m.group(1)), "cjk": int(m.group(2)), "body": []}
            out["system"] = cur
            continue
        m = TOOL_RE.match(ln)
        if m:
            cur = {"kind": "tool", "name": m.group(1), "desc_chars": int(m.group(2)),
                   "desc_cjk": int(m.group(3)), "schema_chars": int(m.group(4)), "body": []}
            out["tools"].append(cur)
            continue
        m = TOTAL_RE.match(ln)
        if m:
            out["total"] = int(m.group(1))
            cur = None
            continue
        if cur is not None:
            cur["body"].append(ln)
    for t in out["tools"] + [out["system"]]:
        body = "\n".join(t["body"]).strip("\n")
        if t is out["system"]:
            t["text"] = body
        else:
            desc, _, schema = body.partition("\n\n")
            t["desc"] = desc
            t["schema"] = schema
    return out


def spans_vs_others(texts, key):
    """key 那份里，和另外几份对不上（非 equal）的字符区间。"""
    mine = texts[key]
    spans = []
    for other, t in texts.items():
        if other == key:
            continue
        sm = difflib.SequenceMatcher(None, mine, t, autojunk=False)
        for tag, i1, i2, j1, j2 in sm.get_opcodes():
            if tag != "equal":
                spans.append((i1, i2))
    # 挨得很近的碎块合成一段：逐字比出来的是「wor/ks/ace」这种碎片，
    # 合成一段才看得懂（GAP 之内算同一处）
    GAP = 8
    spans.sort()
    merged = []
    for a, b in spans:
        if merged and a - merged[-1][1] <= GAP:
            merged[-1] = (merged[-1][0], max(merged[-1][1], b))
        else:
            merged.append((a, b))
    return merged


def highlight(text, spans):
    out, at = [], 0
    for a, b in spans:
        out.append(html.escape(text[at:a]))
        out.append(f'<mark title="这一处是这个形态独有的说法">{html.escape(text[a:b])}</mark>')
        at = b
    out.append(html.escape(text[at:]))
    return "".join(out)


def diff_line(text, spans, limit=5):
    frags = [text[a:b].strip() for a, b in spans]
    frags = [f for f in frags if f]
    if not frags:
        return '<span class="none">和其他形态一字不差</span>'
    shown = frags[:limit]
    extra = f"…还有 {len(frags) - limit} 处" if len(frags) > limit else ""
    parts = " · ".join(f"<code>{html.escape(f[:70])}</code>" for f in shown)
    return f"独有 {len(frags)} 处：{parts} {extra}"


def main():
    data = {k: parse(f"{DIR}/prompt-preview-{k}.txt") for k, _ in PLATFORMS}
    names = [k for k, _ in PLATFORMS]
    base = names[0]

    # 块的名字与顺序由第一份决定（各形态的工具表是同一套）
    blocks = [("system", "系统提示词")]
    for t in data[base]["tools"]:
        n = t["name"]
        blocks.append((f"{n}.description", f"{n} · description"))
        blocks.append((f"{n}.schema", f"{n} · schema"))

    # 同一个块在各形态下的文本（逐字比要在块内比）
    def block_text(key, b):
        if b == "system":
            return data[key]["system"]["text"]
        name, kind = b.split(".")
        tool = next(t for t in data[key]["tools"] if t["name"] == name)
        return tool["desc"] if kind == "description" else tool["schema"]

    cells = {}
    for key in names:
        got = {}
        for b, _ in blocks:
            texts = {k: block_text(k, b) for k in names}
            txt = texts[key]
            spans = spans_vs_others(texts, key)
            cjk = sum(1 for c in txt if "\u4e00" <= c <= "\u9fff")
            meta = f"{len(txt)} 字符" + (f" · 汉字 {cjk}" if cjk else "")
            got[b] = {
                "html": highlight(txt, spans),
                "meta": meta,
                "diff": diff_line(txt, spans),
                "n": len([1 for a, b2 in spans if txt[a:b2].strip()]),
            }
        cells[key] = got

    totals = {k: data[k]["total"] for k in names}
    payload = json.dumps(cells, ensure_ascii=False).replace("</", "<\\/")

    tabs = "".join(
        f'<button class="tab" data-k="{k}" title="{html.escape(HINTS.get(k, ""))}">{html.escape(label)}</button>'
        for k, label in PLATFORMS
    )
    body = "".join(
        f'<section class="blk"><h3>{html.escape(label)}<span class="meta" id="m-{b}"></span></h3>'
        f'<pre class="code" id="c-{b}"></pre>'
        f'<p class="diff" id="d-{b}"></p></section>'
        for b, label in blocks
    )
    total_row = " · ".join(f"{label} {totals[k]}" for k, label in PLATFORMS)

    CSS = """
:root { color-scheme: light; }
body { margin: 0 auto; max-width: 1040px; padding: 24px 20px 120px;
       font: 15px/1.7 -apple-system, "Noto Sans CJK SC", "PingFang SC", system-ui, sans-serif;
       color: #1c1b19; background: #fbfaf8; }
h1 { font-size: 22px; margin: 0 0 4px; }
p.sub { color: #7a766e; margin: 0 0 14px; font-size: 13px; }
.tabs { position: sticky; top: 0; z-index: 5; display: flex; gap: 6px; flex-wrap: wrap;
        padding: 10px 0 12px; background: #fbfaf8; border-bottom: 1px solid #efeae2; margin-bottom: 18px; }
.tab { font: inherit; font-size: 14px; padding: 6px 14px; border-radius: 999px; cursor: pointer;
       border: 1px solid #e0dbd2; background: #fff; color: #4a463f; }
.tab:hover { border-color: #c9c2b6; }
.tab.on { background: #1c1b19; border-color: #1c1b19; color: #fff; }
.blk { margin: 20px 0 24px; }
h3 { font-size: 15px; margin: 0 0 6px; display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; }
h3 .meta { color: #99948a; font-weight: 400; font-size: 12px; font-family: ui-monospace, monospace; }
.code { white-space: pre-wrap; word-break: break-word; background: #fff; border: 1px solid #e8e4dc;
        border-radius: 8px; padding: 14px 16px; margin: 0 0 6px;
        font: 13.5px/1.85 ui-monospace, "Noto Sans Mono CJK SC", monospace; }
mark { background: #ffe9a8; box-shadow: inset 0 -2px 0 #f0c36d; border-radius: 2px; padding: 0 1px; }
.diff { color: #8a6d1f; font-size: 12px; margin: 0; }
.diff code { background: #fdf6e3; border-radius: 3px; padding: 0 3px; }
.diff .none { color: #b0aba1; }
"""

    JS = """
const DATA = __PAYLOAD__;
const KEYS = __KEYS__;
// 形态写进 URL hash：换标签不会丢，#linux 这样的地址还能直接发给别人
let cur = KEYS.includes(location.hash.slice(1)) ? location.hash.slice(1) : KEYS[0];
function render() {
  for (const k of KEYS) document.querySelector('[data-k="' + k + '"]').classList.toggle('on', k === cur);
  const d = DATA[cur] || {};
  for (const b of Object.keys(d)) {
    document.getElementById('c-' + b).innerHTML = d[b].html;
    document.getElementById('m-' + b).textContent = d[b].meta;
    document.getElementById('d-' + b).innerHTML = d[b].diff;
  }
}
document.querySelectorAll('.tab').forEach(el => el.addEventListener('click', () => {
  cur = el.dataset.k;
  history.replaceState(null, '', '#' + cur);
  render();
}));
window.addEventListener('hashchange', () => {
  const k = location.hash.slice(1);
  if (KEYS.includes(k)) { cur = k; render(); }
});
render();
"""
    JS = JS.replace("__PAYLOAD__", payload).replace("__KEYS__", json.dumps(names, ensure_ascii=False))

    now = __import__("datetime").datetime.now().strftime("%Y-%m-%d %H:%M")
    doc = f"""<!doctype html><html lang="zh"><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>提示词成品预览 · android-agent</title><style>{CSS}</style>
<h1>最终拼出来的提示词</h1>
<p class="sub">全部从代码渲染（<code>cargo run -q --example dump_prompt …</code>），没碰设备 · 生成于 {now}<br>
点标签切换形态，内容在<b>同一个位置</b>换；<mark>黄色</mark> = 这一处是这个形态独有的说法。</p>
<div class="tabs">{tabs}</div>
{body}
<p class="sub">可缓存前缀合计（提示词 + 工具表）：{total_row}</p>
<script>{JS}</script>
</html>"""
    out = f"{DIR}/index.html"
    open(out, "w").write(doc)
    print("written", out, len(doc), "bytes")


if __name__ == "__main__":
    main()
