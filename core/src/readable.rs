//! `agent readable`：给一段 HTML，抽出文章正文（纯文本）。
//!
//! 像一个普通的 Unix 工具：只管 HTML → 文字，不联网（网页由 curl / wget 去拿），
//! `curl -sL <url> | agent readable`。
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
//! 和浏览器里那份 `READABLE_JS`（`browser_js.rs`）一个写法 —— 模型从两边拿到的正文长得
//! 一样：段落之间空一行；标题写成 `## 标题`；`<pre>` 包进 ```` ``` ````；一段话基本就是
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
    // 同分取后面的：先序里后面的是更深的那层（只包着一个子元素的外壳和它里面同分）
    let mut best = 0;
    for (i, &s) in v.iter().enumerate().skip(1) {
        if !doc.nodes[i].is_text() && s > 0 && s >= v[best] {
            best = i;
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
    fn counted(s: &str) -> Self {
        Inline {
            s: s.to_string(),
            chars: s.chars().filter(|c| c.is_alphanumeric()).count(),
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

/// 链接去哪：同站只写路径，别的站写 域名 + 路径（`READABLE_JS` 同款，外站的短查询串多留着）。
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
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))
        .unwrap_or("");
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARA: &str = "这是一段足够长的正文，用来让程序认出这里才是文章的主体部分，\
        而不是导航、页脚或者推荐位。它得有几十个字才像一段话。";

    fn page(body: &str) -> String {
        format!(
            r#"<!doctype html><html><head><title>测试文章 - 某站</title>
            <meta property="og:url" content="https://example.com/post/1"></head><body>
            <nav><a href="/">首页</a> <a href="/about">关于</a></nav>
            <article><h1>测试文章</h1>{body}</article>
            <footer><p>版权所有 © 某站，保留一切权利。联系我们，隐私政策，使用条款。</p></footer>
            </body></html>"#
        )
    }

    fn text_of(body: &str) -> Article {
        extract(&page(body)).unwrap()
    }

    #[test]
    fn keeps_the_article_and_drops_the_chrome() {
        let a = text_of(&format!(
            "<p>{PARA}</p><p>{PARA}</p><h2>小节</h2><p>{PARA}</p>"
        ));
        assert!(!a.fallback);
        assert_eq!(a.title, "测试文章", "h1 和 <title> 对得上就用 h1");
        assert!(a.text.contains("## 小节"), "{}", a.text);
        assert!(a.text.contains(PARA));
        for junk in ["首页", "关于", "版权所有"] {
            assert!(!a.text.contains(junk), "{junk} 不该在正文里：{}", a.text);
        }
        // 标题已经在头上了，正文不再以它开头
        assert!(!a.text.starts_with("## 测试文章"), "{}", a.text);
    }

    /// 中文新闻页的典型样子：一大片导航和推荐，正文段落又短，标题在正文容器外面
    #[test]
    fn short_paragraph_news_among_heavy_chrome() {
        let links = |n: usize, what: &str| -> String {
            (1..=n)
                .map(|i| format!("<li><a href=/{i}>{what}第{i}条</a></li>"))
                .collect()
        };
        let html = format!(
            r#"<html><head><title>某地发布新规_新闻</title>
            <meta name="author" content="记者 张三"><meta property="article:published_time" content="2026-10-16">
            </head><body>
            <div class="top"><ul>{}</ul></div>
            <div class="wrap"><h1>某地发布新规</h1>
              <div class="info">2026年10月16日 来源：某报</div>
              <div class="art">
                <p>本报讯 某地今天发布新规，自下月起施行。</p>
                <p>新规明确了三方面要求，涉及出行、住房和教育，覆盖全市所有区县。</p>
                <p>有关负责人表示，新规是在广泛征求意见的基础上制定的，将分步落实。</p>
                <p>下一步，相关部门将出台配套细则，并加强督促检查，确保落到实处。</p>
              </div>
              <div class="hot"><h3>热门推荐</h3><ul>{}</ul></div>
            </div>
            <div class="foot">关于我们 | 联系方式 | 网站地图</div>
            </body></html>"#,
            links(30, "频道"),
            links(10, "推荐")
        );
        let a = extract(&html).unwrap();
        assert!(!a.fallback, "{a:?}");
        assert_eq!(a.title, "某地发布新规");
        assert_eq!(a.meta, "记者 张三 · 2026-10-16");
        assert!(a.text.starts_with("本报讯"), "{}", a.text);
        assert!(a.text.contains("确保落到实处。"), "{}", a.text);
        for junk in ["频道第", "推荐第", "热门推荐", "网站地图"] {
            assert!(!a.text.contains(junk), "{junk}：{}", a.text);
        }
    }

    #[test]
    fn comments_and_hidden_things_are_dropped() {
        let a = text_of(&format!(
            "<p>{PARA}</p><p>{PARA}</p><div style='display: none'>藏起来的一段很长很长的文字，不该被读出来。</div>\
             <section id=comments><p>网友甲：写得好！这篇文章让我想起了很多事情，真的很有感触。</p>\
             <p>网友乙：楼上说得对，我也是这么觉得的，希望作者多写一些这样的文章。</p></section>"
        ));
        for junk in ["藏起来", "网友甲", "网友乙"] {
            assert!(!a.text.contains(junk), "{junk}：{}", a.text);
        }
    }

    #[test]
    fn code_blocks_keep_their_lines() {
        let a = text_of(&format!(
            "<p>{PARA}</p><pre><code>fn main() {{\n    println!(\"hi\");\n}}</code></pre><p>{PARA}</p>"
        ));
        assert!(
            a.text
                .contains("```\nfn main() {\n    println!(\"hi\");\n}\n```"),
            "{}",
            a.text
        );
    }

    #[test]
    fn nested_lists_and_quotes_are_read_once() {
        let a = text_of(&format!(
            "<p>{PARA}</p><ul><li>第一项<ul><li>里面一项</li></ul></li><li>第二项</li></ul>\
             <blockquote><p>引用的一段话。</p></blockquote><p>{PARA}</p>"
        ));
        for s in ["第一项", "里面一项", "第二项", "引用的一段话。"] {
            assert_eq!(a.text.matches(s).count(), 1, "{s} 该出现一次：{}", a.text);
        }
    }

    #[test]
    fn data_tables_become_rows() {
        let a = text_of(&format!(
            "<p>{PARA}</p><table><tr><th>城市</th><th>人口</th></tr>\
             <tr><td>上海</td><td>2487 万</td></tr></table><p>{PARA}</p>"
        ));
        assert!(a.text.contains("城市 | 人口\n上海 | 2487 万"), "{}", a.text);
    }

    #[test]
    fn whitespace_collapses_and_br_breaks() {
        let a = text_of(&format!(
            "<p>{PARA}</p><p>第一行   <b>加粗</b>\n  接着<br>第二行</p><p>{PARA}</p>"
        ));
        assert!(a.text.contains("第一行 加粗 接着\n第二行"), "{}", a.text);
    }

    #[test]
    fn link_only_paragraphs_say_where_they_go() {
        let a = text_of(&format!(
            "<p>{PARA}</p><p>相关：<a href=\"https://example.com/post/2\">下一篇文章的标题</a></p>\
             <p>{PARA}</p><p><a href=\"https://other.org/x?utm_source=feed\">别的站上的一篇文章</a></p><p>{PARA}</p>"
        ));
        assert!(
            a.text.contains("相关：下一篇文章的标题  [/post/2]"),
            "{}",
            a.text
        );
        assert!(
            a.text.contains("别的站上的一篇文章  [other.org/x]"),
            "{}",
            a.text
        );
        assert_eq!(
            where_to("https://example.com/a?b=1#c", "example.com").as_deref(),
            Some("/a?b=1")
        );
        assert_eq!(
            where_to("https://other.org/x?utm_source=feed", "example.com").as_deref(),
            Some("other.org/x")
        );
        assert_eq!(
            where_to("https://news.ycombinator.com/item?id=1", "example.com").as_deref(),
            Some("news.ycombinator.com/item?id=1")
        );
        assert_eq!(where_to("/rel", "").as_deref(), Some("/rel"));
        assert_eq!(where_to("#top", "example.com"), None);
    }

    /// 正文里夹着一串链接（「延伸阅读」）、结尾一串链接：剃掉，连同它的小标题
    #[test]
    fn link_runs_inside_the_article_are_trimmed() {
        let list: String = (1..=4)
            .map(|i| format!("<p><a href=/r/{i}>延伸阅读第{i}篇</a></p>"))
            .collect();
        let a = text_of(&format!(
            "<p>{PARA}</p><h3>延伸阅读</h3>{list}<p>{PARA}再来一段。</p><p><a href=/x>上一篇</a></p>"
        ));
        assert!(!a.text.contains("延伸阅读"), "{}", a.text);
        assert!(!a.text.contains("上一篇"), "{}", a.text);
        assert!(a.text.contains("再来一段。"), "{}", a.text);
    }

    /// 老网页：没有 `<p>`、拿表格排版、`<br><br>` 分段（Paul Graham 的文章就长这样）
    #[test]
    fn old_style_pages_with_br_and_layout_tables() {
        let [p1, p2, p3] = ["一", "二", "三"].map(|n| format!("第{n}段。{}", PARA.repeat(3)));
        let html = format!(
            "<html><head><title>老文章</title></head><body><table><tr><td><img src=menu.gif></td>\
             <td><font>{p1}<br><br>{p2}<br><br>{p3}</font></td></tr></table></body></html>"
        );
        let a = extract(&html).unwrap();
        assert!(!a.fallback, "{a:?}");
        assert!(
            !a.text.contains(" | "),
            "排版用的表不该排成表格：{}",
            a.text
        );
        assert_eq!(a.text, format!("{p1}\n\n{p2}\n\n{p3}"));
    }

    /// 不是文章的页面（门户首页、列表页）：退回整页，不丢内容
    #[test]
    fn non_articles_fall_back_to_everything() {
        let col = |name: &str| {
            let items: String = (1..=8)
                .map(|i| format!("<li><a href=/{name}/{i}>{name}频道的第{i}条新闻标题</a></li>"))
                .collect();
            format!("<div class={name}><h2>{name}</h2><ul>{items}</ul></div>")
        };
        let html = format!(
            "<html><head><title>门户</title></head><body><nav><a href=/>首页</a></nav>{}{}{}</body></html>",
            col("国内"),
            col("国际"),
            col("体育")
        );
        let a = extract(&html).unwrap();
        assert!(a.fallback, "{a:?}");
        assert!(render(&a).starts_with("（没认出正文"), "{}", render(&a));
        for s in [
            "首页",
            "国内频道的第1条",
            "国际频道的第8条",
            "体育频道的第3条",
        ] {
            assert!(a.text.contains(s), "{s}：{}", a.text);
        }
    }

    #[test]
    fn units_count_cjk_chars_and_latin_words() {
        assert_eq!(units("中文三个"), 4);
        assert_eq!(units("three latin words"), 3);
        assert_eq!(
            units("用 Rust 写的 v1.2"),
            6,
            "用 / Rust / 写 / 的 / v1 / 2"
        );
    }

    #[test]
    fn empty_pages_are_an_error() {
        let e = extract(r#"<html><body><div id="app"></div><script>boot()</script></body></html>"#)
            .unwrap_err();
        assert!(e.to_string().contains("JS 渲染"), "{e}");
    }

    #[test]
    fn non_utf8_says_how_to_convert() {
        // 「中文」的 GBK 编码
        let html =
            b"<html><head><meta charset=\"gb2312\"></head><body>\xd6\xd0\xce\xc4</body></html>";
        let e = extract_bytes(html).unwrap_err().to_string();
        assert!(e.contains("iconv -f gb2312 -t utf-8"), "{e}");
    }

    #[test]
    fn render_puts_title_and_meta_on_top() {
        let a = Article {
            title: "标题".into(),
            meta: "张三 · 2026-10-16".into(),
            text: "正文".into(),
            fallback: false,
        };
        assert_eq!(render(&a), "标题\n张三 · 2026-10-16\n\n正文");
        let page = Article {
            meta: String::new(),
            fallback: true,
            ..a
        };
        assert_eq!(
            render(&page),
            "（没认出正文，下面是整页的文字）\n标题\n\n正文"
        );
    }
}
