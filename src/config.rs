use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct RoomConfig {
    pub room_name: String,
    pub human: String,
    pub runtime: RuntimeConfig,
    pub opencode: OpencodeConfig,
    pub agents: Vec<AgentConfig>,
    pub workspace_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub scheduler_mode: String,
    pub event_loop: EventLoopConfig,
    pub rate_limit: RateLimitConfig,
    pub app_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EventLoopConfig {
    pub gather_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub window_ms: u64,
    pub max_posts: usize,
}

#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    pub command: String,
    pub timeout_ms: u64,
    pub runtime_agent_name: String,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub model: String,
    #[allow(dead_code)]
    pub context_dir: String,
    pub tools: AgentTools,
    pub preference: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentTools {
    pub edit: bool,
    pub write: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRoomConfig {
    room_name: Option<String>,
    human: Option<String>,
    agents: Option<Vec<RawAgentConfig>>,
    runtime: Option<RawRuntimeConfig>,
    opencode: Option<RawOpencodeConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRuntimeConfig {
    scheduler_mode: Option<String>,
    event_loop: Option<RawEventLoopConfig>,
    rate_limit: Option<RawRateLimitConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEventLoopConfig {
    gather_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRateLimitConfig {
    window_ms: Option<u64>,
    max_posts: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawOpencodeConfig {
    command: Option<String>,
    timeout_ms: Option<u64>,
    runtime_agent_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAgentConfig {
    id: Option<String>,
    name: Option<String>,
    model: Option<String>,
    context_dir: Option<String>,
    preference: Option<String>,
    tools: Option<RawAgentTools>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawAgentTools {
    edit: Option<bool>,
    write: Option<bool>,
}

pub fn load_room_config(config_path: &Path, workspace_dir: PathBuf) -> Result<RoomConfig> {
    let raw_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed reading room config: {}", config_path.display()))?;
    let raw: RawRoomConfig =
        serde_json::from_str(&raw_text).context("room config must be valid JSON object")?;

    let raw_agents = raw.agents.unwrap_or_default();
    if raw_agents.len() < 2 {
        bail!("room config requires at least 2 agents");
    }

    let mut agent_ids = HashSet::new();
    let mut agents = Vec::with_capacity(raw_agents.len());
    for item in raw_agents {
        let id = item.id.unwrap_or_default().trim().to_string();
        let name = item.name.unwrap_or_default().trim().to_string();
        let model = item.model.unwrap_or_default().trim().to_string();
        if id.is_empty() || name.is_empty() || model.is_empty() {
            bail!("each agent requires id, name, model");
        }
        if !agent_ids.insert(id.clone()) {
            bail!("duplicate agent id: {id}");
        }

        let tools = item.tools.unwrap_or_default();
        agents.push(AgentConfig {
            id: id.clone(),
            name,
            model,
            context_dir: item
                .context_dir
                .unwrap_or_else(|| format!(".room/agents/{id}")),
            tools: AgentTools {
                edit: tools.edit.unwrap_or(false),
                write: tools.write.unwrap_or(false),
            },
            preference: item
                .preference
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        });
    }

    let runtime_raw = raw.runtime.unwrap_or(RawRuntimeConfig {
        scheduler_mode: None,
        event_loop: None,
        rate_limit: None,
    });

    let scheduler_mode = runtime_raw
        .scheduler_mode
        .unwrap_or_else(|| "event_loop".to_string());
    if scheduler_mode != "event_loop" {
        bail!("unsupported runtime.schedulerMode: {scheduler_mode}. Only event_loop is allowed.");
    }

    let gather_interval_ms = positive_u64(
        runtime_raw
            .event_loop
            .and_then(|v| v.gather_interval_ms)
            .unwrap_or(5000),
        5000,
    );
    let window_ms = positive_u64(
        runtime_raw
            .rate_limit
            .as_ref()
            .and_then(|v| v.window_ms)
            .unwrap_or(60_000),
        60_000,
    );
    let max_posts = positive_u64(
        runtime_raw
            .rate_limit
            .as_ref()
            .and_then(|v| v.max_posts)
            .unwrap_or(6),
        6,
    ) as usize;

    let opencode_raw = raw.opencode.unwrap_or(RawOpencodeConfig {
        command: None,
        timeout_ms: None,
        runtime_agent_name: None,
    });

    Ok(RoomConfig {
        room_name: raw.room_name.unwrap_or_else(|| "brainstorm".to_string()),
        human: raw.human.unwrap_or_else(|| "Rex".to_string()),
        runtime: RuntimeConfig {
            scheduler_mode,
            event_loop: EventLoopConfig { gather_interval_ms },
            rate_limit: RateLimitConfig {
                window_ms,
                max_posts,
            },
            app_session_id: None,
        },
        opencode: OpencodeConfig {
            command: opencode_raw
                .command
                .unwrap_or_else(|| "opencode".to_string()),
            timeout_ms: positive_u64(opencode_raw.timeout_ms.unwrap_or(240_000), 240_000).max(1000),
            runtime_agent_name: opencode_raw
                .runtime_agent_name
                .unwrap_or_else(|| "brainstorm-room".to_string()),
        },
        agents,
        workspace_dir,
    })
}

fn positive_u64(value: u64, fallback: u64) -> u64 {
    if value == 0 { fallback } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_agent_ids() {
        let json = r#"{
          "agents": [
            {"id":"a","name":"A","model":"m"},
            {"id":"a","name":"B","model":"m"}
          ]
        }"#;
        let path = std::env::temp_dir().join(format!(
            "crewforge-config-test-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, json).expect("write config");
        let err = load_room_config(&path, std::env::temp_dir()).expect_err("should fail");
        std::fs::remove_file(&path).ok();
        assert!(err.to_string().contains("duplicate agent id"));
    }
}
