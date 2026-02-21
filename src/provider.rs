use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::text::normalize_text;

#[derive(Debug, Clone)]
pub struct OpencodeProviderConfig {
    pub command: String,
    pub timeout_ms: u64,
    pub runtime_agent_name: String,
    pub workspace_dir: PathBuf,
}

#[derive(Debug)]
pub struct OpencodeCliProvider {
    config: OpencodeProviderConfig,
    model: String,
    runtime_dir: PathBuf,
    session_id: String,
}

#[derive(Debug)]
struct ProcessOutput {
    stdout: String,
    stderr: String,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedStreamJson {
    pub reply: String,
    pub parsed_lines: usize,
    pub session_id: String,
}

impl OpencodeCliProvider {
    pub fn new(config: OpencodeProviderConfig, model: String, runtime_dir: PathBuf) -> Self {
        Self {
            config,
            model,
            runtime_dir,
            session_id: String::new(),
        }
    }

    pub async fn send_prompt(&mut self, prompt: &str) -> Result<String> {
        let mut args = vec![
            "run".to_string(),
            "--format".to_string(),
            "json".to_string(),
            "--dir".to_string(),
            self.config.workspace_dir.to_string_lossy().to_string(),
            "-m".to_string(),
            self.model.clone(),
            "--agent".to_string(),
            self.config.runtime_agent_name.clone(),
        ];

        if !self.session_id.is_empty() {
            args.push("-s".to_string());
            args.push(self.session_id.clone());
        }

        args.push(prompt.to_string());

        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.insert(
            "OPENCODE_CONFIG_DIR".to_string(),
            self.runtime_dir.to_string_lossy().to_string(),
        );
        env.insert("OPENCODE_ENABLE_EXA".to_string(), "1".to_string());

        let output = run_process(
            &self.config.command,
            &args,
            self.config.workspace_dir.clone(),
            env,
            self.config.timeout_ms,
        )
        .await?;

        let parsed = parse_stream_json(&output.stdout);
        if !parsed.session_id.is_empty() {
            self.session_id = parsed.session_id.clone();
        }

        if !parsed.reply.is_empty() {
            return Ok(parsed.reply);
        }

        let diagnostic =
            first_diagnostic_line(&output.stderr).or_else(|| first_diagnostic_line(&output.stdout));
        if parsed.parsed_lines == 0 {
            bail!(
                "opencode stream-json parse failed: {}",
                diagnostic.unwrap_or_else(|| "no output".to_string())
            );
        }

        if let Some(line) = diagnostic {
            bail!("opencode returned empty assistant text: {line}");
        }

        Ok("(empty reply)".to_string())
    }
}

async fn run_process(
    command: &str,
    args: &[String],
    cwd: PathBuf,
    env: HashMap<String, String>,
    timeout_ms: u64,
) -> Result<ProcessOutput> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .current_dir(cwd)
        // Ensure child process is terminated when the waiting future/task is dropped
        // (for example, on Ctrl+D shutdown path with task abort).
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(env);

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {command}"))?;
    let output = timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("timeout after {timeout_ms} ms"))?
        .context("failed waiting child process")?;

    let stdout = String::from_utf8(output.stdout).context("stdout is not utf8")?;
    let stderr = String::from_utf8(output.stderr).context("stderr is not utf8")?;

    if output.status.success() {
        return Ok(ProcessOutput { stdout, stderr });
    }

    bail!(
        "exit {}: {}",
        output.status.code().unwrap_or(-1),
        if stderr.trim().is_empty() {
            stdout.clone()
        } else {
            stderr.clone()
        }
    );
}

pub fn parse_stream_json(raw_text: &str) -> ParsedStreamJson {
    let mut fragments = Vec::new();
    let mut parsed_lines = 0usize;
    let mut session_id = String::new();

    for line in raw_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let parsed: Result<StreamEvent, _> = serde_json::from_str(line);
        let Ok(event) = parsed else {
            continue;
        };

        parsed_lines += 1;
        if session_id.is_empty() {
            if let Some(id) = event.session_id {
                session_id = id;
            }
        }

        if let Some(text) = event.text {
            fragments.push(text);
        } else if let Some(delta) = event.delta {
            fragments.push(delta);
        } else if let Some(content) = event.content {
            fragments.push(content);
        } else if let Some(message) = event.message {
            fragments.push(message);
        } else if let Some(output_text) = event.output_text {
            fragments.push(output_text);
        } else if let Some(part) = event.part.and_then(|p| p.text) {
            fragments.push(part);
        }
    }

    ParsedStreamJson {
        reply: normalize_text(&fragments.join("\n")),
        parsed_lines,
        session_id,
    }
}

pub fn first_diagnostic_line(text: &str) -> Option<String> {
    for line in normalize_text(text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !line.chars().all(|ch| ch == '=') {
            return Some(line.to_string());
        }
    }
    None
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamEvent {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    text: Option<String>,
    delta: Option<String>,
    content: Option<String>,
    message: Option<String>,
    output_text: Option<String>,
    part: Option<StreamPart>,
}

#[derive(Debug, Deserialize)]
struct StreamPart {
    text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stream_json_collects_fragments_and_session() {
        let input = r#"{"sessionID":"abc","delta":"hello"}
{"text":"world"}
not-json
{"part":{"text":"!"}}"#;

        let parsed = parse_stream_json(input);
        assert_eq!(parsed.session_id, "abc");
        assert_eq!(parsed.parsed_lines, 3);
        assert_eq!(parsed.reply, "hello\nworld\n!");
    }
}
