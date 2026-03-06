// TODO: convert run_turn to Stream<AgentEvent> once lifetime/async-stream issues are resolved.
// For now it collects events into Vec<AgentEvent> and updates history in-place.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::Tool;
use super::context::skills::{Skill, load_skills};
use super::context::{PromptContext, SystemPromptBuilder};
use super::dispatch::{self, ParsedToolCall, ToolExecutionResult};
use super::history::{auto_compact_history, to_provider_messages_native, trim_history};
use super::research::{self, ResearchConfig};
use super::sandbox::SecurityPolicy;
use super::scrub::scrub_credentials;
use crate::provider::traits::{ChatMessage, ChatRequest, ConversationMessage, Provider, ToolSpec};

/// Max tool output bytes stored in history per single tool call.
/// ~32KB keeps context manageable across models (~8k tokens).
const MAX_TOOL_OUTPUT_CONTEXT_BYTES: usize = 32_768;

// ── Public event/config/stop types ───────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AgentEvent {
    LlmThinking {
        iteration: usize,
    },
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
    ResearchComplete {
        context_length: usize,
        tool_call_count: usize,
        duration_ms: u64,
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
            max_iterations: 50,
            max_history_messages: 200,
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
    security: Arc<SecurityPolicy>,
    research_config: ResearchConfig,
    skills: Vec<Skill>,
    base_system_prompt: String,
    prompt_builder: SystemPromptBuilder,
}

impl AgentSession {
    pub fn new(
        provider: Arc<dyn Provider>,
        model: impl Into<String>,
        base_system_prompt: impl Into<String>,
        tools: Vec<Box<dyn Tool>>,
        config: AgentSessionConfig,
        security: Arc<SecurityPolicy>,
    ) -> Self {
        let workspace = &security.workspace_dir;
        let skills = load_skills(workspace);

        Self {
            provider,
            model: model.into(),
            history: Vec::new(),
            tools: Arc::new(tools),
            config,
            cancelled: Arc::new(AtomicBool::new(false)),
            security,
            research_config: ResearchConfig::default(),
            skills,
            base_system_prompt: base_system_prompt.into(),
            prompt_builder: SystemPromptBuilder::with_defaults(),
        }
    }

    /// Replace the default research config.
    pub fn with_research_config(mut self, config: ResearchConfig) -> Self {
        self.research_config = config;
        self
    }

    /// Replace the default prompt builder.
    pub fn with_prompt_builder(mut self, builder: SystemPromptBuilder) -> Self {
        self.prompt_builder = builder;
        self
    }

    /// Signal the agent to cancel after the current tool call completes.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Clone the cancel handle for external signalling.
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// Build the system prompt via context assembly.
    fn build_system_prompt(&self) -> String {
        let ctx = PromptContext {
            workspace_dir: &self.security.workspace_dir,
            model_name: &self.model,
            tools: &self.tools,
            skills: &self.skills,
            security: &self.security,
            base_system_prompt: &self.base_system_prompt,
        };

        self.prompt_builder.build(&ctx).unwrap_or_else(|_| {
            self.base_system_prompt.clone()
        })
    }

