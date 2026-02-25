use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::from_str;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::chat::{ChatRuntime, handle_user_input};
use crate::kernel::MessageEvent;

const INPUT_MIN_ROWS: u16 = 1;
const INPUT_MAX_ROWS: u16 = 5;
const INPUT_CHROME_ROWS: u16 = 2;
const STATUS_BAR_ROWS: u16 = 1;
const PAGE_SCROLL_KEEP_CONTEXT_ROWS: u16 = 2;
const PREFETCH_TOP_TRIGGER_MIN_ROWS: u16 = 4;
const HISTORY_PAGE_MESSAGES: usize = 200;
const TUI_TICK_INTERVAL: Duration = Duration::from_millis(16);
const RENDER_BATCH_INTERVAL: Duration = Duration::from_millis(14);
const SCROLL_ACCEL_WINDOW: Duration = Duration::from_millis(280);
const PASTE_BURST_MIN_CHARS: u16 = 3;
const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);

#[cfg(not(windows))]
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(30);

#[cfg(not(windows))]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(8);
#[cfg(windows)]
const PASTE_BURST_ACTIVE_IDLE_TIMEOUT: Duration = Duration::from_millis(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AgentStatusState {
    #[default]
    Unknown,
    Idle,
    Active,
    Error,
}

#[derive(Debug, Clone, Default)]
struct AgentStatusEntry {
    state: AgentStatusState,
    reason: Option<String>,
}

#[derive(Debug, Default)]
struct PasteBurst {
    last_plain_char_time: Option<Instant>,
    consecutive_plain_chars: u16,
    burst_window_until: Option<Instant>,
    buffer: String,
    active: bool,
    pending_first_char: Option<(char, Instant)>,
}

#[derive(Debug, PartialEq, Eq)]
enum PasteFlushResult {
    None,
    Typed(char),
    Paste(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum UiDensity {
    #[default]
    Comfort,
    Compact,
}

impl UiDensity {
    fn toggle(self) -> Self {
        match self {
            Self::Comfort => Self::Compact,
            Self::Compact => Self::Comfort,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Comfort => "comfort",
            Self::Compact => "compact",
        }
    }

    fn is_comfort(self) -> bool {
        matches!(self, Self::Comfort)
    }
}

#[derive(Debug, Default)]
struct ScrollMomentum {
    direction: Option<ScrollDirection>,
    last_at: Option<Instant>,
    streak: u8,
}

impl ScrollMomentum {
    fn next_step(&mut self, direction: ScrollDirection, base_step: u16, now: Instant) -> u16 {
        let is_continuation = self.direction == Some(direction)
            && self
                .last_at
                .is_some_and(|last| now.duration_since(last) <= SCROLL_ACCEL_WINDOW);
        if is_continuation {
            self.streak = self.streak.saturating_add(1);
        } else {
            self.direction = Some(direction);
            self.streak = 1;
        }
        self.last_at = Some(now);
        accelerated_scroll_step(base_step, self.streak)
    }

    fn reset(&mut self) {
        self.direction = None;
        self.last_at = None;
        self.streak = 0;
    }
}

impl PasteBurst {
    fn on_plain_char(&mut self, ch: char, now: Instant) {
        self.note_plain_char(now);
        if self.active {
            self.buffer.push(ch);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return;
        }

        if let Some((held, held_at)) = self.pending_first_char
            && now.duration_since(held_at) <= PASTE_BURST_CHAR_INTERVAL
        {
            self.active = true;
            self.pending_first_char = None;
            self.buffer.push(held);
            self.buffer.push(ch);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return;
        }

        self.pending_first_char = Some((ch, now));
    }

    fn flush_if_due(&mut self, now: Instant) -> PasteFlushResult {
        if self.active {
            if let Some(last) = self.last_plain_char_time
                && now.duration_since(last) > PASTE_BURST_ACTIVE_IDLE_TIMEOUT
            {
                self.active = false;
                if !self.buffer.is_empty() {
                    return PasteFlushResult::Paste(std::mem::take(&mut self.buffer));
                }
            }
            return PasteFlushResult::None;
        }

        if let Some((held, held_at)) = self.pending_first_char
            && now.duration_since(held_at) > PASTE_BURST_CHAR_INTERVAL
        {
            self.pending_first_char = None;
            return PasteFlushResult::Typed(held);
        }

        PasteFlushResult::None
    }

    fn flush_before_non_char(&mut self) -> PasteFlushResult {
        self.consecutive_plain_chars = 0;
        self.last_plain_char_time = None;
        if self.active {
            self.active = false;
            if !self.buffer.is_empty() {
                return PasteFlushResult::Paste(std::mem::take(&mut self.buffer));
            }
        }

        if let Some((held, _)) = self.pending_first_char.take() {
            return PasteFlushResult::Typed(held);
        }

        PasteFlushResult::None
    }

    fn should_treat_enter_as_newline(&self, now: Instant) -> bool {
        self.active
            || self
                .burst_window_until
                .is_some_and(|deadline| now <= deadline)
    }

    fn clear_window_after_non_char(&mut self) {
        self.consecutive_plain_chars = 0;
        self.last_plain_char_time = None;
    }

    fn needs_tick(&self) -> bool {
        self.active || self.pending_first_char.is_some()
    }

    fn note_plain_char(&mut self, now: Instant) {
        if let Some(last) = self.last_plain_char_time {
            if now.duration_since(last) <= PASTE_BURST_CHAR_INTERVAL {
                self.consecutive_plain_chars = self.consecutive_plain_chars.saturating_add(1);
            } else {
                self.consecutive_plain_chars = 1;
            }
        } else {
            self.consecutive_plain_chars = 1;
        }
        self.last_plain_char_time = Some(now);
        if self.consecutive_plain_chars >= PASTE_BURST_MIN_CHARS {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisplayLine {
    System(String),
    Human {
        ts: String,
        speaker: String,
        text: String,
    },
    Agent {
        ts: String,
        speaker: String,
        text: String,
        agent_idx: usize,
    },
    AgentStatus {
        agent: String,
        status: String,
        reason: Option<String>,
    },
}

impl DisplayLine {
    pub(crate) fn as_plain_text(&self) -> String {
        match self {
            DisplayLine::System(text) => text.clone(),
            DisplayLine::Human { ts, speaker, text } => {
                format!("[{ts}] {speaker}: {text}")
            }
            DisplayLine::Agent {
                ts, speaker, text, ..
            } => {
                format!("[{ts}] {speaker}: {text}")
            }
            DisplayLine::AgentStatus { .. } => String::new(),
        }
    }

    fn to_styled_lines(&self, density: UiDensity) -> Vec<Line<'static>> {
        match self {
            DisplayLine::System(text) => split_normalized_lines(text)
                .into_iter()
                .map(|line| {
                    Line::from(Span::styled(
                        line,
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                })
                .collect(),
            DisplayLine::Human { ts, speaker, text } => {
                prefixed_message_lines(ts, speaker, Style::default().fg(Color::Cyan), text, density)
            }
            DisplayLine::Agent {
                ts,
                speaker,
                text,
                agent_idx,
            } => prefixed_message_lines(ts, speaker, agent_style(*agent_idx), text, density),
            DisplayLine::AgentStatus { .. } => Vec::new(),
        }
    }
}

fn prefixed_message_lines(
    ts: &str,
    speaker: &str,
    speaker_style: Style,
    text: &str,
    density: UiDensity,
) -> Vec<Line<'static>> {
    let text_lines = split_normalized_lines(text);
    let mut lines = Vec::with_capacity(text_lines.len().max(1));
    let mut iter = text_lines.into_iter();
    let first = iter.next().unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(format!("[{ts}] {speaker}"), speaker_style),
        Span::raw(format!(": {first}")),
    ]));
    for line in iter {
        if line.is_empty() {
            lines.push(Line::default());
            continue;
        }
        let mut spans = Vec::new();
        if density.is_comfort() {
            spans.push(Span::styled(
                "  ",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
            ));
        }
        spans.push(Span::raw(line));
        lines.push(Line::from(spans));
    }
    lines
}

fn split_normalized_lines(text: &str) -> Vec<String> {
    normalize_newlines(text)
        .split('\n')
        .map(ToString::to_string)
        .collect()
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn initial_agent_statuses(agent_names: &[String]) -> BTreeMap<String, AgentStatusEntry> {
    agent_names
        .iter()
        .map(|name| {
            (
                name.clone(),
                AgentStatusEntry {
                    state: AgentStatusState::Idle,
                    reason: None,
                },
            )
        })
        .collect()
}

fn parse_agent_status_state(raw: &str) -> AgentStatusState {
    match raw {
        "idle" => AgentStatusState::Idle,
        "active" => AgentStatusState::Active,
        "error" => AgentStatusState::Error,
        _ => AgentStatusState::Unknown,
    }
}

fn update_agent_status(
    statuses: &mut BTreeMap<String, AgentStatusEntry>,
    agent: String,
    status: String,
    reason: Option<String>,
) {
    statuses.insert(
        agent,
        AgentStatusEntry {
            state: parse_agent_status_state(&status),
            reason,
        },
    );
}

fn agent_status_style(state: AgentStatusState) -> Style {
    match state {
        AgentStatusState::Active => Style::default().fg(Color::Green),
        AgentStatusState::Error => Style::default().fg(Color::Red),
        AgentStatusState::Idle => Style::default().fg(Color::Cyan),
        AgentStatusState::Unknown => Style::default().add_modifier(Modifier::DIM),
    }
}

fn agent_status_symbol(state: AgentStatusState) -> &'static str {
    match state {
        AgentStatusState::Active => "*",
        AgentStatusState::Error => "!",
        AgentStatusState::Idle => "-",
        AgentStatusState::Unknown => "?",
    }
}

fn build_status_line(statuses: &BTreeMap<String, AgentStatusEntry>, density: UiDensity) -> Line<'static> {
    let has_running = statuses
        .values()
        .any(|entry| matches!(entry.state, AgentStatusState::Active));
    let overall_label = if has_running { "running" } else { "idle" };
    let overall_style = if has_running {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
    };
    let density_style = if density.is_comfort() {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)
    };

    let mut spans = vec![
        Span::styled("Status ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{overall_label:<7}"), overall_style),
        Span::raw(" | "),
        Span::styled("View ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(density.label(), density_style),
        Span::raw(" | "),
        Span::styled("Agents ", Style::default().add_modifier(Modifier::BOLD)),
    ];
    if statuses.is_empty() {
        spans.push(Span::styled(
            "none",
            Style::default().add_modifier(Modifier::DIM),
        ));
        return Line::from(spans);
    }

    let mut first = true;
    for (agent, entry) in statuses {
        if !first {
            spans.push(Span::raw("  "));
        }
        first = false;
        let style = agent_status_style(entry.state);
        spans.push(Span::styled(agent_status_symbol(entry.state), style));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(agent.clone(), style));
        if matches!(entry.state, AgentStatusState::Error)
            && let Some(reason) = entry.reason.as_deref()
            && !reason.trim().is_empty()
        {
            spans.push(Span::raw(": "));
            spans.push(Span::styled(
                to_single_line(reason),
                Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
            ));
        }
    }
    Line::from(spans)
}

