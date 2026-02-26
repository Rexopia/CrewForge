use crate::provider::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, TokenUsage, ToolCall as ProviderToolCall, ToolSpec,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub struct AnthropicProvider {
    credential: Option<String>,
    base_url: String,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<Message>,
    temperature: f64,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest<'a> {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec<'a>>>,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: Vec<NativeContentOut>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum NativeContentOut {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Serialize)]
struct NativeToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemPrompt {
    String(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    #[serde(default)]
    content: Vec<NativeContentIn>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct NativeContentIn {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

impl AnthropicProvider {
    pub fn new(credential: Option<&str>) -> Self {
        Self::with_base_url(credential, None)
    }

    pub fn with_base_url(credential: Option<&str>, base_url: Option<&str>) -> Self {
        let base_url = base_url
            .map(|u| u.trim_end_matches('/'))
            .unwrap_or("https://api.anthropic.com")
            .to_string();
        Self {
            credential: credential
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(ToString::to_string),
            base_url,
        }
    }

    fn is_setup_token(token: &str) -> bool {
        token.starts_with("sk-ant-oat01-")
    }

    fn apply_auth(
        &self,
        request: reqwest::RequestBuilder,
        credential: &str,
    ) -> reqwest::RequestBuilder {
        if Self::is_setup_token(credential) {
            request
                .header("Authorization", format!("Bearer {credential}"))
                .header("anthropic-beta", "oauth-2025-04-20")
        } else {
            request.header("x-api-key", credential)
        }
    }

    /// Cache system prompts larger than ~1024 tokens (3KB of text)
    fn should_cache_system(text: &str) -> bool {
        text.len() > 3072
    }

    /// Cache conversations with more than 4 messages (excluding system)
    fn should_cache_conversation(messages: &[ChatMessage]) -> bool {
        messages.iter().filter(|m| m.role != "system").count() > 4
    }

    /// Apply cache control to the last message content block
    fn apply_cache_to_last_message(messages: &mut [NativeMessage]) {
        if let Some(last_msg) = messages.last_mut() {
            if let Some(last_content) = last_msg.content.last_mut() {
                match last_content {
                    NativeContentOut::Text { cache_control, .. }
                    | NativeContentOut::ToolResult { cache_control, .. } => {
                        *cache_control = Some(CacheControl::ephemeral());
                    }
                    NativeContentOut::ToolUse { .. } => {}
                }
            }
        }
    }

    fn convert_tools<'a>(tools: Option<&'a [ToolSpec]>) -> Option<Vec<NativeToolSpec<'a>>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        let mut native_tools: Vec<NativeToolSpec<'a>> = items
            .iter()
            .map(|tool| NativeToolSpec {
                name: &tool.name,
                description: &tool.description,
                input_schema: &tool.parameters,
                cache_control: None,
            })
            .collect();

        // Cache the last tool definition (caches all tools)
        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }

        Some(native_tools)
    }

    fn parse_assistant_tool_call_message(content: &str) -> Option<Vec<NativeContentOut>> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_calls = value
            .get("tool_calls")
            .and_then(|v| serde_json::from_value::<Vec<ProviderToolCall>>(v.clone()).ok())?;

