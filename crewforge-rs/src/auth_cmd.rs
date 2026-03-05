//! `crewforge auth` subcommand — OAuth login, token management, and profile listing.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use crewforge::auth::{self, AuthService, default_state_dir, normalize_provider};
use std::path::PathBuf;

// ── Clap definitions ──────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    /// Login with OAuth (openai-codex)
    Login {
        /// Provider: openai-codex
        #[arg(long)]
        provider: String,

        /// Profile name [default: default]
        #[arg(long, default_value = "default")]
        profile: String,
    },

    /// Save an API key or bearer token for a provider
    PasteToken {
        /// Provider: anthropic, openai, etc.
        #[arg(long)]
        provider: String,

        /// Profile name [default: default]
        #[arg(long, default_value = "default")]
        profile: String,

        /// Token value (read interactively if omitted)
        #[arg(long)]
        token: Option<String>,
    },

    /// Refresh an OAuth access token using the stored refresh token
    Refresh {
        /// Provider: openai-codex
        #[arg(long)]
        provider: String,

        /// Profile name or profile id
        #[arg(long)]
        profile: Option<String>,
    },

    /// Remove an auth profile
    Logout {
        /// Provider
        #[arg(long)]
        provider: String,

        /// Profile name [default: default]
        #[arg(long, default_value = "default")]
        profile: String,
    },

    /// Set the active profile for a provider
    Use {
        /// Provider
        #[arg(long)]
        provider: String,

        /// Profile name or full profile id
        #[arg(long)]
        profile: String,
    },

    /// List all auth profiles
    List,

    /// Show auth status with active profile and token expiry info
    Status,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn auth_state_dir() -> PathBuf {
    default_state_dir()
}

fn secrets_encrypt() -> bool {
    std::env::var("CREWFORGE_SECRETS_ENCRYPT")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}

fn make_auth_service() -> AuthService {
    AuthService::new(&auth_state_dir(), secrets_encrypt())
}

// ── Terminal input ─────────────────────────────────────────────────────────────

/// Read a sensitive value with echo masking. Falls back to plain stdin if not a TTY.
fn read_masked_input(prompt: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};

    if std::io::stdin().is_terminal() {
        let value = cliclack::password(prompt).interact()?;
        return Ok(value.trim().to_string());
    }

    // Non-interactive: just read a line from stdin.
    eprint!("{prompt}: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

// ── Small utilities ────────────────────────────────────────────────────────────

fn extract_openai_account_id(token_set: &auth::profiles::TokenSet) -> Option<String> {
    let id = auth::openai_oauth::extract_account_id_from_tokens(
        token_set.id_token.as_deref(),
        &token_set.access_token,
    );
    if id.is_none() {
        tracing::warn!(
            "Could not extract OpenAI account id from OAuth tokens; \
             requests may fail until re-authentication."
        );
    }
    id
}

fn redact(value: &str) -> String {
    if value.len() <= 4 {
        "***".to_string()
    } else {
        format!("{}***", &value[..4])
    }
}

fn format_expiry(profile: &auth::profiles::AuthProfile) -> String {
    match profile.token_set.as_ref().and_then(|ts| ts.expires_at) {
        Some(ts) => {
            let now: DateTime<Utc> = Utc::now();
            if ts <= now {
                format!("expired at {}", ts.to_rfc3339())
            } else {
                let mins = (ts - now).num_minutes();
                format!("expires in {mins}m ({})", ts.to_rfc3339())
            }
        }
        None => "n/a".to_string(),
    }
}

// ── Public command entry point ────────────────────────────────────────────────

pub async fn run(cmd: AuthCommands) -> Result<()> {
    match cmd {
        AuthCommands::Login { provider, profile } => run_login(provider, profile).await,

        AuthCommands::PasteToken {
            provider,
            profile,
            token,
        } => run_paste_token(provider, profile, token).await,

        AuthCommands::Refresh { provider, profile } => run_refresh(provider, profile).await,

        AuthCommands::Logout { provider, profile } => run_logout(provider, profile).await,

        AuthCommands::Use { provider, profile } => run_use(provider, profile).await,

        AuthCommands::List => run_list().await,

        AuthCommands::Status => run_status().await,
    }
}

// ── Login ─────────────────────────────────────────────────────────────────────

async fn run_login(provider: String, profile: String) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();
    let client = reqwest::Client::new();

    match provider.as_str() {
        "openai-codex" => {
            let device = auth::openai_oauth::start_device_code_flow(&client).await?;
            println!("OpenAI device authorization started.");
            println!("Visit: {}", device.verification_uri);
            println!("Code:  {}", device.user_code);
            println!();
            println!("Waiting for authorization...");
            let token_set = auth::openai_oauth::poll_device_code_tokens(&client, &device).await?;
            let account_id = extract_openai_account_id(&token_set);
            svc.store_openai_tokens(&profile, token_set, account_id, true)
                .await?;
            println!("Saved profile {profile}");
            println!("Active profile for openai-codex: {profile}");
            Ok(())
        }
        _ => bail!("`auth login` supports --provider openai-codex, got: {provider}"),
    }
}

