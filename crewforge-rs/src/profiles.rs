use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GlobalProfile {
    pub name: String,
    pub model: String,
    pub preference: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    profiles: Vec<GlobalProfile>,
}

pub fn global_profiles_path() -> Result<PathBuf> {
    if let Some(raw_path) = std::env::var_os("CREWFORGE_PROFILES_PATH")
        && !raw_path.is_empty()
    {
        return Ok(PathBuf::from(raw_path));
    }

    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow::anyhow!("failed to resolve HOME for global profiles (~/.crewforge/profiles.json)")
    })?;
    Ok(PathBuf::from(home).join(".crewforge").join("profiles.json"))
}

pub async fn load_profiles(path: &Path) -> Result<Vec<GlobalProfile>> {
    let raw_text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading profiles file: {}", path.display()));
        }
    };

    let parsed: ProfilesFile = serde_json::from_str(&raw_text)
        .with_context(|| format!("profiles file must be valid JSON: {}", path.display()))?;
    Ok(parsed.profiles)
}

pub async fn write_profiles(path: &Path, profiles: &[GlobalProfile]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid profiles path: {}", path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed creating profiles dir: {}", parent.display()))?;

    let payload = ProfilesFile {
        profiles: profiles.to_vec(),
    };
    let text = format!("{}\n", serde_json::to_string_pretty(&payload)?);
    tokio::fs::write(path, text)
        .await
        .with_context(|| format!("failed writing profiles file: {}", path.display()))?;
    Ok(())
}

pub fn normalize_name(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;

    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }

    if out.is_empty() {
        "agent".to_string()
    } else {
        out
    }
}

pub fn normalize_preference(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

pub fn ensure_name_available(existing: &[GlobalProfile], name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("profile name cannot be empty");
    }

    if existing.iter().any(|item| item.name == trimmed) {
        bail!("profile name already exists: {trimmed}");
    }

    let normalized = normalize_name(trimmed);
    if existing
        .iter()
        .map(|item| normalize_name(&item.name))
        .any(|existing_id| existing_id == normalized)
    {
        bail!("profile name conflicts after normalization: {trimmed}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_name_collapses_separators() {
        assert_eq!(normalize_name("A B"), "a-b");
        assert_eq!(normalize_name("A-B"), "a-b");
    }

    #[test]
    fn ensure_name_available_rejects_normalized_collision() {
        let existing = vec![GlobalProfile {
            name: "A B".to_string(),
            model: "m".to_string(),
            preference: None,
        }];
        let err = ensure_name_available(&existing, "A-B").expect_err("should reject");
        assert!(err.to_string().contains("conflicts"));
    }
}
