use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::config::{AgentConfig, RateLimitConfig};
use crate::kernel::{MessageEvent, MessageRole, SessionKernel};
use crate::text::normalize_text;

#[derive(Debug, Clone)]
pub struct UnreadResult {
    pub unread: Vec<MessageEvent>,
    pub upto_event_seq: u64,
}

#[derive(Debug, Clone)]
pub struct RateLimitUsage {
    pub remaining: usize,
}

#[derive(Debug, Clone)]
pub struct PostResult {
    pub posted: bool,
    pub reason: String,
    pub message: Option<MessageEvent>,
}

#[derive(Debug, Default)]
struct HubState {
    read_seq_by_agent_id: HashMap<String, u64>,
    last_fetched_seq_by_agent_id: HashMap<String, u64>,
    posted_at_ms_by_agent_id: HashMap<String, Vec<u64>>,
}

#[derive(Debug)]
pub struct RoomHub {
    kernel: Arc<SessionKernel>,
    agent_by_id: HashMap<String, AgentConfig>,
    rate_limit: RateLimitConfig,
    state: Mutex<HubState>,
}

impl RoomHub {
    pub fn new(
        kernel: Arc<SessionKernel>,
        agents: &[AgentConfig],
        rate_limit: RateLimitConfig,
    ) -> Self {
        let mut agent_by_id = HashMap::new();
        let mut state = HubState::default();

        for agent in agents {
            agent_by_id.insert(agent.id.clone(), agent.clone());
            state.read_seq_by_agent_id.insert(agent.id.clone(), 0);
            state
                .last_fetched_seq_by_agent_id
                .insert(agent.id.clone(), 0);
            state
                .posted_at_ms_by_agent_id
                .insert(agent.id.clone(), Vec::new());
        }

        Self {
            kernel,
            agent_by_id,
            rate_limit,
            state: Mutex::new(state),
        }
    }

    pub async fn ack(&self, agent_id: &str, upto_event_seq: u64) -> Result<u64> {
        self.ensure_agent_exists(agent_id)?;
        if upto_event_seq == 0 {
            return Ok(0);
        }

        let mut state = self.state.lock().await;
        let previous = state
            .read_seq_by_agent_id
            .get(agent_id)
            .copied()
            .unwrap_or(0);
        let next = previous.max(upto_event_seq);
        state
            .read_seq_by_agent_id
            .insert(agent_id.to_string(), next);
        Ok(next)
    }

    pub async fn set_all_agent_cursors(&self, upto_event_seq: u64) {
        let mut state = self.state.lock().await;
        for agent_id in self.agent_by_id.keys() {
            state
                .read_seq_by_agent_id
                .insert(agent_id.clone(), upto_event_seq);
            state
                .last_fetched_seq_by_agent_id
                .insert(agent_id.clone(), upto_event_seq);
        }
    }

    pub async fn get_unread(&self, agent_id: &str) -> Result<UnreadResult> {
        self.ensure_agent_exists(agent_id)?;

        let read_seq = {
            self.state
                .lock()
                .await
                .read_seq_by_agent_id
                .get(agent_id)
                .copied()
                .unwrap_or(0)
        };

        let transcript = self.kernel.transcript_snapshot().await;
        let unread: Vec<MessageEvent> = transcript
            .iter()
            .filter(|event| {
                event.event_seq > read_seq
                    && !(event.role == MessageRole::Agent
                        && event.agent_id.as_deref() == Some(agent_id))
            })
            .cloned()
            .collect();

        let upto_event_seq = transcript
            .last()
            .map(|item| item.event_seq)
            .unwrap_or(read_seq);

        {
            let mut state = self.state.lock().await;
            state
                .last_fetched_seq_by_agent_id
                .insert(agent_id.to_string(), upto_event_seq);
            state
                .read_seq_by_agent_id
                .insert(agent_id.to_string(), upto_event_seq);
        }

        Ok(UnreadResult {
            unread,
            upto_event_seq,
        })
    }

    pub async fn has_unread(&self, agent_id: &str) -> Result<bool> {
        self.ensure_agent_exists(agent_id)?;

        let read_seq = {
            self.state
                .lock()
                .await
                .read_seq_by_agent_id
                .get(agent_id)
                .copied()
                .unwrap_or(0)
        };

        let transcript = self.kernel.transcript_snapshot().await;
        Ok(transcript.iter().any(|event| {
            event.event_seq > read_seq
                && !(event.role == MessageRole::Agent
                    && event.agent_id.as_deref() == Some(agent_id))
        }))
    }

    pub async fn get_rate_limit_usage(&self, agent_id: &str) -> Result<RateLimitUsage> {
        self.ensure_agent_exists(agent_id)?;

        let now_ms = now_millis();
        let mut state = self.state.lock().await;
        let posted = state
            .posted_at_ms_by_agent_id
            .get(agent_id)
            .cloned()
            .unwrap_or_default();

        let lower = now_ms.saturating_sub(self.rate_limit.window_ms);
        let trimmed: Vec<u64> = posted.into_iter().filter(|item| *item >= lower).collect();
        let used = trimmed.len();
        let remaining = self.rate_limit.max_posts.saturating_sub(used);
        state
            .posted_at_ms_by_agent_id
            .insert(agent_id.to_string(), trimmed);

        Ok(RateLimitUsage { remaining })
    }

