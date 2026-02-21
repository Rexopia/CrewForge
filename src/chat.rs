use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use cliclack::{input, intro, log, multiselect, outro, outro_cancel, select};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

use crate::config::{AgentConfig, AgentTools, RoomConfig, load_room_config};
use crate::hub::{RateLimitUsage, RoomHub};
use crate::kernel::{MessageRole, SessionKernel};
use crate::managed_opencode::{self, HUB_ACK_TOOL, HUB_GET_TOOL, HUB_POST_TOOL};
use crate::mcp_server::RoomHubMcpServer;
use crate::profiles::{self, GlobalProfile};
use crate::prompt_theme;
use crate::provider::{OpencodeCliProvider, OpencodeProviderConfig};
use crate::scheduler::{WakeDecision, WorkerState, decide_wake, on_wake_finished};
use crate::text::{format_time, to_single_line_error};
use crate::tui::DisplayLine;

const INIT_PLACEHOLDER_MCP_URL: &str = "http://127.0.0.1:0/mcp?token=init";
const MIN_ENABLED_AGENTS: usize = 2;
const CHAT_SETUP_CANCELED_MESSAGE: &str = "chat setup canceled by user";

#[derive(Debug, Clone)]
pub struct ChatArgs {
    pub config_path: String,
    pub resume: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
struct RuntimeAgent {
    id: String,
    name: String,
    model: String,
    runtime_dir: PathBuf,
    hub_token: String,
    tools: AgentTools,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionMeta {
    human: String,
    enabled_names: Vec<String>,
}

#[derive(Debug, Clone)]
struct RoomConfigTemplate {
    room_name: String,
    scheduler_mode: String,
    gather_interval_ms: u64,
    rate_window_ms: u64,
    rate_max_posts: usize,
    opencode_command: String,
    opencode_timeout_ms: u64,
    runtime_agent_name: String,
}

impl Default for RoomConfigTemplate {
    fn default() -> Self {
        Self {
            room_name: "brainstorm".to_string(),
            scheduler_mode: "event_loop".to_string(),
            gather_interval_ms: 5000,
            rate_window_ms: 60_000,
            rate_max_posts: 6,
            opencode_command: "opencode".to_string(),
            opencode_timeout_ms: 240_000,
            runtime_agent_name: "brainstorm-room".to_string(),
        }
    }
}

impl From<&RoomConfig> for RoomConfigTemplate {
    fn from(value: &RoomConfig) -> Self {
        Self {
            room_name: value.room_name.clone(),
            scheduler_mode: value.runtime.scheduler_mode.clone(),
            gather_interval_ms: value.runtime.event_loop.gather_interval_ms,
            rate_window_ms: value.runtime.rate_limit.window_ms,
            rate_max_posts: value.runtime.rate_limit.max_posts,
            opencode_command: value.opencode.command.clone(),
            opencode_timeout_ms: value.opencode.timeout_ms,
            runtime_agent_name: value.opencode.runtime_agent_name.clone(),
        }
    }
}

pub(crate) struct ChatRuntime {
    room: Arc<RoomConfig>,
    runtime_agents: Arc<Vec<RuntimeAgent>>,
    kernel: Arc<SessionKernel>,
    room_hub: Arc<RoomHub>,
    providers_by_agent_id: HashMap<String, Arc<Mutex<OpencodeCliProvider>>>,
    worker_state_by_agent_id: Arc<Mutex<HashMap<String, WorkerState>>>,
    active_wake_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    observed_event_seq: Arc<Mutex<u64>>,
    dry_run: bool,
    agent_index_by_id: HashMap<String, usize>,
    msg_tx: tokio::sync::mpsc::UnboundedSender<DisplayLine>,
}

impl ChatRuntime {
    pub(crate) fn human_name(&self) -> &str {
        &self.room.human
    }

    fn print_system_line(&self, text: &str) {
        self.send_display_line(DisplayLine::System(text.to_string()));
    }

    fn print_plain_line(&self, text: &str) {
        self.send_display_line(DisplayLine::System(text.to_string()));
    }

    fn send_display_line(&self, line: DisplayLine) {
        if self.msg_tx.send(line.clone()).is_err() {
            println!("{}", line.as_plain_text());
        }
    }

    fn print_message(
        &self,
        role: &MessageRole,
        speaker: &str,
        text: &str,
        ts: &str,
        agent_id: Option<&str>,
    ) {
        let ts = format_time(ts);
        let line = match role {
            MessageRole::Human => DisplayLine::Human {
                ts,
                speaker: speaker.to_string(),
                text: text.to_string(),
            },
            MessageRole::Agent => DisplayLine::Agent {
                ts,
                speaker: speaker.to_string(),
                text: text.to_string(),
                agent_idx: agent_id
                    .and_then(|id| self.agent_index_by_id.get(id).copied())
                    .unwrap_or(0),
            },
        };
        self.send_display_line(line);
    }

    async fn mark_agents_dirty(&self, exclude_agent_id: Option<&str>) {
        let mut states = self.worker_state_by_agent_id.lock().await;
        for agent in self.runtime_agents.iter() {
            if exclude_agent_id.is_some() && exclude_agent_id == Some(agent.id.as_str()) {
                continue;
            }
            if let Some(state) = states.get_mut(&agent.id) {
                state.dirty = true;
            }
        }
    }