    /// Run one agent turn, collecting all events into a Vec.
    ///
    /// Adds `initial_message` as a user message, runs the tool-use loop, and
    /// updates `self.history` in-place. Events are returned in emission order.
    ///
    /// TODO: convert to `Stream<Item = AgentEvent>` for true streaming.
    pub async fn run_turn(&mut self, initial_message: &str) -> Vec<AgentEvent> {
        let mut events = Vec::new();

        // Build system prompt via context assembly (identity, tools, skills, etc.)
        let system_prompt = self.build_system_prompt();

        // Ensure system prompt is at the front of history (replace if exists).
        match self.history.first() {
            Some(ConversationMessage::Chat(m)) if m.role == "system" => {
                self.history[0] =
                    ConversationMessage::Chat(ChatMessage::system(system_prompt));
            }
            _ => {
                self.history.insert(
                    0,
                    ConversationMessage::Chat(ChatMessage::system(system_prompt)),
                );
            }
        }

        // Add initial user message to history.
        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(
                initial_message,
            )));

        // Compact and trim history before starting.
        let _ = auto_compact_history(
            &mut self.history,
            self.provider.as_ref(),
            &self.model,
            self.config.max_history_messages,
        )
        .await;
        trim_history(&mut self.history, self.config.max_history_messages);

        // Run research phase if triggered.
        if research::should_trigger(&self.research_config, initial_message) {
            match research::run_research_phase(
                &self.research_config,
                self.provider.as_ref(),
                &self.tools,
                initial_message,
                &self.model,
                self.config.temperature,
            )
            .await
            {
                Ok(result) if !result.context.is_empty() => {
                    events.push(AgentEvent::ResearchComplete {
                        context_length: result.context.len(),
                        tool_call_count: result.tool_call_count,
                        duration_ms: result.duration.as_millis() as u64,
                    });
                    // Inject research findings as context for the main loop.
                    self.history
                        .push(ConversationMessage::Chat(ChatMessage::assistant(format!(
                            "[Research context]\n{}",
                            result.context
                        ))));
                }
                Ok(_) => {} // Empty research — skip.
                Err(e) => {
                    tracing::warn!("Research phase failed: {e}");
                }
            }
        }

        let tool_specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        let mut seen_signatures: HashSet<(String, String)> = HashSet::new();
        let mut final_text: Option<String> = None;
        let mut iterations_used = 0;

        for iteration in 0..self.config.max_iterations {
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
            let messages = to_provider_messages_native(&self.history);

            // Call the provider.
            let request = ChatRequest {
                messages: &messages,
                tools: if tool_specs.is_empty() {
                    None
                } else {
                    Some(&tool_specs)
                },
            };
            let response = self
                .provider
                .chat(request, &self.model, self.config.temperature)
                .await;

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
            let (text, parsed_calls) = dispatch::parse_response(&response);

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
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        text.clone(),
                    )));
                final_text = Some(text);
                events.push(AgentEvent::TurnFinished {
                    final_text: final_text.clone(),
                    iterations_used,
                    stop_reason: StopReason::Done,
                });
                return events;
            }

            // Store the assistant's tool-call turn in history.
            let tool_calls_for_history = response.tool_calls.clone();
            self.history.push(ConversationMessage::AssistantToolCalls {
                text: if text.is_empty() {
                    None
                } else {
                    Some(text.clone())
                },
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

            // Execute tool calls — parallel when possible, sequential otherwise.
            let tool_results = execute_tool_calls(&self.tools, &unique_calls).await;

            for (call, result) in unique_calls.iter().zip(tool_results.iter()) {
                let success = result.tool_result.success;
                let output = if let Some(ref err) = result.tool_result.error {
                    err.clone()
                } else {
                    result.tool_result.output.clone()
                };

                events.push(AgentEvent::ToolCallStarted {
                    iteration,
                    name: call.name.clone(),
                    args: call.arguments.clone(),
                });
                events.push(AgentEvent::ToolCallFinished {
                    name: call.name.clone(),
                    result: scrub_credentials(&truncate_tool_output(&output)),
                    success,
                });
            }

            // Store tool results in history (with context-aware truncation).
            let truncated_results: Vec<ToolExecutionResult> = tool_results
                .into_iter()
                .map(|mut r| {
                    r.tool_result.output = truncate_tool_output(&r.tool_result.output);
                    r
                })
                .collect();
            let history_entry = dispatch::format_results(&truncated_results);
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

/// Truncate tool output to keep context manageable.
fn truncate_tool_output(output: &str) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_CONTEXT_BYTES {
        return output.to_string();
    }
    let mut end = MAX_TOOL_OUTPUT_CONTEXT_BYTES;
    while end > 0 && !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = output[..end].to_string();
    truncated.push_str("\n... [output truncated at 32KB for context management]");
    truncated
}

/// Execute tool calls, choosing parallel or sequential based on mutability.
///
/// When multiple calls are present and none require approval-gated side effects,
/// run them concurrently for lower wall-clock latency.
async fn execute_tool_calls(
    tools: &[Box<dyn Tool>],
    calls: &[&ParsedToolCall],
) -> Vec<ToolExecutionResult> {
    if calls.len() <= 1 {
        // Single call — just run it directly.
        let mut results = Vec::new();
        for call in calls {
            results.push(execute_one_tool(tools, call).await);
        }
        return results;
    }

    // Check if any tool is mutating — if so, run all sequentially for safety.
    let any_mutating = calls.iter().any(|call| {
        tools
            .iter()
            .find(|t| t.name() == call.name)
            .is_some_and(|t| t.is_mutating())
    });

    if any_mutating {
        let mut results = Vec::new();
        for call in calls {
            results.push(execute_one_tool(tools, call).await);
        }
        results
    } else {
        // All read-only — run in parallel.
        let futures: Vec<_> = calls
            .iter()
            .map(|call| execute_one_tool(tools, call))
            .collect();
        futures::future::join_all(futures).await
    }
}

/// Look up and invoke a tool by name. Returns a `ToolExecutionResult` in all cases.
async fn execute_one_tool(tools: &[Box<dyn Tool>], call: &ParsedToolCall) -> ToolExecutionResult {
    let tool = tools.iter().find(|t| t.name() == call.name);
    match tool {
        Some(t) => match t.execute(call.arguments.clone()).await {
            Ok(tool_result) => ToolExecutionResult {
                name: call.name.clone(),
                tool_result,
                tool_call_id: call.tool_call_id.clone(),
            },
            Err(e) => ToolExecutionResult {
                name: call.name.clone(),
                tool_result: super::ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Error: {e}")),
                },
                tool_call_id: call.tool_call_id.clone(),
            },
        },
        None => ToolExecutionResult {
            name: call.name.clone(),
            tool_result: super::ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown tool: {}", call.name)),
            },
            tool_call_id: call.tool_call_id.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
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

    // ── truncate_tool_output tests ───────────────────────────────────────────

    #[test]
    fn truncate_tool_output_short_passthrough() {
        let short = "hello world";
        assert_eq!(truncate_tool_output(short), short);
    }

    #[test]
    fn truncate_tool_output_long_truncates() {
        let long = "x".repeat(MAX_TOOL_OUTPUT_CONTEXT_BYTES + 1000);
        let result = truncate_tool_output(&long);
        assert!(result.len() < long.len());
        assert!(result.contains("[output truncated at 32KB"));
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
            test_security(),
        );

        let events = session.run_turn("hello").await;

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnFinished {
                stop_reason: StopReason::Done,
                ..
            }
        )));

        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::LlmThinking { iteration: 0 }))
        );

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
            test_security(),
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
            test_security(),
        );

        session.cancel();

        let events = session.run_turn("anything").await;

        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::TurnFinished {
                stop_reason: StopReason::Cancelled,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn run_turn_cancel_handle_signals_session() {
        let provider = Arc::new(EchoProvider);
        let session = AgentSession::new(
            provider,
            "test-model",
            "",
            vec![],
            AgentSessionConfig::default(),
            test_security(),
        );

        let handle = session.cancel_handle();
        handle.store(true, Ordering::SeqCst);

        assert!(session.cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_turn_max_iterations() {
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
                Ok(String::new())
            }

            async fn chat(
                &self,
                _request: crate::provider::traits::ChatRequest<'_>,
                _model: &str,
                _temperature: f64,
            ) -> anyhow::Result<crate::provider::traits::ChatResponse> {
                Ok(crate::provider::traits::ChatResponse {
                    text: None,
                    tool_calls: vec![crate::provider::traits::ToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "noop".into(),
                        arguments: "{}".into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                })
            }
        }

        struct NoopTool;

        #[async_trait]
        impl Tool for NoopTool {
            fn name(&self) -> &str {
                "noop"
            }
            fn description(&self) -> &str {
                "no-op"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
            ) -> anyhow::Result<crate::agent::ToolResult> {
                Ok(crate::agent::ToolResult {
                    success: true,
                    output: "done".to_string(),
                    error: None,
                })
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
            test_security(),
        );

        let events = session.run_turn("go").await;

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

    #[tokio::test]
    async fn run_turn_system_prompt_contains_context_assembly() {
        let provider = Arc::new(EchoProvider);
        let mut session = AgentSession::new(
            provider,
            "test-model",
            "Base prompt.",
            vec![],
            AgentSessionConfig::default(),
            test_security(),
        );

        session.run_turn("hello").await;

        // System prompt should be assembled with sections.
        let system = session.history.iter().find_map(|m| {
            if let ConversationMessage::Chat(msg) = m
                && msg.role == "system"
            {
                Some(msg.content.clone())
            } else {
                None
            }
        });
        let system = system.expect("should have system message");
        assert!(system.contains("Base prompt."), "should contain base prompt");
        assert!(system.contains("## Safety"), "should contain safety section");
        assert!(
            system.contains("## Workspace"),
            "should contain workspace section"
        );
    }
}
