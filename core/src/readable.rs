//! 抽文章正文：给一段 HTML，回纯文本。
//!
//! 两个入口共用这一份：
//!
//! - `agent readable`：像一个普通的 Unix 工具，只管 HTML → 文字，不联网（网页由
//!   curl / wget 去拿），`curl -sL <url> | agent readable`
//! - `agent browser get_readable`：浏览器（手机 WebView / 电脑无头 Chrome）取回渲染后的
//!   整页 HTML，看不见的元素已标上 `hidden`（`browser_js.rs` 的 `page_html`），在
//!   `Core::browser` 里过这里
//!
//! ## 怎么找正文
//!
//! 不靠一堆按站点攒出来的规则，靠两个**浅层文本特征**（Kohlschütter 等，《Boilerplate
//! Detection using Shallow Text Features》，WSDM 2010 —— boilerpipe）：每个文本块的
//! **字数**和**链接密度**（链接文字占多少）。正文是「字多、链接少」，导航 / 推荐 / 页脚是
//! 「字少、链接多」。在这之上用了 DOM 结构（CETD 那一路的想法）：
//!
//! 1. 宽松地解析成树（[`crate::html`]，不带 html5ever），按块级元素切成文本块
//! 2. 每块打分：长且链接少的 `+字数`，链接密集的 `−字数`，别的不算
//! 3. **取得分最高的子树**当正文容器 —— 一个子树的分是它里面所有块的和，所以最高的
//!    那个就是「把正文都包进来、又尽量没带上导航」的最紧那一层（树上的最大子段和）
//! 4. 容器里再剃一遍：开头结尾的链接块、连着三块以上的链接（「相关阅读」那种）
//!
//! 「字数」的单位：**一个汉字算 1，一个英文单词算 1**（论文按词数算，中文没有空格）。
//! 明摆着是周边的（`<nav>` / `<aside>` / `<footer>`、类名像 comment / share / sidebar 的）
//! 先标出来不参与打分 —— 但只在它不到整页一半时才算（有的站把整个页面包在一个
//! `class="has-sidebar"` 里）。
//!
//! 页面里没有像样的正文（门户、列表页）时不报错：退回整页文字，第一行说明一句。
//! 一个字都没有才报错 —— 多半是 JS 渲染的页面，HTML 里本来就是空的。
//!
//! ## 排成什么样
//!
//! 段落之间空一行；标题写成 `## 标题`；`<pre>` 包进 ```` ``` ````；一段话基本就是
//! 一个链接时后面附上 `  [去哪]`；相邻的重复块去掉。另外：数据表格一行一条、格子用
//! ` | ` 隔开（排版用的表照普通块读）；`<br><br>` 分段的老网页照样分段。

use crate::html::{self, Doc};

/// 抽出来的文章
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    /// 作者、发布时间（有哪个算哪个，用 ` · ` 连起来）
    pub meta: String,
    /// 正文（已经排好版）
    pub text: String,
    /// 没认出正文，`text` 是整页
    pub fallback: bool,
}

/// 给模型看的那段文字：标题、作者 / 时间、空一行、正文。
pub fn render(a: &Article) -> String {
    let mut head = Vec::new();
    if a.fallback {
        head.push("（没认出正文，下面是整页的文字）".to_string());
    }
    if !a.title.is_empty() {
        head.push(a.title.clone());
    }
    if !a.meta.is_empty() {
        head.push(a.meta.clone());
    }
    if a.text.is_empty() {
        return head.join("\n");
    }
    if head.is_empty() {
        return a.text.clone();
    }
    format!("{}\n\n{}", head.join("\n"), a.text)
}

/// HTML 字节 → 文章。
///
/// 只收 UTF-8：转码表（encoding_rs）光 GBK 那几张就有几百 KB，而 `iconv` 两边都现成
///（Alpine 的 musl-utils、macOS 自带）—— 报错里直接告诉怎么转。
pub fn extract_bytes(bytes: &[u8]) -> anyhow::Result<Article> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(html) => extract(html),
        Err(_) => {
            let cs = declared_charset(bytes).unwrap_or_else(|| "gbk".into());
            anyhow::bail!(
                "这份 HTML 不是 UTF-8（页面声明的编码：{cs}）—— 先转码：\
                 `iconv -f {cs} -t utf-8 | agent readable`"
            )
        }
    }
}

