//! 一个**宽松**的 HTML 解析器：HTML → 一棵够用的树。
//!
//! 不按 HTML5 标准走（那是 html5ever：带上它和 CSS 选择器引擎，`.so` 要胖 1.5MB）。
//! 这里只求把「读字」需要的结构弄对：
//!
//! - 标签、属性、文字、注释；`<script>` / `<style>` / `<title>` 这类元素的内容原样当文字，
//!   不往里找标签（不然 JS 里的 `"<div>"` 字符串会被当成元素）
//! - 常见的省略闭合：`<p>` 碰到块级元素自动结束，`<li>` / `<dt>` / `<dd>` / `<tr>` / `<td>`
//!   碰到同类自动结束 —— 老网页、手写网页几乎都这么写
//! - 对不上的结束标签直接忽略；没写 `<html>` / `<body>` 也照样能用
//! - 实体只解码常见的那几十个和数字实体（`&#20013;` / `&#x4e2d;`），认不出的原样保留
//!
//! 节点按文档顺序存在一个数组里（先序）：一个节点的子树就是它后面连续的一段
//! （[`Doc::end`]），判断「在不在某个元素里」不用往上爬。

/// 一个节点：元素或者文字。
#[derive(Debug, Clone)]
pub struct Node {
    /// 元素名（小写）；文字节点是空串
    pub name: String,
    /// 文字节点的内容（实体已解码）
    pub text: String,
    /// 只留认得的属性（见 [`KEEP_ATTRS`]），名字小写、值已解码
    pub attrs: Vec<(String, String)>,
    pub parent: usize,
    pub children: Vec<usize>,
}

impl Node {
    pub fn is_text(&self) -> bool {
        self.name.is_empty()
    }

    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }
}

/// 解析结果。`nodes[0]` 是文档根（名字 `#root`）。
#[derive(Debug)]
pub struct Doc {
    pub nodes: Vec<Node>,
    /// `end[i]`：节点 i 的子树在数组里结束的位置（不含）
    pub end: Vec<usize>,
}

impl Doc {
    /// `d` 是不是 `a` 自己或者在 `a` 里面
    pub fn contains(&self, a: usize, d: usize) -> bool {
        a <= d && d < self.end[a]
    }

    /// 子树里的所有节点（含自己）
    pub fn subtree(&self, n: usize) -> std::ops::Range<usize> {
        n..self.end[n]
    }

    /// 往上数第一个叫 `name` 的祖先（不含自己）
    pub fn ancestor(&self, n: usize, name: &str) -> Option<usize> {
        let mut cur = n;
        while cur != 0 {
            cur = self.nodes[cur].parent;
            if self.nodes[cur].name == name {
                return Some(cur);
            }
        }
        None
    }

    /// 第一个叫 `name` 的元素
    pub fn find(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    /// 子树里所有文字连起来（不折叠空白）
    pub fn text_of(&self, n: usize) -> String {
        self.subtree(n)
            .filter(|&i| self.nodes[i].is_text())
            .map(|i| self.nodes[i].text.as_str())
            .collect()
    }
}

/// 只留这些属性：读正文用得上的（链接、类名、隐藏、页面元信息）
const KEEP_ATTRS: &[&str] = &[
    "href",
    "class",
    "id",
    "role",
    "hidden",
    "aria-hidden",
    "style",
    "rel",
    "property",
    "name",
    "content",
    "itemprop",
    "datetime",
    "alt",
];

/// 没有内容、也没有结束标签的元素
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// 内容原样当文字的元素（里面不找标签）
const RAW: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes", "noscript",
];

/// 这些元素开始时，没结束的 `<p>` 自动结束
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
];

fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

