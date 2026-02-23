use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::text::normalize_text;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    Human,
    Agent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MessageEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub event_seq: u64,
    pub role: MessageRole,
    pub speaker: String,
    pub agent_id: Option<String>,
    pub text: String,
    pub ts: String,
}

#[derive(Debug, Default)]
struct KernelState {
    transcript: Vec<MessageEvent>,
    next_event_seq: u64,
}

#[derive(Debug)]
pub struct SessionKernel {
    pub session_file: PathBuf,
    state: Mutex<KernelState>,
}

impl SessionKernel {
    pub async fn create_new(sessions_dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(sessions_dir)
            .await
            .with_context(|| {
                format!("failed to create sessions dir: {}", sessions_dir.display())
            })?;

        let session_file = sessions_dir.join(create_session_filename());
        Ok(Self {
            session_file,
            state: Mutex::new(KernelState {
                transcript: Vec::new(),
                next_event_seq: 1,
            }),
        })
    }

    pub async fn load(session_file: PathBuf) -> Result<Self> {
        let raw = tokio::fs::read_to_string(&session_file)
            .await
            .with_context(|| format!("failed reading session file: {}", session_file.display()))?;

        let mut transcript = Vec::new();
        let mut max_event_seq = 0_u64;

        for (idx, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: MessageEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "invalid session event at line {} in {}",
                    idx + 1,
                    session_file.display()
                )
            })?;
            max_event_seq = max_event_seq.max(event.event_seq);
            transcript.push(event);
        }

        Ok(Self {
            session_file,
            state: Mutex::new(KernelState {
                transcript,
                next_event_seq: max_event_seq + 1,
            }),
        })
    }

    pub async fn append_message(
        &self,
        role: MessageRole,
        speaker: String,
        text: String,
        agent_id: Option<String>,
    ) -> Result<MessageEvent> {
        let mut state = self.state.lock().await;
        let message = MessageEvent {
            event_type: "message".to_string(),
            event_seq: state.next_event_seq,
            role,
            speaker,
            agent_id,
            text: normalize_text(&text),
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        };

        state.next_event_seq += 1;
        state.transcript.push(message.clone());

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.session_file)
            .await
            .with_context(|| {
                format!(
                    "failed to open session file: {}",
                    self.session_file.display()
                )
            })?;

        let line = serde_json::to_string(&message).context("failed to encode transcript event")?;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;

        Ok(message)
    }

    pub async fn transcript_snapshot(&self) -> Vec<MessageEvent> {
        self.state.lock().await.transcript.clone()
    }

    pub async fn latest_event_seq(&self) -> u64 {
        self.state.lock().await.next_event_seq.saturating_sub(1)
    }
}

fn create_session_filename() -> String {
    let ts = Utc::now()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
        .replace([':', '.'], "-");
    format!("session-{ts}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn append_message_increments_event_seq() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kernel = SessionKernel::create_new(tmp.path()).await.expect("kernel");

        let first = kernel
            .append_message(
                MessageRole::Human,
                "Rex".to_string(),
                "hello".to_string(),
                None,
            )
            .await
            .expect("append first");
        let second = kernel
            .append_message(
                MessageRole::Agent,
                "Codex".to_string(),
                "world".to_string(),
                Some("codex".to_string()),
            )
            .await
            .expect("append second");

        assert_eq!(first.event_seq, 1);
        assert_eq!(second.event_seq, 2);
    }

    #[tokio::test]
    async fn load_restores_transcript_and_next_event_seq() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let kernel = SessionKernel::create_new(tmp.path()).await.expect("kernel");
        let session_file = kernel.session_file.clone();

        let _ = kernel
            .append_message(
                MessageRole::Human,
                "Rex".to_string(),
                "hello".to_string(),
                None,
            )
            .await
            .expect("append first");
        let _ = kernel
            .append_message(
                MessageRole::Agent,
                "Codex".to_string(),
                "world".to_string(),
                Some("codex".to_string()),
            )
            .await
            .expect("append second");

        let loaded = SessionKernel::load(session_file)
            .await
            .expect("load kernel");
        let snapshot = loaded.transcript_snapshot().await;
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].text, "hello");
        assert_eq!(snapshot[1].text, "world");

        let third = loaded
            .append_message(
                MessageRole::Human,
                "Rex".to_string(),
                "again".to_string(),
                None,
            )
            .await
            .expect("append third");
        assert_eq!(third.event_seq, 3);
    }
}
