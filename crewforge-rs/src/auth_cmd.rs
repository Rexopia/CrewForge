//! `crewforge auth` subcommand — OAuth login, token management, and profile listing.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use crewforge::auth::oauth_common::PkceState;
use crewforge::auth::{self, AuthService, default_state_dir, normalize_provider};
use crewforge::security::SecretStore;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Clap definitions ──────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    /// Login with OAuth (openai-codex or gemini) or save a bearer token (anthropic)
    Login {
        /// Provider: openai-codex, gemini, or anthropic
        #[arg(long)]
        provider: String,

        /// Profile name [default: default]
        #[arg(long, default_value = "default")]
        profile: String,

        /// Use device-code flow instead of browser redirect
        #[arg(long)]
        device_code: bool,
    },

    /// Complete OAuth by pasting the redirect URL or auth code (fallback for browser flow)
    PasteRedirect {
        /// Provider: gemini
        #[arg(long)]
        provider: String,

        /// Profile name [default: default]
        #[arg(long, default_value = "default")]
        profile: String,

        /// Full redirect URL or raw OAuth code (read interactively if omitted)
        #[arg(long)]
        input: Option<String>,
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

        /// Auth kind override: api-key or authorization
        #[arg(long)]
        auth_kind: Option<String>,
    },

    /// Refresh an OAuth access token using the stored refresh token
    Refresh {
        /// Provider: openai-codex or gemini
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
        .unwrap_or(false)
}

fn make_auth_service() -> AuthService {
    AuthService::new(&auth_state_dir(), secrets_encrypt())
}

fn make_secret_store() -> SecretStore {
    SecretStore::new(&auth_state_dir(), secrets_encrypt())
}

// ── Pending OAuth state (browser-flow fallback) ───────────────────────────────

struct PendingOAuthLogin {
    provider: String,
    profile: String,
    code_verifier: String,
    state: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingOAuthLoginFile {
    #[serde(default)]
    provider: Option<String>,
    profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_code_verifier: Option<String>,
    state: String,
    created_at: String,
}

fn pending_path(provider: &str) -> PathBuf {
    auth_state_dir().join(format!("auth-{provider}-pending.json"))
}

fn save_pending(pending: &PendingOAuthLogin) -> Result<()> {
    let path = pending_path(&pending.provider);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = make_secret_store();
    let encrypted = store.encrypt(&pending.code_verifier)?;
    let file = PendingOAuthLoginFile {
        provider: Some(pending.provider.clone()),
        profile: pending.profile.clone(),
        code_verifier: None,
        encrypted_code_verifier: Some(encrypted),
        state: pending.state.clone(),
        created_at: pending.created_at.clone(),
    };
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
    set_owner_only_permissions(&tmp)?;
    std::fs::rename(&tmp, &path)?;
    set_owner_only_permissions(&path)?;
    Ok(())
}

fn load_pending(provider: &str) -> Result<Option<PendingOAuthLogin>> {
    let path = pending_path(provider);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let file: PendingOAuthLoginFile = serde_json::from_slice(&bytes)?;
    let store = make_secret_store();
    let code_verifier = if let Some(enc) = file.encrypted_code_verifier {
        store.decrypt(&enc)?
    } else if let Some(plain) = file.code_verifier {
        plain
    } else {
        bail!("Pending {provider} login is missing code verifier");
    };
    Ok(Some(PendingOAuthLogin {
        provider: file.provider.unwrap_or_else(|| provider.to_string()),
        profile: file.profile,
        code_verifier,
        state: file.state,
        created_at: file.created_at,
    }))
}

fn clear_pending(provider: &str) {
    let path = pending_path(provider);
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_len(0);
        let _ = file.sync_all();
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
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

/// Read a plain visible value. Falls back to plain stdin if not a TTY.
fn read_plain_input(prompt: &str) -> Result<String> {
    use std::io::{IsTerminal, Write};

    if std::io::stdin().is_terminal() {
        let value: String = cliclack::input(prompt).interact()?;
        return Ok(value.trim().to_string());
    }

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
        AuthCommands::Login {
            provider,
            profile,
            device_code,
        } => run_login(provider, profile, device_code).await,

        AuthCommands::PasteRedirect {
            provider,
            profile,
            input,
        } => run_paste_redirect(provider, profile, input).await,

        AuthCommands::PasteToken {
            provider,
            profile,
            token,
            auth_kind,
        } => run_paste_token(provider, profile, token, auth_kind).await,

        AuthCommands::Refresh { provider, profile } => run_refresh(provider, profile).await,

        AuthCommands::Logout { provider, profile } => run_logout(provider, profile).await,

        AuthCommands::Use { provider, profile } => run_use(provider, profile).await,

        AuthCommands::List => run_list().await,

        AuthCommands::Status => run_status().await,
    }
}

// ── Login ─────────────────────────────────────────────────────────────────────

async fn run_login(provider: String, profile: String, device_code: bool) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();
    let client = reqwest::Client::new();

    match provider.as_str() {
        "gemini" => run_gemini_login(&svc, &client, &profile, device_code).await,
        "openai-codex" => run_openai_login(&svc, &client, &profile).await,
        _ => bail!("`auth login` supports --provider openai-codex or gemini, got: {provider}"),
    }
}