// ── Paste token ───────────────────────────────────────────────────────────────

async fn run_paste_token(provider: String, profile: String, token: Option<String>) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => read_masked_input("Paste token")?,
    };
    if token.is_empty() {
        bail!("Token cannot be empty");
    }

    let svc = make_auth_service();
    svc.store_provider_token(
        &provider,
        &profile,
        &token,
        std::collections::HashMap::new(),
        true,
    )
    .await?;
    println!("Saved profile {profile}");
    println!("Active profile for {provider}: {profile}");
    Ok(())
}

// ── Refresh ───────────────────────────────────────────────────────────────────

async fn run_refresh(provider: String, profile: Option<String>) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();

    match provider.as_str() {
        "openai-codex" => {
            match svc
                .get_valid_openai_access_token(profile.as_deref())
                .await?
            {
                Some(_) => println!("OpenAI Codex token is valid (refresh completed if needed)."),
                None => bail!(
                    "No OpenAI Codex auth profile found. \
                     Run `crewforge auth login --provider openai-codex`."
                ),
            }
        }
        _ => bail!("`auth refresh` supports --provider openai-codex, got: {provider}"),
    }
    Ok(())
}

// ── Logout ────────────────────────────────────────────────────────────────────

async fn run_logout(provider: String, profile: String) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();
    let removed = svc.remove_profile(&provider, &profile).await?;
    if removed {
        println!("Removed auth profile {provider}:{profile}");
    } else {
        println!("Auth profile not found: {provider}:{profile}");
    }
    Ok(())
}

// ── Use ───────────────────────────────────────────────────────────────────────

async fn run_use(provider: String, profile: String) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();
    svc.set_active_profile(&provider, &profile).await?;
    println!("Active profile for {provider}: {profile}");
    Ok(())
}

// ── List ──────────────────────────────────────────────────────────────────────

async fn run_list() -> Result<()> {
    let svc = make_auth_service();
    let data = svc.load_profiles().await?;
    if data.profiles.is_empty() {
        println!("No auth profiles configured.");
        return Ok(());
    }
    for (id, profile) in &data.profiles {
        let active = data
            .active_profiles
            .get(&profile.provider)
            .is_some_and(|a| a == id);
        let marker = if active { "*" } else { " " };
        println!("{marker} {id}");
    }
    Ok(())
}

// ── Status ────────────────────────────────────────────────────────────────────

async fn run_status() -> Result<()> {
    let svc = make_auth_service();
    let data = svc.load_profiles().await?;
    if data.profiles.is_empty() {
        println!("No auth profiles configured.");
        return Ok(());
    }

    for (id, profile) in &data.profiles {
        let active = data
            .active_profiles
            .get(&profile.provider)
            .is_some_and(|a| a == id);
        let marker = if active { "*" } else { " " };
        println!(
            "{marker} {} kind={:?} account={} expires={}",
            id,
            profile.kind,
            redact(profile.account_id.as_deref().unwrap_or("unknown")),
            format_expiry(profile),
        );
    }

    println!();
    println!("Active profiles:");
    for (provider, profile_id) in &data.active_profiles {
        println!("  {provider}: {profile_id}");
    }
    Ok(())
}
