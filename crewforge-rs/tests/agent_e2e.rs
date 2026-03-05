//! End-to-end agent tests using real LLM provider (OpenAI Codex).
//!
//! These tests exercise the full agent stack: provider → orchestration → tools → response.
//! They require valid Codex OAuth credentials (`crewforge auth login --provider codex`).
//!
//! Run:   cargo test --manifest-path crewforge-rs/Cargo.toml --test agent_e2e -- --ignored --nocapture
//! Skip:  All tests are `#[ignore]` by default (no Codex auth in CI).
//!
//! Event traces are always printed (via eprintln) so Claude can inspect them during debugging.

use std::path::Path;
use std::sync::Arc;

use crewforge::agent::sandbox::SecurityPolicy;
use crewforge::agent::testing::EventLog;
use crewforge::agent::tools::{TokioRuntime, default_tools};
use crewforge::agent::{AgentSession, AgentSessionConfig, Tool};
use crewforge::provider::{self, Provider};

const MODEL: &str = "gpt-5.3-codex";
const PROVIDER_NAME: &str = "codex";

// ── Setup helpers ───────────────────────────────────────────────────────────

/// Try to create a real Codex provider. Returns None if auth is unavailable.
fn try_codex_provider() -> Option<Box<dyn Provider>> {
    match provider::create_provider(PROVIDER_NAME, None, None) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("[SKIP] Cannot create Codex provider: {e}");
            None
        }
    }
}

/// Create an agent session with real tools in a temp directory.
fn make_session(
    provider: Arc<dyn Provider>,
    workspace: &Path,
    system_prompt: &str,
    max_iterations: usize,
) -> AgentSession {
    let security = Arc::new(SecurityPolicy {
        workspace_dir: workspace.to_path_buf(),
        ..SecurityPolicy::default()
    });
    let runtime = Arc::new(TokioRuntime);
    let tools: Vec<Box<dyn Tool>> = default_tools(security.clone(), runtime);

    let config = AgentSessionConfig {
        max_iterations,
        temperature: 0.0,
        ..Default::default()
    };

    AgentSession::new(provider, MODEL, system_prompt, tools, config, security)
}

/// Create an agent session with NO tools (pure chat).
fn make_chat_session(
    provider: Arc<dyn Provider>,
    workspace: &Path,
    system_prompt: &str,
) -> AgentSession {
    let security = Arc::new(SecurityPolicy {
        workspace_dir: workspace.to_path_buf(),
        ..SecurityPolicy::default()
    });

    let config = AgentSessionConfig {
        max_iterations: 1,
        temperature: 0.0,
        ..Default::default()
    };

    AgentSession::new(provider, MODEL, system_prompt, vec![], config, security)
}

/// Run a turn, print the event trace, and return the EventLog.
async fn run_turn(session: &mut AgentSession, message: &str) -> EventLog {
    let events = session.run_turn(message).await;
    let log = EventLog(events);
    eprintln!("{}", log.dump());
    log
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// Pure chat: ask a factual question, no tools.
#[tokio::test]
#[ignore] // Requires Codex OAuth credentials; run with: cargo test --test agent_e2e -- --ignored
async fn e2e_simple_chat() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let mut session = make_chat_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. Answer in one sentence.",
    );

    let log = run_turn(&mut session, "What is 2 + 3? Answer with just the number.").await;

    log.assert_stop_reason("done");
    log.assert_has_final_text();
    log.assert_no_errors();
    assert_eq!(log.llm_rounds(), 1);
    assert_eq!(log.tool_calls(), 0);

    let text = log.final_text().unwrap();
    assert!(
        text.contains('5'),
        "Expected answer to contain '5', got: {text:?}"
    );
}

/// File read: pre-create a file, ask the agent to read it.
#[tokio::test]
#[ignore]
async fn e2e_file_read() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let test_file = workspace.path().join("hello.txt");
    std::fs::write(&test_file, "The secret word is: PINEAPPLE").unwrap();

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. When asked to read a file, use the file_read tool.",
        5,
    );

    let log = run_turn(
        &mut session,
        &format!(
            "Read the file at {} and tell me the secret word.",
            test_file.display()
        ),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_no_errors();
    log.assert_tool_called("file_read");

    let text = log.final_text().unwrap_or_default();
    assert!(
        text.to_uppercase().contains("PINEAPPLE"),
        "Expected the agent to find 'PINEAPPLE' in the file, got: {text:?}"
    );
}

/// File write: ask the agent to create a file, then verify it exists.
#[tokio::test]
#[ignore]
async fn e2e_file_write() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let target = workspace.path().join("output.txt");

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. When asked to create a file, use the file_write tool. \
         Write exactly the content specified, nothing more.",
        5,
    );

    let log = run_turn(
        &mut session,
        &format!(
            "Create a file at {} with the content: hello world",
            target.display()
        ),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_tool_called("file_write");

    assert!(
        target.exists(),
        "Expected file to be created at {}",
        target.display()
    );
    let content = std::fs::read_to_string(&target).unwrap();
    assert!(
        content.contains("hello world"),
        "Expected file to contain 'hello world', got: {content:?}"
    );
}

