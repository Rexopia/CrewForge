use crate::auth::profiles::TokenSet;
use anyhow::{Context, Result};
use base64::Engine;
use chrono::Utc;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub const OPENAI_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_ISSUER: &str = "https://auth.openai.com";
const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Version string for User-Agent header.
const CREWFORGE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Polling safety margin added to device auth interval.
const DEVICE_AUTH_POLLING_MARGIN_MS: u64 = 3000;

/// Result of the device auth initiation step.
#[derive(Debug, Clone)]
pub struct DeviceCodeStart {
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval_ms: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Response from OpenAI's custom device auth usercode endpoint.
#[derive(Debug, Deserialize)]
struct DeviceAuthUserCodeResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Option<String>,
}

/// Response from OpenAI's custom device auth token endpoint.
#[derive(Debug, Deserialize)]
struct DeviceAuthTokenResponse {
    authorization_code: String,
    code_verifier: String,
}

fn user_agent() -> String {
    format!(
        "crewforge/{} ({} {}; {})",
        CREWFORGE_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )
}

pub async fn refresh_access_token(client: &Client, refresh_token: &str) -> Result<TokenSet> {
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", OPENAI_OAUTH_CLIENT_ID),
    ];

    let response = client
        .post(OPENAI_OAUTH_TOKEN_URL)
        .form(&form)
        .send()
        .await
        .context("Failed to refresh OpenAI OAuth token")?;

    parse_token_response(response).await
}

/// Start OpenAI's custom device auth flow (not RFC 8628).
///
/// Uses `/api/accounts/deviceauth/usercode` which returns a user code
/// that the user enters at `https://auth.openai.com/codex/device`.
pub async fn start_device_code_flow(client: &Client) -> Result<DeviceCodeStart> {
    let body = serde_json::json!({ "client_id": OPENAI_OAUTH_CLIENT_ID });

    let response = client
        .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/usercode"))
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent())
        .json(&body)
        .send()
        .await
        .context("Failed to start OpenAI device authorization")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI device auth start failed ({status}): {body}");
    }

    let parsed: DeviceAuthUserCodeResponse = response
        .json()
        .await
        .context("Failed to parse OpenAI device auth response")?;

    let interval_secs = parsed
        .interval
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5)
        .max(1);

    Ok(DeviceCodeStart {
        device_auth_id: parsed.device_auth_id,
        user_code: parsed.user_code,
        verification_uri: format!("{OPENAI_ISSUER}/codex/device"),
        interval_ms: interval_secs * 1000 + DEVICE_AUTH_POLLING_MARGIN_MS,
    })
}

/// Poll OpenAI's custom device auth token endpoint until the user authorizes.
///
/// On success, exchanges the returned authorization_code for OAuth tokens
/// via the standard token endpoint.
pub async fn poll_device_code_tokens(
    client: &Client,
    device: &DeviceCodeStart,
) -> Result<TokenSet> {
    let timeout = Duration::from_secs(5 * 60);
    let started = std::time::Instant::now();

    loop {
        if started.elapsed() > timeout {
            anyhow::bail!("Device authorization timed out (5 minutes)");
        }

        tokio::time::sleep(Duration::from_millis(device.interval_ms)).await;

        let body = serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        });

        let response = client
            .post(format!("{OPENAI_ISSUER}/api/accounts/deviceauth/token"))
            .header("Content-Type", "application/json")
            .header("User-Agent", user_agent())
            .json(&body)
            .send()
            .await
            .context("Failed polling OpenAI device auth token")?;

        if response.status().is_success() {
            let data: DeviceAuthTokenResponse = response
                .json()
                .await
                .context("Failed to parse device auth token response")?;

            // Exchange the authorization_code + server-provided code_verifier for tokens.
            let form = [
                ("grant_type", "authorization_code"),
                ("code", data.authorization_code.as_str()),
                (
                    "redirect_uri",
                    &format!("{OPENAI_ISSUER}/deviceauth/callback"),
                ),
                ("client_id", OPENAI_OAUTH_CLIENT_ID),
                ("code_verifier", data.code_verifier.as_str()),
            ];

            let token_response = client
                .post(OPENAI_OAUTH_TOKEN_URL)
                .form(&form)
                .send()
                .await
                .context("Failed to exchange device auth code for tokens")?;

            return parse_token_response(token_response).await;
        }

        // 403/404 = authorization pending, keep polling.
        let status = response.status().as_u16();
        if status == 403 || status == 404 {
            continue;
        }

        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI device auth polling failed ({status}): {body}");
    }
}

