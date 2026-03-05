pub mod anthropic_oauth;
pub mod compatible;
pub mod openai_oauth;
pub mod reliable;
pub mod router;
pub mod traits;

pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilities,
    TokenUsage, ToolCall, ToolResultMessage, ToolSpec, build_tool_instructions_text,
};

pub use compatible::{api_error, sanitize_api_error};
pub use reliable::ReliableProvider;
pub use router::{Route, RouterProvider};

use compatible::OpenAiCompatibleProvider;

/// Runtime options for providers that use OAuth/auth services (anthropic, openai-codex).
#[derive(Debug, Default)]
pub struct ProviderRuntimeOptions {
    pub crewforge_dir: Option<std::path::PathBuf>,
    pub secrets_encrypt: bool,
    pub auth_profile_override: Option<String>,
    pub provider_api_url: Option<String>,
    pub reasoning_enabled: Option<bool>,
}

/// Create a provider by name. API key is read from the appropriate environment
/// variable if `api_key` is None or empty.
pub fn create_provider(
    provider_name: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> anyhow::Result<Box<dyn Provider>> {
    let resolved_key = resolve_api_key(provider_name, api_key);
    let p: Box<dyn Provider> = match provider_name.to_lowercase().as_str() {
        "anthropic" | "claude" => {
            let opts = ProviderRuntimeOptions {
                provider_api_url: base_url.map(ToString::to_string),
                ..ProviderRuntimeOptions::default()
            };
            Box::new(
                anthropic_oauth::AnthropicOAuthProvider::new(&opts)
                    .map_err(|e| anyhow::anyhow!("Failed to create Anthropic provider: {e}"))?,
            )
        }
        "openai" | "gpt" => Box::new(OpenAiCompatibleProvider::new(
            "openai",
            base_url.unwrap_or("https://api.openai.com/v1"),
            resolved_key.as_deref(),
        )),
        "openrouter" => Box::new(OpenAiCompatibleProvider::new(
            "openrouter",
            base_url.unwrap_or("https://openrouter.ai/api/v1"),
            resolved_key.as_deref(),
        )),
        "moonshot" | "kimi" => Box::new(OpenAiCompatibleProvider::new(
            "moonshot",
            base_url.unwrap_or("https://api.moonshot.ai/v1"),
            resolved_key.as_deref(),
        )),
        "qwen" | "dashscope" => Box::new(OpenAiCompatibleProvider::new(
            "qwen",
            base_url.unwrap_or("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            resolved_key.as_deref(),
        )),
        "minimax" => Box::new(OpenAiCompatibleProvider::new(
            "minimax",
            base_url.unwrap_or("https://api.minimax.io/v1"),
            resolved_key.as_deref(),
        )),
        "deepseek" => Box::new(OpenAiCompatibleProvider::new(
            "deepseek",
            base_url.unwrap_or("https://api.deepseek.com/v1"),
            resolved_key.as_deref(),
        )),
        "groq" => Box::new(OpenAiCompatibleProvider::new(
            "groq",
            base_url.unwrap_or("https://api.groq.com/openai/v1"),
            resolved_key.as_deref(),
        )),
        "mistral" => Box::new(OpenAiCompatibleProvider::new(
            "mistral",
            base_url.unwrap_or("https://api.mistral.ai/v1"),
            resolved_key.as_deref(),
        )),
        "xai" | "grok" => Box::new(OpenAiCompatibleProvider::new(
            "xai",
            base_url.unwrap_or("https://api.x.ai/v1"),
            resolved_key.as_deref(),
        )),
        "openai-codex" | "codex" => {
            let opts = ProviderRuntimeOptions {
                provider_api_url: base_url.map(ToString::to_string),
                ..ProviderRuntimeOptions::default()
            };
            Box::new(
                openai_oauth::OpenAiCodexProvider::new(&opts)
                    .map_err(|e| anyhow::anyhow!("Failed to create OpenAI Codex provider: {e}"))?,
            )
        }
        other => {
            if let Some(url) = base_url {
                Box::new(OpenAiCompatibleProvider::new(
                    other,
                    url,
                    resolved_key.as_deref(),
                ))
            } else {
                anyhow::bail!(
                    "Unknown provider '{}'. Set --base-url for custom OpenAI-compatible providers.",
                    other
                )
            }
        }
    };
    Ok(p)
}

fn resolve_api_key(provider_name: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(k) = explicit
        && !k.is_empty()
    {
        return Some(k.to_string());
    }
    let env_var = default_api_key_env(provider_name)?;
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
}

/// Default environment variable name for a provider's API key.
pub fn default_api_key_env(provider_name: &str) -> Option<&'static str> {
    match provider_name.to_lowercase().as_str() {
        "openai" | "gpt" => Some("OPENAI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "moonshot" | "kimi" => Some("MOONSHOT_API_KEY"),
        "qwen" | "dashscope" => Some("QWEN_API_KEY"),
        "minimax" => Some("MINIMAX_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "xai" | "grok" => Some("XAI_API_KEY"),
        _ => None,
    }
}