/// 解析。不会失败：再乱的输入也给出一棵树。
pub fn parse(html: &str) -> Doc {
    let mut b = Builder {
        nodes: vec![Node {
            name: "#root".into(),
            text: String::new(),
            attrs: Vec::new(),
            parent: 0,
            children: Vec::new(),
        }],
        stack: vec![0],
    };
    let s = html.as_bytes();
    let mut i = 0;
    let mut text_from = 0;
    while i < s.len() {
        if s[i] != b'<' {
            i += 1;
            continue;
        }
        let next = s.get(i + 1).copied().unwrap_or(0);
        let markup = next.is_ascii_alphabetic() || next == b'/' || next == b'!' || next == b'?';
        if !markup {
            i += 1;
            continue;
        }
        b.text(&html[text_from..i]);
        if html[i..].starts_with("<!--") {
            i = find(s, i + 4, b"-->").map_or(s.len(), |e| e + 3);
        } else if next == b'!' || next == b'?' {
            // <!doctype …> / <![CDATA[…]]> / <?xml …?>：都不要
            i = find(s, i, b">").map_or(s.len(), |e| e + 1);
        } else if next == b'/' {
            let (name, after) = tag_name(html, i + 2);
            i = find(s, after, b">").map_or(s.len(), |e| e + 1);
            if !name.is_empty() {
                b.close(&name);
            }
        } else {
            let (name, after) = tag_name(html, i + 1);
            let (attrs, self_closing, after) = attributes(html, after);
            i = after;
            let raw = RAW.contains(&name.as_str());
            b.open(name.clone(), attrs, self_closing);
            if raw && !self_closing {
                // 原样到 `</name` 为止
                let close = format!("</{name}");
                let end = find_ci(s, i, close.as_bytes()).unwrap_or(s.len());
                b.text(&html[i..end]);
                b.close(&name);
                i = find(s, end, b">").map_or(s.len(), |e| e + 1);
            }
        }
        text_from = i;
    }
    b.text(&html[text_from.min(s.len())..]);

    let mut end: Vec<usize> = (1..=b.nodes.len()).collect();
    for n in (1..b.nodes.len()).rev() {
        let p = b.nodes[n].parent;
        end[p] = end[p].max(end[n]);
    }
    Doc {
        nodes: b.nodes,
        end,
    }
}

struct Builder {
    nodes: Vec<Node>,
    /// 还没结束的元素（栈顶是当前元素）
    stack: Vec<usize>,
}

impl Builder {
    fn top(&self) -> usize {
        *self.stack.last().unwrap_or(&0)
    }

    fn push_node(&mut self, n: Node) -> usize {
        let id = self.nodes.len();
        let parent = n.parent;
        self.nodes.push(n);
        self.nodes[parent].children.push(id);
        id
    }

    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let text = decode(raw);
        let parent = self.top();
        // 和前一个文字节点挨着（中间是被丢掉的注释）：并到一起
        if let Some(&last) = self.nodes[parent].children.last() {
            if self.nodes[last].is_text() {
                self.nodes[last].text.push_str(&text);
                return;
            }
        }
        self.push_node(Node {
            name: String::new(),
            text,
            attrs: Vec::new(),
            parent,
            children: Vec::new(),
        });
    }

    /// 栈里从顶往下找 `name`，碰到 `stop` 里的任何一个就不找了
    fn open_in_scope(&self, name: &str, stop: &[&str]) -> Option<usize> {
        for (k, &id) in self.stack.iter().enumerate().rev() {
            let n = &self.nodes[id].name;
            if n == name {
                return Some(k);
            }
            if stop.contains(&n.as_str()) {
                return None;
            }
        }
        None
    }

    fn pop_to(&mut self, k: usize) {
        self.stack.truncate(k.max(1));
    }

    fn open(&mut self, name: String, attrs: Vec<(String, String)>, self_closing: bool) {
        const SCOPE: &[&str] = &["table", "td", "th", "button", "#root"];
        // 省略闭合（浏览器同款的那几条）
        if CLOSES_P.contains(&name.as_str()) {
            if let Some(k) = self.open_in_scope("p", SCOPE) {
                self.pop_to(k);
            }
        }
        let auto: Option<(&[&str], &[&str])> = match name.as_str() {
            "li" => Some((&["li"], &["ul", "ol", "table", "#root"])),
            "dt" | "dd" => Some((&["dt", "dd"], &["dl", "table", "#root"])),
            "tr" => Some((&["tr"], &["table", "#root"])),
            "td" | "th" => Some((&["td", "th"], &["tr", "table", "#root"])),
            "option" => Some((&["option"], &["select", "#root"])),
            "a" => Some((&["a"], &["#root"])),
            n if is_heading(n) => {
                // 标题里不会再套标题：上一个没结束的直接结束
                if is_heading(&self.nodes[self.top()].name) {
                    self.stack.pop();
                }
                None
            }
            _ => None,
        };
        if let Some((same, stop)) = auto {
            let found = same
                .iter()
                .filter_map(|s| self.open_in_scope(s, stop))
                .max();
            if let Some(k) = found {
                self.pop_to(k);
            }
        }
        let void = VOID.contains(&name.as_str());
        let id = self.push_node(Node {
            name,
            text: String::new(),
            attrs,
            parent: self.top(),
            children: Vec::new(),
        });
        // `<div/>` 在 HTML 里照样是开始标签；只有 void 元素和 svg 里的自闭合才真的闭合
        let foreign_closed = self_closing && self.in_foreign();
        if !void && !foreign_closed {
            self.stack.push(id);
        }
    }

    fn in_foreign(&self) -> bool {
        self.stack
            .iter()
            .any(|&id| matches!(self.nodes[id].name.as_str(), "svg" | "math"))
    }

    fn close(&mut self, name: &str) {
        if name == "br" {
            // `</br>`：浏览器当 `<br>` 处理
            self.open("br".into(), Vec::new(), false);
            return;
        }
        // 不让一个乱来的结束标签把表格 / 格子关掉（浏览器也是这样忽略它的）
        let stop: &[&str] = match name {
            "table" => &["#root"],
            "td" | "th" | "tr" | "tbody" | "thead" | "tfoot" => &["table", "#root"],
            _ => &["td", "th", "table", "#root"],
        };
        if let Some(k) = self.open_in_scope(name, stop) {
            self.pop_to(k);
        }
    }
}

