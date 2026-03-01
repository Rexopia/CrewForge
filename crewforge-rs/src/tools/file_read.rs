use crate::agent::ToolResult;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use std::sync::Arc;

const MAX_FILE_SIZE_BYTES: u64 = 10 * 1024 * 1024;

pub struct FileReadTool {
    security: Arc<SecurityPolicy>,
}

impl FileReadTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl crate::agent::Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read file contents with line numbers. Supports partial reading via offset and limit."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file from workspace root"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number (1-based, default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to return (default: all)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

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

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let full_path = self.security.workspace_dir.join(path);

        let resolved_path = match tokio::fs::canonicalize(&full_path).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                });
            }
        };

        if !self.security.is_resolved_path_allowed(&resolved_path) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(self.security.resolved_path_violation_message(&resolved_path)),
            });
        }

        match tokio::fs::metadata(&resolved_path).await {
            Ok(meta) => {
                if meta.len() > MAX_FILE_SIZE_BYTES {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "File too large: {} bytes (limit: {MAX_FILE_SIZE_BYTES} bytes)",
                            meta.len()
                        )),
                    });
                }
            }
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read file metadata: {e}")),
                });
            }
        }

        match tokio::fs::read_to_string(&resolved_path).await {
            Ok(contents) => {
                let lines: Vec<&str> = contents.lines().collect();
                let total = lines.len();

                if total == 0 {
                    return Ok(ToolResult {
                        success: true,
                        output: String::new(),
                        error: None,
                    });
                }

                let offset = args
                    .get("offset")
                    .and_then(|v| v.as_u64())
                    .map(|v| {
                        usize::try_from(v.max(1))
                            .unwrap_or(usize::MAX)
                            .saturating_sub(1)
                    })
                    .unwrap_or(0);
                let start = offset.min(total);

                let end = match args.get("limit").and_then(|v| v.as_u64()) {
                    Some(l) => {
                        let limit = usize::try_from(l).unwrap_or(usize::MAX);
                        start.saturating_add(limit).min(total)
                    }
                    None => total,
                };

                if start >= end {
                    return Ok(ToolResult {
                        success: true,
                        output: format!("[No lines in range, file has {total} lines]"),
                        error: None,
                    });
                }

                let numbered: String = lines[start..end]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {}", start + i + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");

                let partial = start > 0 || end < total;
                let summary = if partial {
                    format!("\n[Lines {}-{} of {total}]", start + 1, end)
                } else {
                    format!("\n[{total} lines total]")
                };

                Ok(ToolResult {
                    success: true,
                    output: format!("{numbered}{summary}"),
                    error: None,
                })
            }
            Err(_) => {
                let bytes = tokio::fs::read(&resolved_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to read file: {e}"))?;

                let lossy = String::from_utf8_lossy(&bytes).into_owned();
                Ok(ToolResult {
                    success: true,
                    output: lossy,
                    error: None,
                })
            }
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
    async fn file_read_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), "hello world")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("1: hello world"));
        assert!(result.output.contains("[1 lines total]"));
    }

    #[tokio::test]
    async fn file_read_nonexistent_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "no_such_file.txt"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn file_read_blocks_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "../../../etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn file_read_blocks_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "/etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn file_read_blocks_when_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("test.txt"), "ok")
            .await
            .unwrap();

        let security = Arc::new(SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = FileReadTool::new(security);
        let result = tool.execute(json!({"path": "test.txt"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Rate limit"));
    }

    #[tokio::test]
    async fn file_read_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("multi.txt"), "line1\nline2\nline3\nline4\nline5")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "multi.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("2: line2"));
        assert!(result.output.contains("3: line3"));
        assert!(!result.output.contains("4: line4"));
    }

    #[tokio::test]
    async fn file_read_offset_beyond_end() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("short.txt"), "one\ntwo")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "short.txt", "offset": 100}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No lines in range"));
    }

    #[tokio::test]
    async fn file_read_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("empty.txt"), "")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "empty.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.is_empty());
    }

    #[tokio::test]
    async fn file_read_nested_path() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("sub/dir"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("sub/dir/file.txt"), "nested content")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "sub/dir/file.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("nested content"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_read_blocks_symlink_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        tokio::fs::write(outside.path().join("secret.txt"), "secret data")
            .await
            .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            workspace.path().join("link.txt"),
        )
        .unwrap();

        let tool = FileReadTool::new(test_security(workspace.path().to_path_buf()));
        let result = tool.execute(json!({"path": "link.txt"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn file_read_blocks_null_byte_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "file\0.txt"}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn file_read_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let big_file = dir.path().join("big.bin");
        // Create a file just over the limit using sparse writing
        let f = std::fs::File::create(&big_file).unwrap();
        f.set_len(MAX_FILE_SIZE_BYTES + 1).unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool.execute(json!({"path": "big.bin"})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("too large"));
    }

    #[tokio::test]
    async fn file_read_lossy_reads_binary_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("binary.bin"), b"\xff\xfe\x00\x01hello")
            .await
            .unwrap();

        let tool = FileReadTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({"path": "binary.bin"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn file_read_blocks_readonly_not_applicable() {
        // ReadOnly mode should still allow file reads (they don't use enforce_tool_operation)
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("file.txt"), "content")
            .await
            .unwrap();

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        });
        // FileReadTool uses is_rate_limited + is_path_allowed + record_action
        // ReadOnly doesn't block reads via is_path_allowed
        let tool = FileReadTool::new(security);
        let result = tool.execute(json!({"path": "file.txt"})).await.unwrap();
        assert!(result.success);
    }
}
