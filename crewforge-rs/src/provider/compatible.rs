//! Generic OpenAI-compatible provider.
//! Most LLM APIs follow the same `/v1/chat/completions` format with Bearer auth.
//! This module provides a single implementation that works for all of them.

use crate::provider::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall, ToolSpec,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// A provider that speaks the OpenAI-compatible chat completions API.
/// Authentication is always via `Authorization: Bearer <key>`.
pub struct OpenAiCompatibleProvider {
    name: String,
    base_url: String,
    credential: Option<String>,
    client: Client,
}

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest {
    model: String,
    messages: Vec<RequestMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
}

#[derive(Debug, Serialize)]
struct RequestMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolCallOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    function: FunctionRef,
}

#[derive(Debug, Serialize, Deserialize)]
struct FunctionRef {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
struct UsageInfo {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallIn>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallIn {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionRef>,
    // Fallback: some providers use top-level name/arguments
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(rename = "parameters", default)]
    parameters: Option<serde_json::Value>,
}

impl ToolCallIn {
    fn function_name(&self) -> Option<String> {
        self.function
            .as_ref()
            .and_then(|f| f.name.clone())
            .or_else(|| self.name.clone())
    }

    fn function_arguments(&self) -> Option<String> {
        self.function
            .as_ref()
            .and_then(|f| f.arguments.clone())
            .or_else(|| self.arguments.clone())
            .or_else(|| {
                self.parameters
                    .as_ref()
                    .and_then(|p| serde_json::to_string(p).ok())
            })
    }
}

impl ResponseMessage {
    fn effective_content(&self) -> Option<String> {
        self.content
            .as_ref()
            .filter(|c| !c.trim().is_empty())
            .cloned()
            .or_else(|| {
                self.reasoning_content
                    .as_ref()
                    .filter(|c| !c.trim().is_empty())
                    .cloned()
            })
    }
}

// ── Provider ─────────────────────────────────────────────────────────────────

impl OpenAiCompatibleProvider {
    pub fn new(name: &str, base_url: &str, credential: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            credential: credential
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(ToString::to_string),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }

    fn chat_completions_url(&self) -> String {
        if self.base_url.ends_with("/chat/completions") {
            self.base_url.clone()
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    fn require_credential(&self) -> anyhow::Result<&str> {
        self.credential.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "{} API key not set. Set the appropriate env var.",
                self.name
            )
        })
    }

    fn convert_tools(tools: Option<&[ToolSpec]>) -> Option<Vec<serde_json::Value>> {
        let items = tools.filter(|t| !t.is_empty())?;
        Some(
            items
                .iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect(),
        )
    }

    fn convert_messages(messages: &[ChatMessage]) -> Vec<RequestMessage> {
        messages
            .iter()
            .map(|m| {
                // Assistant message with embedded tool_calls JSON
                if m.role == "assistant"
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content)
                    && let Some(tool_calls_value) = value.get("tool_calls")
                    && let Ok(parsed) =
                        serde_json::from_value::<Vec<ProviderToolCall>>(tool_calls_value.clone())
                {
                    let tool_calls = parsed
                        .into_iter()
                        .map(|tc| ToolCallOut {
                            id: Some(tc.id),
                            kind: Some("function".to_string()),
                            function: FunctionRef {
                                name: Some(tc.name),
                                arguments: Some(tc.arguments),
                            },
                        })
                        .collect();

                    let content = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);

                    let reasoning_content = value
                        .get("reasoning_content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);

                    return RequestMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_call_id: None,
                        tool_calls: Some(tool_calls),
                        reasoning_content,
                    };
                }

                // Tool result message
                if m.role == "tool"
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&m.content)
                {
                    let tool_call_id = value
                        .get("tool_call_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                    let content = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| Some(m.content.clone()));

                    return RequestMessage {
                        role: "tool".to_string(),
                        content,
                        tool_call_id,
                        tool_calls: None,
                        reasoning_content: None,
                    };
                }

                // Regular message
                RequestMessage {
                    role: m.role.clone(),
                    content: Some(m.content.clone()),
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: None,
                }
            })
            .collect()
    }

    fn parse_response(
        response: ChatCompletionsResponse,
    ) -> (ProviderChatResponse, Option<TokenUsage>) {
        let usage = response.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        let Some(choice) = response.choices.into_iter().next() else {
            return (
                ProviderChatResponse {
                    text: None,
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                usage,
            );
        };

        let msg = choice.message;
        let text = msg.effective_content();
        let reasoning_content = msg.reasoning_content.clone();

        let tool_calls = msg
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tc| {
                let name = tc.function_name()?;
                let arguments = tc.function_arguments().unwrap_or_else(|| "{}".to_string());
                let arguments = if serde_json::from_str::<serde_json::Value>(&arguments).is_ok() {
                    arguments
                } else {
                    "{}".to_string()
                };
                Some(ProviderToolCall {
                    id: tc.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    name,
                    arguments,
                })
            })
            .collect();

        (
            ProviderChatResponse {
                text,
                tool_calls,
                usage: None,
                reasoning_content,
            },
            usage,
        )
    }
}