/// HTML 文本 → 文章。
pub fn extract(src: &str) -> anyhow::Result<Article> {
    let doc = html::parse(src);
    let host = page_host(&doc);
    let body = doc.find("body").unwrap_or(0);
    let blocks = Segmenter::new(&doc, &host).run(body);

    let (picked, fallback): (Vec<&Block>, bool) = match pick(&doc, &blocks) {
        Some(c) => (trim(&doc, &blocks, c), false),
        None => (blocks.iter().collect(), true),
    };
    let mut chunks: Vec<&str> = Vec::new();
    for b in picked {
        if chunks.last() != Some(&b.text.as_str()) {
            chunks.push(&b.text);
        }
    }
    let text = chunks.join("\n\n");
    if text.trim().is_empty() {
        anyhow::bail!(
            "页面里没有文字（多半是 JS 渲染的页面，HTML 里本来就是空的）—— \
             用 `agent browser navigate <url>` 再 `agent browser get_readable`"
        );
    }
    let title = page_title(&doc);
    // 正文第一块就是标题（`## 标题`）：上面已经有一行了，不重复
    let text = match text.strip_prefix(&format!("## {title}")) {
        Some(rest) if !title.is_empty() && (rest.is_empty() || rest.starts_with("\n\n")) => {
            rest.trim_start().to_string()
        }
        _ => text,
    };
    Ok(Article {
        title,
        meta: if fallback {
            String::new()
        } else {
            page_meta(&doc)
        },
        text,
        fallback,
    })
}

// ---------------------------------------------------------------------------
// 找正文：打分、取最高的子树、剃边
// ---------------------------------------------------------------------------

/// 够这么多「字」（汉字 / 英文词）、链接又少的块，才算正文的一段
const GOOD_UNITS: usize = 15;
/// 整个容器里正文的「字」不到这么多：不像一篇文章，退回整页
const MIN_ARTICLE_UNITS: usize = 60;

fn link_dense(b: &Block) -> bool {
    b.kind != Kind::Code && b.link_chars * 2 > b.chars
}

/// 一块对它所在的每一层祖先贡献多少分
fn score(b: &Block) -> i64 {
    if b.chrome || b.kind == Kind::Heading {
        0
    } else if link_dense(b) {
        -(b.units as i64)
    } else if b.units >= GOOD_UNITS && b.link_chars * 10 <= b.chars * 4 {
        b.units as i64
    } else {
        0
    }
}

/// 正文容器：分最高的那个元素。不像文章（正文太少，或者正文段落大半不在容器里）就回 `None`。
fn pick(doc: &Doc, blocks: &[Block]) -> Option<usize> {
    let mut v = vec![0i64; doc.nodes.len()];
    for b in blocks {
        let s = score(b);
        if s == 0 {
            continue;
        }
        let mut n = b.node;
        loop {
            v[n] += s;
            if n == 0 {
                break;
            }
            n = doc.nodes[n].parent;
        }
    }
    // 容器取**装内容的那层**，不取内容本身。
    //
    // 同一个块沿祖先链一路累加，所以 `<article>` 和它里面唯一的 `<p>` 常常同分。
    // 取最深的话正文容器会缩成那一个 `<p>` —— 和它平级的代码块、表格、小标题
    // 全落在容器外，全被丢掉（实测：`<article><p>正文</p><pre>代码</pre>
    // <table>表</table></article>` 抽出来只剩那一个 `<p>`）。
    //
    // 也不能一路往上：`<body>`/`<html>` 分数一样，取到那儿就等于整页都是容器，
    // 导航又回来了。折中是「同分祖先里最深的、真正装了两个以上子元素的盒子」。
    let mut best = 0;
    for (i, &s) in v.iter().enumerate().skip(1) {
        if !doc.nodes[i].is_text() && s > 0 && s > v[best] {
            best = i;
        }
    }
    if best != 0 {
        // 从最高的同分祖先往下，找第一个「子元素 ≥ 2」的
        let top = v[best];
        let mut root = best;
        while root != 0 && v[doc.nodes[root].parent] == top {
            root = doc.nodes[root].parent;
        }
        let mut cursor = root;
        while cursor < doc.end[root] {
            if v[cursor] == top
                && !doc.nodes[cursor].is_text()
                && element_children(doc, cursor) >= 2
            {
                best = cursor;
                break;
            }
            cursor += 1;
        }
    }

    let good: usize = blocks
        .iter()
        .filter(|b| doc.contains(best, b.node) && score(b) > 0)
        .map(|b| b.units)
        .sum();
    // 门户首页：像样的段落（新闻摘要）散在各处，容器只圈住其中一小撮 —— 文章页上
    // 正文段落几乎全在容器里（实测：文章页 0.89–1.00，新浪首页 0.63）
    let all: usize = blocks
        .iter()
        .filter(|b| score(b) > 0)
        .map(|b| b.units)
        .sum();
    (good >= MIN_ARTICLE_UNITS && good * 10 >= all * 7).then_some(best)
}

/// 一个节点的**元素**子节点个数（不算文本节点）。
fn element_children(doc: &Doc, n: usize) -> usize {
    (n + 1..doc.end[n])
        .filter(|&c| doc.nodes[c].parent == n && !doc.nodes[c].is_text())
        .count()
}

