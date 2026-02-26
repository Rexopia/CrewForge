pub mod anthropic;
pub mod compatible;
pub mod copilot;
pub mod gemini;
pub mod glm;
pub mod ollama;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;
pub mod reliable;
pub mod router;
pub mod traits;

pub use traits::{
    build_tool_instructions_text, ChatMessage, ChatRequest, ChatResponse, ConversationMessage,
    Provider, ProviderCapabilities, ToolCall, ToolResultMessage, ToolSpec, TokenUsage,
};

pub use reliable::ReliableProvider;
pub use router::{Route, RouterProvider};
pub use compatible::{api_error, sanitize_api_error};

use compatible::{AuthStyle, OpenAiCompatibleProvider};

/// Runtime options for providers that use OAuth/auth services (copilot, openai-codex).
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
        "anthropic" | "claude" => Box::new(anthropic::AnthropicProvider::with_base_url(
            resolved_key.as_deref(),
            base_url,
        )),
        "openai" | "gpt" => Box::new(openai::OpenAiProvider::with_base_url(
            base_url,
            resolved_key.as_deref(),
        )),
        "gemini" | "google" => Box::new(gemini::GeminiProvider::new(resolved_key.as_deref())),
        "ollama" => Box::new(ollama::OllamaProvider::new(base_url, resolved_key.as_deref())),
        "openrouter" => Box::new(openrouter::OpenRouterProvider::new(resolved_key.as_deref())),
        "glm" | "zhipuai" | "zhipu" => Box::new(glm::GlmProvider::new(resolved_key.as_deref())),
        "moonshot" | "kimi" => Box::new(OpenAiCompatibleProvider::new(
            "moonshot",
            base_url.unwrap_or("https://api.moonshot.ai/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "qwen" | "dashscope" => Box::new(OpenAiCompatibleProvider::new(
            "qwen",
            base_url.unwrap_or("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "minimax" => Box::new(OpenAiCompatibleProvider::new_merge_system_into_user(
            "minimax",
            base_url.unwrap_or("https://api.minimax.io/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "deepseek" => Box::new(OpenAiCompatibleProvider::new(
            "deepseek",
            base_url.unwrap_or("https://api.deepseek.com/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "groq" => Box::new(OpenAiCompatibleProvider::new(
            "groq",
            base_url.unwrap_or("https://api.groq.com/openai/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "mistral" => Box::new(OpenAiCompatibleProvider::new(
            "mistral",
            base_url.unwrap_or("https://api.mistral.ai/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "xai" | "grok" => Box::new(OpenAiCompatibleProvider::new(
            "xai",
            base_url.unwrap_or("https://api.x.ai/v1"),
            resolved_key.as_deref(),
            AuthStyle::Bearer,
        )),
        "copilot" | "github-copilot" => {
            Box::new(copilot::CopilotProvider::new(resolved_key.as_deref()))
        }
        "openai-codex" | "codex" => {
            let opts = ProviderRuntimeOptions {
                provider_api_url: base_url.map(ToString::to_string),
                ..ProviderRuntimeOptions::default()
            };
            Box::new(
                openai_codex::OpenAiCodexProvider::new(&opts, resolved_key.as_deref())
                    .map_err(|e| anyhow::anyhow!("Failed to create OpenAI Codex provider: {e}"))?,
            )
        }
        other => {
            if let Some(url) = base_url {
                Box::new(OpenAiCompatibleProvider::new(
                    other,
                    url,
                    resolved_key.as_deref(),
                    AuthStyle::Bearer,
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
    if let Some(k) = explicit {
        if !k.is_empty() {
            return Some(k.to_string());
        }
    }
    let env_var = default_api_key_env(provider_name)?;
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
}

/// Default environment variable name for a provider's API key.
pub fn default_api_key_env(provider_name: &str) -> Option<&'static str> {
    match provider_name.to_lowercase().as_str() {
        "anthropic" | "claude" => Some("ANTHROPIC_API_KEY"),
        "openai" | "gpt" => Some("OPENAI_API_KEY"),
        "gemini" | "google" => Some("GEMINI_API_KEY"),
        "ollama" => None,
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "glm" | "zhipuai" | "zhipu" => Some("GLM_API_KEY"),
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
