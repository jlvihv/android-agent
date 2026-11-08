//! 远程 MCP 服务：核心是客户端，模型通过沙箱里的 `agent mcp` 用它。
//!
//! **只接远程（Streamable HTTP）**，不在本地起服务进程（stdio 那种 `npx` / `uvx`）：
//! 配置里只有名字、URL、请求头，从结构上就填不进一条本地命令。
//!
//! **不进工具表**：MCP 的工具一多就是上万 token 的 schema，而工具表是缓存前缀的一部分
//! —— 增删一个服务，所有会话的缓存整段作废。所以跟浏览器、搜索走同一条路：
//!
//! ```sh
//! agent mcp list [服务]                    # 有哪些服务、哪些工具（一行一个）
//! agent mcp <服务> <工具> --help           # 这个工具的说明和参数 schema（用到时才看）
//! agent mcp <服务> <工具> ['<json参数>']    # 调用
//! agent mcp add <名字> <URL> [--token T]    # 接一个新的（先试连，连得上才存）
//! agent mcp remove <名字>
//! ```
//!
//! **接服务也是模型的活**：用户一般不自己去设置里填，而是把地址（和 token）丢给 agent
//! 让它接。所以提示词里那一句（[`prompt_line`]）**一直在**，没接任何服务时也写着怎么接。
//! 凭据存在核心的库里，之后由核心带着去请求。
//!
//! 输出给模型的都是**纯文本**，出错时以 `错误：` 开头（CLI 据此以非 0 退出），和工具
//! 结果的写法一致。工具回的图片落盘、正文里给路径，模型用 `read` 看：手机上落在**这个
//! 会话自己的目录**下面的 `mcp/`（一个会话的东西都在一处），电脑上没有会话目录（cwd 是
//! 用户的项目，不往里写），落在临时目录。见 [`OutDir`]。

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 我们说的协议版本（`initialize` 里报给服务端，之后每个请求都带在头里）。
const PROTOCOL: &str = "2025-06-18";
/// 一次请求最多等多久（工具可能真要跑一阵）
const TIMEOUT: Duration = Duration::from_secs(120);
/// `list` 里每个工具的说明最多多少字（看全的走 `--help`）
const LIST_DESCRIPTION: usize = 80;

/// 一个远程 MCP 服务的配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// 模型敲的名字（`agent mcp <name> …`）：小写字母 / 数字 / `-` / `_`
    pub name: String,
    /// Streamable HTTP 端点
    pub url: String,
    /// 额外的请求头（一般就是 `Authorization: Bearer …`）
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

impl Server {
    /// 存之前校验。名字要能直接写在 shell 里，URL 只认 http(s)（远程服务）。
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.is_empty()
                && self
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "名字只能用小写字母、数字、- 和 _：{}",
            self.name
        );
        anyhow::ensure!(
            !RESERVED.contains(&self.name.as_str()),
            "{} 是 agent mcp 的子命令，不能当名字",
            self.name
        );
        let u = self.url.trim();
        anyhow::ensure!(
            u.starts_with("https://") || u.starts_with("http://"),
            "只接远程 MCP（http:// 或 https:// 开头的地址）：{u}"
        );
        Ok(())
    }
}

/// `agent mcp` 自己的子命令，不能拿来当服务名。
const RESERVED: &[&str] = &["list", "add", "remove", "help"];

/// `agent mcp add <名字> <URL> [--token T] [--header 'K: V']…` 的参数 → 一个服务。
///
/// `--token T` 就是 `Authorization: Bearer T`（远程 MCP 绝大多数是这个）；别的鉴权方式用
/// `--header`。
pub fn parse_add(args: &[String]) -> anyhow::Result<Server> {
    const ADD_USAGE: &str = "用法：agent mcp add <名字> <URL> [--token T] [--header 'K: V']";
    let mut it = args.iter();
    let name = it.next().ok_or_else(|| anyhow::anyhow!("{ADD_USAGE}"))?;
    let url = it.next().ok_or_else(|| anyhow::anyhow!("{ADD_USAGE}"))?;
    let mut headers = BTreeMap::new();
    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} 后面少了值（{ADD_USAGE}）"))?;
        match flag.as_str() {
            "--token" => {
                let t = value.trim();
                let t = t
                    .strip_prefix("Bearer ")
                    .or_else(|| t.strip_prefix("bearer "))
                    .unwrap_or(t);
                headers.insert("Authorization".to_string(), format!("Bearer {t}"));
            }
            "--header" => {
                let (k, v) = value
                    .split_once(':')
                    .ok_or_else(|| anyhow::anyhow!("--header 要写成 'K: V'：{value}"))?;
                headers.insert(k.trim().to_string(), v.trim().to_string());
            }
            other => anyhow::bail!("不认识的参数 {other}（{ADD_USAGE}）"),
        }
    }
    let srv = Server {
        name: name.trim().to_string(),
        url: url.trim().to_string(),
        headers,
    };
    srv.validate()?;
    Ok(srv)
}