/// 容器里的块，剃掉开头结尾的链接块、连成串（≥3）的链接块和零碎的短链接；跟着被剃掉的
/// 那串的标题（「相关阅读」）一起去掉。
fn trim<'a>(doc: &Doc, blocks: &'a [Block], container: usize) -> Vec<&'a Block> {
    let inside: Vec<&Block> = blocks
        .iter()
        .filter(|b| !b.chrome && doc.contains(container, b.node))
        .collect();
    let n = inside.len();
    let dense: Vec<bool> = inside.iter().map(|b| link_dense(b)).collect();
    let mut keep = vec![true; n];
    // 被成串剃掉的（标题跟着它们走；零碎链接剃掉了标题不跟着走）
    let mut run_gone = vec![false; n];
    let mut i = 0;
    while i < n {
        if !dense[i] {
            i += 1;
            continue;
        }
        let j = (i..n).find(|&k| !dense[k]).unwrap_or(n);
        if j - i >= 3 || i == 0 || j == n {
            keep[i..j].fill(false);
            run_gone[i..j].fill(true);
        }
        i = j;
    }
    // 字全在链接里的碎片（维基小节旁的 `[edit]`、「更多」「上一篇」）：在哪都不要。
    // `1、hub` 这种（推荐条目）有链接外的字，留着
    for (k, b) in inside.iter().enumerate() {
        if dense[k] && b.units < 4 && !b.plain_words {
            keep[k] = false;
        }
    }
    for i in (0..n).rev() {
        let next_gone = i + 1 < n && run_gone[i + 1];
        let last = !keep[i + 1..].contains(&true);
        if inside[i].kind == Kind::Heading && (next_gone || last) {
            keep[i] = false;
        }
    }
    inside
        .into_iter()
        .zip(keep)
        .filter_map(|(b, k)| k.then_some(b))
        .collect()
}

// ---------------------------------------------------------------------------
// 页面元信息
// ---------------------------------------------------------------------------

fn meta_content<'a>(doc: &'a Doc, keys: &[&str]) -> Option<&'a str> {
    for k in keys {
        for n in &doc.nodes {
            if n.name != "meta" && n.attr("itemprop").is_none() {
                continue;
            }
            let hit = [n.attr("property"), n.attr("name"), n.attr("itemprop")]
                .into_iter()
                .flatten()
                .any(|v| v.eq_ignore_ascii_case(k));
            let val = n.attr("content").or_else(|| n.attr("datetime"));
            if let Some(v) = val.filter(|v| hit && !v.trim().is_empty()) {
                return Some(v.trim());
            }
        }
    }
    None
}

/// 标题：和 `og:title` / `<title>` 对得上的 `<h1>` > `og:title` > `<title>`。
/// 后两个常带着站名（「… - Wikipedia」「…_新浪网」），`<h1>` 一般不带。
fn page_title(doc: &Doc) -> String {
    let og = meta_content(doc, &["og:title", "twitter:title"]).map(collapse);
    let title = doc
        .find("title")
        .map(|t| collapse(&doc.text_of(t)))
        .unwrap_or_default();
    for (i, n) in doc.nodes.iter().enumerate() {
        if n.name == "h1" {
            let h = collapse(&doc.text_of(i));
            let matches = |t: &str| t.contains(&h);
            if h.chars().count() >= 2 && (og.as_deref().is_some_and(matches) || matches(&title)) {
                return h;
            }
        }
    }
    og.unwrap_or(title)
}

/// 作者、发布时间
fn page_meta(doc: &Doc) -> String {
    let author = meta_content(doc, &["author", "article:author", "byline", "dc.creator"])
        .filter(|a| !a.starts_with("http"));
    let date = meta_content(
        doc,
        &[
            "article:published_time",
            "datePublished",
            "publishdate",
            "pubdate",
            "publish_date",
            "og:release_date",
        ],
    );
    [author, date]
        .into_iter()
        .flatten()
        .map(collapse)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 这一页在哪个站（同站链接只写路径）：页面自己声明的规范地址
fn page_host(doc: &Doc) -> String {
    let canonical = doc.nodes.iter().find_map(|n| {
        (n.name == "link"
            && n.attr("rel")
                .is_some_and(|r| r.eq_ignore_ascii_case("canonical")))
        .then(|| n.attr("href"))
        .flatten()
    });
    canonical
        .or_else(|| meta_content(doc, &["og:url"]))
        .map(|u| host_of(u).to_string())
        .unwrap_or_default()
}

/// `<meta charset=gbk>` / `content="text/html; charset=gb2312"` 里的编码名。
fn declared_charset(bytes: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).to_ascii_lowercase();
    let at = head.find("charset=")? + "charset=".len();
    let cs: String = head[at..]
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!cs.is_empty()).then_some(cs)
}

// ---------------------------------------------------------------------------
// 切块：树 → 一串文本块（已经排好版，带着字数和链接字数）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Heading,
    Text,
    Code,
    Table,
}

