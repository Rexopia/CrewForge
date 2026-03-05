/// Redact credential-like key-value pairs from tool output to prevent accidental exfiltration.
///
/// Matches patterns such as `token=...`, `api_key: "..."`, `password=...`, etc.
/// Preserves the first 4 characters of each value for context; redacts the rest.
pub fn scrub_credentials(input: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;

    // Matches key=value and key: value forms (token=, api_key:, bearer:, etc.)
    static SENSITIVE_KV_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)(token|api[_-]?key|password|secret|user[_-]?key|bearer|credential)["']?\s*[:=]\s*(?:"([^"]{8,})"|'([^']{8,})'|([a-zA-Z0-9_\-\.]{8,}))"#,
        )
        .expect("SENSITIVE_KV_REGEX is a valid static pattern")
    });

    // Matches HTTP Authorization header: "Authorization: Bearer <token>"
    static AUTH_HEADER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)(Authorization)\s*:\s*(Bearer|Token)\s+([a-zA-Z0-9_\-\.\/\+]{8,})"#)
            .expect("AUTH_HEADER_REGEX is a valid static pattern")
    });

    let step1 = SENSITIVE_KV_REGEX
        .replace_all(input, |caps: &regex::Captures| {
            let full_match = &caps[0];
            let key = &caps[1];
            let val = caps
                .get(2)
                .or_else(|| caps.get(3))
                .or_else(|| caps.get(4))
                .map(|m| m.as_str())
                .unwrap_or("");
            let prefix = if val.len() > 4 { &val[..4] } else { "" };
            if full_match.contains(':') {
                if full_match.contains('"') {
                    format!("\"{}\": \"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}: {}*[REDACTED]", key, prefix)
                }
            } else if full_match.contains('=') {
                if full_match.contains('"') {
                    format!("{}=\"{}*[REDACTED]\"", key, prefix)
                } else {
                    format!("{}={}*[REDACTED]", key, prefix)
                }
            } else {
                format!("{}: {}*[REDACTED]", key, prefix)
            }
        })
        .to_string();

    AUTH_HEADER_REGEX
        .replace_all(&step1, |caps: &regex::Captures| {
            let header = &caps[1];
            let scheme = &caps[2];
            let val = &caps[3];
            let prefix = if val.len() > 4 { &val[..4] } else { "" };
            format!("{}: {} {}*[REDACTED]", header, scheme, prefix)
        })
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_credentials_redacts_token_equals() {
        let input = "token=supersecretvalue123";
        let output = scrub_credentials(input);
        assert!(!output.contains("supersecretvalue123"));
        assert!(output.contains("REDACTED"));
    }

    #[test]
    fn scrub_credentials_redacts_api_key_colon() {
        let input = r#"api_key: "mykey1234""#;
        let output = scrub_credentials(input);
        assert!(!output.contains("mykey1234"));
        assert!(output.contains("REDACTED"));
    }

    #[test]
    fn scrub_credentials_preserves_short_values() {
        // Values shorter than 8 chars should not be redacted.
        let input = "token=short";
        let output = scrub_credentials(input);
        assert_eq!(output, input);
    }

    #[test]
    fn scrub_credentials_preserves_unrelated_content() {
        let input = "hello world, status: ok";
        assert_eq!(scrub_credentials(input), input);
    }

    #[test]
    fn scrub_credentials_redacts_bearer() {
        let input = "Authorization: bearer eyJhbGciOiJIUzI1NiJ9.payload";
        let output = scrub_credentials(input);
        assert!(!output.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(output.contains("REDACTED"));
    }
}
