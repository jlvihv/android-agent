//! LLM 接入层。
//!
//! **不引 SDK，自己实现协议**（原决策 D4 是「用 `async-openai`」，已修订）：
//! 我们只用得到一个请求体 + 一条 SSE 流，而 SDK 会在我们拿不到的地方建
//! 自己的 reqwest client（`rustls-no-provider` 下直接 panic）。协议类型在
//! `openai.rs`（Chat Completions）和 `responses.rs`（Responses API）。
//!
//! **优先用 Responses API**（`ApiStyle::Auto`）：它对图片/工具结果的表达更
//! 干净；端点不支持（404/405）就自动退回 Chat Completions 并记住。
//!
//! 覆盖范围：任何提供 **OpenAI 兼容端点**的服务 —— OpenAI、DeepSeek、Kimi、
//! 通义 DashScope、智谱、SiliconFlow、OpenRouter、Groq、xAI、Together、
//! Fireworks、vLLM、LM Studio、Ollama(`/v1`)，以及绝大多数中转站。
//!
//! 已知不支持：Anthropic / Gemini 的**原生**协议（需走中转或另加 adapter）。

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::openai::{self, ChatRequest, StreamChunk, UserContent};
use crate::responses::ResponsesRequest;

/// OpenCode Go 明确要求：客户端要"用自己的 user agent 标识自己，
/// 而不是通用 SDK / HTTP 库名"。
pub const USER_AGENT: &str = concat!("android-agent/", env!("CARGO_PKG_VERSION"));

/// 用哪套协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiStyle {
    /// 先试 Responses，端点不支持就退回 Chat Completions（默认）
    #[default]
    Auto,
    /// 只用 Responses API
    Responses,
    /// 只用 Chat Completions
    Chat,
}

/// `ApiStyle` 落地后的具体协议（不会再有 `Auto`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Responses,
    Chat,
}

/// 规整思考强度：trim + 小写；空串 = 不设置（`None`）。
///
/// 不做白名单校验 —— 各家的档位集合不一样（OpenAI 有 `minimal`，别家可能只有
/// `low/high`），这里只负责「空 = 不发」，具体取值直接透传。
pub fn normalize_effort(s: &str) -> Option<String> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// 一个 provider 配置。`base_url` 是接入任何兼容服务的唯一开关。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    pub model: String,
    /// 不要把 key 广播给前端；前端只看到 `has_key`
    #[serde(skip_serializing)]
    pub api_key: String,
    /// 额外的请求头。OpenCode Go 需要 `x-opencode-session`（见 `client()`）。
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    /// 这个模型能不能吃 `image_url`（`read` 读图片时要发）。
    ///
    /// 默认 **true** —— 关错了只会多一条文字说明，开错了是请求直接 400。
    /// 不能用时可以 `AGENT_LLM_VISION=0`（或 `/api/llm` 的 `vision:false`）关掉。
    #[serde(default = "default_supports_images")]
    pub supports_images: bool,
    /// 这个模型能不能吃 PDF / 文档输入（OpenAI 的 `file` 内容块）。
    ///
    /// 由 models.dev 的 `pdf` 模态自动判定，也持久化到 llm.json。
    /// 默认 **false** —— 绝大多数兼容端点不认 `file` 内容块，瞎发就是 400；
    /// 默认关掉只意味着界面上不显示「文件」入口。
    #[serde(default)]
    pub supports_files: bool,
    /// 协议偏好。见 `ApiStyle`。
    #[serde(default)]
    pub api_style: ApiStyle,
    /// 思考强度：`minimal` / `low` / `medium` / `high`，`None` = 默认档（不发）。
    ///
    /// 各家的档位叫法并不完全一致，这里只是**原样透传**给端点（Chat 放顶层、
    /// Responses 放 `reasoning.effort`）；不认识它的端点会忽略，严格的会 400 ——
    /// 所以默认是 `None`，即一个字都不发。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

pub(crate) fn default_supports_images() -> bool {
    true
}

