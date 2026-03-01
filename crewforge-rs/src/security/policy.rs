use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// How much autonomy the agent has.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AutonomyLevel {
    /// Read-only: can observe but not act
    ReadOnly,
    /// Supervised: acts but requires approval for risky operations
    #[default]
    Supervised,
    /// Full: autonomous execution within policy bounds
    Full,
}

/// Risk score for shell command execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRiskLevel {
    Low,
    Medium,
    High,
}

/// Classifies whether a tool operation is read-only or side-effecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOperation {
    Read,
    Act,
}

/// Sliding-window action tracker for rate limiting.
#[derive(Debug)]
pub struct ActionTracker {
    actions: Mutex<Vec<Instant>>,
}

impl Default for ActionTracker {
    fn default() -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
        }
    }
}

impl ActionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an action and return the current count within the window.
    pub fn record(&self) -> usize {
        let mut actions = self.actions.lock().unwrap();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.push(Instant::now());
        actions.len()
    }

    /// Count of actions in the current window without recording.
    pub fn count(&self) -> usize {
        let mut actions = self.actions.lock().unwrap();
        let cutoff = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        actions.retain(|t| *t > cutoff);
        actions.len()
    }
}

impl Clone for ActionTracker {
    fn clone(&self) -> Self {
        let actions = self.actions.lock().unwrap();
        Self {
            actions: Mutex::new(actions.clone()),
        }
    }
}

/// Security policy enforced on all tool executions.
#[derive(Debug, Clone)]
pub struct SecurityPolicy {
    pub autonomy: AutonomyLevel,
    pub workspace_dir: PathBuf,
    pub workspace_only: bool,
    pub allowed_commands: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub allowed_roots: Vec<PathBuf>,
    pub max_actions_per_hour: u32,
    pub require_approval_for_medium_risk: bool,
    pub block_high_risk_commands: bool,
    pub shell_env_passthrough: Vec<String>,
    pub tracker: ActionTracker,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            autonomy: AutonomyLevel::Supervised,
            workspace_dir: PathBuf::from("."),
            workspace_only: true,
            allowed_commands: vec![
                "git".into(),
                "npm".into(),
                "cargo".into(),
                "ls".into(),
                "cat".into(),
                "grep".into(),
                "find".into(),
                "echo".into(),
                "pwd".into(),
                "wc".into(),
                "head".into(),
                "tail".into(),
                "date".into(),
            ],
            forbidden_paths: vec![
                "/etc".into(),
                "/root".into(),
                "/home".into(),
                "/usr".into(),
                "/bin".into(),
                "/sbin".into(),
                "/lib".into(),
                "/opt".into(),
                "/boot".into(),
                "/dev".into(),
                "/proc".into(),
                "/sys".into(),
                "/var".into(),
                "/tmp".into(),
                "~/.ssh".into(),
                "~/.gnupg".into(),
                "~/.aws".into(),
                "~/.config".into(),
            ],
            allowed_roots: Vec::new(),
            max_actions_per_hour: 60,
            require_approval_for_medium_risk: true,
            block_high_risk_commands: true,
            shell_env_passthrough: vec![],
            tracker: ActionTracker::new(),
        }
    }
}

// ── Shell Command Parsing Utilities ─────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand_user_path(path: &str) -> PathBuf {
    let home = home_dir();
    if let (true, Some(h)) = (path == "~", &home) {
        return h.clone();
    }
    if let (Some(stripped), Some(h)) = (path.strip_prefix("~/"), &home) {
        return h.join(stripped);
    }
    PathBuf::from(path)
}

