pub mod context;
pub mod dispatch;
pub mod history;
pub mod orchestrate;
pub mod sandbox;
pub mod scrub;
pub mod tools;

pub use orchestrate::{AgentEvent, AgentSession, AgentSessionConfig, StopReason};

use crate::provider::traits::ToolSpec;
use async_trait::async_trait;

/// Structured result from tool execution.
/// Security denials use `success: false` with an `error` message —
/// they are business logic, not program errors.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub success: bool,
    pub output: String,
    pub error: Option<String>,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            error: None,
        }
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self {
            success: false,
            output: String::new(),
            error: Some(message.into()),
        }
    }
}

/// Generic tool interface. Implement this for any tool the agent can use.
/// CrewForge hub tools (HubGet, HubAck, HubPost) implement this trait.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: self.description().to_string(),
            parameters: self.parameters(),
        }
    }

    /// Whether this tool has side effects (writes files, runs commands, etc.).
    /// Non-mutating tools (reads, searches) can be auto-approved and parallelized.
    fn is_mutating(&self) -> bool {
        false // safe default: assume read-only
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult>;
}