fn to_single_line(text: &str) -> String {
    normalize_newlines(text)
        .split('\n')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn handle_display_line(
    line: DisplayLine,
    display_lines: &mut Vec<DisplayLine>,
    rendered_line_cache: &mut RenderedLineCache,
    statuses: &mut BTreeMap<String, AgentStatusEntry>,
) -> bool {
    match line {
        DisplayLine::AgentStatus {
            agent,
            status,
            reason,
        } => {
            update_agent_status(statuses, agent, status, reason);
            true
        }
        other => {
            display_lines.push(other);
            rendered_line_cache.invalidate();
            true
        }
    }
}

fn apply_paste_flush(textarea: &mut TextArea, flush: PasteFlushResult) -> bool {
    match flush {
        PasteFlushResult::None => false,
        PasteFlushResult::Typed(ch) => {
            textarea.insert_char(ch);
            true
        }
        PasteFlushResult::Paste(text) => {
            let _ = textarea.insert_str(&text);
            true
        }
    }
}

fn key_plain_char(key: &KeyEvent) -> Option<char> {
    if !key.modifiers.is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Char(ch) if !ch.is_control() => Some(ch),
        _ => None,
    }
}

fn should_handle_key_event(key: &KeyEvent) -> bool {
    match key.kind {
        KeyEventKind::Press => true,
        // Only allow key repeat for plain text input. Repeating Enter / Ctrl+J / nav keys
        // can accidentally inject extra newlines or scroll steps on some terminals.
        KeyEventKind::Repeat => key_plain_char(key).is_some(),
        _ => false,
    }
}

fn mark_render_dirty(render_dirty: &mut bool, render_pending_since: &mut Instant) {
    if !*render_dirty {
        *render_dirty = true;
        *render_pending_since = Instant::now();
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

struct SynchronizedUpdateGuard {
    active: bool,
}

impl SynchronizedUpdateGuard {
    fn begin() -> Self {
        let active = execute!(std::io::stdout(), BeginSynchronizedUpdate).is_ok();
        Self { active }
    }
}

impl Drop for SynchronizedUpdateGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
        }
    }
}

#[derive(Debug, Default)]
struct RenderedLineCache {
    width: u16,
    len: usize,
    density: UiDensity,
    rows: Vec<Line<'static>>,
    valid: bool,
}

impl RenderedLineCache {
    fn invalidate(&mut self) {
        self.valid = false;
    }

    fn ensure_rows(&mut self, lines: &[DisplayLine], view_width: u16, density: UiDensity) {
        let view_width = view_width.max(1);
        if !self.valid || self.width != view_width || self.len != lines.len() || self.density != density {
            self.rows = rendered_rows_for_lines(lines, view_width, density);
            self.width = view_width;
            self.len = lines.len();
            self.density = density;
            self.valid = true;
        }
    }

    fn total_rows(&mut self, lines: &[DisplayLine], view_width: u16, density: UiDensity) -> usize {
        self.ensure_rows(lines, view_width, density);
        self.rows.len()
    }

