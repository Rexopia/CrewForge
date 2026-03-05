//! Agent test observation layer — structured event trace for debugging.
//!
//! This module provides `EventLog`, a wrapper around `Vec<AgentEvent>` with
//! assertion helpers designed for maximum observability. On any assertion failure,
//! the full event trace is printed so Claude (or any developer) can see exactly
//! what happened.
//!
//! No mocks. All tests use real providers and real tools.

use super::{AgentEvent, StopReason};

/// Wrapper around `Vec<AgentEvent>` with assertion helpers.
///
/// On assertion failure, prints the full event trace for debugging.
pub struct EventLog(pub Vec<AgentEvent>);

impl EventLog {
    /// Pretty-print the entire event trace.
    pub fn dump(&self) -> String {
        let mut out = String::from("\n=== Event Trace ===\n");
        for (i, event) in self.0.iter().enumerate() {
            out.push_str(&format!("[{i:2}] {}\n", format_event(event)));
        }
        out.push_str("===================\n");
        out
    }

    /// Total number of events.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Assert the turn finished with a specific stop reason.
    pub fn assert_stop_reason(&self, expected: &str) {
        let finished = self.0.iter().find_map(|e| {
            if let AgentEvent::TurnFinished { stop_reason, .. } = e {
                Some(stop_reason)
            } else {
                None
            }
        });
        let reason_str = match finished {
            Some(StopReason::Done) => "done",
            Some(StopReason::MaxIterations) => "max_iterations",
            Some(StopReason::Cancelled) => "cancelled",
            None => panic!("No TurnFinished event found.{}", self.dump()),
        };
        assert_eq!(
            reason_str, expected,
            "Expected stop_reason={expected}, got {reason_str}.{}",
            self.dump()
        );
    }

    /// Get the final text from TurnFinished.
    pub fn final_text(&self) -> Option<String> {
        self.0.iter().find_map(|e| {
            if let AgentEvent::TurnFinished { final_text, .. } = e {
                final_text.clone()
            } else {
                None
            }
        })
    }

    /// Assert the final text contains a substring.
    pub fn assert_final_text_contains(&self, substring: &str) {
        let text = self.final_text().unwrap_or_default();
        assert!(
            text.contains(substring),
            "Expected final_text to contain {substring:?}, got {text:?}.{}",
            self.dump()
        );
    }

    /// Assert we got a non-empty final text.
    pub fn assert_has_final_text(&self) {
        let text = self.final_text();
        assert!(
            text.as_ref().is_some_and(|t| !t.trim().is_empty()),
            "Expected non-empty final_text, got {text:?}.{}",
            self.dump()
        );
    }

    /// Count events matching a predicate.
    pub fn count<F: Fn(&AgentEvent) -> bool>(&self, f: F) -> usize {
        self.0.iter().filter(|e| f(e)).count()
    }

    /// Count LlmThinking events (= number of LLM round trips).
    pub fn llm_rounds(&self) -> usize {
        self.count(|e| matches!(e, AgentEvent::LlmThinking { .. }))
    }

    /// Count ToolCallStarted events.
    pub fn tool_calls(&self) -> usize {
        self.count(|e| matches!(e, AgentEvent::ToolCallStarted { .. }))
    }

    /// Count ToolCallFinished with success=true.
    pub fn tool_successes(&self) -> usize {
        self.count(|e| matches!(e, AgentEvent::ToolCallFinished { success: true, .. }))
    }

    /// Count ToolCallFinished with success=false.
    pub fn tool_failures(&self) -> usize {
        self.count(|e| matches!(e, AgentEvent::ToolCallFinished { success: false, .. }))
    }

    /// Count Error events.
    pub fn errors(&self) -> usize {
        self.count(|e| matches!(e, AgentEvent::Error { .. }))
    }

    /// Assert no errors occurred.
    pub fn assert_no_errors(&self) {
        let error_count = self.errors();
        assert!(
            error_count == 0,
            "Expected no errors, found {error_count}.{}",
            self.dump()
        );
    }