impl Provider {
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self {
            name: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5".into(),
            api_key: api_key.into(),
            extra_headers: BTreeMap::new(),
            supports_images: true,
            supports_files: false,
            api_style: ApiStyle::Auto,
            reasoning_effort: None,
        }
    }

    pub fn compatible(
        name: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key: api_key.into(),
            extra_headers: BTreeMap::new(),
            supports_images: true,
            supports_files: false,
            api_style: ApiStyle::Auto,
            reasoning_effort: None,
        }
    }

    /// DeepSeek 官方端点
    pub fn deepseek(api_key: impl Into<String>) -> Self {
        Self::compatible(
            "deepseek",
            "https://api.deepseek.com/v1",
            "deepseek-chat",
            api_key,
        )
    }

    /// 本地 Ollama（不需要真 key，占位即可）
    pub fn ollama(model: impl Into<String>) -> Self {
        Self::compatible("ollama", "http://127.0.0.1:11434/v1", model, "ollama")
    }

    /// 从环境变量读配置。
    ///
    /// 优先 `AGENT_LLM_*`（通用），回退到 `OPENCODE_*`（Go / Zen 端点）。
    pub fn from_env() -> Option<Self> {
        let get = |a: &str, b: &str| std::env::var(a).ok().or_else(|| std::env::var(b).ok());

        let key = get("AGENT_LLM_API_KEY", "OPENCODE_API_KEY")?;
        if key.trim().is_empty() {
            return None;
        }
        let base_url = get("AGENT_LLM_BASE_URL", "OPENCODE_BASE_URL")
            .unwrap_or_else(|| "https://api.openai.com/v1".into());
        let model = get("AGENT_LLM_MODEL", "OPENCODE_MODEL").unwrap_or_else(|| "gpt-5".into());
        let name = get("AGENT_LLM_NAME", "OPENCODE_NAME").unwrap_or_else(|| "custom".into());
        // 强制协议：`responses` / `chat` / `auto`（不设 = auto）
        let api_style = match get("AGENT_LLM_API_STYLE", "OPENCODE_API_STYLE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "responses" => ApiStyle::Responses,
            "chat" | "chat_completions" => ApiStyle::Chat,
            _ => ApiStyle::Auto,
        };
        let effort = normalize_effort(
            &get("AGENT_LLM_REASONING_EFFORT", "OPENCODE_REASONING_EFFORT").unwrap_or_default(),
        );

        Some(Self {
            name,
            base_url,
            model,
            api_key: key,
            extra_headers: BTreeMap::new(),
            // 占位；真正的值由 `AppState::supports_images_for` 从 models.dev 自动判定
            supports_images: true,
            supports_files: false,
            api_style,
            reasoning_effort: effort,
        })
    }

    /// 建 TLS 配置，用**内嵌的 Mozilla 根证书**（`webpki-roots`）。
    ///
    /// # 为什么不用系统根证书
    ///
    /// reqwest 0.13 的 `rustls` feature 硬编码走 `rustls-platform-verifier`。
    /// 而它在 Android 上必须先初始化：
    ///
    /// ```text
    /// RUST PANIC: panicked at rustls-platform-verifier-0.7.1/src/android.rs:90:10:
    /// Expect rustls-platform-verifier to be initialized
    /// ```
    ///
    /// （实测踩过：`/api/chat` 只发出 start/start-step 就断了，因为 panic
    /// 打到 stderr，而 Android 上 stderr 是丢掉的 —— 加了 panic hook 才看见。）
    ///
    /// 初始化需要 JNIEnv + Android `Context`。在 Rust 里拿这两个要手写
    /// `JNI_OnLoad` + 反调 `ActivityThread.currentApplication()` —— 正是
    /// **决策 D2 想避开的手写 JNI**。
    ///
    /// 权衡：我们的 agent 只连公开的 LLM API，标准 CA 完全够用。
    ///
    /// # 代价
    ///
    /// 不认用户/企业自签的 CA。如果以后要用企业 MITM 代理（Charles/mitmproxy），
    /// 得换回平台验证器，那时才值得为它写那段 JNI。
    fn tls_config() -> anyhow::Result<rustls::ClientConfig> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Ok(rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth())
    }

    fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base())
    }

    pub fn responses_url(&self) -> String {
        format!("{}/responses", self.base())
    }

    /// 实际用哪套协议（`Auto` 会在这里落地成具体的一种）。
    pub fn transport(&self) -> Transport {
        match self.api_style {
            ApiStyle::Chat => Transport::Chat,
            ApiStyle::Responses => Transport::Responses,
            ApiStyle::Auto => {
                if responses_known_unsupported(self) {
                    Transport::Chat
                } else {
                    Transport::Responses
                }
            }
        }
    }

    /// 发一个 JSON 请求并拿到流式响应。非 2xx 包成 `StreamError::Http`。
    async fn post_stream(
        &self,
        url: &str,
        body: &impl Serialize,
        session_id: &str,
    ) -> Result<reqwest::Response, StreamError> {
        let http = shared_http().map_err(StreamError::Other)?;
        let mut rb = http.post(url).header("user-agent", USER_AGENT).json(body);
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        if !session_id.is_empty() {
            rb = rb.header("x-opencode-session", session_id);
        }
        for (k, v) in &self.extra_headers {
            rb = rb.header(k, v);
        }
        let resp = rb.send().await.map_err(|e| StreamError::Other(e.into()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(StreamError::Http {
                status: status.as_u16(),
                body,
            });
        }
        Ok(resp)
    }

    /// Chat Completions 的流。
    pub async fn stream_chat(
        &self,
        req: &ChatRequest,
        session_id: &str,
    ) -> Result<mpsc::UnboundedReceiver<anyhow::Result<StreamChunk>>, StreamError> {
        let resp = self.post_stream(&self.chat_url(), req, session_id).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let r = openai::pump_frames(resp, tx.clone(), |data| {
                Ok(Some(serde_json::from_str::<StreamChunk>(data)?))
            })
            .await;
            if let Err(e) = r {
                let _ = tx.send(Err(e));
            }
        });
        Ok(rx)
    }

    /// Responses API 的流。事件在 `responses::Mapper` 里映射回 `StreamChunk`。
    pub async fn stream_responses(
        &self,
        req: &ResponsesRequest,
        session_id: &str,
    ) -> Result<mpsc::UnboundedReceiver<anyhow::Result<StreamChunk>>, StreamError> {
        let resp = self
            .post_stream(&self.responses_url(), req, session_id)
            .await?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut mapper = crate::responses::Mapper::default();
            let r = openai::pump_frames(resp, tx.clone(), |data| mapper.map(data)).await;
            if let Err(e) = r {
                let _ = tx.send(Err(e));
            }
        });
        Ok(rx)
    }
}