    fn visible_rows(
        &mut self,
        lines: &[DisplayLine],
        view_width: u16,
        scroll_offset: u16,
        view_height: u16,
        density: UiDensity,
    ) -> Vec<Line<'static>> {
        self.ensure_rows(lines, view_width, density);
        let start = scroll_offset as usize;
        if start >= self.rows.len() {
            return vec![Line::default()];
        }

        let end = start
            .saturating_add(view_height.max(1) as usize)
            .min(self.rows.len());
        self.rows[start..end].to_vec()
    }
}

#[derive(Debug)]
struct HistoryPrefetch {
    start: usize,
    task: JoinHandle<Result<Vec<MessageEvent>>>,
}

#[derive(Debug)]
struct SessionHistoryPager {
    session_file: PathBuf,
    line_offsets: Arc<Vec<u64>>,
    loaded_start: usize,
}

impl SessionHistoryPager {
    async fn open(
        session_file: PathBuf,
        runtime: &ChatRuntime,
    ) -> Result<(Self, Vec<DisplayLine>)> {
        let line_offsets = Arc::new(load_session_line_offsets(session_file.clone()).await?);
        let total = line_offsets.len();
        let loaded_start = total.saturating_sub(HISTORY_PAGE_MESSAGES);
        let events = load_session_events_range(
            session_file.clone(),
            line_offsets.clone(),
            loaded_start,
            total,
        )
        .await?;
        let initial_lines = events
            .iter()
            .map(|event| runtime.display_line_for_event(event))
            .collect::<Vec<_>>();

        Ok((
            Self {
                session_file,
                line_offsets,
                loaded_start,
            },
            initial_lines,
        ))
    }

    fn has_older(&self) -> bool {
        self.loaded_start > 0
    }

    fn next_older_start(&self) -> Option<usize> {
        if !self.has_older() {
            return None;
        }
        Some(self.loaded_start.saturating_sub(HISTORY_PAGE_MESSAGES))
    }

    async fn load_older_page(&mut self, runtime: &ChatRuntime) -> Result<Vec<DisplayLine>> {
        if !self.has_older() {
            return Ok(Vec::new());
        }

        let start = self.loaded_start.saturating_sub(HISTORY_PAGE_MESSAGES);
        let end = self.loaded_start;
        let events = load_session_events_range(
            self.session_file.clone(),
            self.line_offsets.clone(),
            start,
            end,
        )
        .await?;
        self.loaded_start = start;

        Ok(events
            .iter()
            .map(|event| runtime.display_line_for_event(event))
            .collect::<Vec<_>>())
    }
}