// ── Public utilities (used by other provider modules) ────────────────────────

/// Sanitize API error text by scrubbing secrets and truncating length.
pub fn sanitize_api_error(input: &str) -> String {
    let mut result = input.to_string();
    if let Ok(re) = regex::Regex::new(r"sk-[A-Za-z0-9\-_]{8,}") {
        result = re.replace_all(&result, "[REDACTED]").to_string();
    }

    const MAX_CHARS: usize = 200;
    if result.chars().count() <= MAX_CHARS {
        return result;
    }

    let mut end = MAX_CHARS;
    while end > 0 && !result.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &result[..end])
}

/// Build a sanitized provider error from a failed HTTP response.
pub async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read provider error body>".to_string());
    let sanitized = sanitize_api_error(&body);
    anyhow::anyhow!("{provider} API error ({status}): {sanitized}")
}

// ── Provider trait ───────────────────────────────────────────────────────────

#[async_trait]
impl Provider for OpenAiCompatibleProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(credential) = &self.credential {
            let url = self.chat_completions_url();
            let _ = self
                .client
                .get(&url)
                .header("Authorization", format!("Bearer {credential}"))
                .send()
                .await;
        }
        Ok(())
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.require_credential()?;

        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(RequestMessage {
                role: "system".to_string(),
                content: Some(sys.to_string()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            });
        }
        messages.push(RequestMessage {
            role: "user".to_string(),
            content: Some(message.to_string()),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        });

        let request = ChatCompletionsRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(false),
            tools: None,
            tool_choice: None,
        };

        let url = self.chat_completions_url();
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {credential}"))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(api_error(&self.name, response).await);
        }

        let body = response.text().await?;
        let parsed: ChatCompletionsResponse = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!(
                "{} unexpected response: {e}; body={}",
                self.name,
                sanitize_api_error(&body)
            )
        })?;

        let (result, _) = Self::parse_response(parsed);
        result
            .text
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.require_credential()?;

        let request = ChatCompletionsRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            temperature,
            stream: Some(false),
            tools: None,
            tool_choice: None,
        };

        let url = self.chat_completions_url();
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {credential}"))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(api_error(&self.name, response).await);
        }

        let body = response.text().await?;
        let parsed: ChatCompletionsResponse = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!(
                "{} unexpected response: {e}; body={}",
                self.name,
                sanitize_api_error(&body)
            )
        })?;

        let (result, _) = Self::parse_response(parsed);
        result
            .text
            .ok_or_else(|| anyhow::anyhow!("No response from {}", self.name))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.require_credential()?;

        let tools = Self::convert_tools(request.tools);
        let api_request = ChatCompletionsRequest {
            model: model.to_string(),
            messages: Self::convert_messages(request.messages),
            temperature,
            stream: Some(false),
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
        };

        let url = self.chat_completions_url();
        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {credential}"))
            .json(&api_request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(api_error(&self.name, response).await);
        }

        let body = response.text().await?;
        let parsed: ChatCompletionsResponse = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!(
                "{} unexpected response: {e}; body={}",
                self.name,
                sanitize_api_error(&body)
            )
        })?;

        let (mut result, usage) = Self::parse_response(parsed);
        result.usage = usage;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_provider(name: &str, url: &str, key: Option<&str>) -> OpenAiCompatibleProvider {
        OpenAiCompatibleProvider::new(name, url, key)
    }

    #[test]
    fn creates_with_key() {
        let p = make_provider("test", "https://api.example.com/v1", Some("sk-test"));
        assert_eq!(p.credential.as_deref(), Some("sk-test"));
        assert_eq!(p.name, "test");
    }

    #[test]
    fn creates_without_key() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        assert!(p.credential.is_none());
    }

    #[test]
    fn trims_empty_key() {
        let p = make_provider("test", "https://api.example.com/v1", Some("   "));
        assert!(p.credential.is_none());
    }

    #[test]
    fn strips_trailing_slash() {
        let p = make_provider("test", "https://api.example.com/v1/", None);
        assert_eq!(p.base_url, "https://api.example.com/v1");
    }

    #[test]
    fn chat_completions_url_appends_path() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        assert_eq!(
            p.chat_completions_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_keeps_existing_path() {
        let p = make_provider("test", "https://api.example.com/v1/chat/completions", None);
        assert_eq!(
            p.chat_completions_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        let result = p.chat_with_system(None, "hi", "gpt-4o", 0.7).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("API key not set"));
    }

    #[test]
    fn request_serializes_correctly() {
        let request = ChatCompletionsRequest {
            model: "gpt-4o".to_string(),
            messages: vec![RequestMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            }],
            temperature: 0.7,
            stream: Some(false),
            tools: None,
            tool_choice: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("gpt-4o"));
        assert!(json.contains("hello"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("reasoning_content"));
    }

    #[test]
    fn response_deserializes() {
        let json = r#"{"choices":[{"message":{"content":"Hello!"}}]}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
    }

    #[test]
    fn response_with_tool_calls() {
        let json = r#"{
            "choices":[{
                "message":{
                    "content":null,
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"shell","arguments":"{\"cmd\":\"ls\"}"}
                    }]
                }
            }],
            "usage":{"prompt_tokens":10,"completion_tokens":5}
        }"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let (result, usage) = OpenAiCompatibleProvider::parse_response(resp);
        assert!(result.text.is_none());
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "shell");
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn tool_call_fallback_to_top_level_name() {
        let tc = ToolCallIn {
            id: Some("1".into()),
            function: None,
            name: Some("shell".into()),
            arguments: Some("{}".into()),
            parameters: None,
        };
        assert_eq!(tc.function_name().as_deref(), Some("shell"));
        assert_eq!(tc.function_arguments().as_deref(), Some("{}"));
    }

    #[test]
    fn tool_call_fallback_to_parameters() {
        let tc = ToolCallIn {
            id: Some("1".into()),
            function: None,
            name: Some("shell".into()),
            arguments: None,
            parameters: Some(serde_json::json!({"cmd": "ls"})),
        };
        let args = tc.function_arguments().unwrap();
        assert!(args.contains("cmd"));
    }

    #[test]
    fn convert_messages_handles_tool_call_history() {
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: r#"{"content":"checking","tool_calls":[{"id":"c1","name":"shell","arguments":"{}"}]}"#.into(),
        }];
        let converted = OpenAiCompatibleProvider::convert_messages(&messages);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[0].content.as_deref(), Some("checking"));
        assert!(converted[0].tool_calls.is_some());
    }

    #[test]
    fn convert_messages_handles_tool_result() {
        let messages = vec![ChatMessage {
            role: "tool".into(),
            content: r#"{"tool_call_id":"c1","content":"done"}"#.into(),
        }];
        let converted = OpenAiCompatibleProvider::convert_messages(&messages);
        assert_eq!(converted[0].role, "tool");
        assert_eq!(converted[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(converted[0].content.as_deref(), Some("done"));
    }

    #[test]
    fn convert_tools_maps_spec() {
        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let converted = OpenAiCompatibleProvider::convert_tools(Some(&tools)).unwrap();
        assert_eq!(converted.len(), 1);
        assert!(converted[0]["function"]["name"] == "shell");
    }

    #[test]
    fn convert_tools_returns_none_for_empty() {
        assert!(OpenAiCompatibleProvider::convert_tools(Some(&[])).is_none());
        assert!(OpenAiCompatibleProvider::convert_tools(None).is_none());
    }

    #[test]
    fn sanitize_api_error_redacts_keys() {
        let input = "Error with key sk-ant-api-12345678 in request";
        let result = sanitize_api_error(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-ant-api"));
    }

    #[test]
    fn sanitize_api_error_truncates() {
        let long = "x".repeat(300);
        let result = sanitize_api_error(&long);
        assert!(result.ends_with("..."));
        assert!(result.len() < 210);
    }

    #[test]
    fn reasoning_content_fallback() {
        let msg = ResponseMessage {
            content: None,
            reasoning_content: Some("thinking...".into()),
            tool_calls: None,
        };
        assert_eq!(msg.effective_content().as_deref(), Some("thinking..."));
    }

    #[test]
    fn capabilities_reports_native_tools() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        let caps = p.capabilities();
        assert!(caps.native_tool_calling);
        assert!(!caps.vision);
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        assert!(p.warmup().await.is_ok());
    }

    #[test]
    fn reasoning_content_round_trips_in_convert() {
        let messages = vec![ChatMessage {
            role: "assistant".into(),
            content: r#"{"content":"ok","tool_calls":[{"id":"c1","name":"shell","arguments":"{}"}],"reasoning_content":"let me think"}"#.into(),
        }];
        let converted = OpenAiCompatibleProvider::convert_messages(&messages);
        assert_eq!(
            converted[0].reasoning_content.as_deref(),
            Some("let me think")
        );
    }

    #[test]
    fn reasoning_content_omitted_when_none() {
        let msg = RequestMessage {
            role: "assistant".to_string(),
            content: Some("hi".into()),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("reasoning_content"));
    }
}
