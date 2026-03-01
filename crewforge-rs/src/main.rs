mod agent_cmd;
mod auth_cmd;
mod chat;
mod config;
mod hub;
mod init;
mod kernel;
mod managed_opencode;
mod mcp_server;
mod opencode_provider;
mod profiles;
mod prompt_theme;
mod scheduler;
mod text;
mod tui;
mod update;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "crewforge", version, about = "CrewForge CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Manage global CrewForge profiles (~/.crewforge/profiles.json)
    Init(InitCommandArgs),

    /// Start chat runtime (replacement of npm run chat)
    Chat(ChatCommandArgs),

    /// Manage provider OAuth and API key authentication profiles
    Auth {
        #[command(subcommand)]
        auth_command: auth_cmd::AuthCommands,
    },

    /// Run an interactive agent session (native Rust provider stack)
    Agent(agent_cmd::AgentArgs),
}

#[derive(Debug, Args)]
struct InitCommandArgs {
    /// Delete a global profile by name
    #[arg(long = "delete")]
    delete: Option<String>,
}

#[derive(Debug, Args)]
struct ChatCommandArgs {
    /// Room config file path
    #[arg(long = "config", default_value = ".room/room.json")]
    config_path: String,

    /// Resume from an existing session id/path (for example session-... or .room/sessions/session-....jsonl)
    #[arg(long = "resume")]
    resume: Option<String>,

    /// Run without provider calls
    #[arg(long = "dry-run")]
    dry_run: bool,

    /// Emit machine-readable events and accept machine-readable commands on stdin
    #[arg(long = "rpc", value_enum)]
    rpc: Option<RpcModeArg>,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RpcModeArg {
    Jsonl,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init(args) => {
            init::run_init(init::InitArgs {
                delete: args.delete,
            })
            .await
        }
        Commands::Chat(args) => {
            chat::run_chat(chat::ChatArgs {
                config_path: args.config_path,
                resume: args.resume,
                dry_run: args.dry_run,
                rpc_jsonl: matches!(args.rpc, Some(RpcModeArg::Jsonl)),
            })
            .await
        }
        Commands::Auth { auth_command } => auth_cmd::run(auth_command).await,
        Commands::Agent(args) => agent_cmd::run(args).await,
    };

    if let Err(error) = result {
        eprintln!("crewforge failed: {error}");
        std::process::exit(1);
    }
}