pub async fn run_tui_loop(
    runtime: Arc<ChatRuntime>,
    mut msg_rx: UnboundedReceiver<DisplayLine>,
    stop_flag: Arc<AtomicBool>,
    session_file: PathBuf,
) -> Result<()> {
    let _guard = TerminalGuard;
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )
    .context("failed to enter alternate screen")?;
    if matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        let _ = execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        );
    }

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal =
        Terminal::new(backend).context("failed to initialize ratatui terminal backend")?;
    terminal
        .hide_cursor()
        .context("failed to hide terminal cursor")?;

    let prompt = format!("-<{}>", runtime.human_name());
    let mut textarea = build_textarea(&prompt);
    let (mut history_pager, mut display_lines) =
        SessionHistoryPager::open(session_file, &runtime).await?;
    let mut agent_statuses = initial_agent_statuses(&runtime.agent_names());
    let mut ui_density = UiDensity::default();
    let mut scroll_offset: u16 = 0;
    let (mut view_height, mut view_width) = message_view_dimensions(
        size_to_rect(terminal.size().context("failed to read terminal size")?),
        &textarea,
    );
    let mut auto_scroll = true;
    let mut rendered_line_cache = RenderedLineCache::default();
    let mut event_stream = EventStream::new();
    let mut paste_burst = PasteBurst::default();
    let mut scroll_momentum = ScrollMomentum::default();
    let mut pending_prefetch: Option<HistoryPrefetch> = None;
    let mut watchdog_handle: Option<JoinHandle<()>> = None;
    let mut seen_human_message = false;
    let mut render_dirty = true;
    let mut render_pending_since = Instant::now()
        .checked_sub(RENDER_BATCH_INTERVAL)
        .unwrap_or_else(Instant::now);

    'main: loop {
        if render_dirty && render_pending_since.elapsed() >= RENDER_BATCH_INTERVAL {
            (view_height, view_width) = message_view_dimensions(
                size_to_rect(terminal.size().context("failed to read terminal size")?),
                &textarea,
            );

            let mut channel_closed = false;
            loop {
                match msg_rx.try_recv() {
                    Ok(line) => {
                        let _ = handle_display_line(
                            line,
                            &mut display_lines,
                            &mut rendered_line_cache,
                            &mut agent_statuses,
                        );
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        channel_closed = true;
                        break;
                    }
                }
            }

            let max_scroll = max_scroll_offset_for_view(
                &display_lines,
                view_height,
                view_width,
                ui_density,
                &mut rendered_line_cache,
            );
            if auto_scroll {
                scroll_offset = max_scroll;
            } else {
                scroll_offset = scroll_offset.min(max_scroll);
            }

            maybe_start_history_prefetch(
                &mut pending_prefetch,
                &history_pager,
                auto_scroll,
                scroll_offset,
                view_height,
            );

            let _sync_guard = SynchronizedUpdateGuard::begin();
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        &display_lines,
                        &agent_statuses,
                        &textarea,
                        scroll_offset,
                        ui_density,
                        &mut rendered_line_cache,
                    );
                })
                .context("failed to draw ratatui frame")?;
            render_dirty = false;

            if channel_closed || stop_flag.load(Ordering::SeqCst) {
                break;
            }
        }

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if !should_handle_key_event(&key) {
                            continue;
                        }

                        let now = Instant::now();
                        if apply_paste_flush(&mut textarea, paste_burst.flush_if_due(now)) {
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        }

                        if is_exit_key(&key) {
                            stop_flag.store(true, Ordering::SeqCst);
                            break 'main;
                        }

                        if is_clear_input_key(&key) {
                            scroll_momentum.reset();
                            paste_burst = PasteBurst::default();
                            textarea = build_textarea(&prompt);
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_cancel_agents_key(&key) {
                            scroll_momentum.reset();
                            if apply_paste_flush(
                                &mut textarea,
                                paste_burst.flush_before_non_char(),
                            ) {
                                mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            }
                            paste_burst.clear_window_after_non_char();

                            let canceled = runtime.cancel_active_agent_calls().await;
                            let notice = if canceled == 0 {
                                "[interrupt] no active agent calls.".to_string()
                            } else {
                                format!("[interrupt] canceled {canceled} active agent call(s).")
                            };
                            if handle_display_line(
                                DisplayLine::System(notice),
                                &mut display_lines,
                                &mut rendered_line_cache,
                                &mut agent_statuses,
                            ) {
                                mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            }
                            auto_scroll = true;
                            continue;
                        }

                        if let Some(ch) = key_plain_char(&key) {
                            scroll_momentum.reset();
                            paste_burst.on_plain_char(ch, now);
                            continue;
                        }

                        if apply_paste_flush(&mut textarea, paste_burst.flush_before_non_char()) {
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        }
                        paste_burst.clear_window_after_non_char();

                        if is_toggle_density_key(&key) {
                            scroll_momentum.reset();
                            ui_density = ui_density.toggle();
                            rendered_line_cache.invalidate();
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_scroll_up(&key) {
                            auto_scroll = false;
                            let step = scroll_momentum.next_step(
                                ScrollDirection::Up,
                                scroll_step_for_key(&key, view_height),
                                now,
                            );
                            while scroll_offset < step && history_pager.has_older() {
                                if !prepend_next_older_page(
                                    &mut history_pager,
                                    &runtime,
                                    &mut pending_prefetch,
                                    &mut display_lines,
                                    view_width,
                                    ui_density,
                                    &mut scroll_offset,
                                    &mut rendered_line_cache,
                                )
                                .await?
                                {
                                    break;
                                }
                            }
                            scroll_offset = scroll_offset.saturating_sub(step);
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_scroll_down(&key) {
                            let max_scroll =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
                                    ui_density,
                                    &mut rendered_line_cache,
                                );
                            let step = scroll_momentum.next_step(
                                ScrollDirection::Down,
                                scroll_step_for_key(&key, view_height),
                                now,
                            );
                            scroll_offset = scroll_offset
                                .saturating_add(step)
                                .min(max_scroll);
                            auto_scroll = scroll_offset >= max_scroll;
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        scroll_momentum.reset();

                        if is_jump_to_top_key(&key) {
                            auto_scroll = false;
                            // Jump to the top of currently loaded history only.
                            // Older pages remain lazy-loaded by scroll-up actions.
                            scroll_offset = 0;
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_jump_to_bottom_key(&key) {
                            let max_scroll =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
                                    ui_density,
                                    &mut rendered_line_cache,
                                );
                            scroll_offset = max_scroll;
                            auto_scroll = true;
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_newline_key(&key)
                            || (is_submit_key(&key) && paste_burst.should_treat_enter_as_newline(now))
                        {
                            textarea.insert_newline();
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        if is_submit_key(&key) {
                            let submitted =
                                normalize_newlines(&textarea.lines().join("\n")).trim().to_string();
                            textarea = build_textarea(&prompt);
                            auto_scroll = true;

                            let should_exit = handle_user_input(
                                runtime.clone(),
                                submitted,
                                &mut seen_human_message,
                                &mut watchdog_handle,
                                stop_flag.clone(),
                            ).await?;

                            if should_exit {
                                break 'main;
                            }
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                            continue;
                        }

                        textarea.input(key);
                        mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                    }
                    Some(Ok(Event::Paste(data))) => {
                        scroll_momentum.reset();
                        if apply_paste_flush(&mut textarea, paste_burst.flush_before_non_char()) {
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        }
                        paste_burst.clear_window_after_non_char();
                        textarea.insert_str(normalize_newlines(&data));
                        mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        continue;
                    }
                    Some(Ok(Event::Resize(new_width, new_height))) => {
                        scroll_momentum.reset();
                        if apply_paste_flush(&mut textarea, paste_burst.flush_before_non_char()) {
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        }
                        paste_burst.clear_window_after_non_char();
                        (view_height, view_width) = message_view_dimensions(
                            Rect::new(0, 0, new_width, new_height),
                            &textarea,
                        );
                        let max_scroll =
                            max_scroll_offset_for_view(
                                &display_lines,
                                view_height,
                                view_width,
                                ui_density,
                                &mut rendered_line_cache,
                            );
                        if auto_scroll {
                            scroll_offset = max_scroll;
                        } else {
                            scroll_offset = scroll_offset.min(max_scroll);
                        }
                        mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(error).context("failed reading crossterm event stream");
                    }
                    None => break 'main,
                }
            }
            _ = sleep(RENDER_BATCH_INTERVAL), if render_dirty => {}
            _ = sleep(TUI_TICK_INTERVAL), if paste_burst.needs_tick() => {
                if apply_paste_flush(&mut textarea, paste_burst.flush_if_due(Instant::now())) {
                    mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                }
            }
            prefetch_join = async {
                let pending = pending_prefetch
                    .take()
                    .expect("prefetch branch runs only when task exists");
                let start = pending.start;
                let join = pending.task.await;
                (start, join)
            }, if pending_prefetch.is_some() => {
                let (start, join) = prefetch_join;
                let events = join.context("failed joining history prefetch task")??;
                if history_pager.next_older_start() == Some(start) {
                    if apply_loaded_history_events(
                        start,
                        events,
                        &mut history_pager,
                        &runtime,
                        &mut display_lines,
                        view_width,
                        ui_density,
                        &mut scroll_offset,
                        &mut rendered_line_cache,
                    ) {
                        mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                    }
                }
            }
            maybe_line = msg_rx.recv() => {
                match maybe_line {
                    Some(line) => {
                        if handle_display_line(
                            line,
                            &mut display_lines,
                            &mut rendered_line_cache,
                            &mut agent_statuses,
                        ) {
                            mark_render_dirty(&mut render_dirty, &mut render_pending_since);
                        }
                        if auto_scroll {
                            scroll_offset =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
                                    ui_density,
                                    &mut rendered_line_cache,
                                );
                        }
                    }
                    None => break 'main,
                }
            }
        }

        if stop_flag.load(Ordering::SeqCst) {
            break;
        }
    }

    if let Some(handle) = watchdog_handle.take() {
        handle.abort();
        let _ = handle.await;
    }
    if let Some(pending) = pending_prefetch.take() {
        pending.task.abort();
        let _ = pending.task.await;
    }

    terminal
        .show_cursor()
        .context("failed to restore terminal cursor")?;
    Ok(())
}

fn render(
    frame: &mut Frame,
    lines: &[DisplayLine],
    agent_statuses: &BTreeMap<String, AgentStatusEntry>,
    textarea: &TextArea,
    scroll_offset: u16,
    density: UiDensity,
    rendered_line_cache: &mut RenderedLineCache,
) {
    let input_height = input_height_for_textarea(textarea, frame.area().width);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(STATUS_BAR_ROWS),
        Constraint::Length(input_height),
    ])
    .split(frame.area());

    let message_view_height = chunks[0].height.saturating_sub(2).max(1);
    let message_view_width = chunks[0].width.saturating_sub(2).max(1);
    let total_rows = rendered_line_cache.total_rows(lines, message_view_width, density);
    let view_end = scroll_offset
        .saturating_add(message_view_height)
        .min(total_rows.min(u16::MAX as usize) as u16);
    let visible_rows = rendered_line_cache.visible_rows(
        lines,
        message_view_width,
        scroll_offset,
        message_view_height,
        density,
    );
    let messages_block = Block::bordered()
        .title(format!(
            " Chat {view_end}/{total_rows} [{}] ",
            density.label()
        ))
        .border_style(Style::default().fg(Color::DarkGray));
    let messages = Paragraph::new(Text::from(visible_rows)).block(messages_block);
    frame.render_widget(messages, chunks[0]);
    frame.render_widget(
        Paragraph::new(Text::from(build_status_line(agent_statuses, density))),
        chunks[1],
    );
    render_input(frame, chunks[2], textarea);
}

fn build_textarea(prompt: &str) -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(Block::bordered().title(prompt.to_string()));
    textarea.set_cursor_line_style(Style::default());
    textarea
}