/// Skip leading environment variable assignments (e.g. `FOO=bar cmd args`).
fn skip_env_assignments(s: &str) -> &str {
    let mut rest = s;
    loop {
        let Some(word) = rest.split_whitespace().next() else {
            return rest;
        };
        if word.contains('=')
            && word
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            rest = rest[word.len()..].trim_start();
        } else {
            return rest;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteState {
    None,
    Single,
    Double,
}

/// Split a shell command into sub-commands by unquoted separators.
fn split_unquoted_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    let push_segment = |segments: &mut Vec<String>, current: &mut String| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
        current.clear();
    };

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
                current.push(ch);
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    current.push(ch);
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    current.push(ch);
                    continue;
                }

                match ch {
                    '\'' => {
                        quote = QuoteState::Single;
                        current.push(ch);
                    }
                    '"' => {
                        quote = QuoteState::Double;
                        current.push(ch);
                    }
                    ';' | '\n' => push_segment(&mut segments, &mut current),
                    '|' => {
                        if chars.next_if_eq(&'|').is_some() {
                            // `||`
                        }
                        push_segment(&mut segments, &mut current);
                    }
                    '&' => {
                        if chars.next_if_eq(&'&').is_some() {
                            push_segment(&mut segments, &mut current);
                        } else {
                            current.push(ch);
                        }
                    }
                    _ => current.push(ch),
                }
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }

    segments
}

/// Detect a single unquoted `&` operator (background/chain). `&&` is allowed.
fn contains_unquoted_single_ampersand(command: &str) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    '&' => {
                        if chars.next_if_eq(&'&').is_none() {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    false
}

/// Detect an unquoted character in a shell command.
fn contains_unquoted_char(command: &str, target: char) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;

    for ch in command.chars() {
        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    continue;
                }
                match ch {
                    '\'' => quote = QuoteState::Single,
                    '"' => quote = QuoteState::Double,
                    _ if ch == target => return true,
                    _ => {}
                }
            }
        }
    }

    false
}

pub(crate) fn is_valid_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Detect unquoted shell variable expansions that are not explicitly allowlisted.
fn contains_disallowed_unquoted_shell_variable_expansion(
    command: &str,
    allowed_vars: &[String],
) -> bool {
    let mut quote = QuoteState::None;
    let mut escaped = false;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        let ch = chars[i];

        match quote {
            QuoteState::Single => {
                if ch == '\'' {
                    quote = QuoteState::None;
                }
                i += 1;
                continue;
            }
            QuoteState::Double => {
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::None;
                    i += 1;
                    continue;
                }
            }
            QuoteState::None => {
                if escaped {
                    escaped = false;
                    i += 1;
                    continue;
                }
                if ch == '\\' {
                    escaped = true;
                    i += 1;
                    continue;
                }
                if ch == '\'' {
                    quote = QuoteState::Single;
                    i += 1;
                    continue;
                }
                if ch == '"' {
                    quote = QuoteState::Double;
                    i += 1;
                    continue;
                }
            }
        }

        if ch != '$' {
            i += 1;
            continue;
        }

        let Some(next) = chars.get(i + 1).copied() else {
            i += 1;
            continue;
        };

        match next {
            '(' => return true,
            '{' => {
                let mut j = i + 2;
                while j < chars.len() && chars[j] != '}' {
                    j += 1;
                }
                if j >= chars.len() {
                    return true;
                }
                let inner: String = chars[i + 2..j].iter().collect();
                if !is_valid_env_var_name(&inner)
                    || !allowed_vars.iter().any(|allowed| allowed == &inner)
                {
                    return true;
                }
                i = j + 1;
                continue;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i + 2;
                while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                let name: String = chars[i + 1..j].iter().collect();
                if !allowed_vars.iter().any(|allowed| allowed == &name) {
                    return true;
                }
                i = j;
                continue;
            }
            c if c.is_ascii_digit() || matches!(c, '#' | '?' | '!' | '$' | '*' | '@' | '-') => {
                return true;
            }
            _ => {}
        }

        i += 1;
    }

    false
}

fn strip_wrapping_quotes(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'')
}

fn looks_like_path(candidate: &str) -> bool {
    candidate.starts_with('/')
        || candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.starts_with('~')
        || candidate == "."
        || candidate == ".."
        || candidate.contains('/')
}

fn is_allowlist_entry_match(allowed: &str, executable: &str, executable_base: &str) -> bool {
    let allowed = strip_wrapping_quotes(allowed).trim();
    if allowed.is_empty() {
        return false;
    }
    if allowed == "*" {
        return true;
    }
    if looks_like_path(allowed) {
        let allowed_path = expand_user_path(allowed);
        let executable_path = expand_user_path(executable);
        return executable_path == allowed_path;
    }
    allowed == executable_base
}

// ── SecurityPolicy Methods ──────────────────────────────────────────────────

