//! agentctl — low-level test harness for the CrewForge provider + agent stack.
//!
//! > **Deprecated as a standalone binary.**
//! > Prefer `crewforge agent` which integrates auth profiles and is the canonical
//! > user-facing entry point. `agentctl` is retained for internal CI / debugging.
//!
//! Usage:
//!   agentctl --provider anthropic --model claude-opus-4-6
//!   agentctl --provider openai --model gpt-4o --no-tools
//!   agentctl --provider ollama --model llama3.2 --base-url http://localhost:11434

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use crewforge::{
    agent::{AgentEvent, AgentSession, AgentSessionConfig, StopReason, Tool},
    provider::{self, ToolSpec},
};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "agentctl",
    about = "Interactive test CLI for CrewForge provider + agent stack"
)]
struct Args {
    /// Provider name: anthropic, openai, gemini, ollama, openrouter, glm, moonshot, qwen, etc.
    #[arg(long, short = 'p')]
    provider: String,

    /// Model name (e.g. claude-opus-4-6, gpt-4o, gemini-2.0-flash)
    #[arg(long, short = 'm')]
    model: String,

    /// Optional API key override (default: read from env var)
    #[arg(long)]
    api_key: Option<String>,

    /// Optional base URL override for custom or local endpoints
    #[arg(long)]
    base_url: Option<String>,

    /// System prompt for the agent
    #[arg(long, short = 's', default_value = "You are a helpful AI assistant.")]
    system: String,

    /// Disable tools — run as pure chat (useful for testing provider connectivity)
    #[arg(long)]
    no_tools: bool,

    /// Max tool-call iterations per turn
    #[arg(long, default_value = "10")]
    max_iterations: usize,

    /// Temperature
    #[arg(long, default_value = "0.7")]
    temperature: f64,
}

// ── Mock tools for testing ────────────────────────────────────────────────────

/// Echo tool: returns its input as-is. Useful for verifying the full tool-call cycle.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo back the provided message. Use this to verify tool calling works."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to echo back"
                }
            },
            "required": ["message"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> anyhow::Result<String> {
        let msg = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("[no message]");
        Ok(format!("Echo: {msg}"))
    }
}

/// Datetime tool: returns the current UTC time. No arguments.
struct DatetimeTool;

#[async_trait]
impl Tool for DatetimeTool {
    fn name(&self) -> &str {
        "get_datetime"
    }

    fn description(&self) -> &str {
        "Get the current UTC date and time."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn call(&self, _args: serde_json::Value) -> anyhow::Result<String> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Simple ISO-like format without chrono
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        Ok(format!("UTC unix_day={days} {:02}:{:02}:{:02}", h, m, s))
    }
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
            } else {
                if let Some(t) = text {
                    if !t.is_empty() {
                        eprintln!("\x1b[2m[llm]: {t}\x1b[0m");
                    }
                }
            }
            if let Some(u) = usage {
                if u.input_tokens.is_some() || u.output_tokens.is_some() {
                    eprintln!(
                        "\x1b[2m[tokens] in={} out={}\x1b[0m",
                        u.input_tokens.unwrap_or(0),
                        u.output_tokens.unwrap_or(0)
                    );
                }
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
            // final_text already printed via LlmResponse when tool_call_count==0
            if *iterations_used == 0 {
                if let Some(t) = final_text {
                    println!("{t}");
                }
            }
        }
        AgentEvent::Error { message, fatal } => {
            let label = if *fatal { "fatal error" } else { "error" };
            eprintln!("\x1b[31m[{label}] {message}\x1b[0m");
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Resolve API key: explicit --api-key > env var > stored auth profile.
    let resolved_key: Option<String> = if let Some(k) = &args.api_key {
        Some(k.clone())
    } else if provider::default_api_key_env(&args.provider)
        .and_then(|env| std::env::var(env).ok())
        .filter(|v| !v.is_empty())
        .is_some()
    {
        None // create_provider will pick it up from the env var
    } else {
        // Fall back to stored auth profile.
        let svc = crewforge::auth::AuthService::new(
            &crewforge::auth::default_state_dir(),
            false,
        );
        svc.get_provider_bearer_token(&args.provider, None)
            .await
            .unwrap_or(None)
    };

    // Build provider. create_provider returns Box<dyn Provider>; wrap in Arc for AgentSession.
    let provider: Arc<dyn provider::Provider> = Arc::from(provider::create_provider(
        &args.provider,
        resolved_key.as_deref(),
        args.base_url.as_deref(),
    )?);

    // Build tools (unless --no-tools).
    let tools: Vec<Box<dyn Tool>> = if args.no_tools {
        vec![]
    } else {
        vec![Box::new(EchoTool), Box::new(DatetimeTool)]
    };

    let config = AgentSessionConfig {
        max_iterations: args.max_iterations,
        temperature: args.temperature,
        ..Default::default()
    };

    let mut session = AgentSession::new(
        provider,
        &args.model,
        &args.system,
        tools,
        config,
    );

    // Migration notice.
    eprintln!("\x1b[2m[note] agentctl is an internal tool. Use `crewforge agent` instead.\x1b[0m");

    // Print startup banner.
    eprintln!(
        "\x1b[1magentctl\x1b[0m  provider={} model={} tools={}",
        args.provider,
        args.model,
        if args.no_tools { "off" } else { "echo,get_datetime" }
    );
    if !args.no_tools {
        eprintln!("tools: echo(message), get_datetime()");
    }
    eprintln!("Type your message and press Enter. Ctrl-D to exit.\n");

    // REPL loop.
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Print user prompt marker.
        print!("\x1b[1m> \x1b[0m");
        stdout.flush().ok();

        // Run agent turn.
        let events = session.run_turn(trimmed).await;

        for event in &events {
            print_event(event);
        }

        println!(); // blank line between turns
    }

    eprintln!("\nBye.");
    Ok(())
}