/// `<` 之后的标签名（小写）和名字后面的位置
fn tag_name(html: &str, from: usize) -> (String, usize) {
    let s = html.as_bytes();
    let mut j = from;
    while j < s.len() && (s[j].is_ascii_alphanumeric() || matches!(s[j], b'-' | b':' | b'_')) {
        j += 1;
    }
    (html[from..j].to_ascii_lowercase(), j)
}

/// 标签里的属性，到 `>` 为止。回（属性，是不是 `/>`，`>` 之后的位置）。
fn attributes(html: &str, from: usize) -> (Vec<(String, String)>, bool, usize) {
    let s = html.as_bytes();
    let mut attrs = Vec::new();
    let mut j = from;
    let mut self_closing = false;
    loop {
        while j < s.len() && (s[j].is_ascii_whitespace() || s[j] == b'/') {
            self_closing = s[j] == b'/';
            j += 1;
        }
        if j >= s.len() {
            return (attrs, self_closing, s.len());
        }
        if s[j] == b'>' {
            return (attrs, self_closing, j + 1);
        }
        self_closing = false;
        let k0 = j;
        while j < s.len() && !s[j].is_ascii_whitespace() && !matches!(s[j], b'=' | b'>' | b'/') {
            j += 1;
        }
        let key = html[k0..j].to_ascii_lowercase();
        while j < s.len() && s[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut val = String::new();
        if j < s.len() && s[j] == b'=' {
            j += 1;
            while j < s.len() && s[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < s.len() && (s[j] == b'"' || s[j] == b'\'') {
                let q = s[j];
                let v0 = j + 1;
                j = s[v0..]
                    .iter()
                    .position(|&c| c == q)
                    .map_or(s.len(), |p| v0 + p);
                val = decode(&html[v0..j]);
                j = (j + 1).min(s.len());
            } else {
                let v0 = j;
                while j < s.len() && !s[j].is_ascii_whitespace() && s[j] != b'>' {
                    j += 1;
                }
                val = decode(&html[v0..j]);
            }
        }
        if !key.is_empty() && KEEP_ATTRS.contains(&key.as_str()) {
            attrs.push((key, val));
        }
        if j == k0 {
            // 什么也没吃掉（比如一个孤零零的 `=`）：跳过，免得死循环
            j += 1;
        }
    }
}

fn find(s: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= s.len() {
        return None;
    }
    s[from..]
        .windows(pat.len())
        .position(|w| w == pat)
        .map(|p| from + p)
}

fn find_ci(s: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    if from >= s.len() {
        return None;
    }
    s[from..]
        .windows(pat.len())
        .position(|w| w.eq_ignore_ascii_case(pat))
        .map(|p| from + p)
}

/// 解码实体。认不出的原样留着（`&foo;` 还是 `&foo;`）。
pub fn decode(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let body_end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map_or(rest.len(), |p| p + 1);
        let body = &rest[1..body_end];
        let semi = rest[body_end..].starts_with(';');
        let ch = if let Some(num) = body.strip_prefix('#') {
            let n = match num.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => num.parse().ok(),
            };
            n.and_then(char::from_u32).map(|c| c.to_string())
        } else {
            named(body).map(str::to_string)
        };
        match ch {
            Some(c) if !body.is_empty() => {
                out.push_str(&c);
                rest = &rest[body_end + usize::from(semi)..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 常见的命名实体（全表 2000 多个，网页里真出现的就这些）
fn named(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" | "AMP" => "&",
        "lt" | "LT" => "<",
        "gt" | "GT" => ">",
        "quot" | "QUOT" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200c}",
        "zwj" => "\u{200d}",
        "shy" => "\u{ad}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "hellip" => "…",
        "mdash" => "—",
        "ndash" => "–",
        "lsquo" => "‘",
        "rsquo" => "’",
        "sbquo" => "‚",
        "ldquo" => "“",
        "rdquo" => "”",
        "bdquo" => "„",
        "laquo" => "«",
        "raquo" => "»",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "middot" => "·",
        "bull" => "•",
        "prime" => "′",
        "Prime" => "″",
        "times" => "×",
        "divide" => "÷",
        "minus" => "−",
        "plusmn" => "±",
        "deg" => "°",
        "micro" => "µ",
        "para" => "¶",
        "sect" => "§",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "cent" => "¢",
        "pound" => "£",
        "yen" => "¥",
        "euro" => "€",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "le" => "≤",
        "ge" => "≥",
        "ne" => "≠",
        "asymp" => "≈",
        "infin" => "∞",
        "frac12" => "½",
        "frac14" => "¼",
        "frac34" => "¾",
        "sup2" => "²",
        "sup3" => "³",
        "iexcl" => "¡",
        "iquest" => "¿",
        "eacute" => "é",
        "egrave" => "è",
        "agrave" => "à",
        "aacute" => "á",
        "uuml" => "ü",
        "ouml" => "ö",
        "auml" => "ä",
        "szlig" => "ß",
        "ccedil" => "ç",
        "ntilde" => "ñ",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(d: &Doc, n: usize) -> Vec<&str> {
        d.nodes[n]
            .children
            .iter()
            .map(|&c| {
                let n = &d.nodes[c];
                if n.is_text() {
                    n.text.as_str()
                } else {
                    n.name.as_str()
                }
            })
            .collect()
    }

    #[test]
    fn builds_a_tree_with_attributes() {
        let d =
            parse(r#"<div class="a b" id=x><p>你好 <a href='/p?a=1&amp;b=2'>链接</a></p></div>"#);
        let div = d.find("div").unwrap();
        assert_eq!(d.nodes[div].attr("class"), Some("a b"));
        assert_eq!(d.nodes[div].attr("id"), Some("x"));
        let a = d.find("a").unwrap();
        assert_eq!(d.nodes[a].attr("href"), Some("/p?a=1&b=2"));
        assert_eq!(d.text_of(div), "你好 链接");
        assert!(d.contains(div, a) && !d.contains(a, div));
    }

    #[test]
    fn implied_end_tags() {
        let d = parse("<ul><li>一<li>二</ul><p>甲<p>乙<div>丙</div>");
        let ul = d.find("ul").unwrap();
        assert_eq!(names(&d, ul), ["li", "li"]);
        let root_kids = names(&d, 0);
        assert_eq!(
            root_kids,
            ["ul", "p", "p", "div"],
            "p 碰到 p / div 自动结束"
        );
        let d = parse("<table><tr><td>a<td>b<tr><td>c</table>");
        let t = d.find("table").unwrap();
        assert_eq!(names(&d, t), ["tr", "tr"]);
        assert_eq!(names(&d, d.find("tr").unwrap()), ["td", "td"]);
    }

    #[test]
    fn raw_text_is_not_parsed() {
        let d = parse("<script>if (a<b) document.write('<div>x</div>')</script><p>正文</p>");
        let s = d.find("script").unwrap();
        assert_eq!(d.text_of(s), "if (a<b) document.write('<div>x</div>')");
        assert!(d.find("div").is_none());
        assert_eq!(d.text_of(d.find("p").unwrap()), "正文");
        // 大写的结束标签也认
        let d = parse("<STYLE>p{}</STYLE><title>标题 &amp; 副标题</title>");
        assert_eq!(d.text_of(d.find("style").unwrap()), "p{}");
        assert_eq!(d.text_of(d.find("title").unwrap()), "标题 & 副标题");
    }

    #[test]
    fn stray_end_tags_and_comments_are_harmless() {
        let d = parse("<div>a</span>b<!-- </div> --></p>c</div><p>d");
        let div = d.find("div").unwrap();
        assert_eq!(d.text_of(div), "abc");
        assert_eq!(d.text_of(d.find("p").unwrap()), "d");
        // 格子里一个多余的 </div> 不能把表格关掉
        let d = parse("<table><tr><td>a</div>b</td><td>c</td></tr></table>");
        assert_eq!(names(&d, d.find("tr").unwrap()), ["td", "td"]);
    }

    #[test]
    fn entities() {
        assert_eq!(
            decode("&lt;a&gt; &#20013;&#x6587; &copy; &nbsp;"),
            "<a> 中文 © \u{a0}"
        );
        assert_eq!(decode("AT&T &unknown; & x"), "AT&T &unknown; & x");
        assert_eq!(decode("&amp"), "&", "没写分号也认");
    }

    #[test]
    fn never_panics_on_truncated_input() {
        for s in [
            "<",
            "<a",
            "<a href=",
            "<a href=\"x",
            "<!--",
            "<script>x",
            "&#",
            "&#x;",
            "</",
            "<p a=1 /",
        ] {
            let _ = parse(s);
        }
    }
}