/// 装 rustls 的进程级 CryptoProvider（ring）。
///
/// rustls 0.23 **不会**自动装；reqwest 开了 `rustls-no-provider` 后更是要求
/// 调用方自己装，否则建 reqwest Client 时直接 panic：
///
/// ```text
/// No rustls crypto provider is configured...
/// ```
///
/// **必须在建任何 HTTP client 之前调**（搜索、模型目录也各自用 `shared_http`）。
/// 所以放在 `init_tracing()` 里（桌面 / 安卓两条启动路径都经过它）。
/// 幂等：重复调只生效一次。
pub fn install_crypto_provider() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 进程级共享的 HTTP client。
///
/// 之前每次 `stream_one` 都 `ClientBuilder::new()…build()` 造一个新的，而
/// `reqwest::Client` 本身就是连接池 —— 等于每个 step 都重新做 TLS 握手，
/// 完全丢掉 keep-alive。一个回合 5–8 次 LLM 调用，在手机网络上是白送的 RTT。
///
/// TLS 配置与 provider 无关（只用内嵌的 Mozilla 根证书，见 `tls_config`），
/// 所以全进程共享一个就够。`reqwest::Client` 的内部是 Arc，克隆很便宜。
pub(crate) fn shared_http() -> anyhow::Result<reqwest::Client> {
    static HTTP: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    HTTP.get_or_init(|| {
        // rustls 0.23 **不会**自动装进程级 CryptoProvider；而 reqwest 开了
        // `rustls-no-provider` 后不但不装，构建 Client 时还硬要求它已经存在：
        //
        //   No rustls crypto provider is configured. When using the
        //   `rustls-no-provider` feature you must install a crypto provider
        //   before building a Client.
        //
        // 那是个 panic（release 里就是崩）。正常启动路径已在 `init_tracing()`
        // 里装过，这里再保险一下（幂等）。
        install_crypto_provider();

        let tls = Provider::tls_config().map_err(|e| format!("{e:#}"))?;
        // 注意：`tls_backend_preconfigured` **自己会包一层 Some** ——
        // 它的实现是 `let mut tls = Some(tls)` 然后 downcast 到
        // `Option<rustls::ClientConfig>`。所以这里要传裸值，包了就变成
        // `Option<Option<..>>`，downcast 失败，报
        // "Unknown TLS backend passed to `use_preconfigured_tls`"。
        reqwest::ClientBuilder::new()
            .tls_backend_preconfigured(tls)
            // 手机网络会无声无息地断（切 Wi-Fi/蜂窝、进电梯）：没有超时的话
            // 连接就挂在那儿，回合永远「进行中」。读超时是**两次收到数据之间**
            // 的间隔，不是整个回复的时长 —— 流式回复不会被它误杀；给得宽是因为
            // 长上下文的首包（预填充）可能要等一阵。
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| format!("{e:#}"))
    })
    .clone()
    .map_err(|e| anyhow::anyhow!("建 HTTP client 失败: {e}"))
}

