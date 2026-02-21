use serde_json::{Value, json};

pub const MCP_SERVER_KEY: &str = "crewforge";
pub const HUB_GET_TOOL: &str = "crewforge_hub_get_unread";
pub const HUB_ACK_TOOL: &str = "crewforge_hub_ack";
pub const HUB_POST_TOOL: &str = "crewforge_hub_post";
const MCP_TIMEOUT_MS: u64 = 5000;

pub fn build_members(human: &str, agent_names: impl IntoIterator<Item = String>) -> String {
    std::iter::once(human.to_string())
        .chain(agent_names)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn build_managed_agent_prompt(agent_name: &str, members: &str) -> String {
    format!(
        "You are {name} in a shared multi-agent room.\n\nMembers: {members}\n\nWhen taking a turn:\n1. Call {get_tool} at least once to fetch unread updates.\n2. Keep discussion anchored to the latest human topic.\n3. Do not switch to AGENTS.md, system prompt, or orchestration meta unless the human explicitly asks.\n4. If you have clear incremental value, call {post_tool} to publish one concise message.\n5. If you do not have clear incremental value, do not post.",
        name = agent_name,
        members = members,
        get_tool = HUB_GET_TOOL,
        post_tool = HUB_POST_TOOL,
    )
}

pub fn build_managed_permission(allow_edit: bool) -> Value {
    let mut permission = json!({
        "*": "deny",
        HUB_GET_TOOL: "allow",
        HUB_ACK_TOOL: "allow",
        HUB_POST_TOOL: "allow",
        "read": "allow",
        "grep": "allow",
        "glob": "allow",
        "webfetch": "allow",
        "websearch": "allow",
        "question": "deny",
        "plan_enter": "deny",
        "plan_exit": "deny",
    });

    if allow_edit
        && let Some(obj) = permission.as_object_mut()
    {
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
          "prompt": build_managed_agent_prompt(agent_name, members),
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