#[derive(Debug)]
struct Block {
    /// 排好版的文字（`## 标题`、代码块、带去处的链接……）
    text: String,
    /// 它来自哪个元素（打分时往上加）
    node: usize,
    kind: Kind,
    /// 字（字母 / 数字 / 汉字）数 / 其中在链接里的
    chars: usize,
    link_chars: usize,
    /// 「字」数：一个汉字算 1，一个英文词算 1
    units: usize,
    /// 有没有不在链接里的字（`1、hub` 有，`[edit]` 没有）
    plain_words: bool,
    /// 在明摆着的周边里（导航、页脚、评论……）
    chrome: bool,
}

/// 整棵不要的（里面没有给人读的字）
const SKIP: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "math", "button", "select", "input",
    "textarea", "iframe", "object", "embed", "canvas", "audio", "video", "head", "title", "meta",
    "link",
];

/// 块级元素：前后各断开一次
const BLOCK: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "center",
    "details",
    "dialog",
    "dd",
    "div",
    "dl",
    "dt",
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
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "thead",
    "tfoot",
    "tr",
    "td",
    "th",
    "ul",
    "caption",
];

/// 类名 / id 里出现就是周边（不管别的）
const JUNK_ALWAYS: &[&str] = &[
    "comment",
    "cookie",
    "breadcrumb",
    "share",
    "social",
    "related",
    "recommend",
    "paywall",
    "subscribe",
    "newsletter",
    "popup",
    "modal",
    "advert",
    "sponsor",
    "disclaimer",
    "copyright",
];
/// 类名 / id 里出现像周边，但同时像正文（`main-content-with-sidebar`）就不算
const JUNK_MAYBE: &[&str] = &[
    "sidebar", "menu", "navbar", "footer", "banner", "masthead", "toolbar", "login", "signup",
    "promo", "widget",
];
/// 整个词（按 `-` `_` 空格切开）等于它才算：太短，当子串会误伤（`ad` 在 `header` 里）
const JUNK_WORDS: &[&str] = &["nav", "ad", "ads", "tags", "tag", "hot", "rank", "side"];
const CONTENT_WORDS: &[&str] = &[
    "article", "content", "main", "body", "post", "entry", "story", "text",
];

fn is_block(name: &str) -> bool {
    BLOCK.contains(&name)
}

fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

struct Segmenter<'a> {
    doc: &'a Doc,
    host: &'a str,
    /// 子树里的非空白字符数（不含 `SKIP` 里的）
    tlen: Vec<usize>,
    /// 子树里有没有块级元素
    has_block: Vec<bool>,
    total: usize,
    blocks: Vec<Block>,
    /// 还没断开的行内文字（散在块之间的字）
    buf: Inline,
}

impl<'a> Segmenter<'a> {
    fn new(doc: &'a Doc, host: &'a str) -> Self {
        let n = doc.nodes.len();
        let mut tlen = vec![0usize; n];
        let mut has_block = vec![false; n];
        for i in (1..n).rev() {
            let node = &doc.nodes[i];
            if node.is_text() {
                tlen[i] = node.text.chars().filter(|c| !c.is_whitespace()).count();
            } else if SKIP.contains(&node.name.as_str()) {
                tlen[i] = 0;
            }
            let p = node.parent;
            tlen[p] += tlen[i];
            if has_block[i] || is_block(&node.name) {
                has_block[p] = true;
            }
        }
        Segmenter {
            doc,
            host,
            tlen,
            has_block,
            total: 0,
            blocks: Vec::new(),
            buf: Inline::default(),
        }
    }

    fn run(mut self, root: usize) -> Vec<Block> {
        self.total = self.tlen[root];
        self.walk(root, false);
        self.flush(root, false);
        self.blocks
    }

    fn hidden(&self, n: usize) -> bool {
        let node = &self.doc.nodes[n];
        if node.attr("hidden").is_some() || node.attr("aria-hidden") == Some("true") {
            return true;
        }
        node.attr("style").is_some_and(|s| {
            let s: String = s.to_ascii_lowercase().split_whitespace().collect();
            s.contains("display:none") || s.contains("visibility:hidden")
        })
    }

    /// 明摆着的周边：导航 / 侧栏 / 页脚元素，或者类名像。不到整页一半才算。
    fn chrome(&self, n: usize) -> bool {
        let node = &self.doc.nodes[n];
        if self.tlen[n] * 2 >= self.total {
            return false;
        }
        if matches!(node.name.as_str(), "nav" | "aside" | "footer") {
            return true;
        }
        if matches!(
            node.attr("role"),
            Some("navigation" | "complementary" | "contentinfo" | "banner")
        ) {
            return true;
        }
        let names = format!(
            "{} {}",
            node.attr("class").unwrap_or(""),
            node.attr("id").unwrap_or("")
        )
        .to_ascii_lowercase();
        if names.trim().is_empty() {
            return false;
        }
        if JUNK_ALWAYS.iter().any(|j| names.contains(j)) {
            return true;
        }
        let words: Vec<&str> = names
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let looks_junk = JUNK_MAYBE.iter().any(|j| names.contains(j))
            || words.iter().any(|w| JUNK_WORDS.contains(w));
        looks_junk && !CONTENT_WORDS.iter().any(|c| names.contains(c))
    }

