use crate::auth::AuthService;
use crate::auth::anthropic_token::{AnthropicAuthKind, detect_auth_kind};
use crate::provider::ProviderRuntimeOptions;
use crate::provider::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall, ToolSpec,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicOAuthProvider {
    auth: AuthService,
    auth_profile_override: Option<String>,
    base_url: String,
    /// Explicit API key passed via --api-key or env var (bypasses AuthService).
    api_key: Option<String>,
    client: Client,
}

// ── Request types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec<'a>>>,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize)]
struct NativeToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
}

// ── Response types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ResponseContent>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct ResponseContent {
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

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

// ── Provider implementation ──────────────────────────────────────────────────

impl AnthropicOAuthProvider {
    pub fn new(options: &ProviderRuntimeOptions, api_key: Option<&str>) -> anyhow::Result<Self> {
        let state_dir = options
            .crewforge_dir
            .clone()
            .unwrap_or_else(default_crewforge_dir);
        let auth = AuthService::new(&state_dir, options.secrets_encrypt);
        let base_url = options
            .provider_api_url
            .as_deref()
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_string();

        Ok(Self {
            auth,
            auth_profile_override: options.auth_profile_override.clone(),
            base_url,
            api_key: api_key
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(ToString::to_string),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
        })
    }

    /// Resolve credential: explicit api_key → AuthService profile.
    async fn resolve_credential(&self) -> anyhow::Result<String> {
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        self.auth
            .get_provider_bearer_token("anthropic", self.auth_profile_override.as_deref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Anthropic credentials not found. Set ANTHROPIC_API_KEY or run `crewforge auth paste-token --provider anthropic`."
                )
            })
    }

    /// Apply auth headers based on token kind.
    fn apply_auth(
        &self,
        mut req: reqwest::RequestBuilder,
        credential: &str,
    ) -> reqwest::RequestBuilder {
        let kind = detect_auth_kind(credential, None);
        match kind {
            AnthropicAuthKind::Authorization => {
                req = req
                    .header("Authorization", format!("Bearer {credential}"))
                    .header("anthropic-beta", ANTHROPIC_OAUTH_BETA);
            }
            AnthropicAuthKind::ApiKey => {
                req = req.header("x-api-key", credential);
            }
        }
        req
    }

    async fn send_messages(
        &self,
        request: &MessagesRequest<'_>,
    ) -> anyhow::Result<MessagesResponse> {
        let credential = self.resolve_credential().await?;

        let mut req = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(request);

        req = self.apply_auth(req, &credential);

        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(super::api_error("Anthropic", response).await);
        }

        response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Anthropic response parse failed: {e}"))
    }
}

fn default_crewforge_dir() -> PathBuf {
    directories::UserDirs::new().map_or_else(
        || PathBuf::from(".crewforge"),
        |dirs| dirs.home_dir().join(".crewforge"),
    )
}

// ── Message conversion ───────────────────────────────────────────────────────

fn convert_messages(messages: &[ChatMessage]) -> (Option<String>, Vec<NativeMessage>) {
    let mut system_text = None;
    let mut native = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                if system_text.is_none() {
                    system_text = Some(msg.content.clone());
                }
            }
            "assistant" => {
                if let Some(blocks) = parse_assistant_tool_call_content(&msg.content) {
                    native.push(NativeMessage {
                        role: "assistant".to_string(),
                        content: blocks,
                    });
                } else {
                    native.push(NativeMessage {
                        role: "assistant".to_string(),
                        content: vec![ContentBlock::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
            }
            "tool" => {
                if let Some(result_msg) = parse_tool_result_content(&msg.content) {
                    native.push(result_msg);
                } else {
                    native.push(NativeMessage {
                        role: "user".to_string(),
                        content: vec![ContentBlock::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
            }
            _ => {
                native.push(NativeMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: msg.content.clone(),
                    }],
                });
            }
        }
    }

    (system_text, native)
}

fn parse_assistant_tool_call_content(content: &str) -> Option<Vec<ContentBlock>> {
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
        blocks.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }
    for call in tool_calls {
        let input = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        blocks.push(ContentBlock::ToolUse {
            id: call.id,
            name: call.name,
            input,
        });
    }
    Some(blocks)
}

fn parse_tool_result_content(content: &str) -> Option<NativeMessage> {
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
        content: vec![ContentBlock::ToolResult {
            tool_use_id,
            content: result,
        }],
    })
}

