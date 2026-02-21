use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::managed_opencode;

#[derive(Debug, Clone)]
pub struct InitArgs {
    pub config_path: String,
    pub room_name: String,
    pub human: String,
    pub agents: String,
}

#[derive(Debug, Clone)]
struct InitAgent {
    id: String,
    name: String,
    model: String,
    context_dir: String,
}

const DEFAULT_RUNTIME_AGENT_NAME: &str = "brainstorm-room";
const INIT_PLACEHOLDER_MCP_URL: &str = "http://127.0.0.1:0/mcp?token=init";

pub async fn run_init(args: InitArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve current dir")?;

    let room_name = if args.room_name.trim().is_empty() {
        "brainstorm".to_string()
    } else {
        args.room_name.trim().to_string()
    };
    let human = if args.human.trim().is_empty() {
        "Rex".to_string()
    } else {
        args.human.trim().to_string()
    };

    let agent_names = args
        .agents
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();

    if agent_names.len() < 2 {
        bail!("At least 2 agents are required.");
    }

    let agents = build_agents(&agent_names);
    let config_path = cwd.join(Path::new(&args.config_path));

    tokio::fs::create_dir_all(
        config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("invalid config path"))?,
    )
    .await
    .context("failed creating config directory")?;
    tokio::fs::create_dir_all(cwd.join(".room/sessions"))
        .await
        .context("failed creating .room/sessions")?;
    tokio::fs::create_dir_all(cwd.join(".room/runtime"))
        .await
        .context("failed creating .room/runtime")?;

    initialize_managed_agent_configs(&cwd, &agents, &human).await?;

    let room_config = json!({
      "roomName": room_name,
      "human": human,
      "historyWindow": 18,
      "runtime": {
        "schedulerMode": "event_loop",
        "eventLoop": {
          "gatherIntervalMs": 5000
        },
        "rateLimit": {
          "windowMs": 60000,
          "maxPosts": 6
        }
      },
      "opencode": {
        "command": "opencode",
        "timeoutMs": 240000,
        "runtimeAgentName": DEFAULT_RUNTIME_AGENT_NAME
      },
      "agents": agents.iter().map(|agent| json!({
        "id": agent.id,
        "name": agent.name,
        "model": agent.model,
        "contextDir": agent.context_dir,
        "tools": {
          "edit": false,
          "write": false
        }
      })).collect::<Vec<_>>()
    });

    let config_text = format!("{}\n", serde_json::to_string_pretty(&room_config)?);
    tokio::fs::write(&config_path, config_text)
        .await
        .with_context(|| format!("failed writing room config: {}", config_path.display()))?;

    println!(
        "Initialized room config: {}",
        path_relative_or_absolute(&cwd, &config_path)
    );
    for agent in &agents {
        println!(
            "- {} [{}] -> {}/opencode.json",
            agent.name, agent.model, agent.context_dir
        );
    }

    Ok(())
}

fn build_agents(agent_names: &[String]) -> Vec<InitAgent> {
    let mut used_ids = HashSet::new();
    agent_names
        .iter()
        .map(|name| {
            let id = ensure_unique_agent_id(name, &mut used_ids);
            InitAgent {
                id: id.clone(),
                name: name.clone(),
                model: default_model_for_agent(name),
                context_dir: format!(".room/agents/{id}"),
            }
        })
        .collect()
}

async fn initialize_managed_agent_configs(cwd: &Path, agents: &[InitAgent], human: &str) -> Result<()> {
    let members = managed_opencode::build_members(
        human,
        agents.iter().map(|agent| agent.name.clone()),
    );

    for agent in agents {
        let abs_dir = cwd.join(&agent.context_dir);
        tokio::fs::create_dir_all(&abs_dir)
            .await
            .with_context(|| format!("failed creating agent dir: {}", abs_dir.display()))?;

        let config = managed_opencode::build_managed_opencode_config(
            DEFAULT_RUNTIME_AGENT_NAME,
            &agent.name,
            &members,
            INIT_PLACEHOLDER_MCP_URL,
            false,
        );
        let text = format!("{}\n", serde_json::to_string_pretty(&config)?);
        let config_file = abs_dir.join("opencode.json");
        tokio::fs::write(&config_file, text)
            .await
            .with_context(|| {
                format!(
                    "failed writing managed opencode config: {}",
                    config_file.display()
                )
            })?;
    }

    Ok(())
}

fn to_agent_id(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;

    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "agent".to_string()
    } else {
        out
    }
}

fn ensure_unique_agent_id(name: &str, used_ids: &mut HashSet<String>) -> String {
    let base_id = to_agent_id(name);
    let mut next_id = base_id.clone();
    let mut suffix = 2_u32;

    while used_ids.contains(&next_id) {
        next_id = format!("{base_id}-{suffix}");
        suffix += 1;
    }

    used_ids.insert(next_id.clone());
    next_id
}

fn default_model_for_agent(name: &str) -> String {
    match name.trim().to_lowercase().as_str() {
        "codex" => "openai/gpt-5.3-codex".to_string(),
        "gemini" => "openrouter/google/gemini-3.1-pro-preview".to_string(),
        "claude" => "openrouter/anthropic/claude-sonnet-4.6".to_string(),
        "kimi" => "kimi-for-coding/kimi-k2-thinking".to_string(),
        "glm" => "zhipuai-coding-plan/glm-4.7".to_string(),
        _ => "openai/gpt-5.3-codex".to_string(),
    }
}

fn path_relative_or_absolute(cwd: &Path, abs_path: &Path) -> String {
    match abs_path.strip_prefix(cwd) {
        Ok(rel) => rel.display().to_string(),
        Err(_) => abs_path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_agent_ids_are_generated() {
        let agents = build_agents(&["Codex".to_string(), "Codex".to_string()]);
        assert_eq!(agents[0].id, "codex");
        assert_eq!(agents[1].id, "codex-2");
    }
}
