use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }

    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
        }
    }
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Raw token counts from a single LLM API response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// An LLM response that may contain text, tool calls, or both.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Text content of the response (may be empty if only tool calls).
    pub text: Option<String>,
    /// Tool calls requested by the LLM.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage reported by the provider, if available.
    pub usage: Option<TokenUsage>,
    /// Raw reasoning/thinking content from thinking models (e.g. DeepSeek-R1).
    /// Preserved as an opaque pass-through so it can be sent back in subsequent
    /// API requests — some providers reject tool-call history that omits this field.
    pub reasoning_content: Option<String>,
}

impl ChatResponse {
    /// True when the LLM wants to invoke at least one tool.
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }

    /// Convenience: return text content or empty string.
    pub fn text_or_empty(&self) -> &str {
        self.text.as_deref().unwrap_or("")
    }
}

/// Description of a tool for the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Request payload for provider chat calls.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [ToolSpec]>,
}

/// A tool result to feed back to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub content: String,
}

/// A message in a multi-turn conversation, including tool interactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ConversationMessage {
    /// Regular chat message (system, user, assistant).
    Chat(ChatMessage),
    /// Tool calls from the assistant (stored for history fidelity).
    AssistantToolCalls {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
        /// Raw reasoning content from thinking models, preserved for round-trip
        /// fidelity with provider APIs that require it.
        reasoning_content: Option<String>,
    },
    /// Results of tool executions, fed back to the LLM.
    ToolResults(Vec<ToolResultMessage>),
}

/// Build tool instructions text for prompt-guided tool calling.
///
/// Generates a formatted text block describing available tools and how to
/// invoke them using XML-style tags. Used as a fallback when the provider
/// doesn't support native tool calling.
pub fn build_tool_instructions_text(tools: &[ToolSpec]) -> String {
    use std::fmt::Write;
    let mut instructions = String::new();

    instructions.push_str("## Tool Use Protocol\n\n");
    instructions.push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
    instructions.push_str("<tool_call>\n");
    instructions.push_str(r#"{"name": "tool_name", "arguments": {"param": "value"}}"#);
    instructions.push_str("\n</tool_call>\n\n");
    instructions.push_str("You may use multiple tool calls in a single response. ");
    instructions.push_str("After tool execution, results appear in <tool_result> tags. ");
    instructions
        .push_str("Continue reasoning with the results until you can give a final answer.\n\n");
    instructions.push_str("### Available Tools\n\n");

    for tool in tools {
        let _ = writeln!(&mut instructions, "**{}**: {}", tool.name, tool.description);
        let parameters =
            serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string());
        let _ = writeln!(&mut instructions, "Parameters: `{parameters}`");
        instructions.push('\n');
    }

    instructions
}

/// Provider capability declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Whether the provider supports native tool calling via API primitives.
    pub native_tool_calling: bool,
    /// Whether the provider supports vision / image inputs.
    pub vision: bool,
}

/// The core provider trait. Implement this to add a new LLM backend.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Query provider capabilities.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Whether provider supports native tool calls over API.
    fn supports_native_tools(&self) -> bool {
        self.capabilities().native_tool_calling
    }

    /// Whether provider supports multimodal vision input.
    fn supports_vision(&self) -> bool {
        self.capabilities().vision
    }

    /// Warm up the HTTP connection pool (TLS handshake, DNS, HTTP/2 setup).
    /// Default implementation is a no-op; providers with HTTP clients should override.
    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Simple one-shot chat (single user message, no explicit system prompt).
    async fn simple_chat(
        &self,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.chat_with_system(None, message, model, temperature)
            .await
    }

    /// One-shot chat with optional system prompt.
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String>;

    /// Multi-turn conversation. Default implementation extracts the last user
    /// message and delegates to `chat_with_system`.
    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system = messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.as_str());
        let last_user = messages
            .iter()
            .rfind(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        self.chat_with_system(system, last_user, model, temperature)
            .await
    }

    /// Structured chat API for agent loop callers.
    ///
    /// When the provider does not support native tools, tool definitions are
    /// injected into the system prompt via `build_tool_instructions_text`.
    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        if let Some(tools) = request.tools
            && !tools.is_empty()
            && !self.supports_native_tools()
        {
            let tool_instructions = build_tool_instructions_text(tools);
            let mut modified_messages = request.messages.to_vec();

            if let Some(system_message) = modified_messages.iter_mut().find(|m| m.role == "system")
            {
                if !system_message.content.is_empty() {
                    system_message.content.push_str("\n\n");
                }
                system_message.content.push_str(&tool_instructions);
            } else {
                modified_messages.insert(0, ChatMessage::system(tool_instructions));
            }

            let text = self
                .chat_with_history(&modified_messages, model, temperature)
                .await?;
            let (clean_text, tool_calls) = parse_xml_tool_calls(&text);
            return Ok(ChatResponse {
                text: Some(clean_text),
                tool_calls,
                usage: None,
                reasoning_content: None,
            });
        }

        let text = self
            .chat_with_history(request.messages, model, temperature)
            .await?;
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        })
    }
}

