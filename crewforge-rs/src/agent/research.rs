use std::time::Instant;

use crate::agent::Tool;
use crate::provider::traits::{ChatMessage, ChatRequest, Provider, ToolSpec};

use super::dispatch::{self, ParsedToolCall};
use super::scrub::scrub_credentials;

/// Configuration for the research phase.
#[derive(Debug, Clone)]
pub struct ResearchConfig {
    pub trigger: ResearchTrigger,
    pub max_iterations: usize,
}

impl Default for ResearchConfig {
    fn default() -> Self {
        Self {
            trigger: ResearchTrigger::Question,
            max_iterations: 3,
        }
    }
}

/// When to trigger the research phase.
#[derive(Debug, Clone)]
pub enum ResearchTrigger {
    /// Never run research.
    Never,
    /// Always run research before responding.
    Always,
    /// Run research when the message contains a question mark.
    Question,
    /// Run research when the message contains any of these keywords.
    Keywords(Vec<String>),
}

/// Result of the research phase.
#[derive(Debug, Clone)]
pub struct ResearchResult {
    /// Gathered context to inject into the conversation.
    pub context: String,
    /// Number of tool calls made during research.
    pub tool_call_count: usize,
    /// Wall-clock duration of the research phase.
    pub duration: std::time::Duration,
}

const RESEARCH_SYSTEM_PROMPT: &str = "\
You are in RESEARCH MODE. Your goal is to gather facts and information \
that will help answer the user's question. Use the available tools to \
search the web, read files, or look up information.

Rules:
- Focus on gathering relevant facts, not on crafting a response.
- Use tools to verify claims and find supporting data.
- Keep research focused — do not go on tangents.
- When you have gathered enough information, respond with your findings \
  as a concise summary. Do NOT include [RESEARCH COMPLETE] markers.
- If no tools are needed, respond with an empty summary.";

/// Check if the research phase should trigger for a given message.
pub fn should_trigger(config: &ResearchConfig, message: &str) -> bool {
    match &config.trigger {
        ResearchTrigger::Never => false,
        ResearchTrigger::Always => true,
        ResearchTrigger::Question => message.contains('?'),
        ResearchTrigger::Keywords(keywords) => {
            let msg_lower = message.to_lowercase();
            keywords.iter().any(|kw| msg_lower.contains(&kw.to_lowercase()))
        }
    }
}

/// Run the research phase: a separate LLM + tools loop to gather information
/// before the main response.
pub async fn run_research_phase(
    config: &ResearchConfig,
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    user_message: &str,
    model: &str,
    temperature: f64,
) -> anyhow::Result<ResearchResult> {
    let start = Instant::now();
    let tool_specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();

    let mut messages = vec![
        ChatMessage::system(RESEARCH_SYSTEM_PROMPT),
        ChatMessage::user(format!(
            "Research the following to prepare a thorough answer:\n\n{user_message}"
        )),
    ];

    let mut total_tool_calls = 0;

    for _ in 0..config.max_iterations {
        let request = ChatRequest {
            messages: &messages,
            tools: if tool_specs.is_empty() {
                None
            } else {
                Some(&tool_specs)
            },
        };

        let response = provider.chat(request, model, temperature).await?;
        let (text, parsed_calls) = dispatch::parse_response(&response);

        if parsed_calls.is_empty() {
            // No tool calls — research is done, text contains findings.
            return Ok(ResearchResult {
                context: text,
                tool_call_count: total_tool_calls,
                duration: start.elapsed(),
            });
        }

        // Store assistant's tool call turn.
        if !text.is_empty() {
            messages.push(ChatMessage::assistant(text));
        }

        // Execute tool calls sequentially (research doesn't need parallelism).
        for call in &parsed_calls {
            total_tool_calls += 1;
            let result = execute_research_tool(tools, call).await;
            let output = scrub_credentials(&truncate_research_output(&result));
            messages.push(ChatMessage::user(format!("Tool result ({}):\n{output}", call.name)));
        }
    }

    // Max iterations reached — collect whatever context we have.
    let context = messages
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(ResearchResult {
        context,
        tool_call_count: total_tool_calls,
        duration: start.elapsed(),
    })
}

async fn execute_research_tool(tools: &[Box<dyn Tool>], call: &ParsedToolCall) -> String {
    let tool = tools.iter().find(|t| t.name() == call.name);
    match tool {
        Some(t) => match t.execute(call.arguments.clone()).await {
            Ok(result) => {
                if let Some(err) = &result.error {
                    format!("Error: {err}")
                } else {
                    result.output
                }
            }
            Err(e) => format!("Tool execution error: {e}"),
        },
        None => format!("Unknown tool: {}", call.name),
    }
}

/// Truncate research tool output to keep context manageable.
fn truncate_research_output(output: &str) -> String {
    const MAX_CHARS: usize = 8_000;
    if output.len() <= MAX_CHARS {
        return output.to_string();
    }
    let mut end = MAX_CHARS;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = output[..end].to_string();
    truncated.push_str("\n[... truncated for research context]");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_never() {
        let config = ResearchConfig {
            trigger: ResearchTrigger::Never,
            ..Default::default()
        };
        assert!(!should_trigger(&config, "What is Rust?"));
    }

    #[test]
    fn trigger_always() {
        let config = ResearchConfig {
            trigger: ResearchTrigger::Always,
            ..Default::default()
        };
        assert!(should_trigger(&config, "hello"));
    }

    #[test]
    fn trigger_question() {
        let config = ResearchConfig::default(); // Question trigger
        assert!(should_trigger(&config, "What is Rust?"));
        assert!(!should_trigger(&config, "Tell me about Rust"));
    }

    #[test]
    fn trigger_keywords() {
        let config = ResearchConfig {
            trigger: ResearchTrigger::Keywords(vec!["search".into(), "lookup".into()]),
            ..Default::default()
        };
        assert!(should_trigger(&config, "Please search for Rust docs"));
        assert!(should_trigger(&config, "LOOKUP this error"));
        assert!(!should_trigger(&config, "Fix the bug"));
    }

    #[test]
    fn truncate_short_passthrough() {
        assert_eq!(truncate_research_output("short"), "short");
    }

    #[test]
    fn truncate_long_output() {
        let long = "x".repeat(10_000);
        let result = truncate_research_output(&long);
        assert!(result.len() < long.len());
        assert!(result.contains("[... truncated"));
    }
}