fn convert_tools<'a>(tools: Option<&'a [ToolSpec]>) -> Option<Vec<NativeToolSpec<'a>>> {
    let items = tools?;
    if items.is_empty() {
        return None;
    }
    Some(
        items
            .iter()
            .map(|tool| NativeToolSpec {
                name: &tool.name,
                description: &tool.description,
                input_schema: &tool.parameters,
            })
            .collect(),
    )
}

fn parse_response(response: MessagesResponse) -> ProviderChatResponse {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    let usage = response.usage.map(|u| TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
    });

    for block in response.content {
        match block.kind.as_str() {
            "text" => {
                if let Some(text) = block
                    .text
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                {
                    text_parts.push(text);
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

// ── Provider trait ────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for AnthropicOAuthProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let request = MessagesRequest {
            model: model.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            system: system_prompt.map(ToString::to_string),
            messages: vec![NativeMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: message.to_string(),
                }],
            }],
            temperature,
            tools: None,
        };

        let response = self.send_messages(&request).await?;
        response
            .content
            .into_iter()
            .find(|c| c.kind == "text")
            .and_then(|c| c.text)
            .ok_or_else(|| anyhow::anyhow!("No text response from Anthropic"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (system, messages) = convert_messages(request.messages);

        let native_request = MessagesRequest {
            model: model.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            system,
            messages,
            temperature,
            tools: convert_tools(request.tools),
        };

        let response = self.send_messages(&native_request).await?;
        Ok(parse_response(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_dir_is_non_empty() {
        let path = default_crewforge_dir();
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn convert_messages_extracts_system() {
        let messages = vec![
            ChatMessage::system("Be helpful"),
            ChatMessage::user("Hello"),
        ];
        let (system, native) = convert_messages(&messages);
        assert_eq!(system.as_deref(), Some("Be helpful"));
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
        let (_, native) = convert_messages(&messages);
        assert_eq!(native.len(), 2);
        assert!(
            native[0]
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::ToolUse { .. }))
        );
        assert_eq!(native[1].role, "user");
        assert!(
            native[1]
                .content
                .iter()
                .any(|c| matches!(c, ContentBlock::ToolResult { .. }))
        );
    }

    #[test]
    fn parse_response_extracts_text_and_tool_calls() {
        let response = MessagesResponse {
            content: vec![
                ResponseContent {
                    kind: "text".to_string(),
                    text: Some("I'll help".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                ResponseContent {
                    kind: "tool_use".to_string(),
                    text: None,
                    id: Some("call_1".to_string()),
                    name: Some("shell".to_string()),
                    input: Some(serde_json::json!({"command": "date"})),
                },
            ],
            usage: Some(AnthropicUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            }),
        };
        let result = parse_response(response);
        assert_eq!(result.text.as_deref(), Some("I'll help"));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "shell");
        let usage = result.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn convert_tools_maps_spec() {
        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let native = convert_tools(Some(&tools)).unwrap();
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].name, "shell");
    }

    #[test]
    fn convert_tools_returns_none_for_empty() {
        assert!(convert_tools(Some(&[])).is_none());
        assert!(convert_tools(None).is_none());
    }

    #[tokio::test]
    async fn resolve_credential_uses_explicit_key() {
        let opts = ProviderRuntimeOptions::default();
        let provider = AnthropicOAuthProvider::new(&opts, Some("sk-ant-api-test")).unwrap();
        let cred = provider.resolve_credential().await.unwrap();
        assert_eq!(cred, "sk-ant-api-test");
    }

    #[test]
    fn capabilities_reports_native_tools() {
        let opts = ProviderRuntimeOptions::default();
        let provider = AnthropicOAuthProvider::new(&opts, None).unwrap();
        let caps = provider.capabilities();
        assert!(caps.native_tool_calling);
        assert!(!caps.vision);
    }
}