    fn walk(&mut self, n: usize, chrome: bool) {
        let doc = self.doc;
        for &c in &doc.nodes[n].children {
            let node = &doc.nodes[c];
            if node.is_text() {
                self.buf.text(&node.text);
                continue;
            }
            let name = node.name.as_str();
            if SKIP.contains(&name) || self.hidden(c) {
                continue;
            }
            let ch = chrome || ((is_block(name) || self.has_block[c]) && self.chrome(c));
            match name {
                "br" => self.buf.br(),
                "hr" | "img" => self.flush(n, chrome),
                _ if is_heading(name) => {
                    self.flush(n, chrome);
                    let inl = inline(doc, c);
                    let t = tidy(&inl.s).replace('\n', " ");
                    if !t.is_empty() {
                        let t = self.with_hint(t, &inl);
                        self.push(c, Kind::Heading, format!("## {t}"), &inl, ch);
                    }
                }
                "pre" => {
                    self.flush(n, chrome);
                    let t = doc.text_of(c);
                    let t = t.trim_matches('\n').trim_end();
                    if !t.trim().is_empty() {
                        let inl = Inline::counted(t);
                        self.push(c, Kind::Code, format!("```\n{t}\n```"), &inl, ch);
                    }
                }
                "table" => {
                    self.flush(n, chrome);
                    self.table(c, ch);
                }
                "p" | "figcaption" | "dt" | "dd" | "summary" | "caption" | "li" | "td" | "th"
                    if !self.has_block[c] =>
                {
                    self.flush(n, chrome);
                    let inl = inline(doc, c);
                    self.text_block(c, inl, ch);
                }
                _ if is_block(name) || self.has_block[c] => {
                    self.flush(n, chrome);
                    self.walk(c, ch);
                    self.flush(c, ch);
                }
                // 行内元素（a / span / em / code …）：字接着往缓冲里写
                _ => collect_inline(doc, c, &mut self.buf),
            }
        }
    }

    fn flush(&mut self, n: usize, chrome: bool) {
        let inl = std::mem::take(&mut self.buf);
        self.text_block(n, inl, chrome);
    }

    /// 一段文字成一块
    fn text_block(&mut self, n: usize, inl: Inline, chrome: bool) {
        let text = tidy(&inl.s);
        if text.chars().count() < 2 {
            return;
        }
        let out = self.with_hint(text, &inl);
        self.push(n, Kind::Text, out, &inl, chrome);
    }

    /// 基本就是一个链接（目录、相关阅读、带链接的小标题）：后面附上去处
    fn with_hint(&self, text: String, inl: &Inline) -> String {
        if let [(href, len)] = inl.links.as_slice() {
            if len * 2 > inl.chars {
                if let Some(w) = where_to(href, self.host) {
                    return format!("{text}  [{w}]");
                }
            }
        }
        text
    }

    fn push(&mut self, node: usize, kind: Kind, text: String, inl: &Inline, chrome: bool) {
        self.blocks.push(Block {
            units: units(&inl.s),
            plain_words: inl.plain_words,
            text,
            node,
            kind,
            chars: inl.chars,
            link_chars: inl.link_chars,
            chrome,
        });
    }

    /// 表格。数据表格一行一条，格子用 ` | ` 隔开（和 `plain.rs` 的表格一个写法）；
    /// 排版用的表（老网站拿它分栏）照普通块读。
    fn table(&mut self, t: usize, chrome: bool) {
        let doc = self.doc;
        // 只要这张表自己的行，不要嵌在格子里的表的行
        let rows: Vec<usize> = doc
            .subtree(t)
            .filter(|&d| doc.nodes[d].name == "tr" && doc.ancestor(d, "table") == Some(t))
            .collect();
        if !self.is_data_table(t, &rows) {
            self.walk(t, chrome);
            self.flush(t, chrome);
            return;
        }
        if let Some(cap) = doc.subtree(t).find(|&d| doc.nodes[d].name == "caption") {
            let inl = inline(doc, cap);
            self.text_block(cap, inl, chrome);
        }
        let mut all = Inline::default();
        let mut lines = Vec::new();
        for &r in &rows {
            let cells: Vec<String> = cells(doc, r)
                .map(|c| {
                    let inl = inline(doc, c);
                    all.chars += inl.chars;
                    all.link_chars += inl.link_chars;
                    all.s.push_str(&inl.s);
                    all.s.push(' ');
                    tidy(&inl.s).replace('\n', " ")
                })
                .collect();
            if cells.iter().any(|c| !c.is_empty()) {
                lines.push(cells.join(" | "));
            }
        }
        if !lines.is_empty() {
            self.push(t, Kind::Table, lines.join("\n"), &all, chrome);
        }
    }