    async fn add_message(
        &self,
        role: MessageRole,
        speaker: String,
        text: String,
        agent_id: Option<String>,
    ) -> Result<()> {
        let message = self
            .kernel
            .append_message(role.clone(), speaker, text, agent_id.clone())
            .await?;

        self.print_message(
            &message.role,
            &message.speaker,
            &message.text,
            &message.ts,
            message.agent_id.as_deref(),
        );

        match message.role {
            MessageRole::Human => self.mark_agents_dirty(None).await,
            MessageRole::Agent => self.mark_agents_dirty(message.agent_id.as_deref()).await,
        }

        let mut observed = self.observed_event_seq.lock().await;
        if message.event_seq > *observed {
            *observed = message.event_seq;
        }

        Ok(())
    }

    async fn sync_new_events_from_kernel(&self) -> Result<()> {
        let transcript = self.kernel.transcript_snapshot().await;
        let mut observed = self.observed_event_seq.lock().await;
        let mut latest = *observed;
        for event in transcript
            .iter()
            .filter(|event| event.event_seq > *observed)
        {
            match event.role {
                MessageRole::Human => {
                    // Human messages are written via add_message; avoid duplicate side effects.
                }
                MessageRole::Agent => {
                    self.print_message(
                        &event.role,
                        &event.speaker,
                        &event.text,
                        &event.ts,
                        event.agent_id.as_deref(),
                    );
                    self.mark_agents_dirty(event.agent_id.as_deref()).await;
                }
            }
            latest = latest.max(event.event_seq);
        }
        *observed = latest;
        Ok(())
    }

    async fn ask_agent_event_turn(&self, agent_id: &str) -> Result<String> {
        let agent = self
            .runtime_agents
            .iter()
            .find(|item| item.id == agent_id)
            .ok_or_else(|| anyhow!("unknown agent id: {agent_id}"))?;

        let rate = self.room_hub.get_rate_limit_usage(agent_id).await?;

        if self.dry_run {
            let delay_ms: u64 = {
                let mut rng = rand::rng();
                rng.random_range(60_u64..=180_u64)
            };
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;

            let unread_result = self.room_hub.get_unread(agent_id).await?;
            let ack_seq = unread_result.upto_event_seq;

            if unread_result.unread.is_empty() || rate.remaining == 0 {
                return Ok("[DROP]".to_string());
            }

            let _ = self
                .room_hub
                .post(
                    agent_id,
                    &format!(
                        "[dry-run:{}] I want to add an architectural point: stabilize the Room protocol first, then swap the integration layer.",
                        agent.name
                    ),
                    Some(ack_seq),
                )
                .await?;
            return Ok("[POSTED]".to_string());
        }

        let provider = self
            .providers_by_agent_id
            .get(agent_id)
            .ok_or_else(|| anyhow!("missing provider for agent: {agent_id}"))?;

        let mut provider = provider.lock().await;
        let prompt = build_event_turn_prompt(&self.room, agent, &rate);
        provider.send_prompt(&prompt).await
    }

    async fn try_wake(self: Arc<Self>, agent_id: String) -> Result<()> {
        let has_unread = self.room_hub.has_unread(&agent_id).await?;

        let decision = {
            let mut states = self.worker_state_by_agent_id.lock().await;
            let state = states
                .get(&agent_id)
                .copied()
                .ok_or_else(|| anyhow!("worker state missing for agent: {agent_id}"))?;

            let decision = decide_wake(state, has_unread);
            match decision {
                WakeDecision::Skip => return Ok(()),
                WakeDecision::ClearDirty => {
                    if let Some(next) = states.get_mut(&agent_id) {
                        next.dirty = false;
                    }
                    return Ok(());
                }
                WakeDecision::Run => {
                    if let Some(next) = states.get_mut(&agent_id) {
                        next.running = true;
                        next.dirty = false;
                    }
                }
            }
            decision
        };

        if decision != WakeDecision::Run {
            return Ok(());
        }

        if let Err(error) = self.ask_agent_event_turn(&agent_id).await {
            let error_line = to_single_line_error(&error.to_string());
            let speaker = self
                .runtime_agents
                .iter()
                .find(|item| item.id == agent_id)
                .map(|item| item.name.clone())
                .unwrap_or_else(|| agent_id.clone());
            let _ = self
                .add_message(
                    MessageRole::Agent,
                    speaker,
                    format!("[provider error] {error_line}"),
                    Some(agent_id.clone()),
                )
                .await;
        }

        let _ = self.sync_new_events_from_kernel().await;

        let has_unread_after = self.room_hub.has_unread(&agent_id).await.unwrap_or(false);
        let mut states = self.worker_state_by_agent_id.lock().await;
        if let Some(current) = states.get_mut(&agent_id) {
            *current = on_wake_finished(*current, has_unread_after);
        }

        Ok(())
    }

    async fn gather_tick(self: Arc<Self>) {
        let _ = self.sync_new_events_from_kernel().await;
        self.prune_finished_tasks().await;

        for agent in self.runtime_agents.iter() {
            let should_try = {
                let states = self.worker_state_by_agent_id.lock().await;
                states
                    .get(&agent.id)
                    .map(|state| !state.running && state.dirty)
                    .unwrap_or(false)
            };

            if !should_try {
                continue;
            }

            let runtime = self.clone();
            let agent_id = agent.id.clone();
            let task = tokio::spawn(async move {
                if let Err(error) = runtime.clone().try_wake(agent_id).await {
                    runtime.print_system_line(&format!(
                        "[wake error] {}",
                        to_single_line_error(&error.to_string())
                    ));
                }
            });

            self.active_wake_tasks.lock().await.push(task);
        }
    }