fn input_height_for_textarea(textarea: &TextArea, area_width: u16) -> u16 {
    let rows = input_rendered_line_count(textarea, input_content_width(area_width))
        .max(1)
        .min(u16::MAX as usize) as u16;
    let rows = rows.clamp(INPUT_MIN_ROWS, INPUT_MAX_ROWS);
    rows + INPUT_CHROME_ROWS
}

fn max_scroll_offset_for_view(
    lines: &[DisplayLine],
    view_height: u16,
    view_width: u16,
    density: UiDensity,
    rendered_line_cache: &mut RenderedLineCache,
) -> u16 {
    rendered_line_cache
        .total_rows(lines, view_width, density)
        .saturating_sub(view_height.max(1) as usize) as u16
}

fn message_view_dimensions(area: Rect, textarea: &TextArea) -> (u16, u16) {
    let input_height = input_height_for_textarea(textarea, area.width);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(STATUS_BAR_ROWS),
        Constraint::Length(input_height),
    ])
    .split(area);
    (
        chunks[0].height.saturating_sub(2).max(1),
        chunks[0].width.saturating_sub(2).max(1),
    )
}

fn size_to_rect(size: Size) -> Rect {
    Rect::new(0, 0, size.width, size.height)
}

fn render_input(frame: &mut Frame, area: Rect, textarea: &TextArea) {
    let block = textarea.block().cloned().unwrap_or_else(Block::bordered);
    let content_width = input_content_width(area.width);
    let visible_rows = area.height.saturating_sub(INPUT_CHROME_ROWS).max(1) as usize;
    let scroll_top = input_scroll_top_for_view(textarea, area.width, area.height);
    let (cursor_row, cursor_col) = cursor_visual_position(textarea, content_width);

    let input = Paragraph::new(input_text(textarea))
        .block(block)
        .style(textarea.style())
        .scroll((scroll_top, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);

    let cursor_screen_row = cursor_row
        .saturating_sub(scroll_top as usize)
        .min(visible_rows.saturating_sub(1));
    let cursor_screen_col = cursor_col.min(content_width.saturating_sub(1) as usize);
    frame.set_cursor_position((
        area.x
            .saturating_add(1)
            .saturating_add(cursor_screen_col.min(u16::MAX as usize) as u16),
        area.y
            .saturating_add(1)
            .saturating_add(cursor_screen_row.min(u16::MAX as usize) as u16),
    ));
}

fn input_text(textarea: &TextArea) -> Text<'static> {
    Text::from(
        textarea
            .lines()
            .iter()
            .map(|line| Line::from(line.clone()))
            .collect::<Vec<_>>(),
    )
}

fn input_content_width(area_width: u16) -> u16 {
    area_width.saturating_sub(2).max(1)
}

fn input_scroll_top_for_view(textarea: &TextArea, area_width: u16, area_height: u16) -> u16 {
    let content_width = input_content_width(area_width);
    let visible_rows = area_height.saturating_sub(INPUT_CHROME_ROWS).max(1) as usize;
    let total_rows = input_rendered_line_count(textarea, content_width);
    let (cursor_row, _) = cursor_visual_position(textarea, content_width);

    let min_scroll_for_cursor = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let max_scroll = total_rows.saturating_sub(visible_rows);
    min_scroll_for_cursor.min(max_scroll) as u16
}

fn cursor_visual_position(textarea: &TextArea, content_width: u16) -> (usize, usize) {
    let width = content_width.max(1) as usize;
    let lines = textarea.lines();
    if lines.is_empty() {
        return (0, 0);
    }

    let (cursor_row, cursor_col) = textarea.cursor();
    let cursor_row = cursor_row.min(lines.len().saturating_sub(1));
    let mut visual_row = 0_usize;

    for line in lines.iter().take(cursor_row) {
        let display_width = UnicodeWidthStr::width(line.as_str());
        let wrapped_rows = if display_width == 0 {
            1
        } else {
            display_width.saturating_sub(1) / width + 1
        };
        visual_row = visual_row.saturating_add(wrapped_rows);
    }

    let current_line = lines[cursor_row].as_str();
    let current_line_len = current_line.chars().count();
    let cursor_col = cursor_col.min(current_line_len);
    let cursor_display_col = display_width_for_char_prefix(current_line, cursor_col);
    visual_row = visual_row.saturating_add(cursor_display_col / width);
    let visual_col = cursor_display_col % width;

    (visual_row, visual_col)
}

fn display_width_for_char_prefix(line: &str, char_count: usize) -> usize {
    if char_count == 0 {
        return 0;
    }

    let split_at = line
        .char_indices()
        .nth(char_count)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(line.len());
    UnicodeWidthStr::width(&line[..split_at])
}

fn input_rendered_line_count(textarea: &TextArea, content_width: u16) -> usize {
    Paragraph::new(input_text(textarea))
        .wrap(Wrap { trim: false })
        .line_count(content_width.max(1))
        .max(1)
}

fn maybe_start_history_prefetch(
    pending_prefetch: &mut Option<HistoryPrefetch>,
    history_pager: &SessionHistoryPager,
    auto_scroll: bool,
    scroll_offset: u16,
    view_height: u16,
) {
    if pending_prefetch.is_some() || auto_scroll {
        return;
    }

    let Some(start) = history_pager.next_older_start() else {
        return;
    };

    if scroll_offset > prefetch_top_trigger_rows(view_height) {
        return;
    }

    let session_file = history_pager.session_file.clone();
    let line_offsets = history_pager.line_offsets.clone();
    let end = history_pager.loaded_start;
    let task = tokio::spawn(async move {
        load_session_events_range(session_file, line_offsets, start, end).await
    });
    *pending_prefetch = Some(HistoryPrefetch { start, task });
}

async fn prepend_next_older_page(
    history_pager: &mut SessionHistoryPager,
    runtime: &ChatRuntime,
    pending_prefetch: &mut Option<HistoryPrefetch>,
    display_lines: &mut Vec<DisplayLine>,
    view_width: u16,
    density: UiDensity,
    scroll_offset: &mut u16,
    rendered_line_cache: &mut RenderedLineCache,
) -> Result<bool> {
    let Some(expected_start) = history_pager.next_older_start() else {
        return Ok(false);
    };

    if let Some(pending) = pending_prefetch.take() {
        if pending.start == expected_start {
            let events = pending
                .task
                .await
                .context("failed joining history prefetch task")??;
            return Ok(apply_loaded_history_events(
                expected_start,
                events,
                history_pager,
                runtime,
                display_lines,
                view_width,
                density,
                scroll_offset,
                rendered_line_cache,
            ));
        }
        pending.task.abort();
        let _ = pending.task.await;
    }

    prepend_older_history_page(
        history_pager,
        runtime,
        display_lines,
        view_width,
        density,
        scroll_offset,
        rendered_line_cache,
    )
    .await
}

