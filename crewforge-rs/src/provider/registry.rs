//! Data-driven provider registry.
//!
//! Built-in defaults are compiled into the binary. Users can add or override
//! entries via `~/.crewforge/providers.toml`.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// A single provider definition in the registry.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderDef {
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The full registry: canonical name → definition.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderDef>,
}

// ── Built-in defaults ────────────────────────────────────────────────────────

fn builtin_providers() -> BTreeMap<String, ProviderDef> {
    let entries: &[(&str, &str, &str, &[&str])] = &[
        (
            "openai",
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            &["gpt"],
        ),
        (
            "gemini",
            "https://generativelanguage.googleapis.com/v1beta/openai",
            "GEMINI_API_KEY",
            &["google"],
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            &[],
        ),
        (
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            &[],
        ),
        (
            "groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
            &[],
        ),
        (
            "mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
            &[],
        ),
        ("xai", "https://api.x.ai/v1", "XAI_API_KEY", &["grok"]),
        (
            "moonshot",
            "https://api.moonshot.ai/v1",
            "MOONSHOT_API_KEY",
            &["kimi"],
        ),
        (
            "qwen",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "QWEN_API_KEY",
            &["dashscope"],
        ),
        (
            "minimax",
            "https://api.minimax.io/v1",
            "MINIMAX_API_KEY",
            &[],
        ),
    ];

    let mut map = BTreeMap::new();
    for &(name, base_url, api_key_env, aliases) in entries {
        map.insert(
            name.to_string(),
            ProviderDef {
                base_url: base_url.to_string(),
                api_key_env: Some(api_key_env.to_string()),
                aliases: aliases.iter().map(|s| s.to_string()).collect(),
            },
        );
    }
    map
}

// ── Registry ─────────────────────────────────────────────────────────────────

impl ProviderRegistry {
    /// Load the registry: built-in defaults merged with user overrides.
    pub fn load() -> Self {
        let mut providers = builtin_providers();

        // Try loading user overrides from ~/.crewforge/providers.toml
        if let Some(user_path) = default_user_config_path()
            && let Ok(user_providers) = load_toml_file(&user_path)
        {
            for (name, def) in user_providers {
                providers.insert(name, def);
            }
        }

        Self { providers }
    }

    /// Load from built-in defaults only (no filesystem access).
    #[cfg(test)]
    pub fn builtin_only() -> Self {
        Self {
            providers: builtin_providers(),
        }
    }

    /// Look up a provider by canonical name or alias.
    pub fn lookup(&self, name: &str) -> Option<&ProviderDef> {
        let lower = name.to_lowercase();

        // Direct match
        if let Some(def) = self.providers.get(&lower) {
            return Some(def);
        }

        // Alias match
        self.providers
            .values()
            .find(|def| def.aliases.iter().any(|a| a.eq_ignore_ascii_case(&lower)))
    }

    /// Get the API key env var for a provider (by name or alias).
    pub fn api_key_env(&self, name: &str) -> Option<&str> {
        self.lookup(name)
            .and_then(|def| def.api_key_env.as_deref())
    }

    /// Iterate all provider names (canonical only, not aliases).
    pub fn provider_names(&self) -> impl Iterator<Item = &str> {
        self.providers.keys().map(String::as_str)
    }
}

fn default_user_config_path() -> Option<std::path::PathBuf> {
    directories::UserDirs::new()
        .map(|dirs| dirs.home_dir().join(".crewforge").join("providers.toml"))
}