    /// 是不是数据表格（Readability 的 `_markDataTables` 简化版）：
    /// - 套着别的表、格子里有块级元素或者一大段字 → 排版用的
    /// - 有表头 / 标题 → 数据
    /// - 只有一行或一列 → 排版用的；否则看格子够不够多
    fn is_data_table(&self, t: usize, rows: &[usize]) -> bool {
        let doc = self.doc;
        if doc.nodes[t].attr("role") == Some("presentation") {
            return false;
        }
        let (mut cols, mut n) = (0, 0);
        for &r in rows {
            let mut k = 0;
            for c in cells(doc, r) {
                if self.has_block[c] || self.tlen[c] > 200 {
                    return false;
                }
                k += 1;
            }
            cols = cols.max(k);
            n += k;
        }
        let header = doc.subtree(t).any(|d| {
            matches!(doc.nodes[d].name.as_str(), "th" | "caption" | "thead")
                && doc.ancestor(d, "table") == Some(t)
        });
        header || (rows.len() >= 2 && cols >= 2 && n >= 4)
    }
}

fn cells(doc: &Doc, tr: usize) -> impl Iterator<Item = usize> + '_ {
    doc.nodes[tr]
        .children
        .iter()
        .copied()
        .filter(|&c| matches!(doc.nodes[c].name.as_str(), "td" | "th"))
}

/// 一段行内文字：空白折叠过的字，外加字数和链接的统计
#[derive(Default)]
struct Inline {
    s: String,
    /// 字（字母 / 数字 / 汉字）数 / 其中在链接里的
    chars: usize,
    link_chars: usize,
    /// 每个链接：去处、链接文字的字符数
    links: Vec<(String, usize)>,
    /// 正在哪个链接里（`links` 的下标）
    cur: Option<usize>,
    /// 有没有不在链接里的字母 / 数字 / 汉字
    plain_words: bool,
}

impl Inline {
    /// 原样的一段字（代码块）：只数字数
    /// 代码块这类「整段都是一手文字」的：字数是字母数字，且**算作正文**
    /// （`plain_words = true`）。
    ///
    /// 不设 `plain_words` 的话，`<pre>` 会被 `trim` 里那条「字全在链接里的碎片
    /// 就扔掉」剔掉 —— 而代码块本来就不在链接里，它是纯粹的正文。实测：
    /// 一个正文里带 `<pre><code>` 的文章，抽出来的文字里代码块整段消失。
    fn counted(s: &str) -> Self {
        Inline {
            s: s.to_string(),
            chars: s.chars().filter(|c| c.is_alphanumeric()).count(),
            plain_words: true,
            ..Default::default()
        }
    }

    /// 追加一段文字，连续空白（含 `&nbsp;`）折叠成一个空格
    fn text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !self.s.is_empty() && !self.s.ends_with([' ', '\n']) {
                    self.s.push(' ');
                }
                continue;
            }
            self.s.push(ch);
            // 只数字（字母 / 数字 / 汉字），标点不算：`[编辑]` 的方括号不在链接里，
            // 算进去它就成了「一半是链接」
            if !ch.is_alphanumeric() {
                continue;
            }
            self.chars += 1;
            match self.cur {
                Some(k) => {
                    self.link_chars += 1;
                    self.links[k].1 += 1;
                }
                None => self.plain_words = true,
            }
        }
    }

    fn newline(&mut self) {
        while self.s.ends_with(' ') {
            self.s.pop();
        }
        if !self.s.is_empty() && !self.s.ends_with('\n') {
            self.s.push('\n');
        }
    }

    /// `<br>` 换行；老网页不写 `<p>`，拿 `<br><br>` 分段 —— 连着第二个就空一行
    fn br(&mut self) {
        if self.s.ends_with('\n') && !self.s.ends_with("\n\n") {
            self.s.push('\n');
        } else {
            self.newline();
        }
    }
}

/// 一个元素里的字（`innerText` 那样）：空白折叠，`<br>` 和块级元素断行。
fn inline(doc: &Doc, n: usize) -> Inline {
    let mut inl = Inline::default();
    collect_inline(doc, n, &mut inl);
    inl
}

fn collect_inline(doc: &Doc, n: usize, inl: &mut Inline) {
    let node = &doc.nodes[n];
    // 页内锚点（标题旁的 `#` 永久链接、目录）和 `javascript:` 不是去别处的链接
    let href = node.attr("href").map(str::trim).filter(|h| {
        node.name == "a" && !h.is_empty() && !h.starts_with('#') && !h.starts_with("javascript:")
    });
    let prev = inl.cur;
    if let Some(h) = href {
        inl.links.push((h.to_string(), 0));
        inl.cur = Some(inl.links.len() - 1);
    }
    for &c in &node.children {
        let child = &doc.nodes[c];
        if child.is_text() {
            inl.text(&child.text);
            continue;
        }
        let name = child.name.as_str();
        if SKIP.contains(&name) || child.attr("hidden").is_some() {
            continue;
        }
        if name == "br" {
            inl.br();
            continue;
        }
        let block = is_block(name);
        if block {
            inl.newline();
        }
        collect_inline(doc, c, inl);
        if block {
            inl.newline();
        }
    }
    inl.cur = prev;
}