    async fn prune_finished_tasks(&self) {
        let mut tasks = self.active_wake_tasks.lock().await;
        tasks.retain(|task| !task.is_finished());
    }

    async fn wait_active_tasks(&self, max_wait_ms: u64) {
        let mut tasks = {
            let mut guard = self.active_wake_tasks.lock().await;
            std::mem::take(&mut *guard)
        };

        if tasks.is_empty() {
            return;
        }

        let deadline = Instant::now() + Duration::from_millis(max_wait_ms);
        loop {
            let all_finished = tasks.iter().all(|task| task.is_finished());
            if all_finished {
                break;
            }
            if Instant::now() >= deadline {
                for task in &tasks {
                    task.abort();
                }
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }

        for task in tasks.drain(..) {
            let _ = task.await;
        }
        let _ = self.sync_new_events_from_kernel().await;
    }
}

pub async fn run_chat(args: ChatArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve current dir")?;
    let config_path = cwd.join(Path::new(&args.config_path));
    let is_tty = std::io::stdin().is_terminal();
    let sessions_dir = cwd.join(".room/sessions");

    if args.resume.is_none() {
        prepare_room_config_for_chat(&cwd, &config_path, is_tty).await?;
    }

    let mut room = load_room_config(&config_path, cwd.clone())?;

    let (kernel, resumed_from) =
        open_session_kernel(&cwd, &sessions_dir, args.resume.as_deref()).await?;
    let resume_warnings = if let Some(session_file) = resumed_from.as_ref() {
        let meta = read_session_meta(session_file).await?;
        apply_resume_meta(&mut room, &meta).await?
    } else {
        let enabled_names = room
            .agents
            .iter()
            .map(|agent| agent.name.clone())
            .collect::<Vec<_>>();
        write_session_meta(&kernel.session_file, &room.human, &enabled_names).await?;
        Vec::new()
    };

    ensure_room_agent_configs(&cwd, &room).await?;

    let initial_snapshot = kernel.transcript_snapshot().await;
    let initial_observed_event_seq = initial_snapshot
        .iter()
        .map(|item| item.event_seq)
        .max()
        .unwrap_or(0);
    let app_session_id = kernel
        .session_file
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("session")
        .to_string();
    room.runtime.app_session_id = Some(app_session_id.clone());

    let (runtime_agents, _runtime_root) = initialize_runtime_agent_contexts(&room).await?;

    let room_hub = Arc::new(RoomHub::new(
        kernel.clone(),
        &room.agents,
        room.runtime.rate_limit.clone(),
    ));
    if resumed_from.is_some() {
        room_hub
            .set_all_agent_cursors(initial_observed_event_seq)
            .await;
    }

    let mut mcp_server = if args.dry_run {
        None
    } else {
        let mut server = RoomHubMcpServer::new(
            "127.0.0.1",
            runtime_agents
                .iter()
                .map(|agent| (agent.id.clone(), agent.hub_token.clone()))
                .collect(),
        );
        server.start(room_hub.clone()).await?;
        write_runtime_opencode_configs(&room, &runtime_agents, &server).await?;
        Some(server)
    };

    let mut providers_by_agent_id = HashMap::new();
    for agent in &runtime_agents {
        let provider = OpencodeCliProvider::new(
            OpencodeProviderConfig {
                command: room.opencode.command.clone(),
                timeout_ms: room.opencode.timeout_ms,
                runtime_agent_name: room.opencode.runtime_agent_name.clone(),
                workspace_dir: room.workspace_dir.clone(),
            },
            agent.model.clone(),
            agent.runtime_dir.clone(),
        );
        providers_by_agent_id.insert(agent.id.clone(), Arc::new(Mutex::new(provider)));
    }

    let mut worker_state_by_agent_id = HashMap::new();
    let mut agent_index_by_id = HashMap::new();
    for (idx, agent) in runtime_agents.iter().enumerate() {
        worker_state_by_agent_id.insert(agent.id.clone(), WorkerState::new());
        agent_index_by_id.insert(agent.id.clone(), idx);
    }
    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<DisplayLine>();
    let mut tty_msg_rx = Some(msg_rx);
    if !is_tty {
        let _ = tty_msg_rx.take();
    }

    let runtime = Arc::new(ChatRuntime {
        room: Arc::new(room),
        runtime_agents: Arc::new(runtime_agents),
        kernel,
        room_hub,
        providers_by_agent_id,
        worker_state_by_agent_id: Arc::new(Mutex::new(worker_state_by_agent_id)),
        active_wake_tasks: Arc::new(Mutex::new(Vec::new())),
        observed_event_seq: Arc::new(Mutex::new(initial_observed_event_seq)),
        dry_run: args.dry_run,
        agent_index_by_id,
        msg_tx,
    });

    runtime.print_system_line(&format!("Human: {}", runtime.room.human));
    runtime.print_system_line(&format!(
        "Agents: {}",
        runtime
            .runtime_agents
            .iter()
            .map(|item| format!("{}[{}]", item.name, item.model))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    for warning in &resume_warnings {
        runtime.print_system_line(&format!("[warning] {warning}"));
    }
    if args.dry_run {
        runtime.print_system_line("Running in dry-run mode (no provider calls).");
    }
    runtime.print_system_line("Type /help for commands.");
    if resumed_from.is_some() && !initial_snapshot.is_empty() {
        runtime.print_system_line("Loaded session history:");
        for event in &initial_snapshot {
            runtime.print_message(
                &event.role,
                &event.speaker,
                &event.text,
                &event.ts,
                event.agent_id.as_deref(),
            );
        }
    }

    let stop_flag = Arc::new(AtomicBool::new(false));
    let mut watchdog_handle: Option<JoinHandle<()>> = None;
    let mut seen_human_message = false;

    if is_tty {
        let msg_rx = tty_msg_rx
            .take()
            .ok_or_else(|| anyhow!("internal error: missing TTY display receiver"))?;
        crate::tui::run_tui_loop(runtime.clone(), msg_rx, stop_flag.clone()).await?;
    } else {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut line_buf = Vec::new();
        loop {
            line_buf.clear();
            let size = reader.read_until(b'\n', &mut line_buf).await?;
            if size == 0 {
                break;
            }

            let input = decode_user_input_line(&line_buf);
            let should_exit = handle_user_input(
                runtime.clone(),
                input,
                &mut seen_human_message,
                &mut watchdog_handle,
                stop_flag.clone(),
            )
            .await?;

            if should_exit {
                break;
            }
        }
    }

    stop_flag.store(true, Ordering::SeqCst);
    if let Some(handle) = watchdog_handle.take() {
        handle.abort();
        let _ = handle.await;
    }
    runtime.wait_active_tasks(1_000).await;
    if let Some(server) = mcp_server.as_mut() {
        server.stop().await?;
    }

    runtime.print_plain_line(&format!(
        "Resume this session with: {}",
        build_resume_hint_command(&args, &app_session_id)
    ));
    Ok(())
}

async fn prepare_room_config_for_chat(cwd: &Path, config_path: &Path, is_tty: bool) -> Result<()> {
    let has_room_config = tokio::fs::try_exists(config_path).await.with_context(|| {
        format!(
            "failed checking room config existence: {}",
            config_path.display()
        )
    })?;

    if !has_room_config {
        if !is_tty {
            bail!(
                "room config not found: {}. Run `crewforge chat` in an interactive terminal to configure this directory.",
                config_path.display()
            );
        }
    } else if !is_tty {
        return Ok(());
    }

    prompt_theme::install_prompt_theme();

    if let Err(error) = intro("CrewForge Chat Setup") {
        return Err(error).context("failed showing chat setup intro");
    }
    let _ = cliclack::note("Workspace", cwd.display().to_string());

    let result = if !has_room_config {
        let profiles = load_required_global_profiles().await?;
        let template = RoomConfigTemplate::default();
        let human = prompt_human_name("Rex")?;
        let selected_profiles = prompt_enabled_profiles(&profiles, &[])?;
        write_room_config_from_profiles(cwd, config_path, &template, &human, &selected_profiles)
            .await
    } else {
        let existing_room = load_room_config(config_path, cwd.to_path_buf())?;
        if prompt_continue_current_config()? {
            Ok(())
        } else {
            let profiles = load_required_global_profiles().await?;
            let template = RoomConfigTemplate::from(&existing_room);
            let existing_names = existing_room
                .agents
                .iter()
                .map(|item| item.name.clone())
                .collect::<Vec<_>>();
            let human = prompt_human_name(&existing_room.human)?;
            let selected_profiles = prompt_enabled_profiles(&profiles, &existing_names)?;
            write_room_config_from_profiles(cwd, config_path, &template, &human, &selected_profiles)
                .await
        }
    };

    match &result {
        Ok(()) => {
            let _ = outro("Chat configuration ready.");
        }
        Err(error) if is_chat_setup_canceled(error) => {
            let _ = outro_cancel("Chat setup canceled.");
        }
        Err(_) => {
            let _ = outro_cancel("Chat setup failed.");
        }
    }
    result
}

async fn load_required_global_profiles() -> Result<Vec<GlobalProfile>> {
    let profiles_path = profiles::global_profiles_path()?;
    let profiles = profiles::load_profiles(&profiles_path).await?;
    if profiles.is_empty() {
        bail!(
            "no global profiles found in {}. Run `crewforge init` first.",
            profiles_path.display()
        );
    }
    Ok(profiles)
}

fn prompt_continue_current_config() -> Result<bool> {
    let mut picker = select("Room config already exists. Choose action.")
        .item("continue".to_string(), "Continue current configuration", "")
        .item(
            "reconfigure".to_string(),
            "Reconfigure enabled profiles",
            "Rewrite .room/room.json with selected profile names",
        );
    let choice = picker.interact().map_err(chat_setup_prompt_error)?;
    Ok(choice == "continue")
}

fn prompt_human_name(default_value: &str) -> Result<String> {
    prompt_theme::clear_filter_input_highlight();
    let mut prompt = input("Human display name")
        .default_input(default_value)
        .validate(|value: &String| {
            if value.trim().is_empty() {
                Err("human name cannot be empty")
            } else {
                Ok(())
            }
        });
    let human: String = prompt.interact().map_err(chat_setup_prompt_error)?;
    Ok(human.trim().to_string())
}

fn prompt_enabled_profiles(
    profiles: &[GlobalProfile],
    initial_selected_names: &[String],
) -> Result<Vec<GlobalProfile>> {
    loop {
        let _highlight = prompt_theme::filter_input_highlight_scope();
        let mut picker =
            multiselect("Select enabled profiles for this chat\n(Press Space to select/deselect)")
                .filter_mode()
                .max_rows(10)
                .required(false);

        if !initial_selected_names.is_empty() {
            picker = picker.initial_values(initial_selected_names.to_vec());
        }

        for profile in profiles {
            let hint = profile
                .preference
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("");
            let label = format!("{} [{}]", profile.name, profile.model);
            picker = picker.item(profile.name.clone(), label, hint);
        }

        let selected_names = picker.interact().map_err(chat_setup_prompt_error)?;
        if selected_names.len() < MIN_ENABLED_AGENTS {
            let _ = log::warning(format!(
                "Select at least {MIN_ENABLED_AGENTS} profiles for room chat."
            ));
            continue;
        }

        let mut profile_by_name = HashMap::new();
        for profile in profiles {
            profile_by_name.insert(profile.name.as_str(), profile);
        }

        let mut seen = HashSet::new();
        let mut selected_profiles = Vec::new();
        for name in selected_names {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(profile) = profile_by_name.get(name.as_str()) {
                selected_profiles.push((*profile).clone());
            }
        }
        return Ok(selected_profiles);
    }
}

async fn write_room_config_from_profiles(
    cwd: &Path,
    config_path: &Path,
    template: &RoomConfigTemplate,
    human: &str,
    profiles: &[GlobalProfile],
) -> Result<()> {
    let config_parent = config_path
        .parent()
        .ok_or_else(|| anyhow!("invalid config path: {}", config_path.display()))?;
    tokio::fs::create_dir_all(config_parent)
        .await
        .with_context(|| {
            format!(
                "failed creating config directory: {}",
                config_parent.display()
            )
        })?;
    tokio::fs::create_dir_all(cwd.join(".room/sessions"))
        .await
        .context("failed creating .room/sessions")?;
    tokio::fs::create_dir_all(cwd.join(".room/runtime"))
        .await
        .context("failed creating .room/runtime")?;

    let agents = profiles
        .iter()
        .map(profile_to_room_agent_json)
        .collect::<Vec<_>>();
    let room_config = json!({
        "roomName": template.room_name,
        "human": human,
        "historyWindow": 18,
        "runtime": {
            "schedulerMode": template.scheduler_mode,
            "eventLoop": {
                "gatherIntervalMs": template.gather_interval_ms
            },
            "rateLimit": {
                "windowMs": template.rate_window_ms,
                "maxPosts": template.rate_max_posts
            }
        },
        "opencode": {
            "command": template.opencode_command,
            "timeoutMs": template.opencode_timeout_ms,
            "runtimeAgentName": template.runtime_agent_name
        },
        "agents": agents
    });

    let text = format!("{}\n", serde_json::to_string_pretty(&room_config)?);
    tokio::fs::write(config_path, text)
        .await
        .with_context(|| format!("failed writing room config: {}", config_path.display()))?;
    Ok(())
}

fn profile_to_room_agent_json(profile: &GlobalProfile) -> Value {
    let id = profiles::normalize_name(&profile.name);
    let mut value = json!({
        "id": id,
        "name": profile.name,
        "model": profile.model,
        "contextDir": format!(".room/agents/{id}"),
        "tools": {
            "edit": false,
            "write": false
        }
    });

    if let Some(preference) = &profile.preference
        && !preference.trim().is_empty()
    {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("preference".to_string(), json!(preference.trim()));
        }
    }

    value
}

fn chat_setup_prompt_error(error: std::io::Error) -> anyhow::Error {
    if error.kind() == ErrorKind::Interrupted {
        anyhow!(CHAT_SETUP_CANCELED_MESSAGE)
    } else {
        anyhow!("chat setup prompt failed: {error}")
    }
}

fn is_chat_setup_canceled(error: &anyhow::Error) -> bool {
    error.to_string().contains(CHAT_SETUP_CANCELED_MESSAGE)
}

async fn apply_resume_meta(room: &mut RoomConfig, meta: &SessionMeta) -> Result<Vec<String>> {
    let profiles_path = profiles::global_profiles_path()?;
    let global_profiles = profiles::load_profiles(&profiles_path).await?;
    let profile_by_name = global_profiles
        .iter()
        .map(|item| (item.name.clone(), item.clone()))
        .collect::<HashMap<_, _>>();

    let mut warnings = Vec::new();
    let mut selected_agents = Vec::new();
    let mut seen_ids = HashSet::new();

    for raw_name in &meta.enabled_names {
        let name = raw_name.trim();
        if name.is_empty() {
            continue;
        }

        let Some(profile) = profile_by_name.get(name) else {
            warnings.push(format!(
                "profile \"{name}\" was deleted from {} and has been disabled",
                profiles_path.display()
            ));
            continue;
        };

        let agent = profile_to_agent_config(profile);
        if !seen_ids.insert(agent.id.clone()) {
            warnings.push(format!(
                "profile \"{name}\" conflicts after normalization and has been skipped"
            ));
            continue;
        }
        selected_agents.push(agent);
    }

    if selected_agents.is_empty() {
        bail!("resume failed: no enabled profiles available");
    }

    let human = meta.human.trim();
    if !human.is_empty() {
        room.human = human.to_string();
    }
    room.agents = selected_agents;
    Ok(warnings)
}

fn profile_to_agent_config(profile: &GlobalProfile) -> AgentConfig {
    let id = profiles::normalize_name(&profile.name);
    AgentConfig {
        id: id.clone(),
        name: profile.name.clone(),
        model: profile.model.clone(),
        context_dir: format!(".room/agents/{id}"),
        tools: AgentTools::default(),
        preference: profile.preference.clone(),
    }
}

async fn ensure_room_agent_configs(cwd: &Path, room: &RoomConfig) -> Result<()> {
    let members = managed_opencode::build_members(
        &room.human,
        room.agents.iter().map(|item| item.name.clone()),
    );

    for agent in &room.agents {
        let runtime_dir = cwd.join(Path::new(&agent.context_dir));
        tokio::fs::create_dir_all(&runtime_dir)
            .await
            .with_context(|| format!("failed creating runtime dir: {}", runtime_dir.display()))?;

        let config_path = runtime_dir.join("opencode.json");
        if tokio::fs::try_exists(&config_path).await.unwrap_or(false) {
            continue;
        }

        let config = managed_opencode::build_managed_opencode_config(
            &room.opencode.runtime_agent_name,
            &agent.name,
            &members,
            INIT_PLACEHOLDER_MCP_URL,
            false,
            agent.preference.as_deref(),
        );
        let text = format!("{}\n", serde_json::to_string_pretty(&config)?);
        tokio::fs::write(&config_path, text)
            .await
            .with_context(|| {
                format!(
                    "failed writing managed opencode config: {}",
                    config_path.display()
                )
            })?;
    }

    Ok(())
}

fn session_meta_path(session_file: &Path) -> Result<PathBuf> {
    let session_id = session_file
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("invalid session file: {}", session_file.display()))?;
    let parent = session_file
        .parent()
        .ok_or_else(|| anyhow!("invalid session parent: {}", session_file.display()))?;
    Ok(parent.join(format!("{session_id}.meta.json")))
}

async fn read_session_meta(session_file: &Path) -> Result<SessionMeta> {
    let meta_path = session_meta_path(session_file)?;
    let raw = tokio::fs::read_to_string(&meta_path)
        .await
        .with_context(|| format!("failed reading session metadata: {}", meta_path.display()))?;
    let meta: SessionMeta = serde_json::from_str(&raw)
        .with_context(|| format!("invalid session metadata JSON: {}", meta_path.display()))?;
    Ok(meta)
}

async fn write_session_meta(
    session_file: &Path,
    human: &str,
    enabled_names: &[String],
) -> Result<()> {
    let meta_path = session_meta_path(session_file)?;
    let meta = SessionMeta {
        human: human.trim().to_string(),
        enabled_names: enabled_names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect(),
    };
    let text = format!("{}\n", serde_json::to_string_pretty(&meta)?);
    tokio::fs::write(&meta_path, text)
        .await
        .with_context(|| format!("failed writing session metadata: {}", meta_path.display()))?;
    Ok(())
}

fn decode_user_input_line(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).trim().to_string()
}

pub(crate) async fn handle_user_input(
    runtime: Arc<ChatRuntime>,
    user_input: String,
    seen_human_message: &mut bool,
    watchdog_handle: &mut Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
) -> Result<bool> {
    if user_input.is_empty() {
        return Ok(false);
    }

    if user_input == "/exit" || user_input == "/quit" {
        stop_flag.store(true, Ordering::SeqCst);
        return Ok(true);
    }

    if user_input == "/help" {
        for line in help_text().lines() {
            runtime.print_plain_line(line);
        }
        return Ok(false);
    }

    if user_input == "/agents" {
        runtime.print_system_line(
            &runtime
                .runtime_agents
                .iter()
                .map(|agent| {
                    format!(
                        "{} [{}] -> {}",
                        agent.name,
                        agent.model,
                        agent.runtime_dir.display()
                    )
                })
                .collect::<Vec<_>>()
                .join(" | "),
        );
        return Ok(false);
    }

    runtime
        .add_message(
            MessageRole::Human,
            runtime.room.human.clone(),
            user_input,
            None,
        )
        .await?;

    if !*seen_human_message {
        *seen_human_message = true;
        *watchdog_handle = Some(spawn_watchdog(
            runtime.clone(),
            stop_flag,
            runtime.room.runtime.event_loop.gather_interval_ms,
        ));
    }

    Ok(false)
}

fn spawn_watchdog(
    runtime: Arc<ChatRuntime>,
    stop_flag: Arc<AtomicBool>,
    gather_interval_ms: u64,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(gather_interval_ms));
        loop {
            ticker.tick().await;
            if stop_flag.load(Ordering::SeqCst) {
                break;
            }
            runtime.clone().gather_tick().await;
        }
    })
}

