//! 页面里注入的 JS：**只有这一份**。
//!
//! 手机（Kotlin 的无头 WebView）和桌面（CDP）是两套宿主，但页面里那段逻辑必须
//! 一模一样 —— 各写一份就会漂：动作表漂过一次（`new_tab` 把 URL 吃掉、模型对着
//! 白屏截了三张图，见 docs/08），JS 漂了更隐蔽（点不动、抽不出正文，只在真机上
//! 才看得出来）。所以动作级的 JS 全收在核心这一侧：
//!
//! - 桌面：`cdp.rs` 直接调 [`build`]
//! - 手机：Kotlin 通过 uniffi 的 `AgentCore.browserJs(kind, params)` 取
//!   （`ffi.rs`）—— 它那边只剩「往哪只 WebView 里注」这层宿主逻辑
//!
//! 每个 kind 是一段自包含的 IIFE，最后 `JSON.stringify(...)` 回结果；
//! 参数从 `params`（一个 JSON 对象）里取。几个坑都写在代码里了
//! （React 的受控输入、框架的变更通知、事件序列、`target=_blank`）。

use serde_json::Value;

/// 骨架树给模型的节点上限（它会进历史，按钱算）
const BACKBONE_MAX_NODES: usize = 220;

/// 每页开始加载时把 `target=_blank` 掰成 `_self`。
///
/// 不掰的后果：点一个「在新标签打开」的链接，新页面开在**我们管不到**的地方 ——
/// 手机上是一只新 WebView（不在 `tabs` 里），桌面上是一个新 target。模型看到的是
/// 「点了没反应」，然后对着同一屏截三张图。
const TARGET_FIX_JS: &str = r#"
(function() {
  if (window.__agentTargetFix) return;
  window.__agentTargetFix = 1;
  document.addEventListener('click', function(e) {
    var a = e.target && e.target.closest && e.target.closest('a[target]');
    if (a && a.target && a.target !== '_self') a.target = '_self';
  }, true);
})()
"#;

/// 支持哪些 kind（`AgentCore.browserJs` 和测试都看它）
pub const KINDS: &[&str] = &[
    "dom_sig",
    "page_info",
    "backbone",
    "readable",
    "click_sel",
    "click_xy",
    "type",
    "get_text",
    "scroll",
    "press",
    "find_elements",
    "target_fix",
];

/// 造一段页面 JS。`params` 是 JSON 对象（没有参数就给 `{}` 或空串）。
///
/// 参数从 JSON 里取而不是按位置传：两边的调用点（Rust / Kotlin）都不用记顺序，
/// 加参数也不会把旧调用点弄坏。
pub fn build(kind: &str, params: &str) -> Result<String, String> {
    let p: Value = if params.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(params).map_err(|e| format!("参数不是 JSON: {e}"))?
    };
    let s = |k: &str| -> Result<String, String> {
        p.get(k)
            .and_then(|v| v.as_str())
            .map(|v| v.to_string())
            .ok_or_else(|| format!("{kind} 缺参数 {k}"))
    };
    let opt = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let num = |k: &str, d: i64| p.get(k).and_then(|v| v.as_i64()).unwrap_or(d);

    Ok(match kind {
        "dom_sig" => DOM_SIG_JS.to_string(),
        "page_info" => PAGE_INFO_JS.to_string(),
        "readable" => READABLE_JS.to_string(),
        "target_fix" => TARGET_FIX_JS.to_string(),
        "backbone" => backbone_js(num("max_depth", 5).clamp(1, 10) as usize),
        "click_sel" => click_sel_js(&s("selector")?),
        "click_xy" => click_xy_js(
            p.get("x")
                .and_then(|v| v.as_i64())
                .ok_or("click_xy 缺参数 x")?,
            p.get("y")
                .and_then(|v| v.as_i64())
                .ok_or("click_xy 缺参数 y")?,
        ),
        "type" => type_js(&s("selector")?, &opt("text")),
        "get_text" => get_text_js(&opt("selector")),
        "scroll" => scroll_js(&opt("direction"), num("amount", 600), &opt("selector")),
        "press" => press_js(&s("key")?),
        "find_elements" => find_elements_js(&s("selector")?),
        other => return Err(format!("不认识的 JS: {other}（支持：{}）", KINDS.join(" "))),
    })
}

/// 把一段字符串安全地塞进 JS 的单引号字面量里。
pub(crate) fn js_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out
}

