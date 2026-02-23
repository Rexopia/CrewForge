use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result};

fn write_fake_opencode(root: &Path) -> Result<PathBuf> {
    let script_path = root.join("fake-opencode.sh");
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "models" ]; then
  printf "openai/gpt-5.3-codex\n"
  printf "kimi-for-coding/kimi-k2-thinking\n"
  exit 0
fi
echo "unsupported command" >&2
exit 2
"#;
    fs::write(&script_path, script)?;
    let mut perms = fs::metadata(&script_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms)?;
    Ok(script_path)
}

fn write_failing_opencode(root: &Path) -> Result<PathBuf> {
    let script_path = root.join("fake-opencode-fail.sh");
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
echo "boom" >&2
exit 7
"#;
    fs::write(&script_path, script)?;
    let mut perms = fs::metadata(&script_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms)?;
    Ok(script_path)
}

fn write_empty_models_opencode(root: &Path) -> Result<PathBuf> {
    let script_path = root.join("fake-opencode-empty.sh");
    let script = r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "models" ]; then
  exit 0
fi
echo "unsupported command" >&2
exit 2
"#;
    fs::write(&script_path, script)?;
    let mut perms = fs::metadata(&script_path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms)?;
    Ok(script_path)
}

fn run_init_with_input(
    workdir: &Path,
    home: &Path,
    opencode_command: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<Output> {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"));
    command
        .current_dir(workdir)
        .arg("init")
        .args(args)
        .env("HOME", home)
        .env("CREWFORGE_OPENCODE_COMMAND", opencode_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().context("failed to spawn crewforge init")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input)?;
    }

    let output = child
        .wait_with_output()
        .context("failed waiting crewforge init")?;
    Ok(output)
}

#[test]
fn init_adds_global_profile_from_opencode_models() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fake_opencode = write_fake_opencode(temp.path())?;

    let output = run_init_with_input(
        temp.path(),
        temp.path(),
        &fake_opencode,
        &[],
        b"1\nCodex\n\n",
    )?;
    if !output.status.success() {
        anyhow::bail!(
            "crewforge init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let profiles_path = temp.path().join(".crewforge/profiles.json");
    assert!(profiles_path.exists(), "profiles.json should be created");

    let text = fs::read_to_string(&profiles_path)?;
    assert!(text.contains("\"name\": \"Codex\""));
    assert!(text.contains("\"model\": \"openai/gpt-5.3-codex\""));
    assert!(text.contains("\"preference\": null"));
    assert!(!text.contains("\"version\""));
    Ok(())
}

#[test]
fn init_delete_removes_profile_by_name() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fake_opencode = write_fake_opencode(temp.path())?;

    let add_output = run_init_with_input(
        temp.path(),
        temp.path(),
        &fake_opencode,
        &[],
        b"1\nCodex\n\n",
    )?;
    if !add_output.status.success() {
        anyhow::bail!(
            "crewforge init add failed: {}",
            String::from_utf8_lossy(&add_output.stderr)
        );
    }

    let delete_output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .arg("init")
        .arg("--delete")
        .arg("Codex")
        .output()
        .context("failed to run crewforge init --delete")?;
    if !delete_output.status.success() {
        anyhow::bail!(
            "crewforge init --delete failed: {}",
            String::from_utf8_lossy(&delete_output.stderr)
        );
    }

    let profiles_path = temp.path().join(".crewforge/profiles.json");
    let text = fs::read_to_string(profiles_path)?;
    assert!(text.contains("\"profiles\": []"));
    Ok(())
}

#[test]
fn init_delete_trims_name_argument() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fake_opencode = write_fake_opencode(temp.path())?;

    let add_output = run_init_with_input(
        temp.path(),
        temp.path(),
        &fake_opencode,
        &[],
        b"1\nCodex\n\n",
    )?;
    if !add_output.status.success() {
        anyhow::bail!(
            "crewforge init add failed: {}",
            String::from_utf8_lossy(&add_output.stderr)
        );
    }

    let delete_output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .arg("init")
        .arg("--delete")
        .arg("  Codex  ")
        .output()
        .context("failed to run crewforge init --delete with trimmed name")?;
    if !delete_output.status.success() {
        anyhow::bail!(
            "crewforge init --delete failed: {}",
            String::from_utf8_lossy(&delete_output.stderr)
        );
    }

    let profiles_path = temp.path().join(".crewforge/profiles.json");
    let text = fs::read_to_string(profiles_path)?;
    assert!(text.contains("\"profiles\": []"));
    Ok(())
}

#[test]
fn init_delete_missing_profile_returns_error() -> Result<()> {
    let temp = tempfile::tempdir()?;

    let output = Command::new(assert_cmd::cargo::cargo_bin!("crewforge"))
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .arg("init")
        .arg("--delete")
        .arg("Nope")
        .output()
        .context("failed to run crewforge init --delete for missing profile")?;

    assert!(
        !output.status.success(),
        "delete should fail for missing profile"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("profile not found"));
    Ok(())
}

#[test]
fn init_reports_opencode_models_failure_reason() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fake_opencode = write_failing_opencode(temp.path())?;

    let output = run_init_with_input(
        temp.path(),
        temp.path(),
        &fake_opencode,
        &[],
        b"1\nCodex\n\n",
    )?;

    assert!(
        !output.status.success(),
        "init should fail when `opencode models` fails"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`"));
    assert!(stderr.contains("models"));
    assert!(stderr.contains("boom"));
    Ok(())
}

#[test]
fn init_fails_when_opencode_models_returns_empty_list() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let fake_opencode = write_empty_models_opencode(temp.path())?;

    let output = run_init_with_input(
        temp.path(),
        temp.path(),
        &fake_opencode,
        &[],
        b"1\nCodex\n\n",
    )?;

    assert!(
        !output.status.success(),
        "init should fail when no models are returned"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("returned no models"));
    Ok(())
}
