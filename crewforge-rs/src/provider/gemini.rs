//! Google Gemini provider with API key authentication.
//! Supports GEMINI_API_KEY and GOOGLE_API_KEY environment variables.

use crate::provider::traits::{ChatMessage, ChatResponse, Provider, TokenUsage};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Gemini provider supporting API key authentication.
pub struct GeminiProvider {
    api_key: Option<String>,
}

// ══════════════════════════════════════════════════════════════════════════════
// API REQUEST/RESPONSE TYPES
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Clone)]
struct GenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Debug, Serialize, Clone)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize, Clone)]
struct Part {
    text: String,
}

#[derive(Debug, Serialize, Clone)]
struct GenerationConfig {
    temperature: f64,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    /// Thinking models (e.g. gemini-3-pro-preview) mark reasoning parts with `thought: true`.
    #[serde(default)]
    thought: bool,
}

impl CandidateContent {
    /// Extract effective text, skipping thinking/signature parts.
    ///
    /// Gemini thinking models return parts like:
    /// - `{"thought": true, "text": "reasoning..."}` — internal reasoning
    /// - `{"text": "actual answer"}` — the real response
    /// - `{"thoughtSignature": "..."}` — opaque signature (no text field)
    ///
    /// Returns the non-thinking text, falling back to thinking text only when
    /// no non-thinking content is available.
    fn effective_text(self) -> Option<String> {
        let mut answer_parts: Vec<String> = Vec::new();
        let mut first_thinking: Option<String> = None;

        for part in self.parts {
            if let Some(text) = part.text {
                if text.is_empty() {
                    continue;
                }
                if !part.thought {
                    answer_parts.push(text);
                } else if first_thinking.is_none() {
                    first_thinking = Some(text);
                }
            }
        }

        if answer_parts.is_empty() {
            first_thinking
        } else {
            Some(answer_parts.join(""))
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

/// Public API endpoint for API key users.
const PUBLIC_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

impl GeminiProvider {
    /// Create a new Gemini provider.
    ///
    /// Authentication priority:
    /// 1. Explicit API key passed in
    /// 2. `GEMINI_API_KEY` environment variable
    /// 3. `GOOGLE_API_KEY` environment variable
    pub fn new(api_key: Option<&str>) -> Self {
        let resolved_key = api_key
            .and_then(Self::normalize_non_empty)
            .or_else(|| Self::load_non_empty_env("GEMINI_API_KEY"))
            .or_else(|| Self::load_non_empty_env("GOOGLE_API_KEY"));

        Self {
            api_key: resolved_key,
        }
    }

    fn normalize_non_empty(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn load_non_empty_env(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .and_then(|value| Self::normalize_non_empty(&value))
    }

    fn format_model_name(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    fn build_generate_content_url(&self, model: &str) -> String {
        let model_name = Self::format_model_name(model);
        let base_url = format!("{PUBLIC_API_ENDPOINT}/{model_name}:generateContent");
        if let Some(key) = &self.api_key {
            format!("{base_url}?key={key}")
        } else {
            base_url
        }
    }

    fn http_client(&self) -> Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default()
    }

    async fn send_generate_content(
        &self,
        contents: Vec<Content>,
        system_instruction: Option<Content>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<(String, Option<TokenUsage>)> {
        if self.api_key.is_none() {
            anyhow::bail!(
                "Gemini API key not found. Set GEMINI_API_KEY or GOOGLE_API_KEY environment variable, \
                 or pass the key directly."
            );
        }

        let request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
        };

        let url = self.build_generate_content_url(model);

        let response = self.http_client().post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error (HTTP {status}): {body}");
        }

        let parsed: GenerateContentResponse = response.json().await?;

        // Check for API-level errors embedded in a 200 response
        if let Some(err) = parsed.error {
            anyhow::bail!("Gemini API error: {}", err.message);
        }

        let usage = parsed.usage_metadata.map(|u| TokenUsage {
            input_tokens: u.prompt_token_count,
            output_tokens: u.candidates_token_count,
        });

        let text = parsed
            .candidates
            .and_then(|candidates| candidates.into_iter().next())
            .and_then(|c| c.content)
            .and_then(|c| c.effective_text())
            .ok_or_else(|| anyhow::anyhow!("No response from Gemini"))?;

        Ok((text, usage))
    }

    fn messages_to_contents(messages: &[ChatMessage]) -> Vec<Content> {
        messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| {
                let role = if m.role == "assistant" {
                    "model"
                } else {
                    "user"
                };
                Content {
                    role: Some(role.to_string()),
                    parts: vec![Part {
                        text: m.content.clone(),
                    }],
                }
            })
            .collect()
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let contents = vec![Content {
            role: Some("user".to_string()),
            parts: vec![Part {
                text: message.to_string(),
            }],
        }];

        let system_instruction = system_prompt.map(|sys| Content {
            role: None,
            parts: vec![Part {
                text: sys.to_string(),
            }],
        });

        let (text, _usage) = self
            .send_generate_content(contents, system_instruction, model, temperature)
            .await?;
        Ok(text)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system_instruction = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| Content {
                role: None,
                parts: vec![Part {
                    text: m.content.clone(),
                }],
            });

        let contents = Self::messages_to_contents(messages);

        let (text, _usage) = self
            .send_generate_content(contents, system_instruction, model, temperature)
            .await?;
        Ok(text)
    }

    async fn chat(
        &self,
        request: crate::provider::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let system_instruction = request
            .messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| Content {
                role: None,
                parts: vec![Part {
                    text: m.content.clone(),
                }],
            });

        let contents = Self::messages_to_contents(request.messages);

        let (text, usage) = self
            .send_generate_content(contents, system_instruction, model, temperature)
            .await?;

        Ok(ChatResponse {
            text: Some(text),
            tool_calls: vec![],
            usage,
            reasoning_content: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_explicit_key() {
        // Test that explicit key takes priority
        let p = GeminiProvider::new(Some("explicit-key"));
        assert_eq!(p.api_key.as_deref(), Some("explicit-key"));
    }

    #[test]
    fn creates_without_key() {
        // Clear any env vars for clean test
        let old_gemini = std::env::var("GEMINI_API_KEY").ok();
        let old_google = std::env::var("GOOGLE_API_KEY").ok();
        unsafe {
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
        }
        let p = GeminiProvider::new(None);
        assert!(p.api_key.is_none());
        // Restore env vars
        if let Some(v) = old_gemini {
            unsafe { std::env::set_var("GEMINI_API_KEY", v) };
        }
        if let Some(v) = old_google {
            unsafe { std::env::set_var("GOOGLE_API_KEY", v) };
        }
    }

    #[test]
    fn trims_whitespace_key() {
        let p = GeminiProvider::new(Some("  trimmed-key  "));
        assert_eq!(p.api_key.as_deref(), Some("trimmed-key"));
    }

    #[test]
    fn rejects_empty_key() {
        let old_gemini = std::env::var("GEMINI_API_KEY").ok();
        let old_google = std::env::var("GOOGLE_API_KEY").ok();
        unsafe {
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
        }
        let p = GeminiProvider::new(Some(""));
        assert!(p.api_key.is_none());
        if let Some(v) = old_gemini {
            unsafe { std::env::set_var("GEMINI_API_KEY", v) };
        }
        if let Some(v) = old_google {
            unsafe { std::env::set_var("GOOGLE_API_KEY", v) };
        }
    }

    #[test]
    fn format_model_name_adds_prefix() {
        assert_eq!(
            GeminiProvider::format_model_name("gemini-2.5-pro"),
            "models/gemini-2.5-pro"
        );
    }

    #[test]
    fn format_model_name_preserves_prefix() {
        assert_eq!(
            GeminiProvider::format_model_name("models/gemini-2.5-pro"),
            "models/gemini-2.5-pro"
        );
    }

    #[test]
    fn effective_text_skips_thought_parts() {
        let content = CandidateContent {
            parts: vec![
                ResponsePart {
                    text: Some("thinking...".to_string()),
                    thought: true,
                },
                ResponsePart {
                    text: Some("actual answer".to_string()),
                    thought: false,
                },
            ],
        };
        assert_eq!(content.effective_text(), Some("actual answer".to_string()));
    }

    #[test]
    fn effective_text_falls_back_to_thought_if_no_answer() {
        let content = CandidateContent {
            parts: vec![ResponsePart {
                text: Some("only thinking".to_string()),
                thought: true,
            }],
        };
        assert_eq!(content.effective_text(), Some("only thinking".to_string()));
    }

    #[test]
    fn effective_text_returns_none_for_empty_parts() {
        let content = CandidateContent { parts: vec![] };
        assert_eq!(content.effective_text(), None);
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let old_gemini = std::env::var("GEMINI_API_KEY").ok();
        let old_google = std::env::var("GOOGLE_API_KEY").ok();
        unsafe {
            std::env::remove_var("GEMINI_API_KEY");
            std::env::remove_var("GOOGLE_API_KEY");
        }
        let p = GeminiProvider::new(None);
        let result = p
            .chat_with_system(None, "hello", "gemini-2.5-pro", 0.7)
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("API key not found")
        );
        if let Some(v) = old_gemini {
            unsafe { std::env::set_var("GEMINI_API_KEY", v) };
        }
        if let Some(v) = old_google {
            unsafe { std::env::set_var("GOOGLE_API_KEY", v) };
        }
    }

    #[test]
    fn generate_request_serializes_correctly() {
        let req = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part {
                    text: "hello".to_string(),
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("contents").is_some());
        assert!(json.get("generationConfig").is_some());
        assert!(json.get("systemInstruction").is_none());
    }
}
