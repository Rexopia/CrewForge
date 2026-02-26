use crate::provider::traits::{
    ChatMessage, ConversationMessage, Provider, ToolResultMessage,
};
use anyhow::Result;
use std::fmt::Write;

pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 50;
const COMPACTION_KEEP_RECENT_MESSAGES: usize = 20;
const COMPACTION_MAX_SOURCE_CHARS: usize = 12_000;
const COMPACTION_MAX_SUMMARY_CHARS: usize = 2_000;

/// Convert ConversationMessage history to flat ChatMessage list for provider calls.
///
/// NativeDispatcher format:
/// - `AssistantToolCalls` → JSON assistant message with tool_calls payload
/// - `ToolResults` → one tool message per result
pub fn to_provider_messages_native(history: &[ConversationMessage]) -> Vec<ChatMessage> {
    history
        .iter()
        .flat_map(|msg| match msg {
            ConversationMessage::Chat(chat) => vec![chat.clone()],
            ConversationMessage::AssistantToolCalls {
                text,
                tool_calls,
                reasoning_content,
            } => {
                let mut payload = serde_json::json!({
                    "content": text,
                    "tool_calls": tool_calls,
                });
                if let Some(rc) = reasoning_content {
                    payload["reasoning_content"] = serde_json::json!(rc);
                }
                vec![ChatMessage::assistant(payload.to_string())]
            }
            ConversationMessage::ToolResults(results) => results
                .iter()
                .map(|result| {
                    ChatMessage::tool(
                        serde_json::json!({
                            "tool_call_id": result.tool_call_id,
                            "content": result.content,
                        })
                        .to_string(),
                    )
                })
                .collect(),
        })
        .collect()
}

/// Convert ConversationMessage history to flat ChatMessage list for XML/prompt-guided providers.
///
/// XmlDispatcher format:
/// - `AssistantToolCalls` → plain assistant text message (reasoning_content dropped)
/// - `ToolResults` → single user message with XML tool_result tags
pub fn to_provider_messages_xml(history: &[ConversationMessage]) -> Vec<ChatMessage> {
    history
        .iter()
        .flat_map(|msg| match msg {
            ConversationMessage::Chat(chat) => vec![chat.clone()],
            ConversationMessage::AssistantToolCalls { text, .. } => {
                vec![ChatMessage::assistant(text.clone().unwrap_or_default())]
            }
            ConversationMessage::ToolResults(results) => {
                let mut content = String::new();
                for result in results {
                    let _ = writeln!(
                        content,
                        "<tool_result id=\"{}\">\n{}\n</tool_result>",
                        result.tool_call_id, result.content
                    );
                }
                vec![ChatMessage::user(format!("[Tool results]\n{content}"))]
            }
        })
        .collect()
}

/// Trim history to at most `max_messages` non-system ConversationMessages.
///
/// Preserves the system prompt (first `Chat` with role=system) at index 0.
/// When the limit is exceeded, drains oldest non-system messages first.
pub fn trim_history(history: &mut Vec<ConversationMessage>, max_messages: usize) {
    let has_system = matches!(
        history.first(),
        Some(ConversationMessage::Chat(m)) if m.role == "system"
    );
    let non_system_count = if has_system {
        history.len().saturating_sub(1)
    } else {
        history.len()
    };

    if non_system_count <= max_messages {
        return;
    }

    let start = if has_system { 1 } else { 0 };
    let to_remove = non_system_count - max_messages;
    history.drain(start..start + to_remove);
}

/// Build a plain-text transcript of ConversationMessages for compaction summarization.
fn build_compaction_transcript(messages: &[ConversationMessage]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        match msg {
            ConversationMessage::Chat(chat) => {
                let role = chat.role.to_uppercase();
                let _ = writeln!(transcript, "{role}: {}", chat.content.trim());
            }
            ConversationMessage::AssistantToolCalls { text, tool_calls, .. } => {
                let text_str = text.as_deref().unwrap_or("").trim();
                let _ = writeln!(transcript, "ASSISTANT: {text_str}");
                for tc in tool_calls {
                    let _ = writeln!(transcript, "TOOL_CALL: {} {}", tc.name, tc.arguments.trim());
                }
            }
            ConversationMessage::ToolResults(results) => {
                for r in results {
                    let _ = writeln!(transcript, "TOOL_RESULT: {}", r.content.trim());
                }
            }
        }
    }

    if transcript.chars().count() > COMPACTION_MAX_SOURCE_CHARS {
        truncate_chars(&transcript, COMPACTION_MAX_SOURCE_CHARS)
    } else {
        transcript
    }
}