fn load_toml_file(path: &Path) -> anyhow::Result<BTreeMap<String, ProviderDef>> {
    let content = std::fs::read_to_string(path)?;
    let map: BTreeMap<String, ProviderDef> = toml::from_str(&content)?;
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_has_expected_providers() {
        let reg = ProviderRegistry::builtin_only();
        assert!(reg.lookup("openai").is_some());
        assert!(reg.lookup("gemini").is_some());
        assert!(reg.lookup("deepseek").is_some());
        assert!(reg.lookup("groq").is_some());
    }

    #[test]
    fn alias_lookup_works() {
        let reg = ProviderRegistry::builtin_only();
        let by_name = reg.lookup("openai").unwrap();
        let by_alias = reg.lookup("gpt").unwrap();
        assert_eq!(by_name.base_url, by_alias.base_url);
    }

    #[test]
    fn alias_lookup_case_insensitive() {
        let reg = ProviderRegistry::builtin_only();
        assert!(reg.lookup("GPT").is_some());
        assert!(reg.lookup("Google").is_some());
    }

    #[test]
    fn unknown_provider_returns_none() {
        let reg = ProviderRegistry::builtin_only();
        assert!(reg.lookup("nonexistent").is_none());
    }

    #[test]
    fn api_key_env_lookup() {
        let reg = ProviderRegistry::builtin_only();
        assert_eq!(reg.api_key_env("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(reg.api_key_env("gpt"), Some("OPENAI_API_KEY"));
        assert_eq!(reg.api_key_env("unknown"), None);
    }

    #[test]
    fn anthropic_not_in_builtin_registry() {
        let reg = ProviderRegistry::builtin_only();
        // Anthropic's native API is not OpenAI-compatible; not included by default.
        assert!(reg.lookup("anthropic").is_none());
        assert!(reg.lookup("claude").is_none());
    }

    #[test]
    fn user_override_merges_with_builtins() {
        let mut reg = ProviderRegistry::builtin_only();
        // Simulate user adding a custom provider
        reg.providers.insert(
            "my-proxy".to_string(),
            ProviderDef {
                base_url: "https://my-proxy.com/v1".to_string(),
                api_key_env: Some("MY_PROXY_KEY".to_string()),
                aliases: vec![],
            },
        );
        assert!(reg.lookup("my-proxy").is_some());
        // Builtins still present
        assert!(reg.lookup("openai").is_some());
    }

    #[test]
    fn user_override_replaces_builtin() {
        let mut reg = ProviderRegistry::builtin_only();
        reg.providers.insert(
            "openai".to_string(),
            ProviderDef {
                base_url: "https://custom-openai.com/v1".to_string(),
                api_key_env: Some("CUSTOM_OPENAI_KEY".to_string()),
                aliases: vec!["gpt".to_string()],
            },
        );
        let def = reg.lookup("openai").unwrap();
        assert_eq!(def.base_url, "https://custom-openai.com/v1");
    }

    #[test]
    fn provider_names_lists_canonical_names() {
        let reg = ProviderRegistry::builtin_only();
        let names: Vec<&str> = reg.provider_names().collect();
        assert!(names.contains(&"openai"));
        assert!(names.contains(&"gemini"));
        // aliases should NOT appear as canonical names
        assert!(!names.contains(&"gpt"));
        assert!(!names.contains(&"google"));
    }

    #[test]
    fn toml_deserialization() {
        let toml_str = r#"
[my-provider]
base_url = "https://api.example.com/v1"
api_key_env = "EXAMPLE_KEY"
aliases = ["example", "ex"]
"#;
        let map: BTreeMap<String, ProviderDef> = toml::from_str(toml_str).unwrap();
        let def = map.get("my-provider").unwrap();
        assert_eq!(def.base_url, "https://api.example.com/v1");
        assert_eq!(def.api_key_env.as_deref(), Some("EXAMPLE_KEY"));
        assert_eq!(def.aliases, vec!["example", "ex"]);
    }

    #[test]
    fn toml_minimal_entry() {
        let toml_str = r#"
[local-llm]
base_url = "http://localhost:11434/v1"
"#;
        let map: BTreeMap<String, ProviderDef> = toml::from_str(toml_str).unwrap();
        let def = map.get("local-llm").unwrap();
        assert!(def.api_key_env.is_none());
        assert!(def.aliases.is_empty());
    }
}
