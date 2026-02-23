use std::process::Command;

use anyhow::{Context, Result};

#[test]
fn cli_help_lists_primary_subcommands() -> Result<()> {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .arg("--help")
        .output()
        .context("failed to run crewforge --help")?;
    assert!(output.status.success(), "help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("init"));
    assert!(stdout.contains("chat"));
    Ok(())
}

#[test]
fn chat_help_exposes_config_resume_dry_run_and_rpc_flags() -> Result<()> {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .args(["chat", "--help"])
        .output()
        .context("failed to run crewforge chat --help")?;
    assert!(output.status.success(), "chat help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--config"));
    assert!(stdout.contains("--resume"));
    assert!(stdout.contains("--dry-run"));
    assert!(stdout.contains("--rpc"));
    Ok(())
}

#[test]
fn init_help_exposes_delete_flag() -> Result<()> {
    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .args(["init", "--help"])
        .output()
        .context("failed to run crewforge init --help")?;
    assert!(output.status.success(), "init help command should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--delete"));
    Ok(())
}