/// 流式请求的错误。
///
/// `Http` 单独拿出来，是为了让调用方认出「这个端点根本没有 Responses API」
/// 而自动退回 Chat Completions。
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StreamError {
    /// 是不是「端点不支持这个 API」。
    ///
    /// 400 只在报错**说的是路由/端点本身**时才算 —— 以前只要带 "unknown" 就算，
    /// 于是「unknown parameter」「unknown model」这种跟协议无关的 400 也会让这次
    /// 运行剩下的时间全切到 Chat Completions。
    pub fn is_unsupported_endpoint(&self) -> bool {
        match self {
            StreamError::Http { status, body } => {
                if matches!(*status, 404 | 405 | 501) {
                    return true;
                }
                if *status != 400 {
                    return false;
                }
                let b = body.to_ascii_lowercase();
                let negative = [
                    "not found",
                    "unsupported",
                    "not supported",
                    "unknown",
                    "does not exist",
                ]
                .iter()
                .any(|k| b.contains(k));
                let about_route = ["endpoint", "route", "path", "url"]
                    .iter()
                    .any(|k| b.contains(k));
                let about_other = ["parameter", "model", "field", "argument"]
                    .iter()
                    .any(|k| b.contains(k));
                negative && about_route && !about_other
            }
            StreamError::Other(_) => false,
        }
    }

    /// 值得重试的临时故障：限流、网关 / 服务端临时出错、连不上 / 读超时。
    ///
    /// 手机网络上一次抖动不该让一个跑了半天的回合直接报错结束（agent 不限
    /// 步数，长任务中途碰到一次的概率很高）。鉴权失败、请求本身有问题（400）
    /// 这类重试也没用，不算。
    pub fn is_transient(&self) -> bool {
        match self {
            StreamError::Http { status, .. } => {
                matches!(*status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
            }
            // 只认网络层的错误（发请求时 reqwest 报的）；别的（建 client 失败之类）不重试
            StreamError::Other(e) => e.downcast_ref::<reqwest::Error>().is_some(),
        }
    }
}

/// 已知不支持 Responses 的端点：`base_url|model`。
///
/// 探测到一次就记住，后面直接走 Chat Completions，不再每个 step 白试一次。
fn responses_unsupported() -> &'static Mutex<HashSet<String>> {
    static M: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashSet::new()))
}

fn responses_key(p: &Provider) -> String {
    format!("{}|{}", p.base_url.trim_end_matches('/'), p.model)
}

pub fn responses_known_unsupported(p: &Provider) -> bool {
    responses_unsupported()
        .lock()
        .map(|s| s.contains(&responses_key(p)))
        .unwrap_or(false)
}

/// 记下「这个端点不支持 Responses」。返回 `true` = 这是新记下的（调用方据此
/// 落盘，见 `AppState::remember_responses_unsupported`）。
pub fn mark_responses_unsupported(p: &Provider) -> bool {
    responses_unsupported()
        .lock()
        .map(|mut s| s.insert(responses_key(p)))
        .unwrap_or(false)
}