/// DOM 指纹：元素个数 + 可见文字长度 + 滚动高度。变了就说明还没稳。
const DOM_SIG_JS: &str = "(function(){var b=document.body;if(!b)return 'null';\
     return document.querySelectorAll('*').length+':'+(b.innerText||'').length+':'+\
     (document.documentElement.scrollHeight||0);})()";

const PAGE_INFO_JS: &str = r##"
(function() {
  return JSON.stringify({
    url: location.href,
    title: document.title,
    ready: document.readyState,
    scrollY: Math.round(window.scrollY),
    scrollHeight: document.documentElement.scrollHeight,
    innerWidth: window.innerWidth,
    innerHeight: window.innerHeight,
    elements: document.querySelectorAll('*').length
  });
})()
"##;

/// 点一个元素的**完整鼠标事件序列**。
///
/// 只调 `el.click()` 不够：很多站点在 `mousedown`/`mouseup` 上做状态
/// （下拉菜单、拖拽、自定义控件），合成 click 到不了它们。
const CLICK_EVENTS: &str = r##"
  var mk = function(type, bubbles) {
    return new MouseEvent(type, {bubbles: bubbles, cancelable: true, view: window});
  };
  el.dispatchEvent(mk('mouseover', true));
  el.dispatchEvent(mk('mouseenter', false));
  el.dispatchEvent(mk('mousemove', true));
  el.dispatchEvent(mk('mousedown', true));
  el.dispatchEvent(mk('mouseup', true));
  el.click();
  el.dispatchEvent(mk('mouseleave', false));
  el.dispatchEvent(mk('mouseout', true));
"##;

fn click_sel_js(selector: &str) -> String {
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var list = document.querySelectorAll('{q}');
  if (!list.length) return JSON.stringify({{error: '没找到元素: {q}'}});
  var el = list[0];
  if (el.disabled) return JSON.stringify({{error: '元素是 disabled 的'}});
  el.scrollIntoView({{block: 'center'}});
  {CLICK_EVENTS}
  return JSON.stringify({{clicked: true, matched: list.length, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)}});
}})()
"##
    )
}

fn click_xy_js(x: i64, y: i64) -> String {
    format!(
        r##"
(function() {{
  var el = document.elementFromPoint({x}, {y});
  if (!el) return JSON.stringify({{error: '坐标 ({x}, {y}) 上没有元素'}});
  {CLICK_EVENTS}
  return JSON.stringify({{clicked: true, tag: el.tagName, text: (el.innerText || el.value || '').substring(0, 100)}});
}})()
"##
    )
}

/// 输入文本。三件事必须都做，少一件就有站点点不动：
/// 1. **native setter** —— React/Vue 在 `input.value` 上装了自己的 setter，
///    直接赋值它看不见（框里有字、state 是空的）；
/// 2. **逐字符 keydown/input/keyup** —— 靠监听按键的搜索框只认事件；
/// 3. **框架同步** —— Angular 认 `$apply`，Vue 认 `$forceUpdate`。
fn type_js(selector: &str, text: &str) -> String {
    let q = js_quote(selector);
    let val = js_quote(text);
    format!(
        r##"
(function() {{
  var list = document.querySelectorAll('{q}');
  if (!list.length) return JSON.stringify({{error: '没找到元素: {q}'}});
  var el = list[0];
  var val = '{val}';
  el.focus();
  if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') {{
    var proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(proto, 'value');
    if (setter && setter.set) setter.set.call(el, val); else el.value = val;
  }} else if (el.isContentEditable) {{
    el.innerText = val;
  }}
  for (var i = 0; i < val.length; i++) {{
    var c = val[i];
    el.dispatchEvent(new KeyboardEvent('keydown', {{key: c, bubbles: true}}));
    el.dispatchEvent(new KeyboardEvent('keypress', {{key: c, bubbles: true}}));
    el.dispatchEvent(new InputEvent('input', {{data: c, inputType: 'insertText', bubbles: true}}));
    el.dispatchEvent(new KeyboardEvent('keyup', {{key: c, bubbles: true}}));
  }}
  el.dispatchEvent(new Event('input', {{bubbles: true}}));
  el.dispatchEvent(new Event('change', {{bubbles: true}}));
  try {{
    if (window.angular) {{
      var ng = window.angular.element(el);
      var sc = (ng.scope && ng.scope()) || (ng.injector && ng.injector().get('$rootScope'));
      if (sc && !sc.$$phase) sc.$apply();
    }}
  }} catch (e) {{}}
  try {{ if (el.__vue__) el.__vue__.$forceUpdate(); }} catch (e) {{}}
  el.dispatchEvent(new FocusEvent('blur', {{bubbles: true, relatedTarget: null}}));
  el.blur();
  return JSON.stringify({{typed: true, matched: list.length, length: val.length}});
}})()
"##
    )
}

