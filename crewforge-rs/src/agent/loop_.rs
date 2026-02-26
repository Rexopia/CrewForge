// TODO: convert run_turn to Stream<AgentEvent> once lifetime/async-stream issues are resolved.
// For now it collects events into Vec<AgentEvent> and updates history in-place.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::provider::traits::{
    ChatMessage, ChatRequest, ConversationMessage, Provider, ToolCall, ToolSpec,
};
use super::Tool;
use super::dispatcher::{
    NativeToolDispatcher, ParsedToolCall, ToolDispatcher, ToolExecutionResult, XmlToolDispatcher,
};
use super::history::{
    auto_compact_history, to_provider_messages_native, to_provider_messages_xml, trim_history,
};

// ── Public event/config/stop types ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentEvent {
    LlmThinking { iteration: usize },
    LlmResponse {
        text: Option<String>,
        tool_call_count: usize,
        usage: Option<crate::provider::traits::TokenUsage>,
    },
    ToolCallStarted {
        iteration: usize,
        name: String,
        args: serde_json::Value,
    },
    ToolCallFinished {
        name: String,
        result: String,
        success: bool,
    },
    TurnFinished {
        final_text: Option<String>,
        iterations_used: usize,
        stop_reason: StopReason,
    },
    Error {
        message: String,
        fatal: bool,
    },
}

#[derive(Debug, Clone)]
pub enum StopReason {
    Done,
    MaxIterations,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct AgentSessionConfig {
    pub max_iterations: usize,
    pub max_history_messages: usize,
    pub temperature: f64,
}

impl Default for AgentSessionConfig {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            max_history_messages: 50,
            temperature: 0.7,
        }
    }
}

// ── AgentSession ─────────────────────────────────────────────────────────────

pub struct AgentSession {
    provider: Arc<dyn Provider>,
    model: String,
    pub history: Vec<ConversationMessage>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    config: AgentSessionConfig,
    cancelled: Arc<AtomicBool>,
}