async fn run_gemini_login(
    svc: &AuthService,
    client: &reqwest::Client,
    profile: &str,
    device_code: bool,
) -> Result<()> {
    if device_code {
        match auth::gemini_oauth::start_device_code_flow(client).await {
            Ok(device) => {
                println!("Google/Gemini device-code login started.");
                println!("Visit: {}", device.verification_uri);
                println!("Code:  {}", device.user_code);
                if let Some(uri) = &device.verification_uri_complete {
                    println!("Fast link: {uri}");
                }
                let token_set =
                    auth::gemini_oauth::poll_device_code_tokens(client, &device).await?;
                let account_id = token_set
                    .id_token
                    .as_deref()
                    .and_then(auth::gemini_oauth::extract_account_email_from_id_token);
                svc.store_gemini_tokens(profile, token_set, account_id, true)
                    .await?;
                println!("Saved profile {profile}");
                println!("Active profile for gemini: {profile}");
                return Ok(());
            }
            Err(e) => {
                println!("Device-code flow unavailable: {e}. Falling back to browser flow.");
            }
        }
    }

    let pkce = auth::gemini_oauth::generate_pkce_state();
    let authorize_url = auth::gemini_oauth::build_authorize_url(&pkce)?;
    save_pending(&PendingOAuthLogin {
        provider: "gemini".to_string(),
        profile: profile.to_string(),
        code_verifier: pkce.code_verifier.clone(),
        state: pkce.state.clone(),
        created_at: Utc::now().to_rfc3339(),
    })?;

    println!("Open this URL in your browser and authorize access:");
    println!("{authorize_url}");
    println!();
    println!("Waiting for callback at http://localhost:1456/auth/callback ...");

    let code = match auth::gemini_oauth::receive_loopback_code(
        &pkce.state,
        std::time::Duration::from_secs(180),
    )
    .await
    {
        Ok(code) => {
            clear_pending("gemini");
            code
        }
        Err(e) => {
            println!("Callback capture failed: {e}");
            println!("Run `crewforge auth paste-redirect --provider gemini --profile {profile}`");
            return Ok(());
        }
    };

    let token_set = auth::gemini_oauth::exchange_code_for_tokens(client, &code, &pkce).await?;
    let account_id = token_set
        .id_token
        .as_deref()
        .and_then(auth::gemini_oauth::extract_account_email_from_id_token);
    svc.store_gemini_tokens(profile, token_set, account_id, true)
        .await?;
    println!("Saved profile {profile}");
    println!("Active profile for gemini: {profile}");
    Ok(())
}

async fn run_openai_login(
    svc: &AuthService,
    client: &reqwest::Client,
    profile: &str,
) -> Result<()> {
    let device = auth::openai_oauth::start_device_code_flow(client).await?;
    println!("OpenAI device authorization started.");
    println!("Visit: {}", device.verification_uri);
    println!("Code:  {}", device.user_code);
    println!();
    println!("Waiting for authorization...");
    let token_set = auth::openai_oauth::poll_device_code_tokens(client, &device).await?;
    let account_id = extract_openai_account_id(&token_set);
    svc.store_openai_tokens(profile, token_set, account_id, true)
        .await?;
    println!("Saved profile {profile}");
    println!("Active profile for openai-codex: {profile}");
    Ok(())
}

// ── Paste redirect ─────────────────────────────────────────────────────────────

async fn run_paste_redirect(
    provider: String,
    profile: String,
    input: Option<String>,
) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let svc = make_auth_service();
    let client = reqwest::Client::new();

    match provider.as_str() {
        "gemini" => {
            let pending = load_pending("gemini")?.ok_or_else(|| {
                anyhow::anyhow!(
                    "No pending Gemini login found. \
                     Run `crewforge auth login --provider gemini` first."
                )
            })?;
            if pending.profile != profile {
                bail!(
                    "Pending login profile mismatch: pending={}, requested={}",
                    pending.profile,
                    profile
                );
            }
            let redirect_input = match input {
                Some(v) => v,
                None => read_plain_input("Paste redirect URL or OAuth code")?,
            };
            let code = auth::gemini_oauth::parse_code_from_redirect(
                &redirect_input,
                Some(&pending.state),
            )?;
            let pkce = PkceState {
                code_verifier: pending.code_verifier,
                code_challenge: String::new(),
                state: pending.state,
            };
            let token_set =
                auth::gemini_oauth::exchange_code_for_tokens(&client, &code, &pkce).await?;
            let account_id = token_set
                .id_token
                .as_deref()
                .and_then(auth::gemini_oauth::extract_account_email_from_id_token);
            svc.store_gemini_tokens(&profile, token_set, account_id, true)
                .await?;
            clear_pending("gemini");
            println!("Saved profile {profile}");
            println!("Active profile for gemini: {profile}");
        }
        _ => bail!("`auth paste-redirect` supports --provider gemini"),
    }
    Ok(())
}

// ── Paste token ───────────────────────────────────────────────────────────────

async fn run_paste_token(
    provider: String,
    profile: String,
    token: Option<String>,
    auth_kind: Option<String>,
) -> Result<()> {
    let provider = normalize_provider(&provider)?;
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => read_masked_input("Paste token")?,
    };
    if token.is_empty() {
        bail!("Token cannot be empty");
    }
    let kind = auth::anthropic_token::detect_auth_kind(&token, auth_kind.as_deref());
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "auth_kind".to_string(),
        kind.as_metadata_value().to_string(),
    );

    let svc = make_auth_service();
    svc.store_provider_token(&provider, &profile, &token, metadata, true)
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
        "gemini" => {
            match svc
                .get_valid_gemini_access_token(profile.as_deref())
                .await?
            {
                Some(_) => {
                    let name = profile.as_deref().unwrap_or("default");
                    println!("Gemini token refreshed successfully");
                    println!("  Profile: gemini:{name}");
                }
                None => bail!(
                    "No Gemini auth profile found. \
                     Run `crewforge auth login --provider gemini`."
                ),
            }
        }
        _ => bail!("`auth refresh` supports --provider openai-codex or gemini"),
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