fn help_text() -> &'static str {
    "Usage:\n  crewforge chat\n  crewforge chat --dry-run\n  crewforge chat --resume <session-id|path>\n\nCommands:\n  /help    Show help\n  /agents  List members\n  /exit    Quit chat\n  /quit    Quit chat (alias)\n\nScheduler:\n  event_loop (5s gather watchdog)\n\nNotes:\n- Default mode creates a fresh room session.\n- `--resume` appends to an existing session log.\n- Agent config dirs are persistent (for example .room/agents/<id>/opencode.json).\n- CrewForge refreshes MCP runtime endpoint in managed opencode.json files."
}

fn build_event_turn_prompt(
    _room: &RoomConfig,
    _agent: &RuntimeAgent,
    _context: &RateLimitUsage,
) -> String {
    format!(
        "There are new messages in the room. Call {get_tool} to read unread updates.",
        get_tool = HUB_GET_TOOL,
    )
}

fn build_resume_hint_command(args: &ChatArgs, session_id: &str) -> String {
    if args.config_path == ".room/room.json" {
        format!("crewforge chat --resume {session_id}")
    } else {
        format!(
            "crewforge chat --config {} --resume {session_id}",
            args.config_path
        )
    }
}

async fn open_session_kernel(
    cwd: &Path,
    sessions_dir: &Path,
    resume: Option<&str>,
) -> Result<(Arc<SessionKernel>, Option<PathBuf>)> {
    if let Some(raw_resume) = resume {
        let resume = raw_resume.trim();
        if resume.is_empty() {
            return Err(anyhow!("--resume value cannot be empty"));
        }
        let session_file = resolve_resume_session_file(cwd, sessions_dir, resume).await;
        let meta_file = session_meta_path(&session_file)?;
        let meta_exists = tokio::fs::try_exists(&meta_file).await.with_context(|| {
            format!(
                "failed checking resume metadata file existence: {}",
                meta_file.display()
            )
        })?;
        if !meta_exists {
            bail!(
                "resume failed: metadata file not found: {}",
                meta_file.display()
            );
        }
        let kernel = SessionKernel::load(session_file.clone()).await?;
        return Ok((Arc::new(kernel), Some(session_file)));
    }

    let kernel = SessionKernel::create_new(sessions_dir).await?;
    Ok((Arc::new(kernel), None))
}