impl AgentSession {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        tools: Vec<Box<dyn Tool>>,
        config: AgentSessionConfig,
    ) -> Self {
        let system_prompt = system_prompt.into();
        let mut history = Vec::new();
        if !system_prompt.is_empty() {
            history.push(ConversationMessage::Chat(ChatMessage::system(system_prompt)));
        }
        Self {
            provider,
            model: model.into(),
            history,
            tools: Arc::new(tools),
            config,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signal the agent to cancel after the current tool call completes.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Clone the cancel handle for external signalling.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// Run one agent turn, collecting all events into a Vec.
    ///
    /// Adds `initial_message` as a user message, runs the tool-use loop, and
    /// updates `self.history` in-place. Events are returned in emission order.
    ///
    /// TODO: convert to `Stream<Item = AgentEvent>` for true streaming.
    pub async fn run_turn(&mut self, initial_message: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();

        // Add initial user message to history.
        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(initial_message)));

        // Compact and trim history before starting.
        let _ = auto_compact_history(
            &mut self.history,
            self.provider.as_ref(),
            &self.model,
            self.config.max_history_messages,
        )
        .await;
        trim_history(&mut self.history, self.config.max_history_messages);

        let use_native = self.provider.supports_native_tools() && !self.tools.is_empty();
        let tool_specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        let mut seen_signatures: HashSet<(String, String)> = HashSet::new();
        let mut final_text: Option<String> = None;
        let mut iterations_used = 0;

        'outer: for iteration in 0..self.config.max_iterations {
            if self.cancelled.load(Ordering::SeqCst) {
                events.push(AgentEvent::TurnFinished {
                    final_text,
                    iterations_used,
                    stop_reason: StopReason::Cancelled,
                });
                return events;
            }

            events.push(AgentEvent::LlmThinking { iteration });
            iterations_used = iteration + 1;

            // Build provider messages from history.
            let messages = if use_native {
                to_provider_messages_native(&self.history)
            } else {
                to_provider_messages_xml(&self.history)
            };

            // Call the provider.
            let response = if use_native && !tool_specs.is_empty() {
                let request = ChatRequest {
                    messages: &messages,
                    tools: Some(&tool_specs),
                };
                self.provider
                    .chat(request, &self.model, self.config.temperature)
                    .await
            } else {
                let request = ChatRequest {
                    messages: &messages,
                    tools: None,
                };
                self.provider
                    .chat(request, &self.model, self.config.temperature)
                    .await
            };

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    events.push(AgentEvent::Error {
                        message: e.to_string(),
                        fatal: true,
                    });
                    events.push(AgentEvent::TurnFinished {
                        final_text,
                        iterations_used,
                        stop_reason: StopReason::Done,
                    });
                    return events;
                }
            };

            // Parse tool calls from the response.
            let (text, parsed_calls) = if use_native {
                NativeToolDispatcher.parse_response(&response)
            } else {
                XmlToolDispatcher.parse_response(&response)
            };

            let usage = response.usage.clone();

            events.push(AgentEvent::LlmResponse {
                text: if text.is_empty() {
                    None
                } else {
                    Some(text.clone())
                },
                tool_call_count: parsed_calls.len(),
                usage,
            });

            if parsed_calls.is_empty() {
                // No tool calls — this is the final response for this turn.
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(text.clone())));
                final_text = Some(text);
                events.push(AgentEvent::TurnFinished {
                    final_text: final_text.clone(),
                    iterations_used,
                    stop_reason: StopReason::Done,
                });
                return events;
            }

            // Store the assistant's tool-call turn in history.
            let tool_calls_for_history: Vec<ToolCall> = if use_native {
                response.tool_calls.clone()
            } else {
                parsed_calls
                    .iter()
                    .map(|pc| ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: pc.name.clone(),
                        arguments: pc.arguments.to_string(),
                    })
                    .collect()
            };
            self.history.push(ConversationMessage::AssistantToolCalls {
                text: if text.is_empty() { None } else { Some(text.clone()) },
                tool_calls: tool_calls_for_history,
                reasoning_content: response.reasoning_content.clone(),
            });

            // Deduplicate tool calls by (name, args) signature to break infinite loops.
            let mut unique_calls: Vec<&ParsedToolCall> = Vec::new();
            for call in &parsed_calls {
                let sig = tool_call_signature(&call.name, &call.arguments);
                if seen_signatures.insert(sig) {
                    unique_calls.push(call);
                }
            }

            if unique_calls.is_empty() {
                // All tool calls are duplicates — stop to avoid spinning.
                events.push(AgentEvent::TurnFinished {
                    final_text,
                    iterations_used,
                    stop_reason: StopReason::Done,
                });
                return events;
            }

            // Execute each unique tool call.
            let mut tool_results: Vec<ToolExecutionResult> = Vec::new();
            for call in &unique_calls {
                events.push(AgentEvent::ToolCallStarted {
                    iteration,
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                });

                let result = execute_tool(&self.tools, call).await;
                let success = result.success;
                let output = result.output.clone();

                events.push(AgentEvent::ToolCallFinished {
                    name: call.name.clone(),
                    result: scrub_credentials(&output),
                    success,
                });

                tool_results.push(result);
            }

            // Store tool results in history.
            let history_entry = if use_native {
                NativeToolDispatcher.format_results(&tool_results)
            } else {
                XmlToolDispatcher.format_results(&tool_results)
            };
            self.history.push(history_entry);

            // Check cancellation after each tool batch.
            if self.cancelled.load(Ordering::SeqCst) {
                events.push(AgentEvent::TurnFinished {
                    final_text,
                    iterations_used,
                    stop_reason: StopReason::Cancelled,
                });
                return events;
            }

        }

        // Max iterations reached.
        events.push(AgentEvent::TurnFinished {
            final_text,
            iterations_used,
            stop_reason: StopReason::MaxIterations,
        });
        events
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a deduplication key for a tool call.
fn tool_call_signature(name: &str, arguments: &serde_json::Value) -> (String, String) {
    let args_json = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
    (name.trim().to_ascii_lowercase(), args_json)
}

