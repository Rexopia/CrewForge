//! `crewforge agent` subcommand — interactive agent REPL backed by the native Rust provider stack.
//!
//! Credentials are resolved in priority order:
//!   1. `--api-key` flag
//!   2. Provider-specific environment variable (e.g. `OPENROUTER_API_KEY`)
//!   3. Stored auth profile (`crewforge auth paste-token / login`)
//!
//! Usage examples:
//!   crewforge agent --provider openrouter --model minimax/minimax-m2.5
//!   crewforge agent --provider anthropic --model claude-opus-4-6 --no-tools
//!   crewforge agent --provider ollama --model llama3.2 --base-url http://localhost:11434

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use anyhow::Result;
use clap::Args;
use crewforge::{
    agent::{AgentEvent, AgentSession, AgentSessionConfig, StopReason, Tool},
    auth::{AuthService, default_state_dir},
    provider::{self, default_api_key_env},
    security::SecurityPolicy,
    tools::{TokioRuntime, default_tools},
};

// ── Clap args ─────────────────────────────────────────────────────────────────

#[derive(Debug, Args)]
pub struct AgentArgs {
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

    /// Maximum tool-call iterations per turn [default: 10]
    #[arg(long, default_value = "10")]
    max_iterations: usize,

    /// Sampling temperature [default: 0.7]
    #[arg(long, default_value = "0.7")]
    temperature: f64,
}

// ── Event rendering ───────────────────────────────────────────────────────────

fn print_event(event: &AgentEvent) {
    match event {
        AgentEvent::LlmThinking { iteration } => {
            if *iteration == 0 {
                eprintln!("\x1b[2m[thinking...]\x1b[0m");
            } else {
                eprintln!("\x1b[2m[thinking... round {}]\x1b[0m", iteration + 1);
            }
        }
        AgentEvent::LlmResponse {
            text,
            tool_call_count,
            usage,
        } => {
            if *tool_call_count == 0 {
                if let Some(t) = text {
                    println!("{t}");
                }
            } else if let Some(t) = text
                && !t.is_empty()
            {
                eprintln!("\x1b[2m[llm]: {t}\x1b[0m");
            }
            if let Some(u) = usage
                && (u.input_tokens.is_some() || u.output_tokens.is_some())
            {
                eprintln!(
                    "\x1b[2m[tokens] in={} out={}\x1b[0m",
                    u.input_tokens.unwrap_or(0),
                    u.output_tokens.unwrap_or(0)
                );
            }
        }
        AgentEvent::ToolCallStarted {
            iteration,
            name,
            args,
        } => {
            let args_str = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
            eprintln!(
                "\x1b[33m[tool:{}] {}  {}\x1b[0m",
                iteration + 1,
                name,
                args_str
            );
        }
        AgentEvent::ToolCallFinished {
            name,
            result,
            success,
        } => {
            let icon = if *success { "✓" } else { "✗" };
            eprintln!("\x1b[32m[{icon} {name}] {result}\x1b[0m");
        }
        AgentEvent::TurnFinished {
            final_text,
            iterations_used,
            stop_reason,
        } => {
            let reason = match stop_reason {
                StopReason::Done => "done",
                StopReason::MaxIterations => "max_iterations",
                StopReason::Cancelled => "cancelled",
            };
            eprintln!(
                "\x1b[2m[turn finished: {} iteration(s), reason={}]\x1b[0m",
                iterations_used, reason
            );
            if *iterations_used == 0
                && let Some(t) = final_text
            {
                println!("{t}");
            }
        }
        AgentEvent::Error { message, fatal } => {
            let label = if *fatal { "fatal error" } else { "error" };
            eprintln!("\x1b[31m[{label}] {message}\x1b[0m");
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: AgentArgs) -> Result<()> {
    // Resolve API key: flag > env var > auth profile
    let resolved_key: Option<String> = if let Some(k) = &args.api_key {
        Some(k.clone())
    } else if default_api_key_env(&args.provider)
        .and_then(|env| std::env::var(env).ok())
        .filter(|v| !v.is_empty())
        .is_some()
    {
        None // create_provider picks it up from env
    } else {
        let svc = AuthService::new(&default_state_dir(), false);
        svc.get_provider_bearer_token(&args.provider, None)
            .await
            .unwrap_or(None)
    };

    let provider: Arc<dyn provider::Provider> = Arc::from(provider::create_provider(
        &args.provider,
        resolved_key.as_deref(),
        args.base_url.as_deref(),
    )?);

    let tools: Vec<Box<dyn Tool>> = if args.no_tools {
        vec![]
    } else {
        let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let security = Arc::new(SecurityPolicy {
            workspace_dir: workspace,
            ..SecurityPolicy::default()
        });
        let runtime = Arc::new(TokioRuntime);
        default_tools(security, runtime)
    };

    let config = AgentSessionConfig {
        max_iterations: args.max_iterations,
        temperature: args.temperature,
        ..Default::default()
    };

    let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
    let mut session = AgentSession::new(provider, &args.model, &args.system, tools, config);

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
    eprintln!("Type your message and press Enter. Ctrl-D to exit.\n");

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        print!("\x1b[1m> \x1b[0m");
        stdout.flush().ok();

        let events = session.run_turn(trimmed).await;
        for event in &events {
            print_event(event);
        }
        println!();
    }

    eprintln!("\nBye.");
    Ok(())
}
