//! 内置的模型服务商。
//!
//! 用户只填 API Key：Base URL、协议、首选模型都写在这里。每家的 Key 各存一份
//! （`sessions.db` 的 `meta` 表，`providers` 那一行），所以几家可以同时配好，
//! 在「选择模型」里点哪家的模型就切到哪家，不用来回重填。
//!
//! 「其他（OpenAI 兼容）」是唯一要填 Base URL 的：Ollama、中转站、公司网关
//! 都走它，别让固定清单把这些用法堵死。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::llm::{ApiStyle, Provider};

/// 一家内置服务商。
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub base_url: &'static str,
    /// 协议偏好。明知道不支持 Responses 的直接走 Chat，省掉每次启动白试的那一下
    /// （「不支持」的记忆只在进程里，重启就忘）。
    pub api_style: ApiStyle,
    /// 刚配上 Key、还没选过模型时默认用哪个：按顺序取清单里第一个存在的，
    /// 都没有就取清单第一个。
    pub preferred: &'static [&'static str],
    /// 去哪拿 Key（设置页上那个链接）
    pub key_url: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_style: ApiStyle::Auto,
        preferred: &["gpt-5.5", "gpt-5.4", "gpt-5.2", "gpt-5.1", "gpt-5"],
        key_url: "https://platform.openai.com/api-keys",
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        api_style: ApiStyle::Chat,
        preferred: &["deepseek-v4-flash", "deepseek-chat"],
        key_url: "https://platform.deepseek.com/api_keys",
    },
    Preset {
        id: "opencode-go",
        name: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go/v1",
        api_style: ApiStyle::Auto,
        preferred: &["deepseek-v4.1-flash", "deepseek-v4-flash"],
        key_url: "https://opencode.ai/auth",
    },
    Preset {
        id: "opencode-zen",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        api_style: ApiStyle::Auto,
        preferred: &["gpt-5.4", "claude-sonnet-4-5"],
        key_url: "https://opencode.ai/auth",
    },
];

/// 「其他（OpenAI 兼容）」：Base URL 自己填。
pub const CUSTOM: &str = "custom";
pub const CUSTOM_NAME: &str = "其他（OpenAI 兼容）";

/// 一家服务商存下来的凭据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Creds {
    pub api_key: String,
    /// 只有 `custom` 用；内置的 Base URL 以 `PRESETS` 为准（以后改了地址也跟着变）
    #[serde(default)]
    pub base_url: String,
}

/// 服务商 id → 凭据。
pub type Store = BTreeMap<String, Creds>;

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

pub fn display_name(id: &str) -> &'static str {
    preset(id).map(|p| p.name).unwrap_or(CUSTOM_NAME)
}

pub fn is_known(id: &str) -> bool {
    id == CUSTOM || preset(id).is_some()
}

/// 用存下来的凭据造一个 `Provider`。`model` 空着就先占位（调用方拉完清单再定）。
pub fn provider(id: &str, creds: &Creds, model: &str) -> Option<Provider> {
    let (base_url, api_style) = match preset(id) {
        Some(p) => (p.base_url.to_string(), p.api_style),
        None if id == CUSTOM && !creds.base_url.trim().is_empty() => {
            (creds.base_url.trim().trim_end_matches('/').to_string(), ApiStyle::Auto)
        }
        None => return None,
    };
    let mut p = Provider::compatible(id, base_url, model, creds.api_key.clone());
    p.api_style = api_style;
    Some(p)
}

/// 一个 `Provider` 属于哪家：按 Base URL 认（老配置的 `name` 是随手填的，不可信）。
pub fn identify(p: &Provider) -> &'static str {
    let norm = |s: &str| s.trim_end_matches('/').trim_end_matches("/v1").to_ascii_lowercase();
    let url = norm(&p.base_url);
    PRESETS
        .iter()
        .find(|pr| norm(pr.base_url) == url)
        .map(|pr| pr.id)
        .unwrap_or(CUSTOM)
}

/// 刚配上的服务商默认用哪个模型。
pub fn pick_default(id: &str, models: &[String]) -> Option<String> {
    if let Some(p) = preset(id) {
        for want in p.preferred {
            if models.iter().any(|m| m == want) {
                return Some((*want).to_string());
            }
        }
    }
    models.first().cloned()
}

/// 这个模型能不能拿来对话。
///
/// `/models` 是「这个 Key 能调的全部模型」：OpenAI 的清单里一大半是 embedding、
/// 语音、画图、审核，选了也只会 400。按名字筛掉 —— 这些名字各家都很规整。
pub fn is_chat_model(id: &str) -> bool {
    const NOT_CHAT: &[&str] = &[
        "embed", "tts", "whisper", "transcribe", "dall-e", "image", "moderation", "realtime",
        "audio", "sora", "davinci", "babbage", "instruct", "search-preview", "computer-use",
        "rerank",
    ];
    let id = id.to_ascii_lowercase();
    !NOT_CHAT.iter().any(|k| id.contains(k))
}

/// 界面上显示的 Key：只露最后 4 位。
pub fn key_hint(key: &str) -> String {
    let tail: String = key.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_provider_is_recognised_by_its_base_url() {
        let p = Provider::compatible("whatever", "https://opencode.ai/zen/go/v1/", "m", "k");
        assert_eq!(identify(&p), "opencode-go");
        // DeepSeek 带不带 /v1 都是它
        let p = Provider::compatible("x", "https://api.deepseek.com/v1", "m", "k");
        assert_eq!(identify(&p), "deepseek");
        let p = Provider::compatible("x", "http://127.0.0.1:11434/v1", "m", "k");
        assert_eq!(identify(&p), CUSTOM);
    }

    #[test]
    fn presets_carry_their_own_url_and_protocol() {
        let creds = Creds {
            api_key: "sk-1".into(),
            base_url: "https://evil.example".into(), // 内置的不认这个
        };
        let p = provider("deepseek", &creds, "deepseek-chat").unwrap();
        assert_eq!(p.base_url, "https://api.deepseek.com");
        assert_eq!(p.api_style, ApiStyle::Chat);
        assert_eq!(p.api_key, "sk-1");

        // 自定义必须有 Base URL
        assert!(provider(CUSTOM, &Creds::default(), "m").is_none());
        assert!(provider("nope", &creds, "m").is_none());
    }

    #[test]
    fn the_default_model_follows_the_preference_list() {
        let models = vec!["gpt-4.1".to_string(), "gpt-5.4".to_string(), "gpt-5".to_string()];
        assert_eq!(pick_default("openai", &models).as_deref(), Some("gpt-5.4"));
        assert_eq!(pick_default(CUSTOM, &models).as_deref(), Some("gpt-4.1"));
        assert_eq!(pick_default("openai", &[]), None);
    }

    #[test]
    fn non_chat_models_are_filtered_out() {
        for id in ["text-embedding-3-small", "tts-1", "whisper-1", "dall-e-3", "gpt-image-1",
                   "omni-moderation-latest", "gpt-4o-realtime-preview", "chatgpt-image-latest"] {
            assert!(!is_chat_model(id), "{id}");
        }
        for id in ["gpt-5.4", "deepseek-v4-flash", "claude-sonnet-4-5", "qwen3.7-max"] {
            assert!(is_chat_model(id), "{id}");
        }
    }

    #[test]
    fn only_the_last_four_characters_of_a_key_are_shown() {
        assert_eq!(key_hint("sk-abcdefgh1234"), "…1234");
        assert_eq!(key_hint("ab"), "…ab");
    }
}