fn apply_loaded_history_events(
    start: usize,
    events: Vec<MessageEvent>,
    history_pager: &mut SessionHistoryPager,
    runtime: &ChatRuntime,
    display_lines: &mut Vec<DisplayLine>,
    view_width: u16,
    density: UiDensity,
    scroll_offset: &mut u16,
    rendered_line_cache: &mut RenderedLineCache,
) -> bool {
    history_pager.loaded_start = start;
    let older_lines = events
        .iter()
        .map(|event| runtime.display_line_for_event(event))
        .collect::<Vec<_>>();
    prepend_history_lines(
        older_lines,
        display_lines,
        view_width,
        density,
        scroll_offset,
        rendered_line_cache,
    )
}

fn prepend_history_lines(
    older_lines: Vec<DisplayLine>,
    display_lines: &mut Vec<DisplayLine>,
    view_width: u16,
    density: UiDensity,
    scroll_offset: &mut u16,
    rendered_line_cache: &mut RenderedLineCache,
) -> bool {
    if older_lines.is_empty() {
        return false;
    }
    let added_rows = rendered_line_count(&older_lines, view_width, density).min(u16::MAX as usize) as u16;
    display_lines.splice(0..0, older_lines);
    rendered_line_cache.invalidate();
    *scroll_offset = scroll_offset.saturating_add(added_rows);
    true
}

async fn prepend_older_history_page(
    history_pager: &mut SessionHistoryPager,
    runtime: &ChatRuntime,
    display_lines: &mut Vec<DisplayLine>,
    view_width: u16,
    density: UiDensity,
    scroll_offset: &mut u16,
    rendered_line_cache: &mut RenderedLineCache,
) -> Result<bool> {
    let older_lines = history_pager.load_older_page(runtime).await?;
    Ok(prepend_history_lines(
        older_lines,
        display_lines,
        view_width,
        density,
        scroll_offset,
        rendered_line_cache,
    ))
}

async fn load_session_line_offsets(session_file: PathBuf) -> Result<Vec<u64>> {
    tokio::task::spawn_blocking(move || load_session_line_offsets_blocking(&session_file))
        .await
        .context("failed joining session line index task")?
}

fn load_session_line_offsets_blocking(session_file: &Path) -> Result<Vec<u64>> {
    let file = match File::open(session_file) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed opening session file for indexing: {}",
                    session_file.display()
                )
            });
        }
    };

    let mut reader = BufReader::new(file);
    let mut offsets = Vec::new();
    let mut line = String::new();
    let mut cursor = 0_u64;

    loop {
        line.clear();
        let line_start = cursor;
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("failed reading session file: {}", session_file.display()))?;
        if bytes == 0 {
            break;
        }
        cursor = cursor.saturating_add(bytes as u64);
        if !line.trim().is_empty() {
            offsets.push(line_start);
        }
    }

    Ok(offsets)
}

async fn load_session_events_range(
    session_file: PathBuf,
    line_offsets: Arc<Vec<u64>>,
    start: usize,
    end: usize,
) -> Result<Vec<MessageEvent>> {
    tokio::task::spawn_blocking(move || {
        load_session_events_range_blocking(&session_file, &line_offsets, start, end)
    })
    .await
    .context("failed joining session history page task")?
}

fn load_session_events_range_blocking(
    session_file: &Path,
    line_offsets: &[u64],
    start: usize,
    end: usize,
) -> Result<Vec<MessageEvent>> {
    if start >= end || start >= line_offsets.len() {
        return Ok(Vec::new());
    }

    let end = end.min(line_offsets.len());
    let file = match File::open(session_file) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed opening session file for history page: {}",
                    session_file.display()
                )
            });
        }
    };

    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut events = Vec::with_capacity(end.saturating_sub(start));

    for (index, line_offset) in line_offsets.iter().enumerate().take(end).skip(start) {
        line.clear();
        reader
            .seek(SeekFrom::Start(*line_offset))
            .with_context(|| {
                format!(
                    "failed seeking session file at line index {} in {}",
                    index,
                    session_file.display()
                )
            })?;

        let bytes = reader.read_line(&mut line).with_context(|| {
            format!(
                "failed reading session file line index {} in {}",
                index,
                session_file.display()
            )
        })?;
        if bytes == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: MessageEvent = from_str(trimmed).with_context(|| {
            format!(
                "invalid session event at indexed line {} in {}",
                index,
                session_file.display()
            )
        })?;
        if event.event_type == "message" {
            events.push(event);
        }
    }

    Ok(events)
}

fn rendered_line_count(lines: &[DisplayLine], view_width: u16, density: UiDensity) -> usize {
    rendered_rows_for_lines(lines, view_width, density).len()
}

fn rendered_rows_for_lines(lines: &[DisplayLine], view_width: u16, density: UiDensity) -> Vec<Line<'static>> {
    let view_width = view_width.max(1) as usize;
    let mut rows = Vec::new();
    for line in lines {
        let mut rendered_line_rows = Vec::new();
        for styled_line in line.to_styled_lines(density) {
            append_wrapped_rows(&styled_line, view_width, &mut rendered_line_rows);
        }
        if rendered_line_rows.is_empty() {
            continue;
        }
        if density.is_comfort() && !rows.is_empty() {
            rows.push(Line::default());
        }
        rows.extend(rendered_line_rows);
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    rows
}

fn append_wrapped_rows(line: &Line<'static>, view_width: usize, rows: &mut Vec<Line<'static>>) {
    let mut current_spans = Vec::new();
    let mut current_width = 0_usize;

    for span in &line.spans {
        let style = span.style;
        let mut segment = String::new();
        for ch in span.content.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if char_width > 0
                && current_width > 0
                && current_width.saturating_add(char_width) > view_width
            {
                if !segment.is_empty() {
                    current_spans.push(Span::styled(std::mem::take(&mut segment), style));
                }
                rows.push(Line::from(std::mem::take(&mut current_spans)));
                current_width = 0;
            }

            segment.push(ch);
            current_width = current_width.saturating_add(char_width);
        }
        if !segment.is_empty() {
            current_spans.push(Span::styled(segment, style));
        }
    }

    if current_spans.is_empty() {
        rows.push(Line::default());
    } else {
        rows.push(Line::from(current_spans));
    }
}

fn scroll_step_for_key(key: &KeyEvent, view_height: u16) -> u16 {
    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        page_scroll_step(view_height)
    } else if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('p' | 'P' | 'n' | 'N'))
    {
        half_page_scroll_step(view_height)
    } else {
        1
    }
}

fn page_scroll_step(view_height: u16) -> u16 {
    view_height
        .saturating_sub(PAGE_SCROLL_KEEP_CONTEXT_ROWS)
        .max(1)
}

fn half_page_scroll_step(view_height: u16) -> u16 {
    (view_height / 2).max(1)
}

fn prefetch_top_trigger_rows(view_height: u16) -> u16 {
    half_page_scroll_step(view_height).max(PREFETCH_TOP_TRIGGER_MIN_ROWS)
}

fn accelerated_scroll_step(base_step: u16, streak: u8) -> u16 {
    let ratio_pct = match streak {
        0 | 1 => 100_u32,
        2 => 140,
        3 => 180,
        4 => 220,
        _ => 260,
    };
    let scaled = (base_step as u32)
        .saturating_mul(ratio_pct)
        .saturating_add(99)
        / 100;
    scaled.min(u16::MAX as u32) as u16
}

fn is_submit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.is_empty()
}

