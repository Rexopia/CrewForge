use std::io::{ErrorKind, IsTerminal, Write};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use cliclack::{confirm, input, intro, log, outro, outro_cancel, select};
use tokio::process::Command;

use crate::profiles::{self, GlobalProfile};
use crate::prompt_theme;

const INIT_CANCELED_MESSAGE: &str = "init canceled by user";

#[derive(Debug, Clone)]
pub struct InitArgs {
    pub delete: Option<String>,
}

pub async fn run_init(args: InitArgs) -> Result<()> {
    crate::update::maybe_print_update_notice().await;

    let profiles_path = profiles::global_profiles_path()?;

    if let Some(name) = args.delete {
        return run_delete_profile(&profiles_path, &name).await;
    }

    run_add_profiles(&profiles_path).await
}

async fn run_delete_profile(profiles_path: &std::path::Path, raw_name: &str) -> Result<()> {
    let target_name = raw_name.trim();
    if target_name.is_empty() {
        bail!("--delete requires a non-empty profile name");
    }

    let mut profiles = profiles::load_profiles(profiles_path).await?;
    let before = profiles.len();
    profiles.retain(|item| item.name != target_name);
    if profiles.len() == before {
        bail!("profile not found: {target_name}");
    }

    profiles::write_profiles(profiles_path, &profiles).await?;
    println!("Deleted profile: {target_name}");
    println!("Global profiles: {}", profiles_path.display());
    Ok(())
}

