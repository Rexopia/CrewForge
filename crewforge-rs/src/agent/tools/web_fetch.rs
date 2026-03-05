use crate::agent::ToolResult;
use async_trait::async_trait;
use reqwest::Client;

const MAX_RESPONSE_BYTES: usize = 512_000; // 500KB
const CONNECT_TIMEOUT_SECS: u64 = 10;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Fetch a web page and return its content as markdown/text.
pub struct WebFetchTool {
    client: Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .redirect(reqwest::redirect::Policy::limited(3))
                .user_agent("Mozilla/5.0 (compatible; CrewForge/1.0)")
                .build()
                .expect("failed to build HTTP client"),
        }
    }
}

#[async_trait]
impl crate::agent::Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as markdown. \
         Only HTTP/HTTPS URLs are allowed. Private/local hosts are blocked."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch (HTTP or HTTPS)"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        // Validate URL.
        if let Err(e) = validate_url(url) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e),
            });
        }

        match fetch_and_convert(&self.client, url).await {
            Ok(content) => Ok(ToolResult::ok(content)),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Fetch failed: {e}")),
            }),
        }
    }
}

/// Validate URL: must be HTTP(S), no private/local hosts, no userinfo.
fn validate_url(raw: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(raw).map_err(|e| format!("Invalid URL: {e}"))?;

    match url.scheme() {
        "http" | "https" => {}
        s => return Err(format!("Unsupported scheme: {s} (only http/https allowed)")),
    }

    if url.username() != "" || url.password().is_some() {
        return Err("URLs with userinfo (user:pass@) are not allowed".into());
    }

    let host = url
        .host_str()
        .ok_or("URL has no host")?;

    if is_private_or_local(host) {
        return Err(format!("Access to private/local host blocked: {host}"));
    }

    Ok(())
}

/// Check if a host is private or local (SSRF protection).
fn is_private_or_local(host: &str) -> bool {
    let host_lower = host.to_lowercase();

    // Localhost variants.
    if host_lower == "localhost"
        || host_lower == "127.0.0.1"
        || host_lower == "::1"
        || host_lower == "[::1]"
        || host_lower == "0.0.0.0"
    {
        return true;
    }

    // .local domains.
    if host_lower.ends_with(".local") || host_lower.ends_with(".localhost") {
        return true;
    }

    // IPv6 bracket notation.
    if host_lower.starts_with('[') {
        return true; // Block all IPv6 for simplicity.
    }

    // Check private IPv4 ranges.
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let octets = ip.octets();
        return octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168)
            || (octets[0] == 169 && octets[1] == 254); // link-local
    }

    false
}

/// Fetch URL and convert to markdown.
async fn fetch_and_convert(client: &Client, url: &str) -> anyhow::Result<String> {
    let resp = client.get(url).send().await?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();

    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        let text = String::from_utf8_lossy(&bytes[..MAX_RESPONSE_BYTES]);
        return Ok(format!(
            "{text}\n\n[... truncated at {}KB]",
            MAX_RESPONSE_BYTES / 1024
        ));
    }

    let text = String::from_utf8_lossy(&bytes).into_owned();

    // For HTML content, convert to markdown.
    if content_type.contains("text/html") {
        Ok(html2md::parse_html(&text))
    } else {
        // JSON, plain text, etc. — return as-is.
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn validate_url_accepts_http() {
        assert!(validate_url("http://example.com/page").is_ok());
    }

    #[test]
    fn validate_url_rejects_ftp() {
        let err = validate_url("ftp://files.example.com").unwrap_err();
        assert!(err.contains("Unsupported scheme"));
    }

    #[test]
    fn validate_url_rejects_userinfo() {
        let err = validate_url("https://user:pass@example.com").unwrap_err();
        assert!(err.contains("userinfo"));
    }

    #[test]
    fn validate_url_rejects_localhost() {
        assert!(validate_url("http://localhost:8080").is_err());
        assert!(validate_url("http://127.0.0.1").is_err());
        assert!(validate_url("http://0.0.0.0").is_err());
    }

    #[test]
    fn validate_url_rejects_private_ip() {
        assert!(validate_url("http://10.0.0.1").is_err());
        assert!(validate_url("http://192.168.1.1").is_err());
        assert!(validate_url("http://172.16.0.1").is_err());
    }

    #[test]
    fn validate_url_rejects_local_domain() {
        assert!(validate_url("http://myhost.local").is_err());
        assert!(validate_url("http://dev.localhost").is_err());
    }

    #[test]
    fn is_private_detects_link_local() {
        assert!(is_private_or_local("169.254.1.1"));
    }

    #[test]
    fn is_private_public_ip_ok() {
        assert!(!is_private_or_local("8.8.8.8"));
        assert!(!is_private_or_local("example.com"));
    }

    #[test]
    fn tool_metadata() {
        let tool = WebFetchTool::new();
        assert_eq!(crate::agent::Tool::name(&tool), "web_fetch");
        assert!(!crate::agent::Tool::is_mutating(&tool));
    }
}
