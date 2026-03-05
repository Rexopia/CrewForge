use crate::provider::traits::{ChatResponse, ConversationMessage, ToolResultMessage};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: Value,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub tool_result: super::ToolResult,
    pub name: String,
    pub tool_call_id: Option<String>,
}

/// Parse native tool calls from a provider ChatResponse.
pub fn parse_response(response: &ChatResponse) -> (String, Vec<ParsedToolCall>) {
    let text = response.text.clone().unwrap_or_default();
    let calls = response
        .tool_calls
        .iter()
        .map(|tc| ParsedToolCall {
            name: tc.name.clone(),
            arguments: serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                tracing::warn!(
                    tool = %tc.name,
                    error = %e,
                    "Failed to parse native tool call arguments as JSON; defaulting to empty object"
                );
                Value::Object(serde_json::Map::new())
            }),
            tool_call_id: Some(tc.id.clone()),
        })
        .collect();
    (text, calls)
}

/// Format tool execution results as a ConversationMessage for history.
pub fn format_results(results: &[ToolExecutionResult]) -> ConversationMessage {
    let messages: Vec<ToolResultMessage> = results
        .iter()
        .map(|result| {
            let content = match (
                &result.tool_result.error,
                result.tool_result.output.is_empty(),
            ) {
                (Some(err), true) => err.clone(),
                (Some(err), false) => format!("{}\n{}", result.tool_result.output, err),
                (None, _) => result.tool_result.output.clone(),
            };
            ToolResultMessage {
                tool_call_id: result
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                content,
            }
        })
        .collect();
    ConversationMessage::ToolResults(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::traits::ToolCall;

    #[test]
    fn parse_response_extracts_tool_calls() {
        let response = ChatResponse {
            text: Some("ok".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "file_read".into(),
                arguments: "{\"path\":\"a.txt\"}".into(),
            }],
            usage: None,
            reasoning_content: None,
        };
        let (text, calls) = parse_response(&response);
        assert_eq!(text, "ok");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].tool_call_id.as_deref(), Some("tc1"));
    }

    #[test]
    fn parse_response_bad_json_fallback() {
        let response = ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "tc2".into(),
                name: "tool_x".into(),
                arguments: "not-json".into(),
            }],
            usage: None,
            reasoning_content: None,
        };
        let (_, calls) = parse_response(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn format_results_preserves_tool_call_id() {
        let msg = format_results(&[ToolExecutionResult {
            tool_result: crate::agent::ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            },
            name: "shell".into(),
            tool_call_id: Some("tc-1".into()),
        }]);
        match msg {
            ConversationMessage::ToolResults(results) => {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].tool_call_id, "tc-1");
                assert_eq!(results[0].content, "ok");
            }
            _ => panic!("expected ToolResults variant"),
        }
    }

    #[test]
    fn format_results_includes_error() {
        let msg = format_results(&[ToolExecutionResult {
            tool_result: crate::agent::ToolResult {
                success: false,
                output: "partial".into(),
                error: Some("denied".into()),
            },
            name: "shell".into(),
            tool_call_id: Some("tc-2".into()),
        }]);
        match msg {
            ConversationMessage::ToolResults(results) => {
                assert!(results[0].content.contains("denied"));
            }
            _ => panic!("expected ToolResults variant"),
        }
    }

    #[test]
    fn format_results_error_with_empty_output_no_leading_newline() {
        let msg = format_results(&[ToolExecutionResult {
            tool_result: crate::agent::ToolResult::denied("access denied"),
            name: "shell".into(),
            tool_call_id: Some("tc-3".into()),
        }]);
        match msg {
            ConversationMessage::ToolResults(results) => {
                assert_eq!(results[0].content, "access denied");
                assert!(!results[0].content.starts_with('\n'));
            }
            _ => panic!("expected ToolResults variant"),
        }
    }

    // -- reasoning_content pass-through tests --

    #[test]
    fn to_provider_messages_native_includes_reasoning_content() {
        use super::super::history::to_provider_messages_native;
        use crate::provider::traits::ToolCall;
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
            reasoning_content: Some("thinking step".into()),
        }];

        let messages = to_provider_messages_native(&history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "assistant");

        let payload: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert_eq!(payload["reasoning_content"].as_str(), Some("thinking step"));
        assert_eq!(payload["content"].as_str(), Some("answer"));
        assert!(payload["tool_calls"].is_array());
    }

    #[test]
    fn to_provider_messages_native_omits_reasoning_content_when_none() {
        use super::super::history::to_provider_messages_native;
        use crate::provider::traits::ToolCall;
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
            reasoning_content: None,
        }];

        let messages = to_provider_messages_native(&history);
        assert_eq!(messages.len(), 1);

        let payload: serde_json::Value = serde_json::from_str(&messages[0].content).unwrap();
        assert!(payload.get("reasoning_content").is_none());
    }
}