    pub async fn post(
        &self,
        agent_id: &str,
        raw_text: &str,
        ack_event_seq: Option<u64>,
    ) -> Result<PostResult> {
        let agent = self
            .agent_by_id
            .get(agent_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown agent id: {agent_id}"))?;

        let fallback_ack = {
            self.state
                .lock()
                .await
                .last_fetched_seq_by_agent_id
                .get(agent_id)
                .copied()
                .unwrap_or(0)
        };

        let effective_ack = ack_event_seq.unwrap_or(0).max(fallback_ack);
        if effective_ack > 0 {
            let _ = self.ack(agent_id, effective_ack).await?;
        }

        let text = normalize_text(raw_text);
        if text.is_empty() || is_drop_or_skip(&text) {
            return Ok(PostResult {
                posted: false,
                reason: "no_publish".to_string(),
                message: None,
            });
        }

        let usage = self.get_rate_limit_usage(agent_id).await?;
        if usage.remaining == 0 {
            return Ok(PostResult {
                posted: false,
                reason: "rate_limited".to_string(),
                message: None,
            });
        }

        let message = self
            .kernel
            .append_message(
                MessageRole::Agent,
                agent.name,
                text,
                Some(agent_id.to_string()),
            )
            .await?;

        {
            let mut state = self.state.lock().await;
            let now = now_millis();
            let lower = now.saturating_sub(self.rate_limit.window_ms);
            let mut list = state
                .posted_at_ms_by_agent_id
                .remove(agent_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|item| *item >= lower)
                .collect::<Vec<_>>();
            list.push(now);
            state
                .posted_at_ms_by_agent_id
                .insert(agent_id.to_string(), list);
            state
                .read_seq_by_agent_id
                .insert(agent_id.to_string(), message.event_seq);
        }

        Ok(PostResult {
            posted: true,
            reason: "posted".to_string(),
            message: Some(message),
        })
    }

    fn ensure_agent_exists(&self, agent_id: &str) -> Result<()> {
        if !self.agent_by_id.contains_key(agent_id) {
            bail!("unknown agent id: {agent_id}");
        }
        Ok(())
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_drop_or_skip(text: &str) -> bool {
    let upper = normalize_text(text).to_uppercase();
    let token = upper.split_whitespace().next().unwrap_or_default();
    token == "[DROP]" || token == "[SKIP]"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentTools, RateLimitConfig};

    fn make_agents() -> Vec<AgentConfig> {
        vec![
            AgentConfig {
                id: "codex".to_string(),
                name: "Codex".to_string(),
                model: "m1".to_string(),
                context_dir: ".room/agents/codex".to_string(),
                tools: AgentTools::default(),
                preference: None,
            },
            AgentConfig {
                id: "kimi".to_string(),
                name: "Kimi".to_string(),
                model: "m2".to_string(),
                context_dir: ".room/agents/kimi".to_string(),
                tools: AgentTools::default(),
                preference: None,
            },
        ]
    }

    #[tokio::test]
    async fn unread_excludes_own_agent_messages() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let hub = RoomHub::new(
            kernel.clone(),
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        );

        kernel
            .append_message(MessageRole::Human, "Rex".into(), "hello".into(), None)
            .await
            .expect("append human");
        kernel
            .append_message(
                MessageRole::Agent,
                "Codex".into(),
                "self".into(),
                Some("codex".into()),
            )
            .await
            .expect("append codex");

        let unread = hub.get_unread("codex").await.expect("get unread");
        assert_eq!(unread.unread.len(), 1);
        assert_eq!(unread.unread[0].speaker, "Rex");
    }

    #[tokio::test]
    async fn post_honors_drop_and_rate_limit() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let hub = RoomHub::new(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 1,
            },
        );

        let drop_res = hub.post("codex", "[DROP]", None).await.expect("drop");
        assert!(!drop_res.posted);
        assert_eq!(drop_res.reason, "no_publish");

        let post_res = hub.post("codex", "hello", None).await.expect("post");
        assert!(post_res.posted);

        let limited = hub.post("codex", "again", None).await.expect("limited");
        assert!(!limited.posted);
        assert_eq!(limited.reason, "rate_limited");
    }

    #[tokio::test]
    async fn set_all_agent_cursors_skips_historical_unread() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        kernel
            .append_message(MessageRole::Human, "Rex".into(), "m1".into(), None)
            .await
            .expect("append m1");
        kernel
            .append_message(MessageRole::Human, "Rex".into(), "m2".into(), None)
            .await
            .expect("append m2");

        let hub = RoomHub::new(
            kernel.clone(),
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        );

        hub.set_all_agent_cursors(2).await;
        assert!(!hub.has_unread("codex").await.expect("has_unread codex"));
        assert!(!hub.has_unread("kimi").await.expect("has_unread kimi"));
    }
}