/// 一个工具（`tools/list` 里的一项）。
#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// 工具回的图片 / 音频落在哪。
#[derive(Debug, Clone)]
pub struct OutDir {
    /// 磁盘上的目录
    pub host: PathBuf,
    /// 模型看到的同一个目录
    pub guest: String,
    /// 文件名前缀（临时目录里用 `agent-out-`：启动清扫认这个前缀，见 `gc`）
    pub prefix: &'static str,
}

/// 所有远程服务 + 各自的会话。挂在 `AppState` 上。
#[derive(Default)]
pub struct Mcp {
    servers: std::sync::RwLock<Vec<Server>>,
    /// 服务名 → 握过手了（`Some(会话 id)` / `None` = 服务端没给会话 id，无状态）
    sessions: tokio::sync::Mutex<HashMap<String, Option<String>>>,
    /// 停用的服务名（配置留着，模型用不了、提示词里也不提）
    disabled: std::sync::RwLock<std::collections::HashSet<String>>,
    next_id: AtomicU64,
}

impl Mcp {
    pub fn set_servers(&self, mut list: Vec<Server>) {
        list.sort_by(|a, b| a.name.cmp(&b.name));
        if let Ok(mut g) = self.servers.write() {
            *g = list;
        }
    }

    /// 全部服务（按名字排序，含停用的）。
    pub fn servers(&self) -> Vec<Server> {
        self.servers.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// 启用着的服务（提示词里列的就是这些）。
    pub fn enabled_servers(&self) -> Vec<Server> {
        self.servers()
            .into_iter()
            .filter(|s| self.is_enabled(&s.name))
            .collect()
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        self.disabled.read().map_or(true, |g| !g.contains(name))
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) {
        if let Ok(mut g) = self.disabled.write() {
            if enabled {
                g.remove(name);
            } else {
                g.insert(name.to_string());
            }
        }
    }

    /// 能用的那个服务：不存在、停用了都是错（说清楚是哪种）。
    fn server(&self, name: &str) -> anyhow::Result<Server> {
        let srv = self
            .servers()
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                let names: Vec<String> =
                    self.enabled_servers().into_iter().map(|s| s.name).collect();
                if names.is_empty() {
                    anyhow::anyhow!("没有叫 {name} 的 MCP 服务（还没接任何服务）")
                } else {
                    anyhow::anyhow!("没有叫 {name} 的 MCP 服务（有的是：{}）", names.join("、"))
                }
            })?;
        anyhow::ensure!(
            self.is_enabled(name),
            "MCP 服务 {name} 已停用（用户在 设置 → MCP 服务 里关掉的），要用得让用户打开"
        );
        Ok(srv)
    }

    /// 换了配置（改 URL / 请求头 / 删掉）：旧会话作废，下次重新握手。
    pub async fn forget_session(&self, name: &str) {
        self.sessions.lock().await.remove(name);
    }

    /// 全部工具（`tools/list`，跟着游标翻页）。
    pub async fn tools(&self, srv: &Server) -> anyhow::Result<Vec<Tool>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let r = self.rpc(srv, "tools/list", params).await?;
            let page: Vec<Tool> = serde_json::from_value(r["tools"].clone())
                .map_err(|e| anyhow::anyhow!("{} 的工具清单看不懂：{e}", srv.name))?;
            out.extend(page);
            match r.get("nextCursor").and_then(|v| v.as_str()) {
                Some(c) if !c.is_empty() => cursor = Some(c.to_string()),
                _ => return Ok(out),
            }
        }
    }

    /// 调一个工具，回原样的 `result`（`content` / `isError` / `structuredContent`）。
    pub async fn call(&self, srv: &Server, tool: &str, args: Value) -> anyhow::Result<Value> {
        self.rpc(
            srv,
            "tools/call",
            json!({ "name": tool, "arguments": args }),
        )
        .await
    }

    /// 一次 JSON-RPC 请求（没握手就先握手；会话过期 —— 404 —— 就重新握手再来一次）。
    async fn rpc(&self, srv: &Server, method: &str, params: Value) -> anyhow::Result<Value> {
        for attempt in 0..2 {
            let session = self.session(srv).await?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
            let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            let r = post(srv, session.as_deref(), &body).await?;
            if r.status == 404 && session.is_some() && attempt == 0 {
                // 规范：会话过期回 404，客户端要重新 initialize
                self.forget_session(&srv.name).await;
                continue;
            }
            return result_of(srv, &r);
        }
        unreachable!("上面的循环第二次一定 return")
    }

    /// 这个服务的会话 id（没握过手就现握）。
    async fn session(&self, srv: &Server) -> anyhow::Result<Option<String>> {
        let mut g = self.sessions.lock().await;
        if let Some(s) = g.get(&srv.name) {
            return Ok(s.clone());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let init = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": { "name": "android-agent", "version": env!("CARGO_PKG_VERSION") },
            },
        });
        let r = post(srv, None, &init).await?;
        result_of(srv, &r)?;
        let session = r.session;
        // 握手的最后一步：告诉服务端「我准备好了」（通知，没有回复）
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        let n = post(srv, session.as_deref(), &note).await?;
        anyhow::ensure!(
            (200..300).contains(&n.status),
            "{} 握手没完成（notifications/initialized 回 {}）",
            srv.name,
            n.status
        );
        g.insert(srv.name.clone(), session.clone());
        Ok(session)
    }
}