impl SecurityPolicy {
    /// Classify command risk. Any high-risk segment marks the whole command high.
    pub fn command_risk_level(&self, command: &str) -> CommandRiskLevel {
        let mut saw_medium = false;

        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(base_raw) = words.next() else {
                continue;
            };

            let base = base_raw
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            let joined_segment = cmd_part.to_ascii_lowercase();

            if matches!(
                base.as_str(),
                "rm" | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "halt"
                    | "poweroff"
                    | "sudo"
                    | "su"
                    | "chown"
                    | "chmod"
                    | "useradd"
                    | "userdel"
                    | "usermod"
                    | "passwd"
                    | "mount"
                    | "umount"
                    | "iptables"
                    | "ufw"
                    | "firewall-cmd"
                    | "curl"
                    | "wget"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "scp"
                    | "ssh"
                    | "ftp"
                    | "telnet"
            ) {
                return CommandRiskLevel::High;
            }

            if joined_segment.contains("rm -rf /")
                || joined_segment.contains("rm -fr /")
                || joined_segment.contains(":(){:|:&};:")
            {
                return CommandRiskLevel::High;
            }

            let medium = match base.as_str() {
                "git" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "commit"
                            | "push"
                            | "reset"
                            | "clean"
                            | "rebase"
                            | "merge"
                            | "cherry-pick"
                            | "revert"
                            | "branch"
                            | "checkout"
                            | "switch"
                            | "tag"
                    )
                }),
                "npm" | "pnpm" | "yarn" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "install" | "add" | "remove" | "uninstall" | "update" | "publish"
                    )
                }),
                "cargo" => args.first().is_some_and(|verb| {
                    matches!(
                        verb.as_str(),
                        "add" | "remove" | "install" | "clean" | "publish"
                    )
                }),
                "touch" | "mkdir" | "mv" | "cp" | "ln" => true,
                _ => false,
            };

            saw_medium |= medium;
        }

        if saw_medium {
            CommandRiskLevel::Medium
        } else {
            CommandRiskLevel::Low
        }
    }

    /// Validate full command execution policy (allowlist + risk gate).
    pub fn validate_command_execution(
        &self,
        command: &str,
        approved: bool,
    ) -> Result<CommandRiskLevel, String> {
        if !self.is_command_allowed(command) {
            return Err(format!("Command not allowed by security policy: {command}"));
        }

        let risk = self.command_risk_level(command);

        if risk == CommandRiskLevel::High {
            if self.block_high_risk_commands {
                return Err("Command blocked: high-risk command is disallowed by policy".into());
            }
            if self.autonomy == AutonomyLevel::Supervised && !approved {
                return Err(
                    "Command requires explicit approval (approved=true): high-risk operation"
                        .into(),
                );
            }
        }

        if risk == CommandRiskLevel::Medium
            && self.autonomy == AutonomyLevel::Supervised
            && self.require_approval_for_medium_risk
            && !approved
        {
            return Err(
                "Command requires explicit approval (approved=true): medium-risk operation".into(),
            );
        }

        Ok(risk)
    }

    /// Check if a shell command is allowed.
    pub fn is_command_allowed(&self, command: &str) -> bool {
        if self.autonomy == AutonomyLevel::ReadOnly {
            return false;
        }

        if command.contains('`')
            || contains_disallowed_unquoted_shell_variable_expansion(
                command,
                &self.shell_env_passthrough,
            )
            || command.contains("<(")
            || command.contains(">(")
        {
            return false;
        }

        if contains_unquoted_char(command, '>') || contains_unquoted_char(command, '<') {
            return false;
        }

        if command
            .split_whitespace()
            .any(|w| w == "tee" || w.ends_with("/tee"))
        {
            return false;
        }

        if contains_unquoted_single_ampersand(command) {
            return false;
        }

        let segments = split_unquoted_segments(command);
        for segment in &segments {
            let cmd_part = skip_env_assignments(segment);
            let mut words = cmd_part.split_whitespace();
            let executable = strip_wrapping_quotes(words.next().unwrap_or("")).trim();
            let base_cmd = executable.rsplit('/').next().unwrap_or("");

            if base_cmd.is_empty() {
                continue;
            }

            if !self
                .allowed_commands
                .iter()
                .any(|allowed| is_allowlist_entry_match(allowed, executable, base_cmd))
            {
                return false;
            }

            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            if !self.is_args_safe(base_cmd, &args) {
                return false;
            }
        }

        segments.iter().any(|s| {
            let s = skip_env_assignments(s.trim());
            s.split_whitespace().next().is_some_and(|w| !w.is_empty())
        })
    }

    /// Check for dangerous arguments that allow sub-command execution.
    fn is_args_safe(&self, base: &str, args: &[String]) -> bool {
        let base = base.to_ascii_lowercase();
        match base.as_str() {
            "find" => !args.iter().any(|arg| arg == "-exec" || arg == "-ok"),
            "git" => !args.iter().any(|arg| {
                arg == "config"
                    || arg.starts_with("config.")
                    || arg == "alias"
                    || arg.starts_with("alias.")
                    || arg == "-c"
            }),
            _ => true,
        }
    }

    /// Return the first path-like argument blocked by path policy.
    pub fn forbidden_path_argument(&self, command: &str) -> Option<String> {
        let forbidden_candidate = |raw: &str| {
            let candidate = strip_wrapping_quotes(raw).trim();
            if candidate.is_empty() || candidate.contains("://") {
                return None;
            }
            if looks_like_path(candidate) && !self.is_path_allowed(candidate) {
                Some(candidate.to_string())
            } else {
                None
            }
        };

        for segment in split_unquoted_segments(command) {
            let cmd_part = skip_env_assignments(&segment);
            let mut words = cmd_part.split_whitespace();
            let Some(_executable) = words.next() else {
                continue;
            };

            for token in words {
                let candidate = strip_wrapping_quotes(token).trim();
                if candidate.is_empty() || candidate.contains("://") {
                    continue;
                }

                if candidate.starts_with('-') {
                    if let Some((_, value)) = candidate.split_once('=') {
                        let blocked = forbidden_candidate(value);
                        if blocked.is_some() {
                            return blocked;
                        }
                    }
                    continue;
                }

                if let Some(blocked) = forbidden_candidate(candidate) {
                    return Some(blocked);
                }
            }
        }

        None
    }

    /// Check if a file path is allowed (no path traversal, within workspace).
    pub fn is_path_allowed(&self, path: &str) -> bool {
        if path.contains('\0') {
            return false;
        }

        if Path::new(path)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }

        let lower = path.to_lowercase();
        if lower.contains("..%2f") || lower.contains("%2f..") {
            return false;
        }

        if path.starts_with('~') && path != "~" && !path.starts_with("~/") {
            return false;
        }

        let expanded_path = expand_user_path(path);

        if self.workspace_only && expanded_path.is_absolute() {
            return false;
        }

        for forbidden in &self.forbidden_paths {
            let forbidden_path = expand_user_path(forbidden);
            if expanded_path.starts_with(forbidden_path) {
                return false;
            }
        }

        true
    }

    /// Validate that a resolved path is inside the workspace or an allowed root.
    pub fn is_resolved_path_allowed(&self, resolved: &Path) -> bool {
        let workspace_root = self
            .workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| self.workspace_dir.clone());
        if resolved.starts_with(&workspace_root) {
            return true;
        }

        for root in &self.allowed_roots {
            let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&canonical) {
                return true;
            }
        }

        for forbidden in &self.forbidden_paths {
            let forbidden_path = expand_user_path(forbidden);
            if resolved.starts_with(&forbidden_path) {
                return false;
            }
        }

        if !self.workspace_only {
            return true;
        }

        false
    }

    /// Returns human-readable guidance on how to fix path violations.
    pub fn resolved_path_violation_message(&self, resolved: &Path) -> String {
        let guidance = if self.allowed_roots.is_empty() {
            "Add the directory to allowed_roots, or move the file into the workspace."
        } else {
            "Add a matching parent directory to allowed_roots, or move the file into the workspace."
        };
        format!(
            "Resolved path escapes workspace allowlist: {}. {}",
            resolved.display(),
            guidance
        )
    }

    /// Check if autonomy level permits any action at all.
    pub fn can_act(&self) -> bool {
        self.autonomy != AutonomyLevel::ReadOnly
    }

    /// Enforce policy for a tool operation.
    pub fn enforce_tool_operation(
        &self,
        operation: ToolOperation,
        operation_name: &str,
    ) -> Result<(), String> {
        match operation {
            ToolOperation::Read => Ok(()),
            ToolOperation::Act => {
                if !self.can_act() {
                    return Err(format!(
                        "Security policy: read-only mode, cannot perform '{operation_name}'"
                    ));
                }
                if !self.record_action() {
                    return Err("Rate limit exceeded: action budget exhausted".to_string());
                }
                Ok(())
            }
        }
    }

    /// Record an action and check if the rate limit has been exceeded.
    pub fn record_action(&self) -> bool {
        let count = self.tracker.record();
        count <= self.max_actions_per_hour as usize
    }

    /// Check if the rate limit would be exceeded without recording.
    pub fn is_rate_limited(&self) -> bool {
        self.tracker.count() >= self.max_actions_per_hour as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy() -> SecurityPolicy {
        SecurityPolicy::default()
    }

    // ── AutonomyLevel tests ─────────────────────────────────────────────

    #[test]
    fn default_autonomy_is_supervised() {
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Supervised);
    }

    #[test]
    fn can_act_readonly_is_false() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        assert!(!p.can_act());
    }

    #[test]
    fn can_act_supervised_is_true() {
        assert!(test_policy().can_act());
    }

    #[test]
    fn can_act_full_is_true() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };
        assert!(p.can_act());
    }

    // ── Path validation tests ───────────────────────────────────────────

    #[test]
    fn path_relative_allowed() {
        assert!(test_policy().is_path_allowed("src/main.rs"));
    }

    #[test]
    fn path_traversal_blocked() {
        assert!(!test_policy().is_path_allowed("../../../etc/passwd"));
    }

    #[test]
    fn path_absolute_blocked_workspace_only() {
        assert!(!test_policy().is_path_allowed("/tmp/file.txt"));
    }

    #[test]
    fn path_absolute_allowed_workspace_not_only() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        // /some/safe/path not in forbidden list
        assert!(p.is_path_allowed("/some/safe/path"));
    }

    #[test]
    fn path_forbidden_blocked() {
        let p = SecurityPolicy {
            workspace_only: false,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_path_allowed("/etc/passwd"));
    }

    #[test]
    fn path_null_byte_blocked() {
        assert!(!test_policy().is_path_allowed("file\0.txt"));
    }

    #[test]
    fn path_url_encoded_traversal_blocked() {
        assert!(!test_policy().is_path_allowed("..%2f..%2fetc/passwd"));
    }

    #[test]
    fn path_tilde_user_blocked() {
        assert!(!test_policy().is_path_allowed("~root/.ssh/id_rsa"));
    }

    #[test]
    fn path_dotfile_in_workspace_allowed() {
        assert!(test_policy().is_path_allowed(".env"));
    }

    // ── Resolved path tests ─────────────────────────────────────────────

    #[test]
    fn resolved_path_inside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let p = SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        };
        let inside = dir.path().join("src/main.rs");
        assert!(p.is_resolved_path_allowed(&inside));
    }

    #[test]
    fn resolved_path_outside_workspace_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let p = SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            ..SecurityPolicy::default()
        };
        assert!(!p.is_resolved_path_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn resolved_path_allowed_roots() {
        let dir = tempfile::tempdir().unwrap();
        let extra = tempfile::tempdir().unwrap();
        let p = SecurityPolicy {
            workspace_dir: dir.path().to_path_buf(),
            allowed_roots: vec![extra.path().to_path_buf()],
            ..SecurityPolicy::default()
        };
        let inside_extra = extra.path().join("file.txt");
        assert!(p.is_resolved_path_allowed(&inside_extra));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_path_symlink_escape_blocked() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link_path = workspace.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link_path).unwrap();
        let resolved = link_path.canonicalize().unwrap();

        let p = SecurityPolicy {
            workspace_dir: workspace.path().to_path_buf(),
            ..SecurityPolicy::default()
        };
        assert!(!p.is_resolved_path_allowed(&resolved));
    }

    // ── Command allowlist tests ─────────────────────────────────────────

    #[test]
    fn command_allowed_basic() {
        assert!(test_policy().is_command_allowed("ls -la"));
    }

    #[test]
    fn command_blocked_unknown() {
        assert!(!test_policy().is_command_allowed("python3 script.py"));
    }

    #[test]
    fn command_readonly_blocks_all() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_command_allowed("ls"));
    }

    #[test]
    fn command_pipe_both_sides_checked() {
        assert!(test_policy().is_command_allowed("grep foo | head -5"));
        assert!(!test_policy().is_command_allowed("grep foo | python3"));
    }

    #[test]
    fn command_semicolon_injection_blocked() {
        assert!(!test_policy().is_command_allowed("ls; rm -rf /"));
    }

    #[test]
    fn command_backtick_injection_blocked() {
        assert!(!test_policy().is_command_allowed("echo `rm -rf /`"));
    }

    #[test]
    fn command_dollar_paren_blocked() {
        assert!(!test_policy().is_command_allowed("echo $(rm -rf /)"));
    }

    #[test]
    fn command_redirect_blocked() {
        assert!(!test_policy().is_command_allowed("echo secret > /tmp/file"));
    }

    #[test]
    fn command_background_blocked() {
        assert!(!test_policy().is_command_allowed("ls & rm -rf /"));
    }

    #[test]
    fn command_and_chain_allowed() {
        assert!(test_policy().is_command_allowed("ls && echo ok"));
    }

    #[test]
    fn command_tee_blocked() {
        assert!(!test_policy().is_command_allowed("echo secret | tee /tmp/out"));
    }

    #[test]
    fn command_find_exec_blocked() {
        assert!(!test_policy().is_command_allowed("find . -exec rm {} \\;"));
    }

    #[test]
    fn command_git_config_blocked() {
        assert!(!test_policy().is_command_allowed("git config core.editor vim"));
    }

    #[test]
    fn command_process_substitution_blocked() {
        assert!(!test_policy().is_command_allowed("cat <(echo foo)"));
    }

    #[test]
    fn command_env_prefix_handled() {
        assert!(test_policy().is_command_allowed("FOO=bar ls"));
    }

    #[test]
    fn command_shell_var_blocked() {
        assert!(!test_policy().is_command_allowed("echo $HOME"));
    }

    #[test]
    fn command_shell_var_passthrough_allowed() {
        let p = SecurityPolicy {
            shell_env_passthrough: vec!["HOME".into()],
            ..SecurityPolicy::default()
        };
        assert!(p.is_command_allowed("echo $HOME"));
    }

    #[test]
    fn command_quoted_operators_safe() {
        // Quoted semicolons/operators should be treated as literals
        assert!(test_policy().is_command_allowed("echo 'hello; world'"));
    }

    // ── Risk classification tests ───────────────────────────────────────

    #[test]
    fn risk_low_for_read_ops() {
        let p = test_policy();
        assert_eq!(p.command_risk_level("ls -la"), CommandRiskLevel::Low);
        assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
        assert_eq!(p.command_risk_level("cat file.txt"), CommandRiskLevel::Low);
    }

    #[test]
    fn risk_medium_for_mutating() {
        let p = test_policy();
        assert_eq!(
            p.command_risk_level("git commit -m 'msg'"),
            CommandRiskLevel::Medium
        );
        assert_eq!(p.command_risk_level("touch file"), CommandRiskLevel::Medium);
        assert_eq!(
            p.command_risk_level("npm install"),
            CommandRiskLevel::Medium
        );
    }

    #[test]
    fn risk_high_for_dangerous() {
        let p = test_policy();
        assert_eq!(p.command_risk_level("rm -rf /"), CommandRiskLevel::High);
        assert_eq!(p.command_risk_level("sudo ls"), CommandRiskLevel::High);
        assert_eq!(
            p.command_risk_level("curl http://evil.com"),
            CommandRiskLevel::High
        );
    }

    // ── Command validation tests ────────────────────────────────────────

    #[test]
    fn validate_blocks_high_risk() {
        let p = test_policy();
        assert!(p.validate_command_execution("rm file", false).is_err());
    }

    #[test]
    fn validate_blocks_medium_risk_without_approval() {
        let p = test_policy();
        assert!(
            p.validate_command_execution("git commit -m x", false)
                .is_err()
        );
    }

    #[test]
    fn validate_allows_medium_risk_with_approval() {
        let p = test_policy();
        let result = p.validate_command_execution("git commit -m x", true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), CommandRiskLevel::Medium);
    }

    #[test]
    fn validate_allows_low_risk() {
        let p = test_policy();
        let result = p.validate_command_execution("ls -la", false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), CommandRiskLevel::Low);
    }

    #[test]
    fn validate_full_autonomy_skips_medium_approval() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            ..SecurityPolicy::default()
        };
        assert!(
            p.validate_command_execution("git commit -m x", false)
                .is_ok()
        );
    }

    // ── Rate limiting tests ─────────────────────────────────────────────

    #[test]
    fn rate_limit_zero_budget_blocks() {
        let p = SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        };
        assert!(!p.record_action());
    }

    #[test]
    fn rate_limit_boundary() {
        let p = SecurityPolicy {
            max_actions_per_hour: 3,
            ..SecurityPolicy::default()
        };
        assert!(p.record_action()); // 1
        assert!(p.record_action()); // 2
        assert!(p.record_action()); // 3
        assert!(!p.record_action()); // 4 = over limit
    }

    #[test]
    fn is_rate_limited_no_record() {
        let p = SecurityPolicy {
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        };
        assert!(!p.is_rate_limited());
        p.record_action();
        assert!(p.is_rate_limited());
    }

    #[test]
    fn tracker_clone_independence() {
        let p = test_policy();
        p.record_action();
        let p2 = p.clone();
        p.record_action();
        // p has 2 actions, p2 should have 1
        assert_eq!(p.tracker.count(), 2);
        assert_eq!(p2.tracker.count(), 1);
    }

    // ── Enforce tool operation tests ────────────────────────────────────

    #[test]
    fn enforce_read_always_ok() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        assert!(
            p.enforce_tool_operation(ToolOperation::Read, "read")
                .is_ok()
        );
    }

    #[test]
    fn enforce_act_blocked_readonly() {
        let p = SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        };
        assert!(
            p.enforce_tool_operation(ToolOperation::Act, "write")
                .is_err()
        );
    }

    #[test]
    fn enforce_act_rate_limited() {
        let p = SecurityPolicy {
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        };
        assert!(
            p.enforce_tool_operation(ToolOperation::Act, "write")
                .is_ok()
        );
        assert!(
            p.enforce_tool_operation(ToolOperation::Act, "write")
                .is_err()
        );
    }

    // ── Forbidden path argument tests ───────────────────────────────────

    #[test]
    fn forbidden_path_argument_detects_absolute() {
        let p = test_policy();
        assert!(p.forbidden_path_argument("cat /etc/passwd").is_some());
    }

    #[test]
    fn forbidden_path_argument_safe_relative() {
        let p = test_policy();
        assert!(p.forbidden_path_argument("cat src/main.rs").is_none());
    }

    #[test]
    fn forbidden_path_argument_option_value() {
        let p = test_policy();
        assert!(
            p.forbidden_path_argument("cmd --file=/etc/passwd")
                .is_some()
        );
    }

    // ── Default policy sanity ───────────────────────────────────────────

    #[test]
    fn default_policy_sanity() {
        let p = SecurityPolicy::default();
        assert_eq!(p.autonomy, AutonomyLevel::Supervised);
        assert!(p.workspace_only);
        assert!(!p.allowed_commands.is_empty());
        assert!(!p.forbidden_paths.is_empty());
        assert!(p.block_high_risk_commands);
        assert!(p.require_approval_for_medium_risk);
    }

    #[test]
    fn security_checklist_root_path_blocked() {
        assert!(!test_policy().is_path_allowed("/"));
    }

    #[test]
    fn security_checklist_all_system_dirs_blocked() {
        let p = test_policy();
        for dir in &[
            "/etc", "/root", "/usr", "/bin", "/sbin", "/boot", "/dev", "/proc", "/sys",
        ] {
            assert!(!p.is_path_allowed(dir), "{dir} should be blocked");
        }
    }

    #[test]
    fn resolved_path_violation_message_content() {
        let p = test_policy();
        let msg = p.resolved_path_violation_message(Path::new("/etc/passwd"));
        assert!(msg.contains("escapes workspace"));
        assert!(msg.contains("allowed_roots"));
    }
}
