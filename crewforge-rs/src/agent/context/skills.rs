use std::fmt::Write;
use std::path::Path;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub prompts: Vec<String>,
}

// ── Loader ───────────────────────────────────────────────────────────────────

/// Load skills from `<workspace>/skills/*/SKILL.md`.
///
/// Each SKILL.md is parsed for a `# <name>` heading (first H1) and a description
/// (first non-empty paragraph after heading). The full content is treated as the
/// prompt instruction.
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return vec![];
    };

    let mut skills = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_file = path.join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_file) else {
            continue;
        };

        if let Some(skill) = parse_skill_md(&content, &path) {
            skills.push(skill);
        }
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn parse_skill_md(content: &str, dir: &Path) -> Option<Skill> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut description = String::new();
    let mut past_heading = false;

    for line in trimmed.lines() {
        let line = line.trim();
        if line.starts_with("# ") && !past_heading {
            name = line.trim_start_matches("# ").trim().to_string();
            past_heading = true;
            continue;
        }
        if !line.is_empty() && description.is_empty() && (past_heading || !line.starts_with("# ")) {
            description = line.to_string();
            if past_heading {
                break;
            }
        }
    }

    if description.is_empty() {
        description = name.clone();
    }

    Some(Skill {
        name,
        description,
        prompts: vec![trimmed.to_string()],
    })
}

// ── Prompt rendering ─────────────────────────────────────────────────────────

pub fn skills_to_prompt(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Skills\n\n<available_skills>\n");

    for skill in skills {
        let _ = writeln!(out, "<skill>");
        let _ = writeln!(out, "  <name>{}</name>", xml_escape(&skill.name));
        let _ = writeln!(
            out,
            "  <description>{}</description>",
            xml_escape(&skill.description)
        );
        for prompt in &skill.prompts {
            let _ = writeln!(
                out,
                "  <instruction>{}</instruction>",
                xml_escape(prompt)
            );
        }
        let _ = writeln!(out, "</skill>");
    }

    out.push_str("</available_skills>\n");
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_md_extracts_name_and_description() {
        let content = "# Deploy\n\nSafely release to production.\n\n## Steps\n\n1. Run tests\n";
        let skill = parse_skill_md(content, Path::new("/tmp/skills/deploy")).unwrap();
        assert_eq!(skill.name, "Deploy");
        assert_eq!(skill.description, "Safely release to production.");
        assert_eq!(skill.prompts.len(), 1);
    }

    #[test]
    fn parse_skill_md_uses_dir_name_as_fallback() {
        let content = "Just do the thing.\n";
        let skill = parse_skill_md(content, Path::new("/tmp/skills/my-skill")).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Just do the thing.");
    }

    #[test]
    fn parse_skill_md_empty_returns_none() {
        assert!(parse_skill_md("", Path::new("/tmp/skills/empty")).is_none());
        assert!(parse_skill_md("   \n  \n", Path::new("/tmp/skills/empty")).is_none());
    }

    #[test]
    fn skills_to_prompt_empty() {
        assert!(skills_to_prompt(&[]).is_empty());
    }

    #[test]
    fn skills_to_prompt_renders_xml() {
        let skills = vec![Skill {
            name: "code<review>".into(),
            description: "Review & check".into(),
            prompts: vec!["Run linter".into()],
        }];
        let prompt = skills_to_prompt(&skills);
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>code&lt;review&gt;</name>"));
        assert!(prompt.contains("<description>Review &amp; check</description>"));
        assert!(prompt.contains("<instruction>Run linter</instruction>"));
    }

    #[test]
    fn load_skills_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills/deploy");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Deploy\n\nRelease safely.\n",
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "Deploy");
    }

    #[test]
    fn load_skills_no_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }
}
