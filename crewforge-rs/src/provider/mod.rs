pub mod compatible;
pub mod openai_oauth;
pub mod registry;
pub mod reliable;
pub mod router;
pub mod traits;

pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ConversationMessage, Provider, ProviderCapabilities,
    TokenUsage, ToolCall, ToolResultMessage, ToolSpec, build_tool_instructions_text,
};

pub use compatible::{api_error, sanitize_api_error};
pub use registry::ProviderRegistry;
pub use reliable::ReliableProvider;
pub use router::{Route, RouterProvider};

use compatible::OpenAiCompatibleProvider;

/// Runtime options for providers that use OAuth/auth services (openai-codex).
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
    let registry = ProviderRegistry::load();
    let resolved_key = resolve_api_key(&registry, provider_name, api_key);

    let lower = provider_name.to_lowercase();

    // Special case: OpenAI Codex uses OAuth, not the compatible provider.
    if lower == "openai-codex" || lower == "codex" {
        let opts = ProviderRuntimeOptions {
            provider_api_url: base_url.map(ToString::to_string),
            ..ProviderRuntimeOptions::default()
        };
        return Ok(Box::new(
            openai_oauth::OpenAiCodexProvider::new(&opts)
                .map_err(|e| anyhow::anyhow!("Failed to create OpenAI Codex provider: {e}"))?,
        ));
    }

    // Registry lookup: known providers have a default base_url.
    if let Some(def) = registry.lookup(provider_name) {
        let url = base_url.unwrap_or(&def.base_url);
        return Ok(Box::new(OpenAiCompatibleProvider::new(
            provider_name,
            url,
            resolved_key.as_deref(),
        )));
    }

    // Unknown provider: require --base-url.
    if let Some(url) = base_url {
        Ok(Box::new(OpenAiCompatibleProvider::new(
            provider_name,
            url,
            resolved_key.as_deref(),
        )))
    } else {
        anyhow::bail!(
            "Unknown provider '{}'. Use --base-url for custom endpoints, or add it to ~/.crewforge/providers.toml.",
            provider_name
        )
    }
}

fn resolve_api_key(
    registry: &ProviderRegistry,
    provider_name: &str,
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(k) = explicit
        && !k.is_empty()
    {
        return Some(k.to_string());
    }
    let env_var = registry.api_key_env(provider_name)?;
    std::env::var(env_var).ok().filter(|k| !k.is_empty())
}
