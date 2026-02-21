mod chat;
mod config;
mod hub;
mod init;
mod kernel;
mod managed_opencode;
mod mcp_server;
mod provider;
mod scheduler;
mod text;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "crewforge", version, about = "CrewForge CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize .room config and managed agent configs
    Init(InitCommandArgs),

    /// Start chat runtime (replacement of npm run chat)
    Chat(ChatCommandArgs),
}

#[derive(Debug, Args)]
struct InitCommandArgs {
    /// Room config file path
    #[arg(long = "config", default_value = ".room/room.json")]
    config_path: String,

    /// Room name
    #[arg(long = "room", default_value = "brainstorm")]
    room_name: String,

    /// Human display name
    #[arg(long = "human", default_value = "Rex")]
    human: String,

    /// Comma-separated agent names
    #[arg(long = "agents", default_value = "Codex,Kimi,GLM")]
    agents: String,
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init(args) => {
            init::run_init(init::InitArgs {
                config_path: args.config_path,
                room_name: args.room_name,
                human: args.human,
                agents: args.agents,
            })
            .await
        }
        Commands::Chat(args) => {
            chat::run_chat(chat::ChatArgs {
                config_path: args.config_path,
                resume: args.resume,
                dry_run: args.dry_run,
            })
            .await
        }
    };

    if let Err(error) = result {
        eprintln!("crewforge failed: {error}");
        std::process::exit(1);
    }
}