fn get_text_js(selector: &str) -> String {
    if selector.is_empty() {
        return r##"
(function() {
  var t = (document.body.innerText || '').trim();
  return JSON.stringify({text: t.substring(0, 40000), length: t.length, url: location.href, title: document.title});
})()
"##
        .to_string();
    }
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var el = document.querySelector('{q}');
  if (!el) return JSON.stringify({{error: '没找到元素: {q}'}});
  var t = (el.innerText || el.textContent || '').trim();
  return JSON.stringify({{text: t.substring(0, 40000), length: t.length}});
}})()
"##
    )
}

/// 滚动。先试 window；页面没动说明滚动在某个内层容器里（现在满大街都是），
/// 就找「overflow 是 auto/scroll 且内容比容器高」里面**面积最大**的那个 ——
/// 面积最大基本就是主内容区，不是侧边的小列表。
fn scroll_js(direction: &str, amount: i64, selector: &str) -> String {
    let px = if direction == "up" { -amount } else { amount };
    let sel = if selector.is_empty() {
        "null".to_string()
    } else {
        format!("'{}'", js_quote(selector))
    };
    format!(
        r##"
(function() {{
  var sel = {sel};
  var px = {px};
  if (sel) {{
    var el = document.querySelector(sel);
    if (!el) return JSON.stringify({{error: '没找到元素: ' + sel}});
    el.scrollBy(0, px);
    return JSON.stringify({{scrolled: true, element: sel, scrollY: Math.round(el.scrollTop), scrollHeight: el.scrollHeight}});
  }}
  var before = window.scrollY;
  window.scrollBy(0, px);
  if (window.scrollY !== before || document.documentElement.scrollHeight <= window.innerHeight + 2) {{
    return JSON.stringify({{scrolled: true, element: 'window', scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight}});
  }}
  var best = null, bestArea = 0;
  var all = document.body.querySelectorAll('*');
  for (var i = 0; i < all.length; i++) {{
    var e = all[i];
    var st = getComputedStyle(e);
    if ((st.overflowY === 'auto' || st.overflowY === 'scroll') && e.scrollHeight > e.clientHeight + 5) {{
      var area = e.clientWidth * e.clientHeight;
      if (area > bestArea) {{ best = e; bestArea = area; }}
    }}
  }}
  if (best) {{
    best.scrollBy(0, px);
    return JSON.stringify({{scrolled: true, element: best.tagName.toLowerCase(), scrollY: Math.round(best.scrollTop), scrollHeight: best.scrollHeight}});
  }}
  return JSON.stringify({{scrolled: false, scrollY: Math.round(window.scrollY), scrollHeight: document.documentElement.scrollHeight}});
}})()
"##
    )
}

