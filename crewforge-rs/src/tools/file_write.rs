use crate::agent::ToolResult;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use std::sync::Arc;

pub struct FileWriteTool {
    security: Arc<SecurityPolicy>,
}

impl FileWriteTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl crate::agent::Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates parent directories if needed. Refuses to write through symlinks."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to write to"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Security policy: read-only mode, cannot write files".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded".into()),
            });
        }

        if !self.security.is_path_allowed(path) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Path not allowed by security policy: {path}")),
            });
        }

        let full_path = self.security.workspace_dir.join(path);

        // Create parent directories
        if let Some(parent) = full_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to create parent directories: {e}")),
                });
            }

            // Canonicalize parent to check resolved path
            let resolved_parent = match parent.canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to resolve parent path: {e}")),
                    });
                }
            };

            if !self.security.is_resolved_path_allowed(&resolved_parent) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(
                        self.security
                            .resolved_path_violation_message(&resolved_parent),
                    ),
                });
            }
        }

        // Refuse to write through symlinks
        #[cfg(unix)]
        if full_path.exists() {
            let meta = std::fs::symlink_metadata(&full_path);
            if let Ok(m) = meta {
                if m.file_type().is_symlink() {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some("Refusing to write through symlink".into()),
                    });
                }
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        match tokio::fs::write(&full_path, content).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Wrote {} bytes to {path}", content.len()),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to write file: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Tool;
    use crate::security::AutonomyLevel;
    use serde_json::json;

    fn test_security(workspace: std::path::PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        })
    }

    #[tokio::test]
    async fn file_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "new.txt", "content": "hello"}))
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(dir.path().join("new.txt"))
            .await
            .unwrap();
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn file_write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "sub/dir/file.txt", "content": "nested"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(dir.path().join("sub/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn file_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("exist.txt"), "old")
            .await
            .unwrap();

        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "exist.txt", "content": "new"}))
            .await
            .unwrap();
        assert!(result.success);
        let content = tokio::fs::read_to_string(dir.path().join("exist.txt"))
            .await
            .unwrap();
        assert_eq!(content, "new");
    }

    #[tokio::test]
    async fn file_write_blocks_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "../escape.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn file_write_blocks_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "/tmp/bad.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn file_write_blocks_readonly_mode() {
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        let tool = FileWriteTool::new(security);
        let result = tool
            .execute(json!({"path": "file.txt", "content": "no"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn file_write_blocks_when_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let security = Arc::new(SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = FileWriteTool::new(security);
        let result = tool
            .execute(json!({"path": "file.txt", "content": "no"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("link_dir")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "link_dir/file.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_target_file() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.txt");
        tokio::fs::write(&target, "original").await.unwrap();
        std::os::unix::fs::symlink(&target, workspace.path().join("link.txt")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "link.txt", "content": "overwrite"}))
            .await
            .unwrap();
        assert!(!result.success);
        // Verify original file was not modified
        let content = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(content, "original");
    }

    #[tokio::test]
    async fn file_write_blocks_null_byte_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileWriteTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "file\0.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
    }
}