/// 「字」数：一个汉字（假名、谚文）算 1，一串字母数字（一个英文词）算 1
fn units(s: &str) -> usize {
    let mut n = 0;
    let mut in_word = false;
    for ch in s.chars() {
        if is_cjk(ch) {
            n += 1;
            in_word = false;
        } else if ch.is_alphanumeric() {
            if !in_word {
                n += 1;
            }
            in_word = true;
        } else {
            in_word = false;
        }
    }
    n
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30ff}'   // 假名
        | '\u{3400}'..='\u{9fff}' // 汉字
        | '\u{ac00}'..='\u{d7af}' // 谚文
        | '\u{f900}'..='\u{faff}'
        | '\u{20000}'..='\u{2ffff}')
}

/// 每行去掉首尾空白；空行最多留一个（`<br><br>` 分的段），头尾的去掉
fn tidy(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for l in s.lines().map(str::trim) {
        if l.is_empty() && out.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        out.push(l);
    }
    while out.last() == Some(&"") {
        out.pop();
    }
    out.join("\n")
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 链接去哪：同站只写路径，别的站写 域名 + 路径（外站的短查询串留着）。
/// 页内锚点、`javascript:` 不算去处。
fn where_to(href: &str, host: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') || href.starts_with("javascript:") {
        return None;
    }
    let Some(rest) = href
        .strip_prefix("https://")
        .or_else(|| href.strip_prefix("http://"))
        .or_else(|| href.strip_prefix("//"))
    else {
        // 相对地址本来就是同站的路径；mailto: 这种原样给
        return Some(href.to_string());
    };
    let rest = rest.split('#').next().unwrap_or(rest);
    let h = host_of(href);
    if !host.is_empty() && h == host {
        let path = &rest[h.len()..];
        Some(if path.is_empty() {
            "/".into()
        } else {
            path.into()
        })
    } else {
        // 别的站：域名 + 路径。查询串是跟踪参数（`utm_` / `spm`）或者长得没法读就不要；
        // 短的留着 —— `news.ycombinator.com/item?id=1` 去掉 `?id=` 就哪也去不了了
        match rest.split_once('?') {
            Some((path, q)) if q.len() > 60 || q.contains("utm_") || q.contains("spm=") => {
                Some(path.to_string())
            }
            _ => Some(rest.to_string()),
        }
    }
}

fn host_of(url: &str) -> &str {
    crate::text::authority(url)
}

#[cfg(test)]
mod tests {
    //! `render` / `extract` 是**用户和模型直接看到的那段文字**，以前一条测试都没有。
    //! 这里钉住的是「什么样算正文、什么样退回整页」这条判断的边界。
    use super::*;

    /// 一篇结构正常的文章：抽出标题和正文，链接栏不该混进来。
    #[test]
    fn a_normal_article_keeps_prose_and_drops_the_nav() {
        let html = r#"<html><head><title>标题在这里</title></head><body>
            <nav><a href="/a">首页</a><a href="/b">关于</a><a href="/c">联系</a></nav>
            <article>
              <h1>标题在这里</h1>
              <p>第一段正文，写了一些足够长的内容好让它被认成正文，而不是导航或者页脚。</p>
              <p>第二段正文，继续写一些足够长的内容，让这个容器的字数超过识别门槛。</p>
              <p>第三段正文，再多一点文字，保证这段内容不会被当成零散的碎片扔掉。</p>
            </article>
            <footer><a href="/x">版权</a></footer>
          </body></html>"#;
        let a = extract(html).expect("正常文章要能抽出来");
        assert!(!a.fallback, "这不该退回整页：{}", a.text);
        assert!(a.title.contains("标题在这里"), "标题没抽到：{:?}", a.title);
        assert!(a.text.contains("第一段正文"));
        assert!(a.text.contains("第三段正文"));
        assert!(!a.text.contains("版权"), "页脚不该进正文：{}", a.text);
    }

    /// 没有正文结构的页面：`fallback = true`，`text` 是整页文字。
    #[test]
    fn a_page_without_prose_falls_back_to_the_whole_page() {
        let html = "<html><body><div><a href='/1'>链接一</a></div><div><a href='/2'>链接二</a></div></body></html>";
        let a = extract(html).expect("整页也是结果");
        assert!(a.fallback, "全是链接，应该退回整页");
        assert!(a.text.contains("链接一"));
        // 退回整页时不带 meta（作者/时间对整页没意义）
        assert!(a.meta.is_empty());
    }

