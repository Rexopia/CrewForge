//! `crewforge agent` subcommand — single-turn debug interface with persistent sessions.
//!
//! Subcommands:
//!   crewforge agent chat -p <provider> -m <model> "message"
//!   crewforge agent clear
//!   crewforge agent show

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::{Args, Subcommand};
use crewforge::{
    agent::{
        AgentEvent, AgentSession, AgentSessionConfig, StopReason, Tool,
        sandbox::SecurityPolicy,
        tools::{TokioRuntime, default_tools},
    },
    auth::{AuthService, default_state_dir},
    provider::{self, ConversationMessage, ProviderRegistry},
};

const SESSION_FILE: &str = ".crewforge/debug-session.json";

// ── Clap args ────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub command: AgentCommand,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Send a message to the agent (persistent session)
    Chat(ChatArgs),
    /// Clear the session history
    Clear,
    /// Show the current session history
    Show,
}

#[derive(Debug, Args)]
pub struct ChatArgs {
    /// The message to send
    pub message: String,

    /// Provider: anthropic, openai, gemini, openrouter, ollama, copilot, openai-codex, etc.
    #[arg(long, short = 'p')]
    provider: String,

    /// Model name (e.g. claude-opus-4-6, gpt-4o, minimax/minimax-m2.5)
    #[arg(long, short = 'm')]
    model: String,

    /// API key override (default: env var → stored auth profile)
    #[arg(long)]
    api_key: Option<String>,

    /// Base URL override for custom or local endpoints
    #[arg(long)]
    base_url: Option<String>,

    /// System prompt
    #[arg(long, short = 's', default_value = "You are a helpful AI assistant.")]
    system: String,

    /// Disable tools and run as pure chat
    #[arg(long)]
    no_tools: bool,

    /// Maximum tool-call iterations per turn
    #[arg(long, default_value = "10")]
    max_iterations: usize,

    /// Sampling temperature
    #[arg(long, default_value = "0.7")]
    temperature: f64,
}

// ── Session persistence ──────────────────────────────────────────────────────

fn session_path() -> std::path::PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join(SESSION_FILE)
}