async fn resolve_resume_session_file(cwd: &Path, sessions_dir: &Path, resume: &str) -> PathBuf {
    let raw = PathBuf::from(resume);
    if raw.is_absolute() {
        return raw;
    }

    let cwd_candidate = cwd.join(&raw);
    if tokio::fs::try_exists(&cwd_candidate).await.unwrap_or(false) {
        return cwd_candidate;
    }

    if raw.components().count() > 1 {
        return cwd_candidate;
    }

    if resume.ends_with(".jsonl") {
        sessions_dir.join(resume)
    } else {
        sessions_dir.join(format!("{resume}.jsonl"))
    }
}

async fn initialize_runtime_agent_contexts(
    room: &RoomConfig,
) -> Result<(Vec<RuntimeAgent>, PathBuf)> {
    let runtime_root = room.workspace_dir.join(".room").join("agents");
    tokio::fs::create_dir_all(&runtime_root)
        .await
        .with_context(|| format!("failed creating runtime root: {}", runtime_root.display()))?;

    let mut runtime_agents = Vec::with_capacity(room.agents.len());
    for agent in &room.agents {
        let runtime_dir = room.workspace_dir.join(Path::new(&agent.context_dir));
        tokio::fs::create_dir_all(&runtime_dir)
            .await
            .with_context(|| format!("failed creating runtime dir: {}", runtime_dir.display()))?;

        runtime_agents.push(RuntimeAgent {
            id: agent.id.clone(),
            name: agent.name.clone(),
            model: agent.model.clone(),
            runtime_dir,
            hub_token: Uuid::new_v4().simple().to_string(),
            tools: agent.tools.clone(),
        });
    }

    Ok((runtime_agents, runtime_root))
}