struct Reply {
    status: u16,
    session: Option<String>,
    body: String,
}

async fn post(srv: &Server, session: Option<&str>, body: &Value) -> anyhow::Result<Reply> {
    let http = crate::llm::shared_http()?;
    let mut rb = http
        .post(srv.url.trim())
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", PROTOCOL)
        .timeout(TIMEOUT)
        .json(body);
    for (k, v) in &srv.headers {
        rb = rb.header(k, v);
    }
    if let Some(s) = session {
        rb = rb.header("mcp-session-id", s);
    }
    // 连接层失败（请求根本没发出去：握手被掐、刚好断网一下）重试一次 —— 手机网络上
    // 常见。服务端回了响应（哪怕是错误）就不重试：那可能已经执行过了。
    let retry = rb.try_clone();
    let resp = match rb.send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() || e.is_request() => match retry {
            Some(rb) => rb.send().await,
            None => Err(e),
        }
        .map_err(|e| anyhow::anyhow!("连不上 MCP 服务 {}（{}）：{e}", srv.name, srv.url))?,
        Err(e) => anyhow::bail!("连不上 MCP 服务 {}（{}）：{e}", srv.name, srv.url),
    };
    let status = resp.status().as_u16();
    let session = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    Ok(Reply {
        status,
        session,
        body,
    })
}

/// HTTP 回复 → JSON-RPC 的 `result`（HTTP 错、JSON-RPC 错都变成 `Err`）。
fn result_of(srv: &Server, r: &Reply) -> anyhow::Result<Value> {
    let clip = |s: &str| s.chars().take(300).collect::<String>();
    match r.status {
        401 | 403 => anyhow::bail!(
            "MCP 服务 {} 拒绝了（HTTP {}）：多半是凭据不对或过期了，要在设置里改请求头。{}",
            srv.name,
            r.status,
            clip(&r.body)
        ),
        s if !(200..300).contains(&s) => {
            anyhow::bail!("MCP 服务 {} 回 HTTP {s}：{}", srv.name, clip(&r.body))
        }
        _ => {}
    }
    let v = decode_rpc_body(&r.body)?;
    if let Some(e) = v.get("error") {
        let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
        anyhow::bail!("MCP 服务 {} 报错：{msg}（{e}）", srv.name);
    }
    v.get("result").cloned().ok_or_else(|| {
        anyhow::anyhow!(
            "MCP 服务 {} 的回复里没有 result：{}",
            srv.name,
            clip(&r.body)
        )
    })
}