        let mut blocks = Vec::new();
        if let Some(text) = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            blocks.push(NativeContentOut::Text {
                text: text.to_string(),
                cache_control: None,
            });
        }
        for call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            blocks.push(NativeContentOut::ToolUse {
                id: call.id,
                name: call.name,
                input,
                cache_control: None,
            });
        }
        Some(blocks)
    }

    fn parse_tool_result_message(content: &str) -> Option<NativeMessage> {
        let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
        let tool_use_id = value
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)?
            .to_string();
        let result = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        Some(NativeMessage {
            role: "user".to_string(),
            content: vec![NativeContentOut::ToolResult {
                tool_use_id,
                content: result,
                cache_control: None,
            }],
        })
    }

    fn convert_messages(messages: &[ChatMessage]) -> (Option<SystemPrompt>, Vec<NativeMessage>) {
        let mut system_text = None;
        let mut native_messages = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    if system_text.is_none() {
                        system_text = Some(msg.content.clone());
                    }
                }
                "assistant" => {
                    if let Some(blocks) = Self::parse_assistant_tool_call_message(&msg.content) {
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: blocks,
                        });
                    } else {
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                "tool" => {
                    if let Some(tool_result) = Self::parse_tool_result_message(&msg.content) {
                        native_messages.push(tool_result);
                    } else {
                        native_messages.push(NativeMessage {
                            role: "user".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                _ => {
                    native_messages.push(NativeMessage {
                        role: "user".to_string(),
                        content: vec![NativeContentOut::Text {
                            text: msg.content.clone(),
                            cache_control: None,
                        }],
                    });
                }
            }
        }

        // Convert system text to SystemPrompt with cache control if large
        let system_prompt = system_text.map(|text| {
            if Self::should_cache_system(&text) {
                SystemPrompt::Blocks(vec![SystemBlock {
                    block_type: "text".to_string(),
                    text,
                    cache_control: Some(CacheControl::ephemeral()),
                }])
            } else {
                SystemPrompt::String(text)
            }
        });

        (system_prompt, native_messages)
    }

    fn parse_text_response(response: ChatResponse) -> anyhow::Result<String> {
        response
            .content
            .into_iter()
            .find(|c| c.kind == "text")
            .and_then(|c| c.text)
            .ok_or_else(|| anyhow::anyhow!("No response from Anthropic"))
    }

    fn parse_native_response(response: NativeChatResponse) -> ProviderChatResponse {
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        let usage = response.usage.map(|u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });

        for block in response.content {
            match block.kind.as_str() {
                "text" => {
                    if let Some(text) = block.text.map(|t| t.trim().to_string()) {
                        if !text.is_empty() {
                            text_parts.push(text);
                        }
                    }
                }
                "tool_use" => {
                    let name = block.name.unwrap_or_default();
                    if name.is_empty() {
                        continue;
                    }
                    let arguments = block
                        .input
                        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                    tool_calls.push(ProviderToolCall {
                        id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: arguments.to_string(),
                    });
                }
                _ => {}
            }
        }

        ProviderChatResponse {
            text: if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join("\n"))
            },
            tool_calls,
            usage,
            reasoning_content: None,
        }
    }

    fn http_client(&self) -> Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default()
    }
}