/// 全部已知不支持的（落盘用）。
pub fn responses_unsupported_list() -> Vec<String> {
    let mut v: Vec<String> = responses_unsupported()
        .lock()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// 启动时把落过盘的读回来：不然每次重启后的第一个请求都要白白失败一次。
pub fn load_responses_unsupported(keys: impl IntoIterator<Item = String>) {
    if let Ok(mut s) = responses_unsupported().lock() {
        s.extend(keys);
    }
}

/// 构造一条最小的 chat 请求（Chat Completions）。
pub fn build_chat_request(model: &str, system: Option<&str>, user: &str) -> ChatRequest {
    let mut messages = Vec::new();
    if let Some(sys) = system {
        messages.push(openai::Message::System {
            content: sys.to_string(),
        });
    }
    messages.push(openai::Message::User {
        content: UserContent::Text(user.to_string()),
    });
    ChatRequest {
        model: model.to_string(),
        messages,
        tools: Vec::new().into(),
        stream: true,
        stream_options: openai::StreamOptions {
            include_usage: true,
        },
        reasoning_effort: None,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn effort_normalizes_and_empty_means_unset() {
        assert_eq!(normalize_effort("  HIGH "), Some("high".into()));
        assert_eq!(normalize_effort("Minimal"), Some("minimal".into()));
        // 空串 = 不设置（前端用它表达「清掉」）
        assert_eq!(normalize_effort(""), None);
        assert_eq!(normalize_effort("   "), None);
    }
    use super::*;

    #[test]
    fn base_url_is_the_only_switch_for_compatible_providers() {
        let p = Provider::deepseek("sk-test");
        assert_eq!(p.base_url, "https://api.deepseek.com/v1");
        assert_eq!(p.model, "deepseek-chat");
    }

    #[test]
    fn chat_request_builds_without_network() {
        let req = build_chat_request("gpt-5", Some("你是助手"), "你好");
        assert_eq!(req.messages.len(), 2);
        assert!(req.stream);
    }

    #[test]
    fn api_key_is_not_serialized() {
        let p = Provider::openai("sk-secret");
        let j = serde_json::to_string(&p).unwrap();
        assert!(!j.contains("sk-secret"), "key 不能泄漏给前端: {j}");
    }

    #[test]
    fn auto_prefers_responses_and_falls_back_once_marked_unsupported() {
        let p = Provider::compatible("x", "https://example.test/v1", "m", "k");
        assert_eq!(p.transport(), Transport::Responses, "默认优先 Responses");
        mark_responses_unsupported(&p);
        assert_eq!(p.transport(), Transport::Chat, "标记后直接走 Chat");
        // 显式指定的不受探测影响
        let mut q = p.clone();
        q.api_style = ApiStyle::Responses;
        assert_eq!(q.transport(), Transport::Responses);
    }

    #[test]
    fn unsupported_endpoint_is_recognized() {
        let http = |status: u16, body: &str| StreamError::Http {
            status,
            body: body.into(),
        };
        assert!(http(404, "not found").is_unsupported_endpoint());
        assert!(http(405, "").is_unsupported_endpoint());
        assert!(http(400, "Unsupported endpoint: /v1/responses").is_unsupported_endpoint());
        assert!(!http(401, "bad key").is_unsupported_endpoint());
        // 跟协议无关的 400 不能让整个运行切到 Chat Completions
        assert!(!http(400, "Unknown parameter: 'reasoning.effort'").is_unsupported_endpoint());
        assert!(!http(400, "unknown model: gpt-9").is_unsupported_endpoint());
        assert!(!http(400, "context length exceeded").is_unsupported_endpoint());
    }

    #[test]
    fn only_temporary_failures_are_retried() {
        let http = |status: u16| StreamError::Http {
            status,
            body: String::new(),
        };
        for s in [429, 500, 502, 503, 504, 529, 408] {
            assert!(http(s).is_transient(), "{s}");
        }
        for s in [400, 401, 403, 404, 422] {
            assert!(!http(s).is_transient(), "{s}");
        }
        assert!(!StreamError::Other(anyhow::anyhow!("建 HTTP client 失败")).is_transient());
    }

    #[test]
    fn responses_unsupported_round_trips_for_persistence() {
        let p = Provider::compatible("x", "https://persist.test/v1", "m2", "k");
        assert!(mark_responses_unsupported(&p), "第一次是新记下的");
        assert!(!mark_responses_unsupported(&p), "再记一次不算新的");
        assert!(responses_unsupported_list().contains(&"https://persist.test/v1|m2".to_string()));
        load_responses_unsupported(["https://loaded.test/v1|m".to_string()]);
        let q = Provider::compatible("y", "https://loaded.test/v1/", "m", "k");
        assert_eq!(q.transport(), Transport::Chat, "读回来的直接生效");
    }
}
