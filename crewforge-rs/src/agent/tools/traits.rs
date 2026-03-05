use std::path::Path;

/// Abstracts shell command execution for testability.
pub trait RuntimeAdapter: Send + Sync {
    fn build_shell_command(
        &self,
        command: &str,
        workspace_dir: &Path,
    ) -> anyhow::Result<tokio::process::Command>;
}

/// Native runtime: executes shell commands via `sh -c` on Unix.
pub struct TokioRuntime;

impl RuntimeAdapter for TokioRuntime {
    fn build_shell_command(
        &self,
        command: &str,
        workspace_dir: &Path,
    ) -> anyhow::Result<tokio::process::Command> {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd.current_dir(workspace_dir);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        Ok(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokio_runtime_echo() {
        let rt = TokioRuntime;
        let dir = std::env::current_dir().unwrap();
        let mut cmd = rt.build_shell_command("echo hello", &dir).unwrap();
        let output = cmd.output().await.unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.trim() == "hello");
    }

    #[tokio::test]
    async fn tokio_runtime_sets_cwd() {
        let rt = TokioRuntime;
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = rt.build_shell_command("pwd", dir.path()).unwrap();
        let output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(stdout.trim(), canonical.to_str().unwrap());
    }
}
