use std::io::IsTerminal;
use std::process::Stdio;

use semver::Version;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

const CREWFORGE_PACKAGE_NAME: &str = "crewforge";
const DEFAULT_NPM_COMMAND: &str = "npm";
const UPDATE_CHECK_TIMEOUT_MS: u64 = 1_200;
const DISABLE_UPDATE_CHECK_ENV: &str = "CREWFORGE_NO_UPDATE_CHECK";

pub async fn maybe_print_update_notice() {
    if !std::io::stdout().is_terminal() || update_check_disabled() {
        return;
    }

    let Some(latest) = latest_npm_version().await else {
        return;
    };
    let current = env!("CARGO_PKG_VERSION");
    if !is_newer_version(current, &latest) {
        return;
    }

    eprintln!(
        "[update] New crewforge version {latest} is available (current {current}). Run `npm i -g crewforge` to update."
    );
}

async fn latest_npm_version() -> Option<String> {
    let command = std::env::var("CREWFORGE_NPM_COMMAND")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_NPM_COMMAND.to_string());

    let output = timeout(
        Duration::from_millis(UPDATE_CHECK_TIMEOUT_MS),
        Command::new(command)
            .arg("view")
            .arg(CREWFORGE_PACKAGE_NAME)
            .arg("version")
            .arg("--silent")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    first_non_empty_line(&String::from_utf8_lossy(&output.stdout)).map(str::to_string)
}

fn first_non_empty_line(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let line = line.trim();
        if line.is_empty() { None } else { Some(line) }
    })
}

fn update_check_disabled() -> bool {
    let Some(value) = std::env::var(DISABLE_UPDATE_CHECK_ENV).ok() else {
        return false;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes"
    )
}

fn is_newer_version(current: &str, latest: &str) -> bool {
    let Some(current) = Version::parse(current).ok() else {
        return false;
    };
    let Some(latest) = Version::parse(latest).ok() else {
        return false;
    };
    latest > current
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn first_non_empty_line_skips_blanks() {
        assert_eq!(first_non_empty_line("\n  \n0.2.0\n"), Some("0.2.0"));
    }

    #[test]
    fn newer_semver_detected_correctly() {
        assert!(is_newer_version("0.1.3", "0.1.4"));
        assert!(is_newer_version("0.1.3", "0.2.0"));
        assert!(!is_newer_version("0.1.3", "0.1.3"));
        assert!(!is_newer_version("0.2.0", "0.1.9"));
        assert!(!is_newer_version("dev", "0.1.4"));
        assert!(!is_newer_version("0.1.3", "latest"));
    }

    #[test]
    fn update_check_disabled_accepts_truthy_values() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        for value in ["1", "true", "TRUE", "yes", " YeS "] {
            // SAFETY: tests guard process-wide env mutation with a global mutex.
            unsafe {
                std::env::set_var(DISABLE_UPDATE_CHECK_ENV, value);
            }
            assert!(update_check_disabled(), "value should disable: {value}");
        }
        // SAFETY: tests guard process-wide env mutation with a global mutex.
        unsafe {
            std::env::remove_var(DISABLE_UPDATE_CHECK_ENV);
        }
    }

    #[test]
    fn update_check_disabled_ignores_false_or_empty_values() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        for value in ["0", "false", "no", "maybe", ""] {
            // SAFETY: tests guard process-wide env mutation with a global mutex.
            unsafe {
                std::env::set_var(DISABLE_UPDATE_CHECK_ENV, value);
            }
            assert!(
                !update_check_disabled(),
                "value should not disable: {value}"
            );
        }
        // SAFETY: tests guard process-wide env mutation with a global mutex.
        unsafe {
            std::env::remove_var(DISABLE_UPDATE_CHECK_ENV);
        }
        assert!(!update_check_disabled());
    }
}