    /// Get all tool names that were called, in order.
    pub fn tool_names_called(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|e| {
                if let AgentEvent::ToolCallStarted { name, .. } = e {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get the tool result for a given tool name (first match).
    pub fn tool_result(&self, tool_name: &str) -> Option<String> {
        self.0.iter().find_map(|e| {
            if let AgentEvent::ToolCallFinished { name, result, .. } = e
                && name == tool_name
            {
                return Some(result.clone());
            }
            None
        })
    }

    /// Assert that a specific tool was called.
    pub fn assert_tool_called(&self, tool_name: &str) {
        let found = self.0.iter().any(|e| {
            matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == tool_name)
        });
        assert!(
            found,
            "Expected tool {tool_name:?} to be called.{}",
            self.dump()
        );
    }

    /// Assert that a specific tool was NOT called.
    pub fn assert_tool_not_called(&self, tool_name: &str) {
        let found = self.0.iter().any(|e| {
            matches!(e, AgentEvent::ToolCallStarted { name, .. } if name == tool_name)
        });
        assert!(
            !found,
            "Expected tool {tool_name:?} NOT to be called.{}",
            self.dump()
        );
    }

    /// Get iterations used from TurnFinished.
    pub fn iterations_used(&self) -> usize {
        self.0
            .iter()
            .find_map(|e| {
                if let AgentEvent::TurnFinished {
                    iterations_used, ..
                } = e
                {
                    Some(*iterations_used)
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }
}

impl std::fmt::Display for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.dump())
    }
}

fn format_event(event: &AgentEvent) -> String {
    match event {
        AgentEvent::LlmThinking { iteration } => {
            format!("LlmThinking(iter={iteration})")
        }
        AgentEvent::LlmResponse {
            text,
            tool_call_count,
            usage,
        } => {
            let text_preview = text
                .as_ref()
                .map(|t| {
                    if t.len() > 120 {
                        format!("{}...", &t[..120])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_else(|| "(none)".into());
            let usage_str = usage
                .as_ref()
                .map(|u| {
                    format!(
                        " tokens=({},{})",
                        u.input_tokens.unwrap_or(0),
                        u.output_tokens.unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            format!("LlmResponse(text={text_preview:?}, tools={tool_call_count}{usage_str})")
        }
        AgentEvent::ToolCallStarted {
            iteration,
            name,
            args,
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            let args_preview = if args_str.len() > 100 {
                format!("{}...", &args_str[..100])
            } else {
                args_str
            };
            format!("ToolCallStarted(iter={iteration}, name={name:?}, args={args_preview})")
        }
        AgentEvent::ToolCallFinished {
            name,
            result,
            success,
        } => {
            let icon = if *success { "OK" } else { "FAIL" };
            let result_preview = if result.len() > 120 {
                format!("{}...", &result[..120])
            } else {
                result.clone()
            };
            format!("ToolCallFinished({icon} {name:?}: {result_preview:?})")
        }
        AgentEvent::TurnFinished {
            final_text,
            iterations_used,
            stop_reason,
        } => {
            let reason = match stop_reason {
                StopReason::Done => "done",
                StopReason::MaxIterations => "max_iterations",
                StopReason::Cancelled => "cancelled",
            };
            let text_preview = final_text
                .as_ref()
                .map(|t| {
                    if t.len() > 80 {
                        format!("{}...", &t[..80])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_else(|| "(none)".into());
            format!(
                "TurnFinished(reason={reason}, iters={iterations_used}, text={text_preview:?})"
            )
        }
        AgentEvent::ResearchComplete {
            context_length,
            tool_call_count,
            duration_ms,
        } => {
            format!("ResearchComplete(chars={context_length}, tools={tool_call_count}, ms={duration_ms})")
        }
        AgentEvent::Error { message, fatal } => {
            let label = if *fatal { "FATAL" } else { "ERROR" };
            format!("{label}: {message}")
        }
    }
}