/// 把响应体解码成 JSON-RPC 信封。
///
/// Streamable HTTP 的服务端可以回 **SSE**（`event: message` + `data: {…}`），也可以
/// 直接回 JSON —— 两种都认：先扫 `data:` 行，扫不到就整段当 JSON 解。
pub fn decode_rpc_body(body: &str) -> anyhow::Result<Value> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() || rest == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(rest) {
            // 一帧可能是通知（没有 result/error），跳过继续找
            if v.get("result").is_some() || v.get("error").is_some() {
                return Ok(v);
            }
        }
    }
    serde_json::from_str::<Value>(body)
        .map_err(|e| anyhow::anyhow!("MCP 的响应既不是 SSE 也不是 JSON: {e}"))
}

// ---------------------------------------------------------------------------
// `agent mcp …`
// ---------------------------------------------------------------------------

const USAGE: &str = "用法：
  agent mcp list [服务]                    已接的服务和它们的工具
  agent mcp <服务> <工具> --help           这个工具的说明和参数
  agent mcp <服务> <工具> ['<json参数>']    调用
  agent mcp add <名字> <URL> [--token T] [--header 'K: V']   接一个远程 MCP 服务（先试连，连得上才存）
  agent mcp remove <名字>                  删掉一个";

/// `agent mcp` 的参数 → 给模型看的文字。`out` 是工具回的图片落在哪（`None` = 没地方放）。
pub async fn run(mcp: &Mcp, argv: &[String], out: Option<OutDir>) -> anyhow::Result<String> {
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["--help" | "-h" | "help"] => Ok(USAGE.to_string()),
        [] | ["list"] => list(mcp, None).await,
        // 对服务（而不是工具）要帮助：就是列出它的工具 —— 不然 `--help` 会被当成工具名调出去
        ["list", name] | [name] | [name, "--help" | "-h"] => list(mcp, Some(name)).await,
        [name, tool, "--help" | "-h"] => help(mcp, name, tool).await,
        [name, tool] => call(mcp, name, tool, "{}", out).await,
        [name, tool, raw] => call(mcp, name, tool, raw, out).await,
        _ => anyhow::bail!("{USAGE}"),
    }
}

async fn list(mcp: &Mcp, only: Option<&str>) -> anyhow::Result<String> {
    let servers = match only {
        Some(name) => vec![mcp.server(name)?],
        None => mcp.servers(),
    };
    if servers.is_empty() {
        return Ok("还没接任何 MCP 服务。".into());
    }
    let mut out = Vec::new();
    for srv in &servers {
        if !mcp.is_enabled(&srv.name) {
            out.push(format!("{}（已停用）", srv.name));
            continue;
        }
        match mcp.tools(srv).await {
            Ok(tools) => {
                out.push(format!("{}（{} 个工具）", srv.name, tools.len()));
                for t in tools {
                    out.push(format!(
                        "  {}  {}",
                        t.name,
                        first_line(&t.description, LIST_DESCRIPTION)
                    ));
                }
            }
            // 列全部时一个服务挂了不连累别的；只看一个的话就是错
            Err(e) if only.is_none() => out.push(format!("{}（连不上：{e:#}）", srv.name)),
            Err(e) => return Err(e),
        }
    }
    out.push(String::new());
    out.push(
        "看参数：agent mcp <服务> <工具> --help；调用：agent mcp <服务> <工具> '<json参数>'".into(),
    );
    Ok(out.join("\n"))
}

async fn help(mcp: &Mcp, name: &str, tool: &str) -> anyhow::Result<String> {
    let srv = mcp.server(name)?;
    let tools = mcp.tools(&srv).await?;
    let t = tools.into_iter().find(|t| t.name == tool).ok_or_else(|| {
        anyhow::anyhow!("{name} 里没有工具 {tool}（agent mcp list {name} 看有哪些）")
    })?;
    let schema = serde_json::to_string_pretty(&t.input_schema).unwrap_or_default();
    Ok(format!(
        "{name} {tool}\n\n{}\n\n参数（JSON Schema）：\n{schema}\n\n调用：agent mcp {name} {tool} '<json参数>'",
        t.description.trim()
    ))
}