async fn api_error(provider: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read response body>".to_string());
    anyhow::anyhow!("API error ({provider}, {status}): {body}")
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token)."
            )
        })?;

        let request = ChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt.map(ToString::to_string),
            messages: vec![Message {
                role: "user".to_string(),
                content: message.to_string(),
            }],
            temperature,
        };

        let mut req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request);

        req = self.apply_auth(req, credential);

        let response = req.send().await?;

        if !response.status().is_success() {
            return Err(api_error("Anthropic", response).await);
        }

        let chat_response: ChatResponse = response.json().await?;
        Self::parse_text_response(chat_response)
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let credential = self.credential.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Anthropic credentials not set. Set ANTHROPIC_API_KEY or ANTHROPIC_OAUTH_TOKEN (setup-token)."
            )
        })?;

        let (system_prompt, mut messages) = Self::convert_messages(request.messages);

        // Auto-cache last message if conversation is long
        if Self::should_cache_conversation(request.messages) {
            Self::apply_cache_to_last_message(&mut messages);
        }

        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: 4096,
            system: system_prompt,
            messages,
            temperature,
            tools: Self::convert_tools(request.tools),
        };

        let mut req = self
            .http_client()
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&native_request);

        if let Some(tools) = &native_request.tools {
            if !tools.is_empty() {
                req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
            }
        }

        req = self.apply_auth(req, credential);

        let response = req.send().await?;

        if !response.status().is_success() {
            return Err(api_error("Anthropic", response).await);
        }

        let native_response: NativeChatResponse = response.json().await?;
        Ok(Self::parse_native_response(native_response))
    }

    fn supports_native_tools(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_key() {
        let p = AnthropicProvider::new(Some("test-key"));
        assert_eq!(p.credential.as_deref(), Some("test-key"));
    }

    #[test]
    fn creates_without_key() {
        let p = AnthropicProvider::new(None);
        assert!(p.credential.is_none());
    }

    #[test]
    fn trims_empty_key() {
        let p = AnthropicProvider::new(Some("   "));
        assert!(p.credential.is_none());
    }

    #[test]
    fn default_base_url() {
        let p = AnthropicProvider::new(None);
        assert_eq!(p.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn custom_base_url_strips_trailing_slash() {
        let p = AnthropicProvider::with_base_url(None, Some("https://custom.example.com/"));
        assert_eq!(p.base_url, "https://custom.example.com");
    }

    #[test]
    fn is_setup_token_detects_oat01_prefix() {
        assert!(AnthropicProvider::is_setup_token("sk-ant-oat01-abc123"));
        assert!(!AnthropicProvider::is_setup_token("sk-ant-api03-abc123"));
    }

    #[tokio::test]
    async fn chat_fails_without_key() {
        let p = AnthropicProvider::new(None);
        let result = p
            .chat_with_system(None, "hello", "claude-sonnet-4", 0.7)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("credentials not set"));
    }

    #[test]
    fn parse_text_response_extracts_text() {
        let response = ChatResponse {
            content: vec![ContentBlock {
                kind: "text".to_string(),
                text: Some("Hello!".to_string()),
                id: None,
                name: None,
                input: None,
            }],
        };
        let result = AnthropicProvider::parse_text_response(response).unwrap();
        assert_eq!(result, "Hello!");
    }

    #[test]
    fn parse_text_response_fails_with_no_text() {
        let response = ChatResponse {
            content: vec![ContentBlock {
                kind: "tool_use".to_string(),
                text: None,
                id: Some("id".to_string()),
                name: Some("shell".to_string()),
                input: Some(serde_json::json!({})),
            }],
        };
        assert!(AnthropicProvider::parse_text_response(response).is_err());
    }

    #[test]
    fn convert_messages_extracts_system_prompt() {
        let messages = vec![
            ChatMessage::system("Be helpful"),
            ChatMessage::user("Hello"),
        ];
        let (system, native) = AnthropicProvider::convert_messages(&messages);
        assert!(system.is_some());
        assert_eq!(native.len(), 1);
    }

    #[test]
    fn convert_messages_handles_tool_call_history() {
        let tool_call_json = serde_json::json!({
            "content": "Let me check",
            "tool_calls": [{
                "id": "call_1",
                "name": "shell",
                "arguments": "{\"command\":\"date\"}"
            }]
        });
        let messages = vec![
            ChatMessage::assistant(tool_call_json.to_string()),
            ChatMessage {
                role: "tool".to_string(),
                content: r#"{"tool_call_id":"call_1","content":"Mon Dec 1"}"#.to_string(),
            },
        ];
        let (_, native) = AnthropicProvider::convert_messages(&messages);
        assert_eq!(native.len(), 2);
        // First message should contain ToolUse block
        assert!(native[0]
            .content
            .iter()
            .any(|c| matches!(c, NativeContentOut::ToolUse { .. })));
        // Second message (tool result) becomes a user message with ToolResult block
        assert_eq!(native[1].role, "user");
        assert!(native[1]
            .content
            .iter()
            .any(|c| matches!(c, NativeContentOut::ToolResult { .. })));
    }

    #[test]
    fn parse_native_response_extracts_tool_calls() {
        let response = NativeChatResponse {
            content: vec![
                NativeContentIn {
                    kind: "text".to_string(),
                    text: Some("I'll use a tool".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                NativeContentIn {
                    kind: "tool_use".to_string(),
                    text: None,
                    id: Some("call_1".to_string()),
                    name: Some("shell".to_string()),
                    input: Some(serde_json::json!({"command": "date"})),
                },
            ],
            usage: None,
        };
        let result = AnthropicProvider::parse_native_response(response);
        assert_eq!(result.text.as_deref(), Some("I'll use a tool"));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "shell");
    }

    #[test]
    fn parse_native_response_reports_usage() {
        let response = NativeChatResponse {
            content: vec![NativeContentIn {
                kind: "text".to_string(),
                text: Some("Hi".to_string()),
                id: None,
                name: None,
                input: None,
            }],
            usage: Some(AnthropicUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            }),
        };
        let result = AnthropicProvider::parse_native_response(response);
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn should_cache_system_triggers_for_large_text() {
        let small = "small";
        let large = "x".repeat(4096);
        assert!(!AnthropicProvider::should_cache_system(small));
        assert!(AnthropicProvider::should_cache_system(&large));
    }

    #[test]
    fn should_cache_conversation_triggers_after_4_non_system_messages() {
        let messages: Vec<ChatMessage> = (0..5)
            .map(|i| ChatMessage::user(format!("msg {i}")))
            .collect();
        assert!(AnthropicProvider::should_cache_conversation(&messages));
        let short: Vec<ChatMessage> = (0..3)
            .map(|i| ChatMessage::user(format!("msg {i}")))
            .collect();
        assert!(!AnthropicProvider::should_cache_conversation(&short));
    }
}
