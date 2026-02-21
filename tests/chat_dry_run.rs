use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

fn write_room_config(root: &std::path::Path) -> Result<()> {
    let room_dir = root.join(".room");
    fs::create_dir_all(room_dir.join("sessions"))?;

    let config = serde_json::json!({
        "roomName": "brainstorm",
        "human": "Rex",
        "runtime": {
            "schedulerMode": "event_loop",
            "eventLoop": {
                "gatherIntervalMs": 20
            },
            "rateLimit": {
                "windowMs": 60000,
                "maxPosts": 6
            }
        },
        "opencode": {
            "command": "opencode",
            "timeoutMs": 240000,
            "runtimeAgentName": "brainstorm-room"
        },
        "agents": [
            {
                "id": "codex",
                "name": "Codex",
                "model": "openai/gpt-5.3-codex",
                "contextDir": ".room/agents/codex",
                "tools": { "edit": false, "write": false }
            },
            {
                "id": "kimi",
                "name": "Kimi",
                "model": "kimi-for-coding/kimi-k2-thinking",
                "contextDir": ".room/agents/kimi",
                "tools": { "edit": false, "write": false }
            }
        ]
    });

    fs::write(
        room_dir.join("room.json"),
        format!("{}\n", serde_json::to_string_pretty(&config)?),
    )?;
    Ok(())
}

fn run_chat_with_input(
    workdir: &std::path::Path,
    args: &[&str],
    input: &[u8],
    wait_before_close_ms: u64,
) -> Result<String> {
    let mut command_args = vec!["chat", "--dry-run"];
    command_args.extend_from_slice(args);

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(workdir)
        .args(command_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn crewforge")?;

    let stdin = child.stdin.as_mut().context("stdin unavailable")?;
    stdin.write_all(input)?;
    thread::sleep(Duration::from_millis(wait_before_close_ms));

    let output = child.wait_with_output().context("failed waiting process")?;
    if !output.status.success() {
        anyhow::bail!(
            "crewforge chat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[test]
fn dry_run_chat_generates_session_log() -> Result<()> {
    let temp = tempfile::tempdir()?;
    write_room_config(temp.path())?;

    let mut child = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .arg("chat")
        .arg("--dry-run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn crewforge")?;

    let stdin = child.stdin.as_mut().context("stdin unavailable")?;
    stdin.write_all(b"hello\n")?;
    thread::sleep(Duration::from_millis(350));
    stdin.write_all(b"/exit\n")?;

    let output = child.wait_with_output().context("failed waiting process")?;
    if !output.status.success() {
        anyhow::bail!(
            "crewforge chat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let sessions_dir = temp.path().join(".room/sessions");
    let entries = fs::read_dir(&sessions_dir)?.collect::<Vec<_>>();
    assert!(!entries.is_empty(), "session file should be created");

    let mut found_human_line = false;
    for entry in entries {
        let entry = entry?;
        let content = fs::read_to_string(entry.path())?;
        if content
            .lines()
            .any(|line| line.contains("\"role\":\"human\""))
        {
            found_human_line = true;
            break;
        }
    }
    assert!(found_human_line, "session should contain human message");

    Ok(())
}

#[test]
fn help_command_mentions_crewforge_chat() -> Result<()> {
    let temp = tempfile::tempdir()?;
    write_room_config(temp.path())?;

    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .arg("chat")
        .arg("--dry-run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("failed to run crewforge")?;

    if !output.status.success() {
        anyhow::bail!(
            "crewforge chat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // no stdin means immediate EOF; still prints startup lines and exits
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Type /help for commands."));
    Ok(())
}

#[test]
fn chat_resume_appends_same_session_file() -> Result<()> {
    let temp = tempfile::tempdir()?;
    write_room_config(temp.path())?;

    let _ = run_chat_with_input(temp.path(), &[], b"hello-first\n/exit\n", 250)?;

    let sessions_dir = temp.path().join(".room/sessions");
    let mut entries = fs::read_dir(&sessions_dir)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    assert_eq!(entries.len(), 1, "first run should create one session file");

    let session_path = entries[0].path();
    let session_id = session_path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("missing session id")?
        .to_string();

    let stdout = run_chat_with_input(
        temp.path(),
        &["--resume", &session_id],
        b"hello-second\n/exit\n",
        250,
    )?;
    assert!(
        stdout.contains("Session mode: resumed"),
        "stdout should indicate resumed mode"
    );
    assert!(
        stdout.contains("hello-first"),
        "resumed chat should render historical transcript in output"
    );

    let mut entries_after = fs::read_dir(&sessions_dir)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries_after.sort_by_key(|entry| entry.file_name());
    assert_eq!(
        entries_after.len(),
        1,
        "resume should append to existing file without creating a second session"
    );

    let content = fs::read_to_string(&session_path)?;
    assert!(content.contains("hello-first"));
    assert!(content.contains("hello-second"));
    Ok(())
}
