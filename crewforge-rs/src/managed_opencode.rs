use serde_json::{Value, json};

pub const MCP_SERVER_KEY: &str = "crewforge";
pub const HUB_GET_TOOL: &str = "crewforge_hub_get_unread";
pub const HUB_ACK_TOOL: &str = "crewforge_hub_ack";
pub const HUB_POST_TOOL: &str = "crewforge_hub_post";
const MCP_TIMEOUT_MS: u64 = 5000;
const DEFAULT_AGENT_STEPS: u64 = 8;
const READONLY_BASH_PATTERNS: &[&str] = &[
    "ls",
    "ls *",
    "cat",
    "cat *",
    "head",
    "head *",
    "tail",
    "tail *",
    "wc",
    "wc *",
    "rg",
    "rg *",
    "git diff",
    "git diff *",
    "git log",
    "git log *",
    "git show",
    "git show *",
    "git status",
    "git status *",
];

pub fn build_members(human: &str, agent_names: impl IntoIterator<Item = String>) -> String {
    std::iter::once(human.to_string())
        .chain(agent_names)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn build_managed_agent_prompt(
    agent_name: &str,
    members: &str,
    preference: Option<&str>,
) -> String {
    let mut prompt = format!(
        "You are {name} in a shared multi-agent room.\n\nMembers: {members}\n\nWhen taking a turn:\n1. Call {get_tool} at least once to fetch unread updates.\n2. Keep discussion anchored to the latest human topic.\n3. Do not switch to AGENTS.md, system prompt, or orchestration meta unless the human explicitly asks.\n4. If you have clear incremental value, call {post_tool} to publish one concise message.\n5. If you do not have clear incremental value, do not post.",
        name = agent_name,
        members = members,
        get_tool = HUB_GET_TOOL,
        post_tool = HUB_POST_TOOL,
    );

    if let Some(raw_preference) = preference {
        let preference = raw_preference.trim();
        if !preference.is_empty() {
            prompt.push_str("\n\nPreference:\n");
            prompt.push_str(preference);
        }
    }

    prompt
}

pub fn build_managed_permission(allow_edit: bool) -> Value {
    let mut bash = serde_json::Map::new();
    for pattern in READONLY_BASH_PATTERNS {
        bash.insert((*pattern).to_string(), json!("allow"));
    }

    let mut permission = json!({
        "*": "deny",
        HUB_GET_TOOL: "allow",
        HUB_ACK_TOOL: "allow",
        HUB_POST_TOOL: "allow",
        "read": "allow",
        "grep": "allow",
        "glob": "allow",
        "bash": bash,
        "webfetch": "allow",
        "websearch": "allow",
        "doom_loop": "deny",
        "question": "deny",
        "plan_enter": "deny",
        "plan_exit": "deny",
    });

    if allow_edit && let Some(obj) = permission.as_object_mut() {
        obj.insert("edit".to_string(), json!("allow"));
    }

    permission
}

pub fn build_managed_opencode_config(
    runtime_agent_name: &str,
    agent_name: &str,
    members: &str,
    mcp_url: &str,
    allow_edit: bool,
    preference: Option<&str>,
) -> Value {
    json!({
      "$schema": "https://opencode.ai/config.json",
      "mcp": {
        MCP_SERVER_KEY: {
          "type": "remote",
          "url": mcp_url,
          "timeout": MCP_TIMEOUT_MS
        }
      },
      "agent": {
        runtime_agent_name: {
          "description": "CrewForge managed room participant agent",
          "prompt": build_managed_agent_prompt(agent_name, members, preference),
          "steps": DEFAULT_AGENT_STEPS,
          "permission": build_managed_permission(allow_edit)
        }
      }
    })
}

pub fn upsert_mcp_endpoint(config: &mut Value, mcp_url: &str) -> bool {
    let Some(root) = config.as_object_mut() else {
        return false;
    };

    let mcp_value = root
        .entry("mcp".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !mcp_value.is_object() {
        *mcp_value = Value::Object(serde_json::Map::new());
    }
    let mcp_obj = mcp_value
        .as_object_mut()
        .expect("mcp value should be object");

    let server_value = mcp_obj
        .entry(MCP_SERVER_KEY.to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !server_value.is_object() {
        *server_value = Value::Object(serde_json::Map::new());
    }
    let server_obj = server_value
        .as_object_mut()
        .expect("mcp server value should be object");

    server_obj.insert("type".to_string(), json!("remote"));
    server_obj.insert("url".to_string(), json!(mcp_url));
    server_obj.insert("timeout".to_string(), json!(MCP_TIMEOUT_MS));
    true
}

pub fn upsert_runtime_agent_permission(
    config: &mut Value,
    runtime_agent_name: &str,
    allow_edit: bool,
) -> bool {
    let Some(runtime_agent_obj) = config
        .get_mut("agent")
        .and_then(|value| value.as_object_mut())
        .and_then(|agent| agent.get_mut(runtime_agent_name))
        .and_then(|value| value.as_object_mut())
    else {
        return false;
    };

    runtime_agent_obj.insert(
        "permission".to_string(),
        build_managed_permission(allow_edit),
    );
    true
}

pub fn upsert_runtime_agent_steps(config: &mut Value, runtime_agent_name: &str) -> bool {
    let Some(runtime_agent_obj) = config
        .get_mut("agent")
        .and_then(|value| value.as_object_mut())
        .and_then(|agent| agent.get_mut(runtime_agent_name))
        .and_then(|value| value.as_object_mut())
    else {
        return false;
    };

    runtime_agent_obj.insert("steps".to_string(), json!(DEFAULT_AGENT_STEPS));
    true
}

pub fn upsert_runtime_agent_prompt(
    config: &mut Value,
    runtime_agent_name: &str,
    agent_name: &str,
    members: &str,
    preference: Option<&str>,
) -> bool {
    let Some(runtime_agent_obj) = config
        .get_mut("agent")
        .and_then(|value| value.as_object_mut())
        .and_then(|agent| agent.get_mut(runtime_agent_name))
        .and_then(|value| value.as_object_mut())
    else {
        return false;
    };

    runtime_agent_obj.insert(
        "prompt".to_string(),
        json!(build_managed_agent_prompt(agent_name, members, preference)),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_permission_contains_readonly_bash_allowlist() {
        let permission = build_managed_permission(false);
        let bash = permission
            .get("bash")
            .and_then(|value| value.as_object())
            .expect("bash permission object");

        for pattern in READONLY_BASH_PATTERNS {
            assert_eq!(bash.get(*pattern), Some(&json!("allow")));
        }
        assert!(permission.get("edit").is_none());
        assert_eq!(permission.get("doom_loop"), Some(&json!("deny")));
    }

    #[test]
    fn managed_permission_adds_edit_when_requested() {
        let permission = build_managed_permission(true);
        assert_eq!(permission.get("edit"), Some(&json!("allow")));
    }

    #[test]
    fn readonly_bash_patterns_do_not_allow_shell_chaining_tokens() {
        for pattern in READONLY_BASH_PATTERNS {
            assert!(
                !pattern.contains("&&")
                    && !pattern.contains(';')
                    && !pattern.contains('|')
                    && !pattern.contains("`")
                    && !pattern.contains("$("),
                "pattern should stay single-command only: {pattern}"
            );
        }
    }

    #[test]
    fn upsert_runtime_agent_prompt_refreshes_members_and_preference() {
        let mut config = build_managed_opencode_config(
            "brainstorm-room",
            "Gemini",
            "Rex, Gemini, Claude",
            "http://127.0.0.1:1/mcp?token=old",
            false,
            None,
        );

        let updated = upsert_runtime_agent_prompt(
            &mut config,
            "brainstorm-room",
            "Gemini",
            "Rex, Gemini, Kimi",
            Some("Prefer concise replies"),
        );
        assert!(updated);

        let prompt = config
            .get("agent")
            .and_then(|value| value.get("brainstorm-room"))
            .and_then(|value| value.get("prompt"))
            .and_then(|value| value.as_str())
            .expect("prompt");
        assert!(prompt.contains("Members: Rex, Gemini, Kimi"));
        assert!(prompt.contains("Preference:\nPrefer concise replies"));
        assert!(!prompt.contains("Members: Rex, Gemini, Claude"));
    }

    #[test]
    fn managed_config_includes_default_steps() {
        let config = build_managed_opencode_config(
            "brainstorm-room",
            "Gemini",
            "Rex, Gemini",
            "http://127.0.0.1:1/mcp?token=old",
            false,
            None,
        );
        assert_eq!(
            config
                .get("agent")
                .and_then(|value| value.get("brainstorm-room"))
                .and_then(|value| value.get("steps")),
            Some(&json!(8))
        );
    }

    #[test]
    fn upsert_runtime_agent_steps_sets_default_steps() {
        let mut config = build_managed_opencode_config(
            "brainstorm-room",
            "Gemini",
            "Rex, Gemini",
            "http://127.0.0.1:1/mcp?token=old",
            false,
            None,
        );
        config
            .get_mut("agent")
            .and_then(|value| value.get_mut("brainstorm-room"))
            .and_then(|value| value.as_object_mut())
            .expect("agent object")
            .remove("steps");

        let updated = upsert_runtime_agent_steps(&mut config, "brainstorm-room");
        assert!(updated);
        assert_eq!(
            config
                .get("agent")
                .and_then(|value| value.get("brainstorm-room"))
                .and_then(|value| value.get("steps")),
            Some(&json!(8))
        );
    }
}