fn is_newline_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('j' | 'J'))
}

fn is_exit_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('d' | 'D'))
}

fn is_clear_input_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
}

fn is_cancel_agents_key(key: &KeyEvent) -> bool {
    key.modifiers.is_empty() && key.code == KeyCode::Esc
}

fn is_toggle_density_key(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('t' | 'T'))
}

fn is_scroll_up(key: &KeyEvent) -> bool {
    is_page_up_alias(key)
        || (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Up)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Up)
}

fn is_scroll_down(key: &KeyEvent) -> bool {
    is_page_down_alias(key)
        || (key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Down)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down)
}

fn is_page_up_alias(key: &KeyEvent) -> bool {
    key.code == KeyCode::PageUp
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('p' | 'P')))
}

fn is_page_down_alias(key: &KeyEvent) -> bool {
    key.code == KeyCode::PageDown
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n' | 'N')))
}

fn is_jump_to_top_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Home
}

fn is_jump_to_bottom_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::End
}

fn agent_style(agent_idx: usize) -> Style {
    match agent_idx % 5 {
        0 => Style::default().fg(Color::Green),
        1 => Style::default().fg(Color::Yellow),
        2 => Style::default().fg(Color::Magenta),
        3 => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::Red),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key_event_with_kind(code, modifiers, KeyEventKind::Press)
    }

    fn key_event_with_kind(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn input_height_stays_single_line_by_default() {
        let textarea = TextArea::default();
        assert_eq!(input_height_for_textarea(&textarea, 80), 3);
    }

    #[test]
    fn textarea_default_has_single_empty_line() {
        let textarea = TextArea::default();
        assert_eq!(textarea.lines().len(), 1);
        assert_eq!(textarea.cursor(), (0, 0));
    }

    #[test]
    fn input_height_grows_with_newlines_and_caps_at_five_rows() {
        let mut textarea = TextArea::default();
        textarea.insert_str("line1\nline2\nline3\nline4\nline5\nline6");
        assert_eq!(input_height_for_textarea(&textarea, 80), 7);
    }

    #[test]
    fn input_height_wraps_long_single_line() {
        let mut textarea = TextArea::default();
        textarea.insert_str("1234567890");
        assert_eq!(input_height_for_textarea(&textarea, 7), 4);
    }

    #[test]
    fn input_height_does_not_expand_at_exact_wrap_boundary() {
        let mut textarea = TextArea::default();
        textarea.insert_str("12345");
        assert_eq!(input_height_for_textarea(&textarea, 7), 3);
    }

    #[test]
    fn plain_enter_submits_while_modified_enter_does_not() {
        let plain_enter = key_event(KeyCode::Enter, KeyModifiers::empty());
        assert!(is_submit_key(&plain_enter));
        assert!(!is_newline_key(&plain_enter));

        let ctrl_j = key_event(KeyCode::Char('j'), KeyModifiers::CONTROL);
        assert!(!is_submit_key(&ctrl_j));
        assert!(is_newline_key(&ctrl_j));
    }

    #[test]
    fn key_repeat_is_ignored_for_non_plain_keys() {
        let repeat_enter =
            key_event_with_kind(KeyCode::Enter, KeyModifiers::empty(), KeyEventKind::Repeat);
        let repeat_ctrl_j = key_event_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        );
        let repeat_page_up =
            key_event_with_kind(KeyCode::PageUp, KeyModifiers::empty(), KeyEventKind::Repeat);
        let repeat_plain_char = key_event_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::empty(),
            KeyEventKind::Repeat,
        );

        assert!(!should_handle_key_event(&repeat_enter));
        assert!(!should_handle_key_event(&repeat_ctrl_j));
        assert!(!should_handle_key_event(&repeat_page_up));
        assert!(should_handle_key_event(&repeat_plain_char));
    }

    #[test]
    fn ctrl_c_clears_and_ctrl_d_exits() {
        let ctrl_c = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_clear_input_key(&ctrl_c));
        assert!(!is_exit_key(&ctrl_c));

        let ctrl_d = key_event(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!is_clear_input_key(&ctrl_d));
        assert!(is_exit_key(&ctrl_d));

        let esc = key_event(KeyCode::Esc, KeyModifiers::empty());
        assert!(is_cancel_agents_key(&esc));
        assert!(!is_exit_key(&esc));

        let ctrl_t = key_event(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(is_toggle_density_key(&ctrl_t));
    }

    #[test]
    fn rendered_line_count_matches_wrapped_content() {
        let lines = vec![
            DisplayLine::System("1234567890".to_string()),
            DisplayLine::System("x".to_string()),
        ];
        assert_eq!(rendered_line_count(&lines, 5, UiDensity::Compact), 3);
    }

    #[test]
    fn rendered_line_count_respects_explicit_newlines() {
        let lines = vec![DisplayLine::System("alpha\nbeta".to_string())];
        assert_eq!(rendered_line_count(&lines, 80, UiDensity::Compact), 2);
    }

    #[test]
    fn rendered_line_count_handles_wide_chars() {
        let lines = vec![DisplayLine::System("你好你好".to_string())];
        assert_eq!(rendered_line_count(&lines, 4, UiDensity::Compact), 2);
    }

    #[test]
    fn comfort_density_inserts_spacing_between_messages() {
        let lines = vec![
            DisplayLine::System("first".to_string()),
            DisplayLine::System("second".to_string()),
        ];
        assert_eq!(rendered_line_count(&lines, 80, UiDensity::Comfort), 3);
        assert_eq!(rendered_line_count(&lines, 80, UiDensity::Compact), 2);
    }

    #[test]
    fn agent_multiline_render_keeps_single_blank_line_without_indent_padding() {
        let lines = DisplayLine::Agent {
            ts: "00:00:00".to_string(),
            speaker: "Gemini".to_string(),
            text: "alpha\n\nbeta".to_string(),
            agent_idx: 0,
        }
        .to_styled_lines(UiDensity::Compact);

        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().ends_with(": alpha"));
        assert_eq!(lines[1].to_string(), "");
        assert_eq!(lines[2].to_string(), "beta");
    }

    #[test]
    fn max_scroll_offset_uses_rendered_rows_instead_of_message_count() {
        let lines = vec![
            DisplayLine::System("1234567890".to_string()),
            DisplayLine::System("x".to_string()),
        ];
        let mut cache = RenderedLineCache::default();
        assert_eq!(
            max_scroll_offset_for_view(&lines, 2, 5, UiDensity::Compact, &mut cache),
            1
        );
    }

    #[test]
    fn page_up_down_use_view_relative_steps() {
        let page_up = key_event(KeyCode::PageUp, KeyModifiers::empty());
        let ctrl_p = key_event(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let ctrl_n = key_event(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let ctrl_up = key_event(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(scroll_step_for_key(&page_up, 20), 18);
        assert_eq!(scroll_step_for_key(&ctrl_p, 20), 10);
        assert_eq!(scroll_step_for_key(&ctrl_n, 20), 10);
        assert!(is_scroll_up(&ctrl_p));
        assert!(is_scroll_down(&ctrl_n));
        assert_eq!(scroll_step_for_key(&ctrl_up, 20), 1);
        assert_eq!(scroll_step_for_key(&page_up, 1), 1);
        assert_eq!(scroll_step_for_key(&ctrl_p, 1), 1);
    }

    #[test]
    fn scroll_momentum_accelerates_for_fast_repeated_input() {
        let mut momentum = ScrollMomentum::default();
        let now = Instant::now();
        let step1 = momentum.next_step(ScrollDirection::Up, 10, now);
        let step2 = momentum.next_step(ScrollDirection::Up, 10, now + Duration::from_millis(40));
        let step3 = momentum.next_step(ScrollDirection::Up, 10, now + Duration::from_millis(80));
        assert_eq!(step1, 10);
        assert!(step2 > step1);
        assert!(step3 > step2);
    }

    #[test]
    fn scroll_momentum_resets_after_pause_or_direction_change() {
        let mut momentum = ScrollMomentum::default();
        let now = Instant::now();
        let _ = momentum.next_step(ScrollDirection::Up, 10, now);
        let _ = momentum.next_step(ScrollDirection::Up, 10, now + Duration::from_millis(40));
        let step_after_pause = momentum.next_step(
            ScrollDirection::Up,
            10,
            now + Duration::from_millis(40) + SCROLL_ACCEL_WINDOW + Duration::from_millis(1),
        );
        assert_eq!(step_after_pause, 10);
        let step_other_dir = momentum.next_step(
            ScrollDirection::Down,
            10,
            now + Duration::from_millis(40) + SCROLL_ACCEL_WINDOW + Duration::from_millis(20),
        );
        assert_eq!(step_other_dir, 10);
    }

    #[test]
    fn prefetch_trigger_uses_half_page_with_minimum() {
        assert_eq!(prefetch_top_trigger_rows(20), 10);
        assert_eq!(prefetch_top_trigger_rows(2), PREFETCH_TOP_TRIGGER_MIN_ROWS);
    }

    #[test]
    fn handle_display_line_tracks_agent_status_separately() {
        let mut display_lines = Vec::new();
        let mut cache = RenderedLineCache::default();
        let mut statuses = BTreeMap::new();

        handle_display_line(
            DisplayLine::AgentStatus {
                agent: "Alice".to_string(),
                status: "error".to_string(),
                reason: Some("provider timeout\ndetails".to_string()),
            },
            &mut display_lines,
            &mut cache,
            &mut statuses,
        );

        assert!(display_lines.is_empty());
        let alice = statuses.get("Alice").expect("status exists");
        assert_eq!(alice.state, AgentStatusState::Error);
        assert_eq!(alice.reason.as_deref(), Some("provider timeout\ndetails"));

        let line = build_status_line(&statuses, UiDensity::Comfort).to_string();
        assert!(line.contains("Agents"));
        assert!(line.contains("Alice"));
        assert!(line.contains("provider timeout"));
    }

    #[test]
    fn status_line_shows_running_when_any_agent_is_active() {
        let mut statuses = BTreeMap::new();
        statuses.insert(
            "Gemini".to_string(),
            AgentStatusEntry {
                state: AgentStatusState::Active,
                reason: None,
            },
        );
        statuses.insert(
            "Kimi".to_string(),
            AgentStatusEntry {
                state: AgentStatusState::Idle,
                reason: None,
            },
        );

        let line = build_status_line(&statuses, UiDensity::Comfort).to_string();
        assert!(line.contains("Status running |"));
        assert!(line.contains("View comfort"));
    }

    #[test]
    fn status_line_pads_idle_to_align_separator() {
        let mut statuses = BTreeMap::new();
        statuses.insert(
            "Gemini".to_string(),
            AgentStatusEntry {
                state: AgentStatusState::Idle,
                reason: None,
            },
        );

        let line = build_status_line(&statuses, UiDensity::Comfort).to_string();
        assert!(line.contains("Status idle    |"));
    }

    #[test]
    fn paste_burst_single_char_flushes_as_typed() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();
        burst.on_plain_char('a', now);

        assert_eq!(burst.flush_if_due(now), PasteFlushResult::None);
        assert_eq!(
            burst.flush_if_due(now + PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1)),
            PasteFlushResult::Typed('a')
        );
    }

    #[test]
    fn paste_burst_rapid_chars_flush_as_paste() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();
        burst.on_plain_char('a', now);
        burst.on_plain_char('b', now + Duration::from_millis(1));
        burst.on_plain_char('c', now + Duration::from_millis(2));

        assert_eq!(
            burst.flush_if_due(now + Duration::from_millis(3)),
            PasteFlushResult::None
        );
        assert_eq!(
            burst.flush_if_due(now + PASTE_BURST_ACTIVE_IDLE_TIMEOUT + Duration::from_millis(5)),
            PasteFlushResult::Paste("abc".to_string())
        );
    }

    #[test]
    fn paste_burst_enter_window_is_temporarily_enabled_after_paste() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();
        burst.on_plain_char('a', now);
        burst.on_plain_char('b', now + Duration::from_millis(1));
        burst.on_plain_char('c', now + Duration::from_millis(2));
        let flush_at = now + PASTE_BURST_ACTIVE_IDLE_TIMEOUT + Duration::from_millis(5);
        assert!(matches!(
            burst.flush_if_due(flush_at),
            PasteFlushResult::Paste(_)
        ));

        assert!(burst.should_treat_enter_as_newline(flush_at));
        assert!(!burst.should_treat_enter_as_newline(
            flush_at + PASTE_ENTER_SUPPRESS_WINDOW + Duration::from_millis(1)
        ));
    }

    #[test]
    fn normalize_newlines_converts_crlf_and_cr() {
        assert_eq!(normalize_newlines("a\r\nb\rc"), "a\nb\nc");
    }

    #[test]
    fn input_scroll_tracks_cursor_for_multiline_content() {
        let mut textarea = TextArea::default();
        textarea.insert_str("l1\nl2\nl3\nl4\nl5\nl6");

        // Input area height 5 -> 3 visible rows after borders.
        assert_eq!(input_scroll_top_for_view(&textarea, 80, 5), 3);

        textarea.input(key_event(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(input_scroll_top_for_view(&textarea, 80, 5), 2);

        for _ in 0..5 {
            textarea.input(key_event(KeyCode::Up, KeyModifiers::empty()));
        }
        assert_eq!(input_scroll_top_for_view(&textarea, 80, 5), 0);
    }

    #[test]
    fn cursor_visual_position_handles_exact_wrap_boundary() {
        let mut textarea = TextArea::default();
        textarea.insert_str("12345\nx");

        let (row, col) = cursor_visual_position(&textarea, 5);
        assert_eq!((row, col), (1, 1));
    }

    #[test]
    fn cursor_visual_position_accounts_for_wide_char_width() {
        let mut textarea = TextArea::default();
        textarea.insert_str("你好a");

        let (row, col) = cursor_visual_position(&textarea, 4);
        assert_eq!((row, col), (1, 1));
    }

    #[test]
    fn cursor_visual_position_counts_wrapped_wide_lines_before_cursor_row() {
        let mut textarea = TextArea::default();
        textarea.insert_str("你好你好\nx");

        let (row, col) = cursor_visual_position(&textarea, 4);
        assert_eq!((row, col), (2, 1));
    }
}
