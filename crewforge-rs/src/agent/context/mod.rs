pub mod memory;
pub mod skills;

use crate::agent::Tool;
use crate::agent::sandbox::SecurityPolicy;
use anyhow::Result;
use std::fmt::Write;
use std::path::Path;

use self::skills::Skill;

// ── PromptSection trait ──────────────────────────────────────────────────────

pub trait PromptSection: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String>;
}

pub struct PromptContext<'a> {
    pub workspace_dir: &'a Path,
    pub model_name: &'a str,
    pub tools: &'a [Box<dyn Tool>],
    pub skills: &'a [Skill],
    pub security: &'a SecurityPolicy,
    pub base_system_prompt: &'a str,
}

// ── SystemPromptBuilder ──────────────────────────────────────────────────────

pub struct SystemPromptBuilder {
    sections: Vec<Box<dyn PromptSection>>,
}

impl Default for SystemPromptBuilder {
    fn default() -> Self {
        Self::with_defaults()
    }
}

impl SystemPromptBuilder {
    pub fn with_defaults() -> Self {
        Self {
            sections: vec![
                Box::new(IdentitySection),
                Box::new(ToolsSection),
                Box::new(ShellPolicySection),
                Box::new(SafetySection),
                Box::new(SkillsSection),
                Box::new(WorkspaceSection),
                Box::new(DateTimeSection),
            ],
        }
    }

    pub fn add_section(mut self, section: Box<dyn PromptSection>) -> Self {
        self.sections.push(section);
        self
    }

    pub fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut output = String::new();
        for section in &self.sections {
            let part = section.build(ctx)?;
            if part.trim().is_empty() {
                continue;
            }
            output.push_str(part.trim_end());
            output.push_str("\n\n");
        }
        Ok(output)
    }
}

// ── Sections ─────────────────────────────────────────────────────────────────

pub struct IdentitySection;
pub struct ToolsSection;
pub struct ShellPolicySection;
pub struct SafetySection;
pub struct SkillsSection;
pub struct MemorySection;
pub struct WorkspaceSection;
pub struct DateTimeSection;

impl PromptSection for IdentitySection {
    fn name(&self) -> &str {
        "identity"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut prompt = String::new();

        // Base system prompt (user-provided)
        if !ctx.base_system_prompt.is_empty() {
            prompt.push_str(ctx.base_system_prompt);
            prompt.push_str("\n\n");
        }

        // Inject workspace identity files
        prompt.push_str("## Project Context\n\n");
        for file in ["AGENTS.md", "CLAUDE.md", "IDENTITY.md", "MEMORY.md"] {
            inject_workspace_file(&mut prompt, ctx.workspace_dir, file);
        }

        Ok(prompt)
    }
}

impl PromptSection for ToolsSection {
    fn name(&self) -> &str {
        "tools"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if ctx.tools.is_empty() {
            return Ok(String::new());
        }

        let mut out = String::from("## Tools\n\n");
        for tool in ctx.tools {
            let _ = writeln!(
                out,
                "- **{}**: {}\n  Parameters: `{}`",
                tool.name(),
                tool.description(),
                tool.parameters()
            );
        }
        Ok(out)
    }
}

impl PromptSection for ShellPolicySection {
    fn name(&self) -> &str {
        "shell_policy"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        use crate::agent::sandbox::AutonomyLevel;

        let mut out = String::from("## Shell Policy\n\n");
        let _ = writeln!(
            out,
            "When using the `shell` tool, follow these runtime constraints exactly.\n"
        );

        let label = match ctx.security.autonomy {
            AutonomyLevel::ReadOnly => "read_only",
            AutonomyLevel::Supervised => "supervised",
            AutonomyLevel::Full => "full",
        };
        let _ = writeln!(out, "- Autonomy level: `{label}`");

        if ctx.security.autonomy == AutonomyLevel::ReadOnly {
            out.push_str(
                "- Shell execution is disabled in `read_only` mode. Do not emit shell tool calls.\n",
            );
            return Ok(out);
        }

        if ctx.security.allowed_commands.is_empty() {
            out.push_str("- Allowed commands: wildcard (any command).\n");
        } else {
            let shown: Vec<String> = ctx
                .security
                .allowed_commands
                .iter()
                .take(64)
                .map(|cmd| format!("`{cmd}`"))
                .collect();
            let _ = write!(out, "- Allowed commands: {}", shown.join(", "));
            let hidden = ctx.security.allowed_commands.len().saturating_sub(64);
            if hidden > 0 {
                let _ = write!(out, " (+{hidden} more)");
            }
            out.push('\n');
        }

        if ctx.security.autonomy == AutonomyLevel::Supervised {
            out.push_str(
                "- Medium-risk commands require explicit approval in `supervised` mode.\n",
            );
        }
        out.push_str(
            "- If a requested command is outside policy, choose allowed alternatives and explain.\n",
        );

        Ok(out)
    }
}