async fn write_runtime_opencode_configs(
    room: &RoomConfig,
    runtime_agents: &[RuntimeAgent],
    mcp_server: &RoomHubMcpServer,
) -> Result<()> {
    let members = managed_opencode::build_members(
        &room.human,
        room.agents.iter().map(|item| item.name.clone()),
    );

    for agent in runtime_agents {
        let mcp_url = mcp_server.get_mcp_url_for_agent(&agent.id)?;
        let config_path = agent.runtime_dir.join("opencode.json");
        let updated = update_runtime_opencode_mcp_url(
            &config_path,
            &mcp_url,
            &room.opencode.runtime_agent_name,
        )
        .await?;

        if !updated {
            let config = build_runtime_opencode_config_fallback(room, agent, &members, &mcp_url);
            let text = format!("{}\n", serde_json::to_string_pretty(&config)?);
            tokio::fs::write(&config_path, text)
                .await
                .with_context(|| {
                    format!(
                        "failed writing runtime opencode.json: {}",
                        config_path.display()
                    )
                })?;
        }
    }
    Ok(())
}

async fn update_runtime_opencode_mcp_url(
    config_path: &Path,
    mcp_url: &str,
    runtime_agent_name: &str,
) -> Result<bool> {
    let raw = match tokio::fs::read_to_string(config_path).await {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed reading runtime opencode.json: {}",
                    config_path.display()
                )
            });
        }
    };

    let mut config: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };

    if !is_managed_agent_config_compatible(&config, runtime_agent_name) {
        return Ok(false);
    }

    if !managed_opencode::upsert_mcp_endpoint(&mut config, mcp_url) {
        return Ok(false);
    }

    let text = format!("{}\n", serde_json::to_string_pretty(&config)?);
    tokio::fs::write(config_path, text).await.with_context(|| {
        format!(
            "failed writing runtime opencode.json: {}",
            config_path.display()
        )
    })?;

    Ok(true)
}

