//! Generic OpenAI-compatible provider.
//! Most LLM APIs follow the same `/v1/chat/completions` format with Bearer auth.
//! This module provides a single implementation that works for all of them.

use crate::provider::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall, ToolSpec,
};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Image marker syntax: `[IMAGE:<url-or-data-uri>]`
/// Supports data URIs (`data:image/png;base64,...`) and HTTPS URLs.
const IMAGE_MARKER_PREFIX: &str = "[IMAGE:";
const IMAGE_MARKER_SUFFIX: &str = "]";

/// A provider that speaks the OpenAI-compatible chat completions API.
/// Authentication is always via `Authorization: Bearer <key>`.
pub struct OpenAiCompatibleProvider {
    name: String,
    base_url: String,
    credential: Option<String>,
    stream: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
struct RequestMessage {
    role: String,
    #[serde(flatten)]
    content: RequestContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallOut>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

/// Message content: either a plain string or an array of content parts (multimodal).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum RequestContent {
    Text {
        content: Option<String>,
    },
    Multimodal {
        content: Vec<ContentPart>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlRef },
}

#[derive(Debug, Serialize)]
struct ImageUrlRef {
    url: String,
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

// ── SSE streaming types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct StreamToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Accumulates streamed SSE deltas into a final response.
#[derive(Default)]
struct StreamAccumulator {
    content: String,
    reasoning_content: String,
    tool_calls: Vec<ToolCallAccum>,
    usage: Option<UsageInfo>,
}

struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    fn apply_chunk(&mut self, chunk: StreamChunk) {
        if let Some(u) = chunk.usage {
            self.usage = Some(u);
        }
        let Some(choice) = chunk.choices.into_iter().next() else {
            return;
        };
        let Some(delta) = choice.delta else {
            return;
        };

        if let Some(text) = delta.content {
            self.content.push_str(&text);
        }
        if let Some(text) = delta.reasoning_content {
            self.reasoning_content.push_str(&text);
        }
        if let Some(tool_calls) = delta.tool_calls {
            for tc_delta in tool_calls {
                let idx = tc_delta.index.unwrap_or(self.tool_calls.len());
                // Grow the vec if needed
                while self.tool_calls.len() <= idx {
                    self.tool_calls.push(ToolCallAccum {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let accum = &mut self.tool_calls[idx];
                if let Some(id) = tc_delta.id {
                    accum.id = id;
                }
                if let Some(func) = tc_delta.function {
                    if let Some(name) = func.name {
                        accum.name.push_str(&name);
                    }
                    if let Some(args) = func.arguments {
                        accum.arguments.push_str(&args);
                    }
                }
            }
        }
    }

    fn into_response(self) -> (ProviderChatResponse, Option<TokenUsage>) {
        let usage = self.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        let has_content = !self.content.trim().is_empty();
        let has_reasoning = !self.reasoning_content.trim().is_empty();

        let reasoning_content = if has_reasoning {
            Some(self.reasoning_content.clone())
        } else {
            None
        };

        let text = if has_content {
            Some(self.content)
        } else if has_reasoning {
            Some(self.reasoning_content)
        } else {
            None
        };

        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|tc| !tc.name.is_empty())
            .map(|tc| {
                let arguments =
                    if serde_json::from_str::<serde_json::Value>(&tc.arguments).is_ok() {
                        tc.arguments
                    } else {
                        "{}".to_string()
                    };
                ProviderToolCall {
                    id: if tc.id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        tc.id
                    },
                    name: tc.name,
                    arguments,
                }
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

// ── Image marker parsing ─────────────────────────────────────────────────────

/// Check if text contains `[IMAGE:...]` markers.
fn has_image_markers(text: &str) -> bool {
    text.contains(IMAGE_MARKER_PREFIX)
}

/// Parse text with `[IMAGE:url]` markers into content parts.
/// Text segments become `text` parts, image markers become `image_url` parts.
fn parse_content_parts(text: &str) -> Vec<ContentPart> {
    let mut parts = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(IMAGE_MARKER_PREFIX) {
        // Text before the marker
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            parts.push(ContentPart::Text {
                text: before.to_string(),
            });
        }

        let after_prefix = &remaining[start + IMAGE_MARKER_PREFIX.len()..];
        if let Some(end) = after_prefix.find(IMAGE_MARKER_SUFFIX) {
            let url = after_prefix[..end].trim();
            if !url.is_empty() {
                parts.push(ContentPart::ImageUrl {
                    image_url: ImageUrlRef {
                        url: url.to_string(),
                    },
                });
            }
            remaining = &after_prefix[end + IMAGE_MARKER_SUFFIX.len()..];
        } else {
            // Malformed marker — treat rest as text
            parts.push(ContentPart::Text {
                text: remaining.to_string(),
            });
            remaining = "";
        }
    }

    // Trailing text
    if !remaining.trim().is_empty() {
        parts.push(ContentPart::Text {
            text: remaining.to_string(),
        });
    }

    // If no parts were produced (empty text, no images), return a single empty text part
    if parts.is_empty() {
        parts.push(ContentPart::Text {
            text: String::new(),
        });
    }

    parts
}

/// Build RequestContent: multimodal if images present, plain text otherwise.
fn build_content(text: Option<&str>) -> RequestContent {
    match text {
        Some(t) if has_image_markers(t) => RequestContent::Multimodal {
            content: parse_content_parts(t),
        },
        other => RequestContent::Text {
            content: other.map(ToString::to_string),
        },
    }
}

// ── Provider ─────────────────────────────────────────────────────────────────

impl OpenAiCompatibleProvider {
    pub fn new(name: &str, base_url: &str, credential: Option<&str>) -> Self {
        Self::with_stream(name, base_url, credential, false)
    }

    pub fn with_stream(
        name: &str,
        base_url: &str,
        credential: Option<&str>,
        stream: bool,
    ) -> Self {
        Self {
            name: name.to_string(),
            base_url: base_url.trim_end_matches('/').to_string(),
            credential: credential
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(ToString::to_string),
            stream,
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

                    let content_str = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);

                    let reasoning_content = value
                        .get("reasoning_content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);

                    return RequestMessage {
                        role: "assistant".to_string(),
                        content: RequestContent::Text {
                            content: content_str,
                        },
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
                        content: RequestContent::Text { content },
                        tool_call_id,
                        tool_calls: None,
                        reasoning_content: None,
                    };
                }

                // Regular message — may contain image markers
                RequestMessage {
                    role: m.role.clone(),
                    content: build_content(Some(&m.content)),
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

    /// Send a request and decode the SSE stream into a final response.
    async fn send_streaming(
        &self,
        request: ChatCompletionsRequest,
    ) -> anyhow::Result<(ProviderChatResponse, Option<TokenUsage>)> {
        let credential = self.require_credential()?;
        let url = self.chat_completions_url();

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {credential}"))
            .header("Accept", "text/event-stream")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(api_error(&self.name, response).await);
        }

        let mut accum = StreamAccumulator::default();
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process complete SSE events (separated by \n\n)
            while let Some(boundary) = buffer.find("\n\n") {
                let event_block = buffer[..boundary].to_string();
                buffer = buffer[boundary + 2..].to_string();

                for line in event_block.lines() {
                    let data = match line.strip_prefix("data:") {
                        Some(d) => d.trim(),
                        None => continue,
                    };
                    if data == "[DONE]" || data.is_empty() {
                        continue;
                    }
                    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                        accum.apply_chunk(chunk);
                    }
                }
            }
        }

        // Process any remaining data in the buffer
        for line in buffer.lines() {
            let data = match line.strip_prefix("data:") {
                Some(d) => d.trim(),
                None => continue,
            };
            if data == "[DONE]" || data.is_empty() {
                continue;
            }
            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                accum.apply_chunk(chunk);
            }
        }

        Ok(accum.into_response())
    }

    /// Send a request and decode as a single JSON response.
    async fn send_non_streaming(
        &self,
        request: ChatCompletionsRequest,
    ) -> anyhow::Result<(ProviderChatResponse, Option<TokenUsage>)> {
        let credential = self.require_credential()?;
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

        Ok(Self::parse_response(parsed))
    }

    /// Send a request, dispatching to streaming or non-streaming based on config.
    async fn send_request(
        &self,
        request: ChatCompletionsRequest,
    ) -> anyhow::Result<(ProviderChatResponse, Option<TokenUsage>)> {
        if self.stream {
            self.send_streaming(request).await
        } else {
            self.send_non_streaming(request).await
        }
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
            vision: true,
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
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(RequestMessage {
                role: "system".to_string(),
                content: build_content(Some(sys)),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            });
        }
        messages.push(RequestMessage {
            role: "user".to_string(),
            content: build_content(Some(message)),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        });

        let request = ChatCompletionsRequest {
            model: model.to_string(),
            messages,
            temperature,
            stream: Some(self.stream),
            tools: None,
            tool_choice: None,
            max_tokens: None,
        };

        let (result, _) = self.send_request(request).await?;
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
        let request = ChatCompletionsRequest {
            model: model.to_string(),
            messages: Self::convert_messages(messages),
            temperature,
            stream: Some(self.stream),
            tools: None,
            tool_choice: None,
            max_tokens: None,
        };

        let (result, _) = self.send_request(request).await?;
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
        let tools = Self::convert_tools(request.tools);
        let api_request = ChatCompletionsRequest {
            model: model.to_string(),
            messages: Self::convert_messages(request.messages),
            temperature,
            stream: Some(self.stream),
            tool_choice: tools.as_ref().map(|_| "auto".to_string()),
            tools,
            max_tokens: None,
        };

        let (mut result, usage) = self.send_request(api_request).await?;
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
                content: RequestContent::Text {
                    content: Some("hello".to_string()),
                },
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            }],
            temperature: 0.7,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            max_tokens: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("gpt-4o"));
        assert!(json.contains("hello"));
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("reasoning_content"));
        assert!(!json.contains("max_tokens"));
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
        // Content is in the flattened RequestContent::Text variant
        let json = serde_json::to_string(&converted[0]).unwrap();
        assert!(json.contains("checking"));
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
    fn capabilities_reports_native_tools_and_vision() {
        let p = make_provider("test", "https://api.example.com/v1", None);
        let caps = p.capabilities();
        assert!(caps.native_tool_calling);
        assert!(caps.vision);
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
            content: RequestContent::Text {
                content: Some("hi".into()),
            },
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("reasoning_content"));
    }

    // ── Image/vision tests ───────────────────────────────────────────────────

    #[test]
    fn has_image_markers_detects_marker() {
        assert!(has_image_markers(
            "Check this [IMAGE:https://example.com/img.png] out"
        ));
        assert!(!has_image_markers("No images here"));
    }

    #[test]
    fn parse_content_parts_text_only() {
        let parts = parse_content_parts("Hello world");
        assert_eq!(parts.len(), 1);
        let json = serde_json::to_value(&parts[0]).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "Hello world");
    }

    #[test]
    fn parse_content_parts_single_image() {
        let parts =
            parse_content_parts("Look at this [IMAGE:https://example.com/cat.jpg] what is it?");
        assert_eq!(parts.len(), 3);
        let json: Vec<serde_json::Value> = parts.iter().map(|p| serde_json::to_value(p).unwrap()).collect();
        assert_eq!(json[0]["type"], "text");
        assert_eq!(json[0]["text"], "Look at this ");
        assert_eq!(json[1]["type"], "image_url");
        assert_eq!(json[1]["image_url"]["url"], "https://example.com/cat.jpg");
        assert_eq!(json[2]["type"], "text");
        assert_eq!(json[2]["text"], " what is it?");
    }

    #[test]
    fn parse_content_parts_data_uri() {
        let parts = parse_content_parts("[IMAGE:data:image/png;base64,iVBOR...]");
        assert_eq!(parts.len(), 1);
        let json = serde_json::to_value(&parts[0]).unwrap();
        assert_eq!(json["type"], "image_url");
        assert_eq!(json["image_url"]["url"], "data:image/png;base64,iVBOR...");
    }

    #[test]
    fn parse_content_parts_multiple_images() {
        let parts = parse_content_parts(
            "Image 1: [IMAGE:https://a.com/1.jpg] Image 2: [IMAGE:https://b.com/2.jpg] done",
        );
        assert_eq!(parts.len(), 5);
    }

    #[test]
    fn build_content_plain_text() {
        let content = build_content(Some("hello"));
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn build_content_with_image() {
        let content = build_content(Some("Check [IMAGE:https://example.com/x.png] this"));
        let json = serde_json::to_value(&content).unwrap();
        assert!(json["content"].is_array());
        assert_eq!(json["content"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn build_content_none() {
        let content = build_content(None);
        let json = serde_json::to_value(&content).unwrap();
        assert!(json["content"].is_null());
    }

    #[test]
    fn multimodal_message_serializes_as_array() {
        let msg = RequestMessage {
            role: "user".to_string(),
            content: RequestContent::Multimodal {
                content: vec![
                    ContentPart::Text {
                        text: "What is this?".to_string(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrlRef {
                            url: "https://example.com/img.png".to_string(),
                        },
                    },
                ],
            },
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["role"], "user");
        assert!(value["content"].is_array());
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][1]["type"], "image_url");
    }

    #[test]
    fn convert_messages_detects_image_in_user_message() {
        let messages = vec![ChatMessage {
            role: "user".into(),
            content: "Describe [IMAGE:https://example.com/photo.jpg] this".into(),
        }];
        let converted = OpenAiCompatibleProvider::convert_messages(&messages);
        let json = serde_json::to_string(&converted[0]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["content"].is_array());
    }

    #[test]
    fn max_tokens_omitted_when_none() {
        let request = ChatCompletionsRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            temperature: 0.7,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            max_tokens: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("max_tokens"));
    }

    #[test]
    fn max_tokens_included_when_set() {
        let request = ChatCompletionsRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            temperature: 0.7,
            stream: Some(false),
            tools: None,
            tool_choice: None,
            max_tokens: Some(4096),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"max_tokens\":4096"));
    }

    // ── Streaming tests ──────────────────────────────────────────────────────

    #[test]
    fn stream_accumulator_text_deltas() {
        let mut accum = StreamAccumulator::default();
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"Hello"}}]}"#,
        ).unwrap());
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"content":" world"}}]}"#,
        ).unwrap());
        let (resp, _) = accum.into_response();
        assert_eq!(resp.text.as_deref(), Some("Hello world"));
        assert!(resp.tool_calls.is_empty());
    }

    #[test]
    fn stream_accumulator_tool_call_deltas() {
        let mut accum = StreamAccumulator::default();
        // First chunk: tool call id + name
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"shell","arguments":""}}]}}]}"#,
        ).unwrap());
        // Second chunk: arguments fragment
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\""}}]}}]}"#,
        ).unwrap());
        // Third chunk: arguments rest
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"ls\"}"}}]}}]}"#,
        ).unwrap());

        let (resp, _) = accum.into_response();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "shell");
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].arguments, r#"{"cmd":"ls"}"#);
    }

    #[test]
    fn stream_accumulator_multiple_tool_calls() {
        let mut accum = StreamAccumulator::default();
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{}"}}]}}]}"#,
        ).unwrap());
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"c2","function":{"name":"write","arguments":"{}"}}]}}]}"#,
        ).unwrap());

        let (resp, _) = accum.into_response();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "read");
        assert_eq!(resp.tool_calls[1].name, "write");
    }

    #[test]
    fn stream_accumulator_usage_from_last_chunk() {
        let mut accum = StreamAccumulator::default();
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"hi"}}]}"#,
        ).unwrap());
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#,
        ).unwrap());

        let (resp, usage) = accum.into_response();
        assert_eq!(resp.text.as_deref(), Some("hi"));
        let usage = usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn stream_accumulator_reasoning_content() {
        let mut accum = StreamAccumulator::default();
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":"thinking"}}]}"#,
        ).unwrap());
        accum.apply_chunk(serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
        ).unwrap());

        let (resp, _) = accum.into_response();
        assert_eq!(resp.text.as_deref(), Some("answer"));
        assert_eq!(resp.reasoning_content.as_deref(), Some("thinking"));
    }

    #[test]
    fn stream_accumulator_empty_produces_none() {
        let accum = StreamAccumulator::default();
        let (resp, usage) = accum.into_response();
        assert!(resp.text.is_none());
        assert!(resp.tool_calls.is_empty());
        assert!(usage.is_none());
    }

    #[test]
    fn stream_chunk_deserializes() {
        let json = r#"{"choices":[{"delta":{"content":"Hello"}}]}"#;
        let chunk: StreamChunk = serde_json::from_str(json).unwrap();
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(
            chunk.choices[0].delta.as_ref().unwrap().content.as_deref(),
            Some("Hello")
        );
    }

    #[test]
    fn with_stream_constructor() {
        let p = OpenAiCompatibleProvider::with_stream(
            "test", "https://api.example.com/v1", Some("sk-test"), true,
        );
        assert!(p.stream);
        assert_eq!(p.credential.as_deref(), Some("sk-test"));

        let p2 = OpenAiCompatibleProvider::new("test", "https://api.example.com/v1", None);
        assert!(!p2.stream);
    }
}