fn load_session() -> Vec<ConversationMessage> {
    let path = session_path();
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_session(history: &[ConversationMessage]) -> Result<()> {
    let path = session_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(history)?;
    std::fs::write(&path, json)?;
    Ok(())
}

// ── Debug output ─────────────────────────────────────────────────────────────

fn format_timestamp(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    let millis = elapsed.subsec_millis();
    format!("[{:02}:{:02}.{:03}]", secs / 60, secs % 60, millis)
}

fn print_history(history: &[ConversationMessage]) {
    if history.is_empty() {
        eprintln!("[HISTORY] (empty)");
        return;
    }
    eprintln!("[HISTORY]");
    for msg in history {
        // Skip system messages in display — they're rebuilt each turn.
        if let ConversationMessage::Chat(m) = msg
            && m.role == "system"
        {
            continue;
        }
        println!("{}", serde_json::to_string(msg).unwrap_or_default());
    }
    println!();
}

fn print_event(start: &Instant, event: &AgentEvent) {
    let ts = format_timestamp(start.elapsed());
    match event {
        AgentEvent::LlmThinking { iteration } => {
            eprintln!("{ts} [EVENT] LlmThinking iter={iteration}");
        }
        AgentEvent::LlmResponse {
            text,
            tool_call_count,
            usage,
        } => {
            let text_preview = text
                .as_ref()
                .map(|t| {
                    if t.len() > 200 {
                        format!("{}...", &t[..200])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_else(|| "(none)".into());
            let usage_str = usage
                .as_ref()
                .map(|u| {
                    format!(
                        " tokens=({},{})",
                        u.input_tokens.unwrap_or(0),
                        u.output_tokens.unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            eprintln!(
                "{ts} [EVENT] LlmResponse tools={tool_call_count}{usage_str} text={text_preview:?}"
            );
        }
        AgentEvent::ToolCallStarted {
            iteration,
            name,
            args,
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_default();
            let args_preview = if args_str.len() > 200 {
                format!("{}...", &args_str[..200])
            } else {
                args_str
            };
            eprintln!("{ts} [EVENT] ToolCallStarted iter={iteration} name={name:?} args={args_preview}");
        }
        AgentEvent::ToolCallFinished {
            name,
            result,
            success,
        } => {
            let icon = if *success { "OK" } else { "FAIL" };
            let result_preview = if result.len() > 300 {
                format!("{}...", &result[..300])
            } else {
                result.clone()
            };
            eprintln!("{ts} [EVENT] ToolCallFinished {icon} {name:?}: {result_preview:?}");
        }
        AgentEvent::TurnFinished {
            iterations_used,
            stop_reason,
            ..
        } => {
            let reason = match stop_reason {
                StopReason::Done => "done",
                StopReason::MaxIterations => "max_iterations",
                StopReason::Cancelled => "cancelled",
            };
            eprintln!("{ts} [EVENT] TurnFinished iters={iterations_used} reason={reason}");
        }
        AgentEvent::ResearchComplete {
            context_length,
            tool_call_count,
            duration_ms,
        } => {
            eprintln!(
                "{ts} [EVENT] ResearchComplete chars={context_length} tools={tool_call_count} ms={duration_ms}"
            );
        }
        AgentEvent::Error { message, fatal } => {
            let label = if *fatal { "FATAL" } else { "ERROR" };
            eprintln!("{ts} [{label}] {message}");
        }
    }
}

// ── Subcommand handlers ──────────────────────────────────────────────────────

async fn run_chat(args: ChatArgs) -> Result<()> {
    // Resolve API key: flag > env var > auth profile.
    let resolved_key: Option<String> = if let Some(k) = &args.api_key {
        Some(k.clone())
    } else if ProviderRegistry::load()
        .api_key_env(&args.provider)
        .and_then(|env| std::env::var(env).ok())
        .filter(|v| !v.is_empty())
        .is_some()
    {
        None // create_provider picks it up from env
    } else {
        let svc = AuthService::new(&default_state_dir(), true);
        svc.get_provider_bearer_token(&args.provider, None)
            .await
            .unwrap_or(None)
    };

    let provider: Arc<dyn provider::Provider> = Arc::from(provider::create_provider(
        &args.provider,
        resolved_key.as_deref(),
        args.base_url.as_deref(),
    )?);

    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let security = Arc::new(SecurityPolicy {
        workspace_dir: workspace,
        ..SecurityPolicy::default()
    });

    let tools: Vec<Box<dyn Tool>> = if args.no_tools {
        vec![]
    } else {
        let runtime = Arc::new(TokioRuntime);
        default_tools(security.clone(), runtime)
    };

    let config = AgentSessionConfig {
        max_iterations: args.max_iterations,
        temperature: args.temperature,
        ..Default::default()
    };

    let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let mut session =
        AgentSession::new(provider, &args.model, &args.system, tools, config, security);

    // Load existing session history.
    let saved_history = load_session();

    // Print header.
    eprintln!(
        "\x1b[1mcrewforge agent\x1b[0m  provider={} model={} tools={}",
        args.provider,
        args.model,
        if args.no_tools {
            "off".to_string()
        } else {
            tool_names.join(", ")
        }
    );

    // Print existing history.
    print_history(&saved_history);

    // Restore history into session.
    session.history = saved_history;

    // Run one turn.
    let start = Instant::now();
    let ts = format_timestamp(start.elapsed());
    eprintln!("{ts} [USER] {}", args.message);

    let events = session.run_turn(&args.message).await;
    for event in &events {
        print_event(&start, event);
    }

    // Print the final assistant response prominently.
    let final_text = events.iter().find_map(|e| {
        if let AgentEvent::TurnFinished { final_text, .. } = e {
            final_text.clone()
        } else {
            None
        }
    });
    if let Some(text) = &final_text {
        let ts = format_timestamp(start.elapsed());
        println!("\n{ts} [ASSISTANT]\n{text}");
    }

    // Persist updated history (strip system prompt — it's rebuilt each turn).
    let history_to_save: Vec<ConversationMessage> = session
        .history
        .into_iter()
        .filter(|m| {
            !matches!(m, ConversationMessage::Chat(msg) if msg.role == "system")
        })
        .collect();
    save_session(&history_to_save)?;

    let path = session_path();
    eprintln!(
        "\n\x1b[2m[session saved: {} messages → {}]\x1b[0m",
        history_to_save.len(),
        path.display()
    );

    Ok(())
}

fn run_clear() -> Result<()> {
    let path = session_path();
    if path.exists() {
        std::fs::remove_file(&path)?;
        eprintln!("Session cleared: {}", path.display());
    } else {
        eprintln!("No session file found at {}", path.display());
    }
    Ok(())
}

fn run_show() -> Result<()> {
    let history = load_session();
    if history.is_empty() {
        eprintln!("No session history.");
        return Ok(());
    }
    for (i, msg) in history.iter().enumerate() {
        let json = serde_json::to_string_pretty(msg)?;
        println!("[{i}] {json}");
    }
    eprintln!("\n({} messages total)", history.len());
    Ok(())
}

// ── Entry point ──────────────────────────────────────────────────────────────

pub async fn run(args: AgentArgs) -> Result<()> {
    match args.command {
        AgentCommand::Chat(chat_args) => run_chat(chat_args).await,
        AgentCommand::Clear => run_clear(),
        AgentCommand::Show => run_show(),
    }
}