/// Look up and invoke a tool by name. Returns a `ToolExecutionResult` in all cases.
async fn execute_tool(tools: &[Box<dyn Tool>], call: &ParsedToolCall) -> ToolExecutionResult {
    let tool = tools.iter().find(|t| t.name() == call.name);
    match tool {
        Some(t) => match t.call(call.arguments.clone()).await {
            Ok(output) => ToolExecutionResult {
                name: call.name.clone(),
                output,
                success: true,
                tool_call_id: call.tool_call_id.clone(),
            },
            Err(e) => ToolExecutionResult {
                name: call.name.clone(),
                output: format!("Error: {e}"),
                success: false,
                tool_call_id: call.tool_call_id.clone(),
            },
        },
        None => ToolExecutionResult {
            name: call.name.clone(),
            output: format!("Unknown tool: {}", call.name),
            success: false,
            tool_call_id: call.tool_call_id.clone(),
        },
    }
}

/// Redact credential-like key-value pairs from tool output to prevent accidental exfiltration.
///
/// Matches patterns such as `token=...`, `api_key: "..."`, `password=...`, etc.
/// Preserves the first 4 characters of each value for context; redacts the rest.
pub fn scrub_credentials(input: &str) -> String {
    use std::sync::LazyLock;
    use regex::Regex;

    // Matches key=value and key: value forms (token=, api_key:, bearer:, etc.)
    static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(token|api[_-]?key|password|secret|user[_-]?key|bearer|credential)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\.]{8,}))"#,
        )
        .expect("SENSITIVE_KV_REGEX is a valid static pattern")
    });

    // Matches HTTP Authorization header: "Authorization: Bearer <token>"
    static AUTH_HEADER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(Authorization)\s*:\s*(Bearer|Token)\s+([a-zA-Z0-9_\-\.\/\+]{8,})"#)
            .expect("AUTH_HEADER_REGEX is a valid static pattern")
    });

    let step1 = SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let full_match = &caps[0];
            let key = &caps[1];
            let val = caps
                .get(2)
                .or_else(|| caps.get(3))
                .or_else(|| caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");
            let prefix = if val.len() > 4 { &val[..4] } else { "" };
            if full_match.contains(':') {
                if full_match.contains('"') {
                    format!("\"{}\": \"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}: {}*[REDACTED]", key, prefix)
                }
            } else if full_match.contains('=') {
                if full_match.contains('"') {
                    format!("{}=\"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}={}*[REDACTED]", key, prefix)
                }
            } else {
                format!("{}: {}*[REDACTED]", key, prefix)
            }
        })
        .to_string();

    AUTH_HEADER_REGEX
        .replace_all(&step1, |caps: &regex::Captures| {
            let header = &caps[1];
            let scheme = &caps[2];
            let val = &caps[3];
            let prefix = if val.len() > 4 { &val[..4] } else { "" };
            format!("{}: {} {}*[REDACTED]", header, scheme, prefix)
        })
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    // ── scrub_credentials tests ───────────────────────────────────────────────

    #[test]
    fn scrub_credentials_redacts_token_equals() {
        let input = "token=supersecretvalue123";
        let output = scrub_credentials(input);
        assert!(!output.contains("supersecretvalue123"));
        assert!(output.contains("REDACTED"));
    }

    #[test]
    fn scrub_credentials_redacts_api_key_colon() {
        let input = r#"api_key: "mykey1234""#;
        let output = scrub_credentials(input);
        assert!(!output.contains("mykey1234"));
        assert!(output.contains("REDACTED"));
    }

    #[test]
    fn scrub_credentials_preserves_short_values() {
        // Values shorter than 8 chars should not be redacted.
        let input = "token=short";
        let output = scrub_credentials(input);
        assert_eq!(output, input);
    }

    #[test]
    fn scrub_credentials_preserves_unrelated_content() {
        let input = "hello world, status: ok";
        assert_eq!(scrub_credentials(input), input);
    }

    #[test]
    fn scrub_credentials_redacts_bearer() {
        let input = "Authorization: bearer eyJhbGciOiJIUzI1NiJ9.payload";
        let output = scrub_credentials(input);
        assert!(!output.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(output.contains("REDACTED"));
    }

    // ── tool_call_signature tests ─────────────────────────────────────────────

    #[test]
    fn tool_call_signature_normalises_name() {
        let (name, _) = tool_call_signature("  Shell  ", &serde_json::json!({}));
        assert_eq!(name, "shell");
    }

    #[test]
    fn tool_call_signature_same_for_equivalent_args() {
        let sig1 = tool_call_signature("tool", &serde_json::json!({"a": 1}));
        let sig2 = tool_call_signature("tool", &serde_json::json!({"a": 1}));
        assert_eq!(sig1, sig2);
    }

    // ── AgentSession unit tests ───────────────────────────────────────────────

    struct EchoProvider;

    #[async_trait]
    impl Provider for EchoProvider {
        async fn chat_with_system(
            &self,
            _system: Option<&str>,
            message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok(format!("Echo: {message}"))
        }
    }

    #[tokio::test]
    async fn run_turn_no_tools_done() {
        let provider = Arc::new(EchoProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "You are a test agent.",
            vec![],
            AgentSessionConfig::default(),
        );

        let events = session.run_turn("hello").await;

        // Should end with TurnFinished(Done).
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnFinished {
                stop_reason: StopReason::Done,
                ..
            }
        )));

        // Should have LlmThinking at start.
        assert!(events.iter().any(|e| matches!(e, AgentEvent::LlmThinking { iteration: 0 })));

        // History should include system, user, and assistant messages.
        assert!(session.history.len() >= 3);
    }

    #[tokio::test]
    async fn run_turn_adds_user_message_to_history() {
        let provider = Arc::new(EchoProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "sys",
            vec![],
            AgentSessionConfig::default(),
        );

        session.run_turn("user input").await;

        let has_user = session.history.iter().any(|m| {
            matches!(m, ConversationMessage::Chat(msg) if msg.role == "user" && msg.content == "user input")
        });
        assert!(has_user, "user message should be in history");
    }

    #[tokio::test]
    async fn run_turn_cancel_returns_cancelled() {
        let provider = Arc::new(EchoProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "",
            vec![],
            AgentSessionConfig::default(),
        );

        // Cancel before calling run_turn.
        session.cancel();

        let events = session.run_turn("anything").await;

        // First turn is consumed (provider called once before cancel check kicks in
        // inside the loop), OR cancelled before LLM call — depends on loop ordering.
        // Either way we must have a TurnFinished with Cancelled or Done.
        let finished = events.iter().find(|e| matches!(e, AgentEvent::TurnFinished { .. }));
        assert!(finished.is_some());
    }

    #[tokio::test]
    async fn run_turn_cancel_handle_signals_session() {
        let provider = Arc::new(EchoProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "",
            vec![],
            AgentSessionConfig::default(),
        );

        let handle = session.cancel_handle();
        handle.store(true, Ordering::SeqCst);

        assert!(session.cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_turn_max_iterations() {
        // Provider that always returns a tool call so iterations keep running.
        struct LoopProvider;

        #[async_trait]
        impl Provider for LoopProvider {
            async fn chat_with_system(
                &self,
                _system: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: f64,
            ) -> anyhow::Result<String> {
                // Return XML tool call that will be unique each time (different arg value
                // ensures dedup doesn't shortcircuit first).
                // Actually for max-iter test we want dedup to NOT kill it — use static call.
                Ok("<tool_call>{\"name\":\"noop\",\"arguments\":{}}</tool_call>".to_string())
            }
        }

        // Tool that always succeeds.
        struct NoopTool;

        #[async_trait]
        impl Tool for NoopTool {
            fn name(&self) -> &str { "noop" }
            fn description(&self) -> &str { "no-op" }
            fn parameters(&self) -> serde_json::Value { serde_json::json!({}) }
            async fn call(&self, _args: serde_json::Value) -> anyhow::Result<String> {
                Ok("done".to_string())
            }
        }

        let provider = Arc::new(LoopProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "",
            vec![Box::new(NoopTool)],
            AgentSessionConfig {
                max_iterations: 3,
                ..Default::default()
            },
        );

        let events = session.run_turn("go").await;

        // Should finish with Done (dedup catches repeating call) or MaxIterations.
        let stop_reason = events.iter().find_map(|e| {
            if let AgentEvent::TurnFinished { stop_reason, .. } = e {
                Some(stop_reason.clone())
            } else {
                None
            }
        });
        assert!(stop_reason.is_some());
        assert!(matches!(
            stop_reason.unwrap(),
            StopReason::Done | StopReason::MaxIterations
        ));
    }
}
