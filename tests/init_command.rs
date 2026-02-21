use std::fs;
use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn init_command_generates_room_config_and_managed_agent_configs() -> Result<()> {
    let temp = tempfile::tempdir()?;

    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .arg("init")
        .arg("--human")
        .arg("Rex")
        .arg("--agents")
        .arg("Codex,Kimi")
        .output()
        .context("failed to run crewforge init")?;

    if !output.status.success() {
        anyhow::bail!(
            "crewforge init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let room_config = temp.path().join(".room/room.json");
    assert!(room_config.exists(), "room config should be created");

    let codex_config = temp.path().join(".room/agents/codex/opencode.json");
    let kimi_config = temp.path().join(".room/agents/kimi/opencode.json");
    assert!(codex_config.exists(), "codex managed opencode config should exist");
    assert!(kimi_config.exists(), "kimi managed opencode config should exist");

    let room_text = fs::read_to_string(room_config)?;
    assert!(room_text.contains("\"roomName\": \"brainstorm\""));
    assert!(room_text.contains("\"id\": \"codex\""));
    assert!(room_text.contains("\"id\": \"kimi\""));

    let codex_text = fs::read_to_string(codex_config)?;
    assert!(codex_text.contains("\"brainstorm-room\""));
    assert!(codex_text.contains("crewforge_hub_get_unread"));

    Ok(())
}