/// Parse `<tool_call>{"name":"...","arguments":{...}}</tool_call>` tags from text.
///
/// Returns the text with tool_call tags removed, and a list of parsed ToolCall structs.
/// Used by the default `Provider::chat()` implementation for non-native-tool providers.
pub fn parse_xml_tool_calls(text: &str) -> (String, Vec<ToolCall>) {
    let mut tool_calls = Vec::new();
    let mut clean_text = String::new();
    let mut remaining = text;
    let mut call_index = 0;

    while let Some(start) = remaining.find("<tool_call>") {
        // Add text before the tag
        clean_text.push_str(&remaining[..start]);

        let after_tag = &remaining[start + "<tool_call>".len()..];
        if let Some(end) = after_tag.find("</tool_call>") {
            let json_str = after_tag[..end].trim();
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                let name = parsed
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = parsed
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let arguments_str =
                    serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".into());

                tool_calls.push(ToolCall {
                    id: format!("xml_tc_{call_index}"),
                    name,
                    arguments: arguments_str,
                });
                call_index += 1;
            }
            remaining = &after_tag[end + "</tool_call>".len()..];
        } else {
            // Malformed: no closing tag, keep the rest as-is
            clean_text.push_str(&remaining[start..]);
            remaining = "";
            break;
        }
    }

    clean_text.push_str(remaining);

    // Also strip hallucinated <tool_result> tags that the LLM may emit.
    let cleaned = strip_tag(&clean_text, "tool_result");

    (cleaned.trim().to_string(), tool_calls)
}

/// Remove all occurrences of `<tag>...</tag>` from text.
fn strip_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut result = String::new();
    let mut remaining = text;

    while let Some(start) = remaining.find(&open) {
        result.push_str(&remaining[..start]);
        let after = &remaining[start + open.len()..];
        if let Some(end) = after.find(&close) {
            remaining = &after[end + close.len()..];
        } else {
            remaining = after;
        }
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_constructors() {
        let sys = ChatMessage::system("Be helpful");
        assert_eq!(sys.role, "system");
        assert_eq!(sys.content, "Be helpful");

        let user = ChatMessage::user("Hello");
        assert_eq!(user.role, "user");

        let asst = ChatMessage::assistant("Hi there");
        assert_eq!(asst.role, "assistant");

        let tool = ChatMessage::tool("{}");
        assert_eq!(tool.role, "tool");
    }

    #[test]
    fn chat_response_helpers() {
        let empty = ChatResponse {
            text: None,
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
        };
        assert!(!empty.has_tool_calls());
        assert_eq!(empty.text_or_empty(), "");

        let with_tools = ChatResponse {
            text: Some("Let me check".into()),
            tool_calls: vec![ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
            usage: None,
            reasoning_content: None,
        };
        assert!(with_tools.has_tool_calls());
        assert_eq!(with_tools.text_or_empty(), "Let me check");
    }

    #[test]
    fn token_usage_default_is_none() {
        let usage = TokenUsage::default();
        assert!(usage.input_tokens.is_none());
        assert!(usage.output_tokens.is_none());
    }

    #[test]
    fn conversation_message_variants() {
        let chat = ConversationMessage::Chat(ChatMessage::user("hi"));
        let json = serde_json::to_string(&chat).unwrap();
        assert!(json.contains("\"type\":\"Chat\""));

        let tool_result = ConversationMessage::ToolResults(vec![ToolResultMessage {
            tool_call_id: "1".into(),
            content: "done".into(),
        }]);
        let json = serde_json::to_string(&tool_result).unwrap();
        assert!(json.contains("\"type\":\"ToolResults\""));
    }

    #[test]
    fn parse_xml_tool_calls_extracts_single_call() {
        let text = r#"Let me read the file.
<tool_call>
{"name":"file_read","arguments":{"path":"a.txt"}}
</tool_call>"#;
        let (clean, calls) = parse_xml_tool_calls(text);
        assert_eq!(clean, "Let me read the file.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        let args: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(args["path"], "a.txt");
    }

    #[test]
    fn parse_xml_tool_calls_extracts_multiple_calls() {
        let text = r#"I'll search both.
<tool_call>
{"name":"search","arguments":{"q":"foo"}}
</tool_call>
<tool_call>
{"name":"search","arguments":{"q":"bar"}}
</tool_call>
Done."#;
        let (clean, calls) = parse_xml_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert!(clean.contains("I'll search both."));
        assert!(clean.contains("Done."));
    }

    #[test]
    fn parse_xml_tool_calls_no_tags_passthrough() {
        let text = "Just plain text, no tools.";
        let (clean, calls) = parse_xml_tool_calls(text);
        assert_eq!(clean, text);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_xml_tool_calls_malformed_json_skipped() {
        let text = "<tool_call>not valid json</tool_call>rest";
        let (clean, calls) = parse_xml_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(clean, "rest");
    }
}