impl PromptSection for SafetySection {
    fn name(&self) -> &str {
        "safety"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok("## Safety\n\n\
            - Do not exfiltrate private data.\n\
            - Do not run destructive commands without asking.\n\
            - Do not bypass oversight or approval mechanisms.\n\
            - Prefer `trash` over `rm`.\n\
            - When in doubt, ask before acting externally."
            .into())
    }
}

impl PromptSection for SkillsSection {
    fn name(&self) -> &str {
        "skills"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(skills::skills_to_prompt(ctx.skills))
    }
}

/// Optional section for injecting pre-loaded memory context.
/// Not included in defaults — memory is accessed via tools instead.
/// Can be added via `SystemPromptBuilder::add_section()` if needed.
impl PromptSection for MemorySection {
    fn name(&self) -> &str {
        "memory"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok(String::new())
    }
}

impl PromptSection for WorkspaceSection {
    fn name(&self) -> &str {
        "workspace"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(format!(
            "## Workspace\n\nWorking directory: `{}`",
            ctx.workspace_dir.display()
        ))
    }
}

impl PromptSection for DateTimeSection {
    fn name(&self) -> &str {
        "datetime"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        let now = chrono::Local::now();
        Ok(format!(
            "## Current Date & Time\n\n{} ({})",
            now.format("%Y-%m-%d %H:%M:%S"),
            now.format("%Z")
        ))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

const BOOTSTRAP_MAX_CHARS: usize = 20_000;

fn inject_workspace_file(prompt: &mut String, workspace_dir: &Path, filename: &str) {
    let path = workspace_dir.join(filename);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return; // File not found — silently skip
    };

    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }

    let _ = writeln!(prompt, "### {filename}\n");

    if trimmed.chars().count() > BOOTSTRAP_MAX_CHARS {
        let truncated: String = trimmed.chars().take(BOOTSTRAP_MAX_CHARS).collect();
        prompt.push_str(&truncated);
        let _ = writeln!(
            prompt,
            "\n\n[... truncated at {BOOTSTRAP_MAX_CHARS} chars]\n"
        );
    } else {
        prompt.push_str(trimmed);
        prompt.push_str("\n\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::sandbox::AutonomyLevel;

    fn test_ctx<'a>(
        workspace: &'a Path,
        tools: &'a [Box<dyn Tool>],
        security: &'a SecurityPolicy,
    ) -> PromptContext<'a> {
        PromptContext {
            workspace_dir: workspace,
            model_name: "test-model",
            tools,
            skills: &[],
            security,
            base_system_prompt: "You are a helpful assistant.",
        }
    }

    #[test]
    fn builder_assembles_sections() {
        let security = SecurityPolicy::default();
        let ctx = test_ctx(Path::new("/tmp"), &[], &security);
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("You are a helpful assistant."));
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Workspace"));
        assert!(prompt.contains("## Current Date & Time"));
    }

    #[test]
    fn shell_policy_readonly() {
        let security = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        let ctx = test_ctx(Path::new("/tmp"), &[], &security);
        let section = ShellPolicySection;
        let output = section.build(&ctx).unwrap();
        assert!(output.contains("read_only"));
        assert!(output.contains("disabled"));
    }

    #[test]
    fn shell_policy_supervised() {
        let security = SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        };
        let ctx = test_ctx(Path::new("/tmp"), &[], &security);
        let section = ShellPolicySection;
        let output = section.build(&ctx).unwrap();
        assert!(output.contains("supervised"));
        assert!(output.contains("approval"));
    }

    #[test]
    fn identity_injects_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Agent instructions here").unwrap();

        let security = SecurityPolicy::default();
        let ctx = test_ctx(dir.path(), &[], &security);
        let section = IdentitySection;
        let output = section.build(&ctx).unwrap();
        assert!(output.contains("Agent instructions here"));
    }

    #[test]
    fn memory_section_empty_when_no_context() {
        let security = SecurityPolicy::default();
        let ctx = test_ctx(Path::new("/tmp"), &[], &security);
        let section = MemorySection;
        let output = section.build(&ctx).unwrap();
        assert!(output.is_empty());
    }

    #[test]
    fn skills_section_renders() {
        let security = SecurityPolicy::default();
        let skills = vec![Skill {
            name: "deploy".into(),
            description: "Release safely".into(),
            prompts: vec!["Run tests first.".into()],
        }];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test",
            tools: &[],
            skills: &skills,
            security: &security,
            base_system_prompt: "",
        };
        let section = SkillsSection;
        let output = section.build(&ctx).unwrap();
        assert!(output.contains("<name>deploy</name>"));
    }

    #[test]
    fn custom_section() {
        struct CustomSection;
        impl PromptSection for CustomSection {
            fn name(&self) -> &str {
                "custom"
            }
            fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
                Ok("## Custom\n\nHello from custom section.".into())
            }
        }

        let security = SecurityPolicy::default();
        let ctx = test_ctx(Path::new("/tmp"), &[], &security);
        let prompt = SystemPromptBuilder::with_defaults()
            .add_section(Box::new(CustomSection))
            .build(&ctx)
            .unwrap();
        assert!(prompt.contains("Hello from custom section."));
    }
}
