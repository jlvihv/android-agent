//! LLM 接入层。
//!
//! 决策 D4：**不手写 OpenAI 协议**。用 `async-openai`，只开 3 个 feature
//! （该 crate 共 57 个）：`rustls` + `chat-completion` + `responses`。
//!
//! 覆盖范围：任何提供 **OpenAI 兼容端点**的服务 —— OpenAI、DeepSeek、Kimi、
//! 通义 DashScope、智谱、SiliconFlow、OpenRouter、Groq、xAI、Together、
//! Fireworks、vLLM、LM Studio、Ollama(`/v1`)，以及绝大多数中转站。
//!
//! 已知不支持：Anthropic / Gemini 的**原生**协议（需走中转或另加 adapter）。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use async_openai::config::OpenAIConfig;
use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
    ChatCompletionRequestUserMessageArgs, CreateChatCompletionRequest,
    CreateChatCompletionRequestArgs,
};
use async_openai::Client;
use serde::{Deserialize, Serialize};

/// OpenCode Go 明确要求：客户端要"用自己的 user agent 标识自己，
/// 而不是通用 SDK / HTTP 库名"。
pub const USER_AGENT: &str = concat!("android-agent/", env!("CARGO_PKG_VERSION"));

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
}

impl Provider {
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self {
            name: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5".into(),
            api_key: api_key.into(),
            extra_headers: BTreeMap::new(),
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

        Some(Self {
            name,
            base_url,
            model,
            api_key: key,
            extra_headers: BTreeMap::new(),
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

    /// 建客户端。`session_id` 非空时会作为 `x-opencode-session` 发出去 ——
    /// OpenCode Go 用它做路由和 prompt cache 优化，缺了会直接 400。
    pub fn client(&self, session_id: &str) -> anyhow::Result<Client<OpenAIConfig>> {
        let mut cfg = OpenAIConfig::new()
            .with_api_base(self.base_url.clone())
            .with_api_key(self.api_key.clone())
            // OpenCode Go 要求客户端用自己的 UA 标识自己，
            // 而不是通用 SDK / HTTP 库名。
            .with_header("user-agent", USER_AGENT)?;

        if !session_id.is_empty() {
            cfg = cfg.with_header("x-opencode-session", session_id)?;
        }
        for (k, v) in &self.extra_headers {
            // IntoHeaderName 要求 'static，所以得用 owned 的 HeaderName
            let name = http::header::HeaderName::from_bytes(k.as_bytes())?;
            cfg = cfg.with_header(name, v.clone())?;
        }

        // 把自建的 reqwest client 注入进去。
        // `Client::with_http_client` 是 async-openai 给的官方口子。
        Ok(Client::with_config(cfg).with_http_client(shared_http()?))
    }
}

/// 进程级共享的 HTTP client。
///
/// 之前每次 `stream_one` 都 `ClientBuilder::new()…build()` 造一个新的，而
/// `reqwest::Client` 本身就是连接池 —— 等于每个 step 都重新做 TLS 握手，
/// 完全丢掉 keep-alive。一个回合 5–8 次 LLM 调用，在手机网络上是白送的 RTT。
///
/// TLS 配置与 provider 无关（只用内嵌的 Mozilla 根证书，见 `tls_config`），
/// 所以全进程共享一个就够。`reqwest::Client` 的内部是 Arc，克隆很便宜。
fn shared_http() -> anyhow::Result<reqwest::Client> {
    static HTTP: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    HTTP.get_or_init(|| {
        let tls = Provider::tls_config().map_err(|e| format!("{e:#}"))?;
        // 注意：`tls_backend_preconfigured` **自己会包一层 Some** ——
        // 它的实现是 `let mut tls = Some(tls)` 然后 downcast 到
        // `Option<rustls::ClientConfig>`。所以这里要传裸值，包了就变成
        // `Option<Option<..>>`，downcast 失败，报
        // "Unknown TLS backend passed to `use_preconfigured_tls`"。
        reqwest::ClientBuilder::new()
            .tls_backend_preconfigured(tls)
            .build()
            .map_err(|e| format!("{e:#}"))
    })
    .clone()
    .map_err(|e| anyhow::anyhow!("建 HTTP client 失败: {e}"))
}

/// 构造一条最小的 chat 请求。存在的意义之一是让 `chat-completion`
/// feature 的类型真的被编译到，避免"选了 feature 其实没生效"。
pub fn build_chat_request(
    model: &str,
    system: Option<&str>,
    user: &str,
) -> anyhow::Result<CreateChatCompletionRequest> {
    let mut messages: Vec<ChatCompletionRequestMessage> = Vec::new();

    if let Some(sys) = system {
        messages.push(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(sys)
                .build()?
                .into(),
        );
    }
    messages.push(
        ChatCompletionRequestUserMessageArgs::default()
            .content(user)
            .build()?
            .into(),
    );

    let req = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(messages)
        .build()?;

    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_is_the_only_switch_for_compatible_providers() {
        let p = Provider::deepseek("sk-test");
        assert_eq!(p.base_url, "https://api.deepseek.com/v1");
        assert_eq!(p.model, "deepseek-chat");
    }

    #[test]
    fn chat_request_builds_without_network() {
        let req = build_chat_request("gpt-5", Some("你是助手"), "你好").unwrap();
        assert_eq!(req.messages.len(), 2);
    }

    #[test]
    fn api_key_is_not_serialized() {
        let p = Provider::openai("sk-secret");
        let j = serde_json::to_string(&p).unwrap();
        assert!(!j.contains("sk-secret"), "key 不能泄漏给前端: {j}");
    }
}