    /// 完全没文字的页面：明确报错（并告诉调用方该用浏览器那条路）。
    #[test]
    fn an_empty_page_is_an_error_with_a_hint() {
        let e = extract("<html><body><div id=app></div></body></html>").unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("没有文字"), "错误要说清原因：{msg}");
        assert!(msg.contains("agent browser"), "要给出下一步：{msg}");
    }

    /// 非 UTF-8 的字节：报错里带页面声明的编码和转码命令，不静默变成乱码。
    #[test]
    fn non_utf8_bytes_report_the_charset_and_how_to_convert() {
        let mut bytes = b"<html><head><meta charset=\"gbk\"></head><body>".to_vec();
        bytes.extend_from_slice(&[0xC4, 0xE3]); // GBK 的「你」，不是合法 UTF-8
        bytes.extend_from_slice(b"</body></html>");
        let e = extract_bytes(&bytes).unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("iconv"), "要告诉怎么转码：{msg}");
        assert!(
            msg.contains("gbk") || msg.contains("GBK"),
            "要说清编码：{msg}"
        );
        // UTF-8 BOM 要去掉再解析
        let mut with_bom = b"\xEF\xBB\xBF".to_vec();
        with_bom.extend_from_slice(b"<html><body><p>");
        with_bom.extend_from_slice(
            "中文正文，足够长的一段话，用来跨过识别门槛。"
                .repeat(4)
                .as_bytes(),
        );
        with_bom.extend_from_slice(b"</p></body></html>");
        let a = extract_bytes(&with_bom).expect("带 BOM 的 UTF-8 要能解析");
        assert!(a.text.contains("中文正文"));
    }

    /// `render` 的形状：标题一行、meta 一行、空一行、正文；退回整页时先说明一句。
    #[test]
    fn render_puts_title_meta_and_body_in_order() {
        let a = Article {
            title: "标题".into(),
            meta: "作者 · 2026-01-02".into(),
            text: "正文".into(),
            fallback: false,
        };
        assert_eq!(render(&a), "标题\n作者 · 2026-01-02\n\n正文");

        // 没有 title/meta 时不要留空行
        let a = Article {
            title: String::new(),
            meta: String::new(),
            text: "正文".into(),
            fallback: false,
        };
        assert_eq!(render(&a), "正文");

        // 退回整页要显式告诉模型
        let a = Article {
            title: "标题".into(),
            meta: String::new(),
            text: "整页文字".into(),
            fallback: true,
        };
        assert_eq!(
            render(&a),
            "（没认出正文，下面是整页的文字）\n标题\n\n整页文字"
        );

        // 一个字都没有：只有头，没有多余空行
        let a = Article {
            title: "只有标题".into(),
            meta: String::new(),
            text: String::new(),
            fallback: false,
        };
        assert_eq!(render(&a), "只有标题");
    }

    /// 真实页面（维基、HN 首页）在改动前后的抽取结果对比。`#[ignore]`：
    /// 它只是打印，不判胜负，人工看有没有「正文变短 / 导航混进来」。
    #[test]
    #[ignore = "人工对比用，跑 --ignored --nocapture"]
    fn dump_real_pages() {
        for path in ["/tmp/wiki.html", "/tmp/hn.html"] {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(a) = extract_bytes(&bytes) else {
                println!("{path}: ERROR");
                continue;
            };
            println!(
                "=== {path} fallback={} title={:?} 正文字数={} ===",
                a.fallback,
                a.title,
                a.text.chars().count()
            );
            println!("{}", a.text.chars().take(300).collect::<String>());
        }
    }

    /// 各种坏 HTML 都不许 panic（输入来自任意网页）。
    #[test]
    fn malformed_html_never_panics() {
        let cases = [
            "",
            "<",
            "<html",
            "<p>没闭合",
            "<div><div><div><p>深层嵌套",
            "<script>var x = '<p>假的';</script>",
            "<p>&#x4E2D;&amp;&lt;&gt;&nbsp;</p>",
            &"<p>段落</p>".repeat(2000),
            "<table><tr><td>单元格</td></tr></table>",
        ];
        for html in cases {
            let _ = extract(html);
        }
    }

    /// 表格和代码块要保留结构（模型靠它理解内容）。
    #[test]
    fn code_and_tables_survive_extraction() {
        let body = "这是一段足够长的正文，用来把容器撑过识别门槛，免得整页回退。".repeat(6);
        let html = format!(
            r#"<html><head><title>T</title></head><body><article>
              <p>{body}</p>
              <pre><code>fn main() {{ println!("hi"); }}</code></pre>
              <table><tr><th>列一</th><th>列二</th></tr><tr><td>1</td><td>2</td></tr></table>
            </article></body></html>"#
        );
        let a = extract(&html).expect("该抽出正文");
        assert!(a.text.contains("fn main"), "代码块要保留：{}", a.text);
        assert!(a.text.contains("列一"), "表头要保留：{}", a.text);
        assert!(a.text.contains("code") || a.text.contains("```") || a.text.contains("fn main"));
    }
}