/// Replace older history messages with a single compaction summary when history
/// exceeds `max_messages` non-system messages.
///
/// Uses the provider to generate a bullet-point summary of older messages.
/// Falls back to local truncation if the provider call fails.
///
/// Returns `true` if compaction was performed.
pub async fn auto_compact_history(
    history: &mut Vec<ConversationMessage>,
    provider: &dyn Provider,
    model: &str,
    max_messages: usize,
) -> Result<bool> {
    let has_system = matches!(
        history.first(),
        Some(ConversationMessage::Chat(m)) if m.role == "system"
    );
    let non_system_count = if has_system {
        history.len().saturating_sub(1)
    } else {
        history.len()
    };

    if non_system_count <= max_messages {
        return Ok(false);
    }

    let start = if has_system { 1 } else { 0 };
    let keep_recent = COMPACTION_KEEP_RECENT_MESSAGES.min(non_system_count);
    let compact_count = non_system_count.saturating_sub(keep_recent);
    if compact_count == 0 {
        return Ok(false);
    }

    let compact_end = start + compact_count;
    let to_compact: Vec<ConversationMessage> = history[start..compact_end].to_vec();
    let transcript = build_compaction_transcript(&to_compact);

    let summarizer_system = "You are a conversation compaction engine. \
        Summarize older chat history into concise context for future turns. \
        Preserve: user preferences, commitments, decisions, unresolved tasks, key facts. \
        Omit: filler, repeated chit-chat, verbose tool logs. \
        Output plain text bullet points only.";

    let summarizer_user = format!(
        "Summarize the following conversation history for context preservation. \
        Keep it short (max 12 bullet points).\n\n{}",
        transcript
    );

    let summary_raw = provider
        .chat_with_system(Some(summarizer_system), &summarizer_user, model, 0.2)
        .await
        .unwrap_or_else(|_| truncate_chars(&transcript, COMPACTION_MAX_SUMMARY_CHARS));

    let summary = truncate_chars(&summary_raw, COMPACTION_MAX_SUMMARY_CHARS);

    // Replace the compacted range with a single summary assistant message.
    let summary_msg = ConversationMessage::Chat(ChatMessage::assistant(format!(
        "[Compaction summary]\n{}",
        summary.trim()
    )));
    history.splice(start..compact_end, std::iter::once(summary_msg));

    Ok(true)
}

/// Truncate a string to at most `max_chars` Unicode scalar values, appending "…" if truncated.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::traits::{ToolCall, ToolResultMessage};

    fn make_chat(role: &str, content: &str) -> ConversationMessage {
        ConversationMessage::Chat(ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        })
    }

    #[test]
    fn to_provider_messages_native_chat_passthrough() {
        let history = vec![
            make_chat("system", "sys"),
            make_chat("user", "hello"),
            make_chat("assistant", "hi"),
        ];
        let msgs = to_provider_messages_native(&history);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
    }

    #[test]
    fn to_provider_messages_native_assistant_tool_calls() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("thinking".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }],
            reasoning_content: Some("step1".into()),
        }];
        let msgs = to_provider_messages_native(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        let payload: serde_json::Value = serde_json::from_str(&msgs[0].content).unwrap();
        assert_eq!(payload["content"].as_str(), Some("thinking"));
        assert_eq!(payload["reasoning_content"].as_str(), Some("step1"));
        assert!(payload["tool_calls"].is_array());
    }

    #[test]
    fn to_provider_messages_native_tool_results() {
        let history = vec![ConversationMessage::ToolResults(vec![
            ToolResultMessage {
                tool_call_id: "tc1".into(),
                content: "output".into(),
            },
        ])];
        let msgs = to_provider_messages_native(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "tool");
        let payload: serde_json::Value = serde_json::from_str(&msgs[0].content).unwrap();
        assert_eq!(payload["tool_call_id"].as_str(), Some("tc1"));
        assert_eq!(payload["content"].as_str(), Some("output"));
    }

    #[test]
    fn to_provider_messages_xml_assistant_tool_calls_text_only() {
        let history = vec![ConversationMessage::AssistantToolCalls {
            text: Some("answer".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
            reasoning_content: Some("ignored".into()),
        }];
        let msgs = to_provider_messages_xml(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert_eq!(msgs[0].content, "answer");
    }

    #[test]
    fn to_provider_messages_xml_tool_results_as_user() {
        let history = vec![ConversationMessage::ToolResults(vec![
            ToolResultMessage {
                tool_call_id: "tc1".into(),
                content: "result_data".into(),
            },
        ])];
        let msgs = to_provider_messages_xml(&history);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
        assert!(msgs[0].content.contains("[Tool results]"));
        assert!(msgs[0].content.contains("result_data"));
    }

    #[test]
    fn trim_history_preserves_system_and_recent() {
        let mut history = vec![
            make_chat("system", "system prompt"),
            make_chat("user", "msg1"),
            make_chat("assistant", "resp1"),
            make_chat("user", "msg2"),
            make_chat("assistant", "resp2"),
            make_chat("user", "msg3"),
        ];
        trim_history(&mut history, 3);
        // system + 3 most recent non-system
        assert_eq!(history.len(), 4);
        // system is still first
        assert!(
            matches!(&history[0], ConversationMessage::Chat(m) if m.role == "system")
        );
        // most recent messages are preserved
        assert!(
            matches!(&history[history.len()-1], ConversationMessage::Chat(m) if m.content == "msg3")
        );
    }

    #[test]
    fn trim_history_no_system_message() {
        let mut history = vec![
            make_chat("user", "a"),
            make_chat("assistant", "b"),
            make_chat("user", "c"),
            make_chat("assistant", "d"),
        ];
        trim_history(&mut history, 2);
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn trim_history_within_limit_no_change() {
        let mut history = vec![
            make_chat("system", "sys"),
            make_chat("user", "a"),
            make_chat("assistant", "b"),
        ];
        trim_history(&mut history, 10);
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn truncate_chars_short_string() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_exact_limit() {
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncate_chars_over_limit() {
        let result = truncate_chars("hello world", 6);
        assert!(result.len() <= 10); // "hello" + "…" encoded in UTF-8
        assert!(result.ends_with('…'));
    }
}