/// 按键。合成事件不会引发「隐式提交」和「Tab 移焦」，这两件事自己补。
fn press_js(key: &str) -> String {
    let name = js_quote(key);
    format!(
        r##"
(function() {{
  var name = '{name}';
  var CODES = {{
    Enter: 13, Tab: 9, Escape: 27, Backspace: 8, Delete: 46, ' ': 32,
    ArrowUp: 38, ArrowDown: 40, ArrowLeft: 37, ArrowRight: 39,
    PageUp: 33, PageDown: 34, Home: 36, End: 35
  }};
  var code = CODES[name];
  if (code === undefined) return JSON.stringify({{error: '不支持的按键: ' + name + '（支持 Enter Tab Escape Backspace Delete Space Arrow* PageUp PageDown Home End）'}});
  var el = document.activeElement || document.body;
  function mk(type) {{
    var e = new KeyboardEvent(type, {{
      key: name, code: name, keyCode: code, which: code,
      bubbles: true, cancelable: true, view: window
    }});
    try {{
      Object.defineProperty(e, 'keyCode', {{get: function() {{ return code; }}}});
      Object.defineProperty(e, 'which', {{get: function() {{ return code; }}}});
    }} catch (err) {{}}
    return e;
  }}
  var target = (el === document.body) ? document : el;
  target.dispatchEvent(mk('keydown'));
  target.dispatchEvent(mk('keypress'));
  target.dispatchEvent(mk('keyup'));

  var acted = '';
  if (name === 'Enter') {{
    var form = el.form || (el.closest && el.closest('form'));
    if (form) {{
      try {{
        if (typeof form.requestSubmit === 'function') {{ form.requestSubmit(); acted = '已提交表单'; }}
        else {{
          var b = form.querySelector('button[type=submit], input[type=submit]');
          if (b) {{ b.click(); acted = '已点提交键'; }}
        }}
      }} catch (err) {{ acted = '提交表单失败: ' + err.message; }}
    }}
  }}
  if (name === 'Tab') {{
    var all = document.querySelectorAll('a[href], button, input, select, textarea, [tabindex]');
    var list = [];
    for (var i = 0; i < all.length; i++) {{
      var e2 = all[i];
      if (e2.disabled || e2.tabIndex < 0) continue;
      var st = getComputedStyle(e2);
      if (st.display === 'none' || st.visibility === 'hidden') continue;
      list.push(e2);
    }}
    var at = list.indexOf(el);
    if (list.length) {{
      var next = list[(at + 1 + list.length) % list.length];
      try {{ next.focus(); acted = '焦点移到 <' + next.tagName + '>'; }} catch (err) {{}}
    }}
  }}
  if (name === 'Escape' && document.activeElement && document.activeElement.blur) {{
    try {{ document.activeElement.blur(); acted = '已取消焦点'; }} catch (err) {{}}
  }}
  return JSON.stringify({{pressed: name, tag: el.tagName, acted: acted}});
}})()
"##
    )
}

/// 列出一个 selector 匹配到的元素。坐标是**视口坐标**（`click` 的 x/y 要的
/// 正是它），另给 pageY 让模型知道要滚多远。
fn find_elements_js(selector: &str) -> String {
    let q = js_quote(selector);
    format!(
        r##"
(function() {{
  var all = document.querySelectorAll('{q}');
  var sx = window.scrollX || 0, sy = window.scrollY || 0;
  var items = [];
  var limit = Math.min(all.length, 20);
  for (var i = 0; i < limit; i++) {{
    var el = all[i];
    var r = el.getBoundingClientRect();
    var st = getComputedStyle(el);
    items.push({{
      i: i,
      tag: el.tagName,
      text: (el.innerText || el.value || '').replace(/\s+/g, ' ').trim().substring(0, 80),
      href: (el.href || '').substring(0, 200),
      x: Math.round(r.x + r.width / 2),
      y: Math.round(r.y + r.height / 2),
      w: Math.round(r.width), h: Math.round(r.height),
      pageY: Math.round(r.top + sy),
      visible: r.width > 0 && r.height > 0 && r.bottom > 0 && r.top < window.innerHeight &&
               st.display !== 'none' && st.visibility !== 'hidden'
    }});
  }}
  return JSON.stringify({{count: all.length, shown: limit, items: items, scrollY: Math.round(sy)}});
}})()
"##
    )
}

/// 页面骨架：不把 HTML 丢给模型（太大且全是噪音），而是抽一棵**可见元素树**。
///
/// 1. 挑候选：可见、够大、且「有内容」（语义元素，或自己带文字且没有可见子元素）；
/// 2. **合并同位置的包装层**（±8px）—— 现代页面上百个无意义 div 靠这步消失；
/// 3. 每个节点算一个**唯一的短 selector**（这是模型能"点"的前提）；
/// 4. 内外层文字一样时只留内层；
/// 5. 按面积建树、剪掉空壳、按 max_depth 截断；
/// 6. 直接拼成缩进文本（模型读的就是它），带上 rect。
fn backbone_js(max_depth: usize) -> String {
    BACKBONE_JS
        .replace("__MAX_DEPTH__", &max_depth.to_string())
        .replace("__LIMIT__", &BACKBONE_MAX_NODES.to_string())
}