/// Extract account ID from a JWT token's claims.
///
/// Checks multiple claim paths used by OpenAI across different token types.
pub fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let claims = parse_jwt_claims(token)?;
    extract_account_id_from_claims(&claims)
}

/// Extract account ID from an id_token first, falling back to access_token.
///
/// OpenAI's id_token typically contains richer claims including organization info.
pub fn extract_account_id_from_tokens(
    id_token: Option<&str>,
    access_token: &str,
) -> Option<String> {
    if let Some(id) = id_token.and_then(|t| {
        let claims = parse_jwt_claims(t)?;
        extract_account_id_from_claims(&claims)
    }) {
        return Some(id);
    }
    extract_account_id_from_jwt(access_token)
}

fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn extract_account_id_from_claims(claims: &serde_json::Value) -> Option<String> {
    // Direct account ID fields
    for key in ["chatgpt_account_id", "account_id", "accountId"] {
        if let Some(value) = claims.get(key).and_then(|v| v.as_str())
            && !value.trim().is_empty()
        {
            return Some(value.to_string());
        }
    }

    // Nested under https://api.openai.com/auth
    if let Some(auth) = claims.get("https://api.openai.com/auth")
        && let Some(value) = auth.get("chatgpt_account_id").and_then(|v| v.as_str())
        && !value.trim().is_empty()
    {
        return Some(value.to_string());
    }

    // First organization ID as fallback
    if let Some(orgs) = claims.get("organizations").and_then(|v| v.as_array())
        && let Some(first) = orgs.first()
        && let Some(id) = first.get("id").and_then(|v| v.as_str())
        && !id.trim().is_empty()
    {
        return Some(id.to_string());
    }

    None
}

async fn parse_token_response(response: reqwest::Response) -> Result<TokenSet> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI OAuth token request failed ({status}): {body}");
    }

    let token: TokenResponse = response
        .json()
        .await
        .context("Failed to parse OpenAI token response")?;

    let expires_at = token.expires_in.and_then(|seconds| {
        if seconds <= 0 {
            None
        } else {
            Some(Utc::now() + chrono::Duration::seconds(seconds))
        }
    });

    Ok(TokenSet {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        expires_at,
        token_type: token.token_type,
        scope: token.scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_account_id_from_jwt_payload() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("{\"account_id\":\"acct_123\"}");
        let token = format!("{header}.{payload}.sig");

        let account = extract_account_id_from_jwt(&token);
        assert_eq!(account.as_deref(), Some("acct_123"));
    }

    #[test]
    fn extract_chatgpt_account_id() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("{\"chatgpt_account_id\":\"chatgpt_456\"}");
        let token = format!("{header}.{payload}.sig");

        let account = extract_account_id_from_jwt(&token);
        assert_eq!(account.as_deref(), Some("chatgpt_456"));
    }

    #[test]
    fn extract_nested_auth_claim() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "nested_789"
            }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(claims.to_string());
        let token = format!("{header}.{payload}.sig");

        let account = extract_account_id_from_jwt(&token);
        assert_eq!(account.as_deref(), Some("nested_789"));
    }

    #[test]
    fn extract_organization_id_fallback() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let claims = serde_json::json!({
            "organizations": [{"id": "org_001"}]
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(claims.to_string());
        let token = format!("{header}.{payload}.sig");

        let account = extract_account_id_from_jwt(&token);
        assert_eq!(account.as_deref(), Some("org_001"));
    }

    #[test]
    fn extract_from_tokens_prefers_id_token() {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("{}");
        let id_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("{\"chatgpt_account_id\":\"from_id_token\"}");
        let access_payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode("{\"chatgpt_account_id\":\"from_access_token\"}");
        let id_token = format!("{header}.{id_payload}.sig");
        let access_token = format!("{header}.{access_payload}.sig");

        let account = extract_account_id_from_tokens(Some(&id_token), &access_token);
        assert_eq!(account.as_deref(), Some("from_id_token"));
    }

    #[test]
    fn user_agent_is_non_empty() {
        let ua = user_agent();
        assert!(ua.starts_with("crewforge/"));
        assert!(ua.contains(std::env::consts::OS));
    }
}
