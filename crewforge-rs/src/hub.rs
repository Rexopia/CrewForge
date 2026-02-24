use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use tokio::sync::Mutex;

use crate::config::{AgentConfig, RateLimitConfig};
use crate::kernel::{MessageEvent, MessageRole, SessionKernel};
use crate::text::normalize_text;

pub const HUB_TOOL_GET_UNREAD: &str = "hub_get_unread";
pub const HUB_TOOL_ACK: &str = "hub_ack";
pub const HUB_TOOL_POST: &str = "hub_post";

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

#[derive(Debug, Clone, Copy)]
pub struct WakeBudgetConfig {
    pub max_get_unread_per_wake: usize,
    pub max_post_per_wake: usize,
    pub max_tool_calls_per_wake: usize,
    pub max_wake_ms: u64,
}

impl Default for WakeBudgetConfig {
    fn default() -> Self {
        Self {
            max_get_unread_per_wake: 1,
            max_post_per_wake: 1,
            max_tool_calls_per_wake: 10,
            max_wake_ms: 25_000,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct WakeBudgetState {
    active: bool,
    started_at_ms: u64,
    total_tool_calls: usize,
    get_unread_calls: usize,
    post_calls: usize,
}

#[derive(Debug, Default)]
struct HubState {
    read_seq_by_agent_id: HashMap<String, u64>,
    last_fetched_seq_by_agent_id: HashMap<String, u64>,
    posted_at_ms_by_agent_id: HashMap<String, Vec<u64>>,
    wake_budget_by_agent_id: HashMap<String, WakeBudgetState>,
}

#[derive(Debug)]
pub struct RoomHub {
    kernel: Arc<SessionKernel>,
    agent_by_id: HashMap<String, AgentConfig>,
    rate_limit: RateLimitConfig,
    wake_budget: WakeBudgetConfig,
    state: Mutex<HubState>,
}

impl RoomHub {
    pub fn new(
        kernel: Arc<SessionKernel>,
        agents: &[AgentConfig],
        rate_limit: RateLimitConfig,
    ) -> Self {
        Self::with_wake_budget(kernel, agents, rate_limit, WakeBudgetConfig::default())
    }

    pub fn with_wake_budget(
        kernel: Arc<SessionKernel>,
        agents: &[AgentConfig],
        rate_limit: RateLimitConfig,
        wake_budget: WakeBudgetConfig,
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
            state
                .wake_budget_by_agent_id
                .insert(agent.id.clone(), WakeBudgetState::default());
        }

        Self {
            kernel,
            agent_by_id,
            rate_limit,
            wake_budget,
            state: Mutex::new(state),
        }
    }

    pub async fn begin_wake(&self, agent_id: &str) -> Result<()> {
        self.ensure_agent_exists(agent_id)?;

        let mut state = self.state.lock().await;
        state.wake_budget_by_agent_id.insert(
            agent_id.to_string(),
            WakeBudgetState {
                active: true,
                started_at_ms: now_millis(),
                ..WakeBudgetState::default()
            },
        );
        Ok(())
    }

    pub async fn finish_wake(&self, agent_id: &str) -> Result<()> {
        self.ensure_agent_exists(agent_id)?;

        let mut state = self.state.lock().await;
        let budget_before_reset = state
            .wake_budget_by_agent_id
            .get(agent_id)
            .cloned()
            .unwrap_or_default();
        state
            .wake_budget_by_agent_id
            .insert(agent_id.to_string(), WakeBudgetState::default());

        if budget_before_reset.active {
            let elapsed_ms = now_millis().saturating_sub(budget_before_reset.started_at_ms);
            if elapsed_ms > self.wake_budget.max_wake_ms {
                bail!(
                    "wake budget exceeded: wake duration {}ms > {}ms",
                    elapsed_ms,
                    self.wake_budget.max_wake_ms
                );
            }
        }

        Ok(())
    }

    pub async fn register_wake_tool_call(&self, agent_id: &str, tool_name: &str) -> Result<()> {
        self.ensure_agent_exists(agent_id)?;

        let mut state = self.state.lock().await;
        let now = now_millis();
        let budget = state
            .wake_budget_by_agent_id
            .entry(agent_id.to_string())
            .or_default();

        if !budget.active {
            bail!("wake budget is not active for agent: {agent_id}");
        }

        let elapsed_ms = now.saturating_sub(budget.started_at_ms);
        if elapsed_ms > self.wake_budget.max_wake_ms {
            bail!(
                "wake budget exceeded: wake duration {}ms > {}ms",
                elapsed_ms,
                self.wake_budget.max_wake_ms
            );
        }

        if budget.total_tool_calls >= self.wake_budget.max_tool_calls_per_wake {
            bail!(
                "wake budget exceeded: max {} tool calls per wake",
                self.wake_budget.max_tool_calls_per_wake
            );
        }

        if tool_name == HUB_TOOL_GET_UNREAD
            && budget.get_unread_calls >= self.wake_budget.max_get_unread_per_wake
        {
            bail!(
                "wake budget exceeded: max {} {} calls per wake",
                self.wake_budget.max_get_unread_per_wake,
                HUB_TOOL_GET_UNREAD
            );
        }

        if tool_name == HUB_TOOL_POST && budget.post_calls >= self.wake_budget.max_post_per_wake {
            bail!(
                "wake budget exceeded: max {} {} calls per wake",
                self.wake_budget.max_post_per_wake,
                HUB_TOOL_POST
            );
        }

        budget.total_tool_calls += 1;
        if tool_name == HUB_TOOL_GET_UNREAD {
            budget.get_unread_calls += 1;
        }
        if tool_name == HUB_TOOL_POST {
            budget.post_calls += 1;
        }

        Ok(())
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

    #[tokio::test]
    async fn wake_budget_blocks_repeated_get_unread_and_resets_next_wake() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        kernel
            .append_message(MessageRole::Human, "Rex".into(), "hello".into(), None)
            .await
            .expect("append human");

        let hub = RoomHub::new(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        );

        hub.begin_wake("codex").await.expect("begin wake");
        hub.register_wake_tool_call("codex", HUB_TOOL_GET_UNREAD)
            .await
            .expect("first get");
        let err = hub
            .register_wake_tool_call("codex", HUB_TOOL_GET_UNREAD)
            .await
            .expect_err("second get should fail");
        assert!(format!("{err:#}").contains("max 1 hub_get_unread calls per wake"));
        hub.finish_wake("codex").await.expect("finish wake");

        hub.begin_wake("codex").await.expect("begin next wake");
        hub.register_wake_tool_call("codex", HUB_TOOL_GET_UNREAD)
            .await
            .expect("get in next wake");
    }

    #[tokio::test]
    async fn wake_budget_enforces_total_tool_calls() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let budget = WakeBudgetConfig {
            max_tool_calls_per_wake: 2,
            ..WakeBudgetConfig::default()
        };
        let hub = RoomHub::with_wake_budget(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
            budget,
        );

        hub.begin_wake("codex").await.expect("begin wake");
        hub.register_wake_tool_call("codex", HUB_TOOL_ACK)
            .await
            .expect("call 1");
        hub.register_wake_tool_call("codex", HUB_TOOL_ACK)
            .await
            .expect("call 2");
        let err = hub
            .register_wake_tool_call("codex", HUB_TOOL_ACK)
            .await
            .expect_err("call 3 should fail");
        assert!(format!("{err:#}").contains("max 2 tool calls per wake"));
    }

    #[tokio::test]
    async fn wake_budget_enforces_post_limit() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let hub = RoomHub::new(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        );

        hub.begin_wake("codex").await.expect("begin wake");
        hub.register_wake_tool_call("codex", HUB_TOOL_POST)
            .await
            .expect("first post call");
        let err = hub
            .register_wake_tool_call("codex", HUB_TOOL_POST)
            .await
            .expect_err("second post call should fail");
        assert!(format!("{err:#}").contains("max 1 hub_post calls per wake"));
    }

    #[tokio::test]
    async fn wake_budget_requires_active_wake() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let hub = RoomHub::new(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
        );

        let err = hub
            .register_wake_tool_call("codex", HUB_TOOL_ACK)
            .await
            .expect_err("missing begin_wake should fail");
        assert!(format!("{err:#}").contains("wake budget is not active"));
    }

    #[tokio::test]
    async fn wake_budget_enforces_max_wake_duration() {
        let tmp = tempfile::tempdir().expect("tmp");
        let kernel = Arc::new(SessionKernel::create_new(tmp.path()).await.expect("kernel"));
        let budget = WakeBudgetConfig {
            max_wake_ms: 1,
            ..WakeBudgetConfig::default()
        };
        let hub = RoomHub::with_wake_budget(
            kernel,
            &make_agents(),
            RateLimitConfig {
                window_ms: 60_000,
                max_posts: 6,
            },
            budget,
        );

        hub.begin_wake("codex").await.expect("begin wake");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let err = hub
            .register_wake_tool_call("codex", HUB_TOOL_ACK)
            .await
            .expect_err("expired wake should fail");
        assert!(format!("{err:#}").contains("wake duration"));
    }
}