const BACKBONE_JS: &str = r##"
(function() {
  var MAX_DEPTH = __MAX_DEPTH__;
  var MERGE = 8;
  var MIN = 6;
  var LIMIT = __LIMIT__;

  function vis(el) {
    var st = getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden' || parseFloat(st.opacity) === 0) return false;
    var r = el.getBoundingClientRect();
    return r.width >= MIN && r.height >= MIN;
  }
  var SEM = {IMG:1, SVG:1, VIDEO:1, CANVAS:1, INPUT:1, TEXTAREA:1, SELECT:1, BUTTON:1, A:1, IFRAME:1, AUDIO:1};
  var CLICK = {A:1, BUTTON:1, INPUT:1, TEXTAREA:1, SELECT:1, SUMMARY:1};
  var SKIP = {SCRIPT:1, STYLE:1, NOSCRIPT:1, BR:1, HR:1, META:1, LINK:1, TEMPLATE:1, HEAD:1};
  function text(el) { return ((el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim()); }
  function isLeaf(el) {
    if (SKIP[el.tagName]) return false;
    if (SEM[el.tagName]) return true;
    var kids = el.children;
    for (var i = 0; i < kids.length; i++) {
      if (!SKIP[kids[i].tagName] && vis(kids[i])) return false;
    }
    return true;
  }
  function near(a, b) {
    return Math.abs(a.left - b.left) <= MERGE && Math.abs(a.top - b.top) <= MERGE &&
           Math.abs(a.right - b.right) <= MERGE && Math.abs(a.bottom - b.bottom) <= MERGE;
  }
  // selector **必须唯一** —— 模型拿它去点，指到十几个元素上就点不动了。
  function uniq(s) { try { return document.querySelectorAll(s).length === 1; } catch (e) { return false; } }
  // 数字开头的 id **不能**写成 `#50051583`（CSS 里非法，querySelector 直接抛）
  function idSel(id) {
    if (/^[A-Za-z_-][A-Za-z0-9_-]*$/.test(id)) return '#' + id;
    return '[id="' + String(id).replace(/"/g, '\\"') + '"]';
  }
  function sel(el) {
    if (el.id) { var byId = idSel(el.id); if (uniq(byId)) return byId; }
    var parts = [], cur = el, guard = 0;
    while (cur && cur !== document.body && cur !== document.documentElement && guard++ < 8) {
      if (cur.id) { parts.unshift(idSel(cur.id)); break; }
      var seg = cur.tagName.toLowerCase();
      var p = cur.parentElement;
      if (p) {
        var same = 0, idx = 0;
        for (var i = 0; i < p.children.length; i++) {
          if (p.children[i].tagName === cur.tagName) { same++; if (p.children[i] === cur) idx = same; }
        }
        if (same > 1) seg += ':nth-of-type(' + idx + ')';
      }
      parts.unshift(seg);
      cur = p;
      if (parts.length >= 2 && uniq(parts.join(' > '))) return parts.join(' > ');
    }
    return parts.join(' > ');
  }

  var all = document.body.querySelectorAll('*');
  var nodes = [];
  for (var i = 0; i < all.length; i++) {
    var el = all[i];
    if (!vis(el) || !isLeaf(el)) continue;
    var r = el.getBoundingClientRect();
    var t = text(el);
    var interesting = t.length > 1 || SEM[el.tagName] || (el.tagName === 'A' && el.href);
    if (!interesting) continue;
    // 合并同位置的包装层：一路提升到最外层的"外壳"。
    // **碰见可点的语义元素就停** —— 继续往上合并的话 selector 会指到一个
    // div/td/span 上，而点 wrapper 是点不出反应的。
    var rep = el, rr = r;
    var up = el.parentElement;
    while (!CLICK[rep.tagName] && up && up !== document.body && up !== document.documentElement &&
           vis(up) && near(rr, up.getBoundingClientRect())) {
      rep = up; rr = up.getBoundingClientRect();
      up = up.parentElement;
    }
    var dup = false;
    for (var k = 0; k < nodes.length; k++) { if (nodes[k].el === rep) { dup = true; break; } }
    if (dup) continue;
    nodes.push({el: rep, rect: rr, area: rr.width * rr.height, text: t.substring(0, 200)});
  }

  // 外层和内层文字一模一样时只留内层（三层包装一份标题 = 三倍噪音，
  // 而且**能点的通常是内层那个 a**）
  var kept = [];
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i], dupText = false;
    if (n.text) {
      for (var j = 0; j < nodes.length; j++) {
        if (j !== i && nodes[j].el !== n.el && n.el.contains(nodes[j].el) && nodes[j].text === n.text) { dupText = true; break; }
      }
    }
    if (!dupText) kept.push(n);
  }
  nodes = kept;

  nodes.sort(function(a, b) { return b.area - a.area; });
  function contains(a, b) { return a.el !== b.el && a.el.contains(b.el); }
  var roots = [];
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i], parent = null, best = Infinity;
    for (var j = 0; j < i; j++) {
      if (contains(nodes[j], n) && nodes[j].area < best) { parent = nodes[j]; best = nodes[j].area; }
    }
    if (parent) { (parent.kids = parent.kids || []).push(n); } else { roots.push(n); }
  }
  function prune(n) {
    (n.kids || []).forEach(prune);
    if ((!n.kids || n.kids.length === 1) && !n.text) {
      var c = n.kids && n.kids[0];
      if (c) { n.el = c.el; n.rect = c.rect; n.text = c.text; n.kids = c.kids; }
    }
    if (n.kids) n.kids = n.kids.filter(function(k) { return (k.kids && k.kids.length) || k.text || SEM[k.el.tagName]; });
  }
  function textOf(n) { return (n.text || '').substring(0, 120); }
  roots.forEach(prune);
  roots = roots.filter(function(r) { return textOf(r) || SEM[r.el.tagName] || (r.kids && r.kids.length); });

  var lines = [], count = 0, depth = 0;
  function emit(n, d, pad) {
    if (count >= LIMIT) return;
    count++; if (d > depth) depth = d;
    var el = n.el, r = n.rect;
    var head = pad + '<' + el.tagName.toLowerCase() + '>';
    if (el.id) head += ' #' + el.id;
    var cls = (typeof el.className === 'string' ? el.className : '').trim().split(/\s+/).filter(Boolean).slice(0, 2);
    if (cls.length) head += ' .' + cls.join('.');
    head += ' | ' + sel(el);
    head += ' | ' + Math.round(r.left) + ',' + Math.round(r.top) + ' ' + Math.round(r.width) + 'x' + Math.round(r.height);
    lines.push(head);
    var t = textOf(n);
    if (t) lines.push(pad + '  "' + t + '"');
    if (el.tagName === 'A' && el.href) {
      try {
        var u = new URL(el.href);
        var host = (u.host === location.host) ? '' : (u.host + ' ');
        lines.push(pad + '  -> ' + host + u.pathname + (u.search || ''));
      }
      catch (e) { lines.push(pad + '  -> ' + el.href.substring(0, 100)); }
    }
    if (el.tagName === 'IMG') lines.push(pad + '  img ' + (el.naturalWidth || 0) + 'x' + (el.naturalHeight || 0) + ' ' + (el.currentSrc || el.src || '').substring(0, 100));
    if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
      var tp = el.type || el.tagName.toLowerCase();
      var v = el.type === 'password' ? '[redacted]' : (el.value || '');
      lines.push(pad + '  input ' + tp + (v ? ' value="' + String(v).substring(0, 60) + '"' : '') + (el.placeholder ? ' placeholder="' + el.placeholder.substring(0, 60) + '"' : ''));
    }
    if (d >= MAX_DEPTH) return;
    (n.kids || []).forEach(function(k) { emit(k, d + 1, pad + '  '); });
  }
  roots.forEach(function(r) { emit(r, 1, ''); });
  if (!lines.length) lines.push('（这一屏没有可读的可见元素）');
  return JSON.stringify({text: lines.join('\n'), nodes: count, depth: depth});
})()
"##;

