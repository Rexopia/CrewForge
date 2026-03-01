pub mod dispatcher;
pub mod history;
pub mod loop_;
pub mod prompt;

pub use loop_::{AgentEvent, AgentSession, AgentSessionConfig, StopReason};

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

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult>;
}