async fn call(
    mcp: &Mcp,
    name: &str,
    tool: &str,
    raw: &str,
    out: Option<OutDir>,
) -> anyhow::Result<String> {
    let srv = mcp.server(name)?;
    let args: Value = serde_json::from_str(raw).map_err(|e| {
        anyhow::anyhow!("参数得是一个 JSON 对象（agent mcp {name} {tool} --help 看要哪些）：{e}")
    })?;
    anyhow::ensure!(
        args.is_object(),
        "参数得是一个 JSON 对象，比如 '{{\"query\":\"…\"}}'（agent mcp {name} {tool} --help 看要哪些）"
    );
    let result = mcp.call(&srv, tool, args).await?;
    let text = render_result(&result, out.as_ref());
    if result.get("isError").and_then(serde_json::Value::as_bool) == Some(true) {
        anyhow::bail!("{name} {tool} 失败：{text}");
    }
    Ok(text)
}

/// `tools/call` 的 `result` → 文字。图片 / 音频落盘给路径，资源给正文或链接。
fn render_result(result: &Value, out: Option<&OutDir>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in result["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match c["type"].as_str().unwrap_or("") {
            "text" => parts.push(c["text"].as_str().unwrap_or("").to_string()),
            kind @ ("image" | "audio") => {
                let mime = c["mimeType"].as_str().unwrap_or("application/octet-stream");
                parts.push(
                    match save_blob(c["data"].as_str().unwrap_or(""), mime, out) {
                        Ok(path) => format!(
                            "[{kind}（{mime}）：{path}{}]",
                            if kind == "image" {
                                "，用 read 看"
                            } else {
                                ""
                            }
                        ),
                        Err(e) => format!("[{kind}（{mime}）没能保存：{e:#}]"),
                    },
                );
            }
            "resource" => {
                let r = &c["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                match r["text"].as_str() {
                    Some(t) => parts.push(format!("[资源 {uri}]\n{t}")),
                    None => parts.push(format!("[资源 {uri}（二进制，没展开）]")),
                }
            }
            "resource_link" => parts.push(format!(
                "[链接 {}：{}]",
                c["name"].as_str().unwrap_or(""),
                c["uri"].as_str().unwrap_or("")
            )),
            other => parts.push(format!("[{other}] {c}")),
        }
    }
    // 只有结构化结果、没有正文的工具：把结构化那份摊开
    if parts.is_empty() {
        if let Some(s) = result.get("structuredContent") {
            parts.push(serde_json::to_string_pretty(s).unwrap_or_default());
        }
    }
    if parts.is_empty() {
        return "(无输出)".into();
    }
    parts.join("\n")
}

/// 工具回的二进制（base64）落盘，回模型看到的路径。
fn save_blob(b64: &str, mime: &str, out: Option<&OutDir>) -> anyhow::Result<String> {
    use base64::Engine;
    let out = out.ok_or_else(|| anyhow::anyhow!("没地方放"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
    // 图片格式走 `imaging::ext_of` 那唯一一份表；音频只有 MCP 服务会给，另开两个
    let ext = match mime {
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        other => crate::imaging::ext_of(other),
    };
    // uuid v7 按时间排序：同一个目录里 ls 出来就是先后顺序
    let name = format!("{}{}.{ext}", out.prefix, uuid::Uuid::now_v7());
    std::fs::create_dir_all(&out.host)?;
    std::fs::write(out.host.join(&name), bytes)?;
    Ok(format!("{}/{name}", out.guest.trim_end_matches('/')))
}

fn first_line(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// 拼进系统提示词的那一句（传进来的是**启用着的**服务）。**一直在**：没接任何服务时
/// 也要告诉模型怎么接（接服务一般是用户让它做的）。
pub fn prompt_line(servers: &[Server]) -> String {
    let have = if servers.is_empty() {
        // 不说「还没接」：可能是接了但被用户停用了 —— 那种不该让模型再去接一遍
        "现在没有可用的服务".to_string()
    } else {
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        format!(
            "已接 {}：`agent mcp list` 看有哪些工具，`agent mcp <服务> <工具> --help` 看参数，`agent mcp <服务> <工具> '<json参数>'` 调用",
            names.join("、")
        )
    };
    format!(
        "- `agent mcp`（远程 MCP 服务）：{have}。用户给你一个 MCP 地址时用 `agent mcp add <名字> <URL> [--token T]` 接上（只接 http(s) 的远程服务），`agent mcp remove <名字>` 删掉。"
    )
}