async fn run_add_profiles(profiles_path: &std::path::Path) -> Result<()> {
    let mut profiles = profiles::load_profiles(profiles_path).await?;
    let models = load_models_from_opencode().await?;
    let interactive_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    if interactive_tty {
        prompt_theme::install_prompt_theme();
        if let Err(error) = intro("CrewForge Init") {
            return Err(error).context("failed showing init intro");
        }
        if !profiles.is_empty() {
            let note_body = existing_profiles_note_body(&profiles);
            if let Err(error) = cliclack::note("Existing Profiles", note_body) {
                return Err(error).context("failed showing existing profiles");
            }
        }

        let result = run_add_profiles_cliclack(profiles_path, &models, &mut profiles).await;
        match &result {
            Ok(()) => {
                let _ = outro(format!("Done. Total profiles: {}", profiles.len(),));
            }
            Err(error) if is_init_canceled(error) => {
                let _ = outro_cancel("Init canceled.");
            }
            Err(_) => {
                let _ = outro_cancel("Init failed.");
            }
        }
        return result;
    }

    println!("crewforge init");
    println!("Global profiles file: {}", path_for_display(profiles_path));
    if !profiles.is_empty() {
        println!(
            "Existing profiles: {}",
            profiles
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    run_add_profiles_plain(profiles_path, &models, &mut profiles).await?;

    println!(
        "Done. Total profiles: {} ({})",
        profiles.len(),
        path_for_display(profiles_path)
    );
    Ok(())
}

async fn run_add_profiles_plain(
    profiles_path: &std::path::Path,
    models: &[String],
    profiles: &mut Vec<GlobalProfile>,
) -> Result<()> {
    let model = prompt_select_model_plain(models)?;
    let name = prompt_profile_name_plain(profiles)?;
    let preference = prompt_optional_preference_plain()?;

    profiles.push(GlobalProfile {
        name: name.clone(),
        model: model.clone(),
        preference: preference.clone(),
    });
    profiles::write_profiles(profiles_path, profiles).await?;

    println!("Added profile: {name} -> {model}");
    if let Some(pref) = preference {
        println!("Preference: {pref}");
    } else {
        println!("Preference: (empty)");
    }
    Ok(())
}

async fn run_add_profiles_cliclack(
    profiles_path: &std::path::Path,
    models: &[String],
    profiles: &mut Vec<GlobalProfile>,
) -> Result<()> {
    loop {
        let model = prompt_select_model_cliclack(models)?;
        let name = prompt_profile_name_cliclack(profiles)?;
        let preference = prompt_optional_preference_cliclack()?;

        profiles.push(GlobalProfile {
            name: name.clone(),
            model: model.clone(),
            preference: preference.clone(),
        });
        profiles::write_profiles(profiles_path, profiles).await?;

        let _ = log::success(format!("Added profile: {name} -> {model}"));
        if let Some(pref) = &preference {
            let _ = log::info(format!("Preference: {pref}"));
        } else {
            let _ = log::info("Preference: (empty)");
        }

        let mut another = confirm("Add another profile?").initial_value(true);
        let should_continue = another.interact().map_err(prompt_error)?;
        if !should_continue {
            break;
        }
    }

    Ok(())
}

fn prompt_select_model_cliclack(models: &[String]) -> Result<String> {
    let _highlight = prompt_theme::filter_input_highlight_scope();
    let mut picker = select("Select model").filter_mode().max_rows(10);
    for model in models {
        picker = picker.item(model.clone(), model.clone(), "");
    }
    picker.interact().map_err(prompt_error)
}

fn prompt_profile_name_cliclack(existing: &[GlobalProfile]) -> Result<String> {
    prompt_theme::clear_filter_input_highlight();
    loop {
        let mut name_prompt = input("Profile name (unique)").placeholder("Codex");
        let candidate: String = name_prompt.interact().map_err(prompt_error)?;
        match profiles::ensure_name_available(existing, &candidate) {
            Ok(()) => return Ok(candidate.trim().to_string()),
            Err(error) => {
                let _ = log::error(format!("Invalid profile name: {error}"));
            }
        }
    }
}

fn prompt_optional_preference_cliclack() -> Result<Option<String>> {
    prompt_theme::clear_filter_input_highlight();
    let mut preference_prompt = input("Preference (optional)")
        .required(false)
        .placeholder("Press Enter to skip");
    let text: String = preference_prompt.interact().map_err(prompt_error)?;
    Ok(profiles::normalize_preference(&text))
}

async fn load_models_from_opencode() -> Result<Vec<String>> {
    let command =
        std::env::var("CREWFORGE_OPENCODE_COMMAND").unwrap_or_else(|_| "opencode".to_string());
    let output = Command::new(&command)
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to run `{command} models`"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let reason = first_non_empty_line(&stderr)
            .or_else(|| first_non_empty_line(&stdout))
            .unwrap_or_else(|| format!("exit {}", output.status.code().unwrap_or(-1)));
        bail!("`{command} models` failed: {reason}");
    }

    let models = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if models.is_empty() {
        bail!("`{command} models` returned no models");
    }
    Ok(models)
}

fn prompt_select_model_plain(models: &[String]) -> Result<String> {
    let mut filter = String::new();

    loop {
        let filtered = filtered_models(models, &filter);
        if filtered.is_empty() {
            println!("No models matched filter: {filter}");
        } else {
            println!();
            println!("Available models:");
            for (idx, model) in filtered.iter().enumerate() {
                println!("  {}. {}", idx + 1, model);
            }
        }

        let input = prompt_line_plain(
            "Select model number, or enter /search <keyword>, /clear to reset filter: ",
        )?;

        if input == "/clear" {
            filter.clear();
            continue;
        }
        if let Some(keyword) = input.strip_prefix("/search ") {
            filter = keyword.trim().to_lowercase();
            continue;
        }

        let selected = input.parse::<usize>().ok();
        let Some(idx) = selected else {
            println!("Invalid selection, expected a number or /search.");
            continue;
        };
        if idx == 0 || idx > filtered.len() {
            println!("Selection out of range.");
            continue;
        }
        return Ok(filtered[idx - 1].to_string());
    }
}

fn prompt_profile_name_plain(existing: &[GlobalProfile]) -> Result<String> {
    loop {
        let candidate = prompt_line_plain("Profile name (unique): ")?;
        match profiles::ensure_name_available(existing, &candidate) {
            Ok(()) => return Ok(candidate.trim().to_string()),
            Err(error) => println!("Invalid profile name: {error}"),
        }
    }
}

fn prompt_optional_preference_plain() -> Result<Option<String>> {
    let input = prompt_line_plain("Preference (optional, press Enter to skip): ")?;
    Ok(profiles::normalize_preference(&input))
}

fn prompt_line_plain(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;

    let mut line = String::new();
    let read = std::io::stdin()
        .read_line(&mut line)
        .context("failed to read stdin")?;
    if read == 0 {
        bail!("stdin closed");
    }

    Ok(line.trim().to_string())
}

fn filtered_models<'a>(models: &'a [String], filter: &str) -> Vec<&'a str> {
    let normalized_filter = filter.trim().to_lowercase();
    models
        .iter()
        .filter(|model| {
            normalized_filter.is_empty() || model.to_lowercase().contains(&normalized_filter)
        })
        .map(|model| model.as_str())
        .collect()
}

fn existing_profiles_note_body(profiles: &[GlobalProfile]) -> String {
    profiles
        .iter()
        .map(|profile| format!("{}[{}]", profile.name, profile.model))
        .collect::<Vec<_>>()
        .join("\n")
}

fn first_non_empty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn prompt_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == ErrorKind::Interrupted {
        anyhow!(INIT_CANCELED_MESSAGE)
    } else {
        anyhow!(error).context("interactive prompt failed")
    }
}

fn is_init_canceled(error: &anyhow::Error) -> bool {
    error.to_string().contains(INIT_CANCELED_MESSAGE)
}

fn path_for_display(path: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        if let Ok(relative) = path.strip_prefix(&home) {
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}