/// Shell execution: run a simple command and verify output.
#[tokio::test]
#[ignore]
async fn e2e_shell_command() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. When asked to run a command, use the shell tool.",
        5,
    );

    let log = run_turn(&mut session, "Run `echo CREWFORGE_TEST_42` and tell me the output.").await;

    log.assert_stop_reason("done");
    log.assert_no_errors();
    log.assert_tool_called("shell");

    let text = log.final_text().unwrap_or_default();
    assert!(
        text.contains("CREWFORGE_TEST_42"),
        "Expected agent to report the echo output, got: {text:?}"
    );
}

/// Multi-step: read a file, transform it, write the result.
#[tokio::test]
#[ignore]
async fn e2e_multi_step_read_write() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let source = workspace.path().join("source.txt");
    std::fs::write(&source, "alice bob charlie").unwrap();
    let target = workspace.path().join("upper.txt");

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. Use tools to read and write files.",
        10,
    );

    let log = run_turn(
        &mut session,
        &format!(
            "Read {} and write its content in UPPERCASE to {}",
            source.display(),
            target.display()
        ),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_tool_called("file_read");
    log.assert_tool_called("file_write");
    assert!(log.llm_rounds() >= 2, "Should take at least 2 LLM rounds");

    assert!(target.exists(), "Target file should be created");
    let content = std::fs::read_to_string(&target).unwrap();
    assert!(
        content.contains("ALICE") || content.contains("BOB") || content.contains("CHARLIE"),
        "Expected uppercase content, got: {content:?}"
    );
}

/// Glob search: create some files, ask the agent to find them.
#[tokio::test]
#[ignore]
async fn e2e_glob_search() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    // Create a few test files
    for name in ["alpha.rs", "beta.rs", "gamma.py"] {
        std::fs::write(workspace.path().join(name), format!("// {name}")).unwrap();
    }

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. Use glob_search to find files.",
        5,
    );

    let log = run_turn(
        &mut session,
        &format!(
            "Find all .rs files in {} and list their names.",
            workspace.path().display()
        ),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_tool_called("glob_search");

    let text = log.final_text().unwrap_or_default();
    assert!(
        text.contains("alpha") && text.contains("beta"),
        "Expected both .rs files mentioned, got: {text:?}"
    );
    assert!(
        !text.contains("gamma.py") || text.contains("gamma"),
        "gamma.py is a .py file, may or may not be mentioned"
    );
}

/// Content search: search for a pattern in files.
#[tokio::test]
#[ignore]
async fn e2e_content_search() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    std::fs::write(
        workspace.path().join("data.txt"),
        "line1: foo\nline2: bar\nline3: foo_bar\nline4: baz\n",
    )
    .unwrap();

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. Use content_search to grep files.",
        5,
    );

    let log = run_turn(
        &mut session,
        &format!(
            "Search for lines containing 'foo' in {} and tell me which lines match.",
            workspace.path().display()
        ),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_tool_called("content_search");

    let text = log.final_text().unwrap_or_default();
    // Should find at least line1 and line3
    assert!(
        text.contains("line1") || text.contains("foo"),
        "Expected search results mentioning matches, got: {text:?}"
    );
}

/// Error recovery: ask to read a non-existent file.
#[tokio::test]
#[ignore]
async fn e2e_error_recovery() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();
    let fake_path = workspace.path().join("does_not_exist.txt");

    let mut session = make_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. If a tool fails, explain the error to the user.",
        5,
    );

    let log = run_turn(
        &mut session,
        &format!("Read the file at {}", fake_path.display()),
    )
    .await;

    log.assert_stop_reason("done");
    log.assert_tool_called("file_read");

    // The tool should have reported an error (file not found)
    assert!(
        log.tool_failures() >= 1,
        "Expected at least one tool failure for non-existent file.{}",
        log.dump()
    );

    // The agent should still give a final answer explaining the error
    log.assert_has_final_text();
}

/// Multi-turn: verify conversation context carries across turns.
#[tokio::test]
#[ignore]
async fn e2e_multi_turn() {
    let Some(provider) = try_codex_provider() else {
        return;
    };
    let workspace = tempfile::tempdir().unwrap();

    let mut session = make_chat_session(
        Arc::from(provider),
        workspace.path(),
        "You are a concise assistant. Remember everything from previous messages.",
    );

    // Turn 1: establish a fact
    let log1 = run_turn(
        &mut session,
        "Remember this: the magic number is 42. Just say OK.",
    )
    .await;
    log1.assert_stop_reason("done");
    log1.assert_has_final_text();

    // Turn 2: recall the fact
    let log2 = run_turn(&mut session, "What was the magic number I told you?").await;
    log2.assert_stop_reason("done");

    let text = log2.final_text().unwrap_or_default();
    assert!(
        text.contains("42"),
        "Expected agent to recall '42', got: {text:?}"
    );
}