fn is_managed_agent_config_compatible(config: &Value, runtime_agent_name: &str) -> bool {
    let Some(agent_obj) = config.get("agent").and_then(|value| value.as_object()) else {
        return false;
    };
    let Some(runtime_agent_obj) = agent_obj
        .get(runtime_agent_name)
        .and_then(|value| value.as_object())
    else {
        return false;
    };
    let Some(permission_obj) = runtime_agent_obj
        .get("permission")
        .and_then(|value| value.as_object())
    else {
        return false;
    };

    permission_obj.contains_key(HUB_GET_TOOL)
        && permission_obj.contains_key(HUB_ACK_TOOL)
        && permission_obj.contains_key(HUB_POST_TOOL)
}

fn build_runtime_opencode_config_fallback(
    room: &RoomConfig,
    agent: &RuntimeAgent,
    members: &str,
    mcp_url: &str,
) -> Value {
    let agent_name = if agent.name.trim().is_empty() {
        "Agent"
    } else {
        &agent.name
    };

    let preference = room
        .agents
        .iter()
        .find(|item| item.id == agent.id)
        .and_then(|item| item.preference.as_deref());

    managed_opencode::build_managed_opencode_config(
        &room.opencode.runtime_agent_name,
        agent_name,
        members,
        mcp_url,
        agent.tools.edit || agent.tools.write,
        preference,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_uses_crewforge_command() {
        assert!(help_text().contains("crewforge chat"));
        assert!(!help_text().contains("npm run chat"));
        assert!(help_text().contains("--resume"));
    }

    #[test]
    fn event_prompt_directs_get_unread() {
        let room = RoomConfig {
            room_name: "brainstorm".to_string(),
            human: "Rex".to_string(),
            runtime: crate::config::RuntimeConfig {
                scheduler_mode: "event_loop".to_string(),
                event_loop: crate::config::EventLoopConfig {
                    gather_interval_ms: 5000,
                },
                rate_limit: crate::config::RateLimitConfig {
                    window_ms: 60_000,
                    max_posts: 6,
                },
                app_session_id: None,
            },
            opencode: crate::config::OpencodeConfig {
                command: "opencode".to_string(),
                timeout_ms: 240_000,
                runtime_agent_name: "brainstorm-room".to_string(),
            },
            agents: vec![crate::config::AgentConfig {
                id: "codex".to_string(),
                name: "Codex".to_string(),
                model: "m".to_string(),
                context_dir: ".room/agents/codex".to_string(),
                tools: AgentTools::default(),
                preference: None,
            }],
            workspace_dir: PathBuf::new(),
        };

        let agent = RuntimeAgent {
            id: "codex".to_string(),
            name: "Codex".to_string(),
            model: "m".to_string(),
            runtime_dir: PathBuf::new(),
            hub_token: "token".to_string(),
            tools: AgentTools::default(),
        };

        let prompt = build_event_turn_prompt(&room, &agent, &RateLimitUsage { remaining: 6 });

        assert!(prompt.contains(HUB_GET_TOOL));
    }

    #[test]
    fn decode_user_input_line_is_utf8_lossy() {
        let text = decode_user_input_line(&[0xff, 0xfe, b'h', b'i', b'\n']);
        assert!(text.contains("hi"));
    }

    #[tokio::test]
    async fn resolve_resume_session_id_defaults_to_sessions_dir() {
        let cwd = PathBuf::from("/tmp/work");
        let sessions = cwd.join(".room/sessions");
        let resolved =
            resolve_resume_session_file(&cwd, &sessions, "session-2026-02-21T12-34").await;
        assert_eq!(resolved, sessions.join("session-2026-02-21T12-34.jsonl"));
    }

    #[tokio::test]
    async fn resolve_resume_relative_path_uses_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().to_path_buf();
        let sessions = cwd.join(".room/sessions");
        std::fs::create_dir_all(&sessions).expect("create sessions dir");
        let relative = ".room/sessions/session-2026-02-21T12-34-56-789Z.jsonl";
        std::fs::write(cwd.join(relative), "").expect("write session file");
        let resolved = resolve_resume_session_file(&cwd, &sessions, relative).await;
        assert_eq!(
            resolved,
            cwd.join(".room/sessions/session-2026-02-21T12-34-56-789Z.jsonl")
        );
    }

    #[test]
    fn build_resume_hint_command_with_default_config() {
        let args = ChatArgs {
            config_path: ".room/room.json".to_string(),
            resume: None,
            dry_run: false,
        };
        assert_eq!(
            build_resume_hint_command(&args, "session-2026-02-21T13-53-50-911Z"),
            "crewforge chat --resume session-2026-02-21T13-53-50-911Z"
        );
    }

    #[test]
    fn build_resume_hint_command_with_custom_config() {
        let args = ChatArgs {
            config_path: "custom/room.json".to_string(),
            resume: None,
            dry_run: false,
        };
        assert_eq!(
            build_resume_hint_command(&args, "session-2026-02-21T13-53-50-911Z"),
            "crewforge chat --config custom/room.json --resume session-2026-02-21T13-53-50-911Z"
        );
    }
}