/// 抽正文（Readability 的简化版，只留真管用的那几条）。
const READABLE_JS: &str = r##"
(function() {
  var SKIP_TAGS = {NAV:1, FOOTER:1, ASIDE:1, HEADER:1, FORM:1, SCRIPT:1, STYLE:1, NOSCRIPT:1, IFRAME:1, BUTTON:1, SELECT:1};
  var JUNK = /(^|[\s_-])(nav|menu|sidebar|footer|header|comment|share|sharing|social|promo|ads?|advert|breadcrumb|cookie|consent|privacy|legal|terms|gdpr|ccpa|disclaimer|banner|subscribe|newsletter|signup|login|paywall|related|recommend|popup|modal|toolbar|masthead)([\s_-]|$)/i;
  var POS = /(^|[\s_-])(article|body|content|entry|main|page|post|story|text|blog|read)([\s_-]|$)/i;
  var NEG = /(^|[\s_-])(nav|menu|sidebar|footer|comment|promo|related|recommend|popular|banner|cookie|consent|privacy|legal|terms|gdpr|subscribe|newsletter|signup|login|paywall|popup|modal|advert|ads?|social|share|widget|tool|tags)([\s_-]|$)/i;
  function t(el) { return (el.innerText || el.textContent || '').trim(); }
  function names(el) {
    return (typeof el.className === 'string' ? el.className : '') + ' ' +
           (el.id || '') + ' ' + (el.getAttribute ? (el.getAttribute('role') || '') : '');
  }
  function inChrome(el) {
    try { return !!el.closest('nav, footer, aside, header, form, [aria-hidden="true"]'); } catch (e) { return false; }
  }
  // 自己或**上面 6 层**里任何一层像周边，就当周边。只查自己不够 ——
  // 周边信息常常长这样：一堆正常 `<p>` 包在一个 `#...CookieNotice` 壳里。
  function junk(el) {
    var cur = el, depth = 0;
    while (cur && cur !== document.body && depth++ < 6) {
      if (JUNK.test(names(cur))) return true;
      cur = cur.parentElement;
    }
    return false;
  }
  function hidden(el) {
    var st = getComputedStyle(el);
    return st.display === 'none' || st.visibility === 'hidden';
  }
  function bad(el) { return SKIP_TAGS[el.tagName] || inChrome(el) || hidden(el) || junk(el); }
  function weight(el) {
    var s = names(el);
    if (NEG.test(s)) return 0.15;
    if (POS.test(s)) return 2;
    return 1;
  }

  var h1 = null;
  var hs = document.querySelectorAll('h1');
  for (var i = 0; i < hs.length; i++) {
    if (!bad(hs[i]) && t(hs[i]).length > 3) { h1 = hs[i]; break; }
  }

  var cands = document.querySelectorAll('article, main, [role=main], div, section, td');
  var best = null, bestScore = 0;
  for (var i = 0; i < cands.length; i++) {
    var c = cands[i];
    if (bad(c)) continue;
    var ps = c.querySelectorAll('p');
    if (!ps.length) continue;
    var s = 0;
    for (var j = 0; j < ps.length; j++) {
      var p = ps[j];
      if (bad(p)) continue;
      var pt = t(p);
      if (pt.length < 25) continue;
      s += 1 + Math.min(pt.length / 100, 3);
    }
    if (s <= 0) continue;
    s *= weight(c);
    if (h1 && c.contains(h1)) s += 20;
    var all = t(c);
    var linkLen = 0;
    var as = c.querySelectorAll('a');
    for (var k = 0; k < as.length; k++) linkLen += t(as[k]).length;
    if (all.length > 0 && linkLen / all.length > 0.5) s *= 0.3;
    s += Math.min(all.length / 1000, 5);
    if (s > bestScore) { bestScore = s; best = c; }
  }

  var host = best, source = 'best-block';
  if (!host || bestScore < 6) { host = document.body; source = 'body(fallback)'; }

  var chunks = [];
  var nodes = host.querySelectorAll('h1, h2, h3, p, li, blockquote, pre, figcaption');
  for (var i = 0; i < nodes.length; i++) {
    var n = nodes[i];
    if (bad(n)) continue;
    if (n.tagName === 'LI' && n.querySelector('p')) continue;
    var txt = t(n);
    if (!txt || txt.length < 2) continue;
    if (n.tagName === 'PRE') { chunks.push('```\n' + (n.innerText || '') + '\n```'); continue; }
    if (n.tagName === 'H1' || n.tagName === 'H2' || n.tagName === 'H3') { chunks.push('## ' + txt); continue; }
    if (chunks.length > 400) break;
    var links = n.querySelectorAll('a[href]');
    if (links.length === 1 && t(links[0]).length > txt.length * 0.6) {
      try {
        var u = new URL(links[0].href);
        var where = (u.host === location.host) ? (u.pathname + (u.search || '')) : (u.host + u.pathname);
        chunks.push(txt + '  [' + where + ']');
        continue;
      } catch (e) {}
    }
    chunks.push(txt);
  }
  var out = [];
  for (var i = 0; i < chunks.length; i++) {
    if (i > 0 && chunks[i] === chunks[i - 1]) continue;
    out.push(chunks[i]);
  }
  var body = out.join('\n\n');
  return JSON.stringify({text: body.substring(0, 40000), length: body.length, source: source, title: document.title, url: location.href});
})()
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个 kind 都得能造出来，参数缺失要报清楚
    #[test]
    fn every_kind_builds_and_missing_params_are_named() {
        // 每个 kind 一组能用的参数（新增 kind 要在这儿补一条，下面那句 assert 盯着）
        let ok: &[(&str, &str)] = &[
            ("dom_sig", "{}"),
            ("page_info", "{}"),
            ("backbone", "{}"),
            ("readable", "{}"),
            ("target_fix", "{}"),
            ("click_sel", r#"{"selector":".a"}"#),
            ("click_xy", r#"{"x":1,"y":2}"#),
            ("type", r#"{"selector":".a","text":"x"}"#),
            ("get_text", "{}"),
            ("scroll", "{}"),
            ("press", r#"{"key":"Enter"}"#),
            ("find_elements", r#"{"selector":"a"}"#),
        ];
        assert_eq!(ok.len(), KINDS.len(), "kind 和测试用例对不上了");
        for (k, params) in ok {
            let js = build(k, params).unwrap_or_else(|e| panic!("{k}: {e}"));
            assert!(
                js.trim_start().starts_with("(function"),
                "{k} 的结果不是自包含的 IIFE：{js}"
            );
        }
        for (k, params) in [
            ("click_sel", "{}"),
            ("click_xy", "{}"),
            ("type", "{}"),
            ("press", "{}"),
            ("find_elements", "{}"),
        ] {
            let e = build(k, params).unwrap_err();
            assert!(e.contains("缺参数"), "{k}: {e}");
        }
        assert!(build("nope", "{}").unwrap_err().contains("不认识的 JS"));
        assert!(build("backbone", "{oops")
            .unwrap_err()
            .contains("不是 JSON"));
    }

    /// 骨架 JS 的两个占位符都得被替换掉（留着的话页面里是语法错误）
    #[test]
    fn backbone_js_has_no_placeholders_left() {
        let js = build("backbone", r#"{"max_depth":3}"#).unwrap();
        assert!(
            !js.contains("__MAX_DEPTH__") && !js.contains("__LIMIT__"),
            "{js}"
        );
        assert!(js.contains("var MAX_DEPTH = 3;"));
        assert!(js.contains(&format!("var LIMIT = {BACKBONE_MAX_NODES};")));
        // 深度夹在 1..10
        assert!(build("backbone", r#"{"max_depth":99}"#)
            .unwrap()
            .contains("var MAX_DEPTH = 10;"));
        assert!(build("backbone", r#"{"max_depth":0}"#)
            .unwrap()
            .contains("var MAX_DEPTH = 1;"));
        // 不给 max_depth 用默认值
        assert!(build("backbone", "{}")
            .unwrap()
            .contains("var MAX_DEPTH = 5;"));
    }

    /// **Kotlin 那边不许再有页面 JS**（"只有一份"这条规矩的守门测试）。
    ///
    /// 判据是页面 DOM 的 API：`querySelectorAll` / `getComputedStyle` 这些只可能
    /// 出现在注入的 JS 里。宿主逻辑（`evaluateJavascript` 的踢一脚+轮询、截图那套
    /// 问页面要滚动位置）不算 —— 它们和桌面不共享。
    #[test]
    fn kotlin_has_no_page_js_left() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../android/app/src/main/java/dev/vihv/agent/BrowserController.kt");
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("没有 {}（不在完整仓库里？跳过）", path.display());
            return;
        };
        for api in ["querySelectorAll", "getComputedStyle", "document.title"] {
            assert!(
                !text.contains(api),
                "BrowserController.kt 里又出现了页面 JS（{api}）—— 应该向核心要 \
                 （`AgentCore.browserJs`），见 docs/08 第 15 节"
            );
        }
    }

    /// 选择器 / 文本里的引号和换行不能把注入的 JS 拼坏（引号逃逸）
    #[test]
    fn js_quote_escapes_what_would_break_out() {
        assert_eq!(js_quote("a'b"), "a\\'b");
        assert_eq!(js_quote("a\\b"), "a\\\\b");
        assert_eq!(js_quote("a\nb"), "a\\nb");
        assert_eq!(js_quote("a\u{2028}b"), "a\\u2028b");
        // 拼进 JS 之后，那个字面量里不该出现裸的引号
        let js = build("click_sel", r#"{"selector":"div[data-x='y']"}"#).unwrap();
        assert!(
            js.contains(r#"querySelectorAll('div[data-x=\'y\']')"#),
            "{js}"
        );
    }
}
