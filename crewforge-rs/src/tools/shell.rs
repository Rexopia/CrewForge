use crate::agent::ToolResult;
use crate::security::SecurityPolicy;
use crate::security::policy::is_valid_env_var_name;
use crate::tools::traits::RuntimeAdapter;
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

const SHELL_TIMEOUT_SECS: u64 = 60;
const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1 MB
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

pub struct ShellTool {
    security: Arc<SecurityPolicy>,
    runtime: Arc<dyn RuntimeAdapter>,
}

impl ShellTool {
    pub fn new(security: Arc<SecurityPolicy>, runtime: Arc<dyn RuntimeAdapter>) -> Self {
        Self { security, runtime }
    }
}

pub(crate) fn collect_allowed_shell_env_vars(security: &SecurityPolicy) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for key in SAFE_ENV_VARS
        .iter()
        .copied()
        .chain(security.shell_env_passthrough.iter().map(|s| s.as_str()))
    {
        let candidate = key.trim();
        if candidate.is_empty() || !is_valid_env_var_name(candidate) {
            continue;
        }
        if seen.insert(candidate.to_string()) {
            out.push(candidate.to_string());
        }
    }
    out
}

fn extract_command_argument(args: &serde_json::Value) -> Option<String> {
    if let Some(command) = args
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
    {
        return Some(command.to_string());
    }

    for alias in [
        "cmd",
        "script",
        "shell_command",
        "command_line",
        "bash",
        "sh",
        "input",
    ] {
        if let Some(command) = args
            .get(alias)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|cmd| !cmd.is_empty())
        {
            return Some(command.to_string());
        }
    }

    args.as_str()
        .map(str::trim)
        .filter(|cmd| !cmd.is_empty())
        .map(ToString::to_string)
}

fn truncate_utf8(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[async_trait]
impl crate::agent::Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Execute a shell command in the workspace directory"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk commands",
                    "default": false
                }
            },
            "required": ["command"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = extract_command_argument(&args)
            .ok_or_else(|| anyhow::anyhow!("Missing 'command' parameter"))?;
        let approved = args
            .get("approved")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded".into()),
            });
        }

        match self.security.validate_command_execution(&command, approved) {
            Ok(_) => {}
            Err(reason) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(reason),
                });
            }
        }

        if let Some(path) = self.security.forbidden_path_argument(&command) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Path blocked by security policy: {path}")),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let mut cmd = match self
            .runtime
            .build_shell_command(&command, &self.security.workspace_dir)
        {
            Ok(cmd) => cmd,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build runtime command: {e}")),
                });
            }
        };
        // Environment sandboxing: clear all env vars then selectively restore
        // safe ones. This is intentionally done here (not in RuntimeAdapter) so
        // the security policy controls which vars are passed through.
        cmd.env_clear();

        for var in collect_allowed_shell_env_vars(&self.security) {
            if let Ok(val) = std::env::var(&var) {
                cmd.env(&var, val);
            }
        }

        let result =
            tokio::time::timeout(Duration::from_secs(SHELL_TIMEOUT_SECS), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if stdout.len() > MAX_OUTPUT_BYTES {
                    let boundary = truncate_utf8(&stdout, MAX_OUTPUT_BYTES);
                    stdout.truncate(boundary);
                    stdout.push_str("\n... [output truncated at 1MB]");
                }
                if stderr.len() > MAX_OUTPUT_BYTES {
                    let boundary = truncate_utf8(&stderr, MAX_OUTPUT_BYTES);
                    stderr.truncate(boundary);
                    stderr.push_str("\n... [stderr truncated at 1MB]");
                }

                Ok(ToolResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if stderr.is_empty() {
                        None
                    } else {
                        Some(stderr)
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SHELL_TIMEOUT_SECS}s and was killed"
                )),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Tool;
    use crate::security::AutonomyLevel;
    use crate::tools::traits::TokioRuntime;
    use serde_json::json;

    fn test_security(autonomy: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_runtime() -> Arc<dyn RuntimeAdapter> {
        Arc::new(TokioRuntime)
    }

    #[test]
    fn shell_tool_is_mutating() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert!(tool.is_mutating());
    }

    #[test]
    fn shell_name_and_schema() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        assert_eq!(tool.name(), "shell");
        let schema = tool.parameters();
        assert!(schema["properties"]["command"].is_object());
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("command"))
        );
    }

    #[tokio::test]
    async fn shell_executes_simple_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.trim().contains("hello"));
    }

    #[tokio::test]
    async fn shell_blocks_dangerous_command() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool.execute(json!({"command": "rm -rf /"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn shell_blocks_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            max_actions_per_hour: 0,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());
        let result = tool.execute(json!({"command": "echo test"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Rate limit"));
    }

    #[tokio::test]
    async fn shell_blocks_readonly_mode() {
        let tool = ShellTool::new(test_security(AutonomyLevel::ReadOnly), test_runtime());
        let result = tool.execute(json!({"command": "ls"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn shell_forbidden_path_in_args() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "cat /etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Path blocked")
        );
    }

    #[tokio::test]
    async fn shell_captures_exit_code() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool
            .execute(json!({"command": "ls /nonexistent_dir_xyz"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn shell_captures_stderr() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Full), test_runtime());
        let result = tool
            .execute(json!({"command": "echo error_msg >&2"}))
            .await
            .unwrap();
        assert!(result.error.as_deref().unwrap_or("").contains("error_msg"));
    }

    #[tokio::test]
    async fn shell_missing_command_param() {
        let tool = ShellTool::new(test_security(AutonomyLevel::Supervised), test_runtime());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn extract_command_supports_aliases() {
        assert_eq!(
            extract_command_argument(&json!({"cmd": "echo from-cmd"})).as_deref(),
            Some("echo from-cmd")
        );
        assert_eq!(
            extract_command_argument(&json!({"script": "echo from-script"})).as_deref(),
            Some("echo from-script")
        );
        assert_eq!(
            extract_command_argument(&json!("echo from-string")).as_deref(),
            Some("echo from-string")
        );
    }

    #[test]
    fn shell_safe_env_vars_excludes_secrets() {
        for var in SAFE_ENV_VARS {
            let lower = var.to_lowercase();
            assert!(
                !lower.contains("key") && !lower.contains("secret") && !lower.contains("token"),
                "SAFE_ENV_VARS must not include: {var}"
            );
        }
    }

    #[test]
    fn invalid_env_var_names_are_filtered() {
        let security = SecurityPolicy {
            shell_env_passthrough: vec![
                "VALID_NAME".into(),
                "BAD-NAME".into(),
                "1NOPE".into(),
                "ALSO_VALID".into(),
            ],
            ..SecurityPolicy::default()
        };
        let vars = collect_allowed_shell_env_vars(&security);
        assert!(vars.contains(&"VALID_NAME".to_string()));
        assert!(vars.contains(&"ALSO_VALID".to_string()));
        assert!(!vars.contains(&"BAD-NAME".to_string()));
        assert!(!vars.contains(&"1NOPE".to_string()));
    }

    #[tokio::test]
    async fn shell_record_action_budget_exhaustion() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            max_actions_per_hour: 1,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ShellTool::new(security, test_runtime());

        let r1 = tool
            .execute(json!({"command": "echo first"}))
            .await
            .unwrap();
        assert!(r1.success);

        let r2 = tool
            .execute(json!({"command": "echo second"}))
            .await
            .unwrap();
        assert!(!r2.success);
        assert!(
            r2.error.as_deref().unwrap_or("").contains("Rate limit")
                || r2.error.as_deref().unwrap_or("").contains("budget")
        );
    }
}
