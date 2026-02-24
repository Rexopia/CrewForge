use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
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
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthStr;

use crate::chat::{ChatRuntime, handle_user_input};
use crate::kernel::MessageEvent;

const INPUT_MIN_ROWS: u16 = 1;
const INPUT_MAX_ROWS: u16 = 5;
const INPUT_CHROME_ROWS: u16 = 2;
const STATUS_BAR_ROWS: u16 = 1;
const KEY_SCROLL_STEP: u16 = 3;
const HISTORY_PAGE_MESSAGES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStatusState {
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

impl Default for AgentStatusState {
    fn default() -> Self {
        Self::Unknown
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

    fn to_styled_lines(&self) -> Vec<Line<'static>> {
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
            DisplayLine::Human { ts, speaker, text } => prefixed_message_lines(
                format!("[{ts}] {speaker}"),
                Style::default().fg(Color::Cyan),
                text,
            ),
            DisplayLine::Agent {
                ts,
                speaker,
                text,
                agent_idx,
            } => prefixed_message_lines(format!("[{ts}] {speaker}"), agent_style(*agent_idx), text),
            DisplayLine::AgentStatus { .. } => Vec::new(),
        }
    }
}

fn prefixed_message_lines(prefix: String, prefix_style: Style, text: &str) -> Vec<Line<'static>> {
    let text_lines = split_normalized_lines(text);
    let mut lines = Vec::with_capacity(text_lines.len().max(1));
    let mut iter = text_lines.into_iter();
    let first = iter.next().unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(prefix, prefix_style),
        Span::raw(format!(": {first}")),
    ]));
    for line in iter {
        lines.push(Line::from(vec![Span::raw("  "), Span::raw(line)]));
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
        .map(|name| (name.clone(), AgentStatusEntry::default()))
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

fn build_status_line(statuses: &BTreeMap<String, AgentStatusEntry>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "Agents ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
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
) {
    match line {
        DisplayLine::AgentStatus {
            agent,
            status,
            reason,
        } => update_agent_status(statuses, agent, status, reason),
        other => {
            display_lines.push(other);
            rendered_line_cache.invalidate();
        }
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

#[derive(Debug, Default)]
struct RenderedLineCache {
    width: u16,
    len: usize,
    line_count: usize,
    valid: bool,
}

impl RenderedLineCache {
    fn invalidate(&mut self) {
        self.valid = false;
    }

    fn line_count(&mut self, lines: &[DisplayLine], view_width: u16) -> usize {
        if !self.valid || self.width != view_width || self.len != lines.len() {
            self.line_count = rendered_line_count(lines, view_width);
            self.width = view_width;
            self.len = lines.len();
            self.valid = true;
        }
        self.line_count
    }
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
    let mut scroll_offset: u16 = 0;
    let mut view_height: u16;
    let mut view_width: u16;
    let mut auto_scroll = true;
    let mut rendered_line_cache = RenderedLineCache::default();
    let mut event_stream = EventStream::new();
    let mut watchdog_handle: Option<JoinHandle<()>> = None;
    let mut seen_human_message = false;

    'main: loop {
        (view_height, view_width) = message_view_dimensions(
            size_to_rect(terminal.size().context("failed to read terminal size")?),
            &textarea,
        );

        let mut channel_closed = false;
        loop {
            match msg_rx.try_recv() {
                Ok(line) => handle_display_line(
                    line,
                    &mut display_lines,
                    &mut rendered_line_cache,
                    &mut agent_statuses,
                ),
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
            &mut rendered_line_cache,
        );
        if auto_scroll {
            scroll_offset = max_scroll;
        } else {
            scroll_offset = scroll_offset.min(max_scroll);
        }

        terminal
            .draw(|frame| {
                render(
                    frame,
                    &display_lines,
                    &agent_statuses,
                    &textarea,
                    scroll_offset,
                );
            })
            .context("failed to draw ratatui frame")?;

        if channel_closed || stop_flag.load(Ordering::SeqCst) {
            break;
        }

        tokio::select! {
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            continue;
                        }

                        if is_exit_key(&key) {
                            stop_flag.store(true, Ordering::SeqCst);
                            break 'main;
                        }

                        if is_clear_input_key(&key) {
                            textarea = build_textarea(&prompt);
                            continue;
                        }

                        if is_scroll_up(&key) {
                            auto_scroll = false;
                            let step = scroll_step_for_key(&key);
                            while scroll_offset < step && history_pager.has_older() {
                                if !prepend_older_history_page(
                                    &mut history_pager,
                                    &runtime,
                                    &mut display_lines,
                                    view_width,
                                    &mut scroll_offset,
                                    &mut rendered_line_cache,
                                )
                                .await?
                                {
                                    break;
                                }
                            }
                            scroll_offset = scroll_offset.saturating_sub(step);
                            continue;
                        }

                        if is_scroll_down(&key) {
                            let max_scroll =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
                                    &mut rendered_line_cache,
                                );
                            scroll_offset = scroll_offset
                                .saturating_add(scroll_step_for_key(&key))
                                .min(max_scroll);
                            auto_scroll = scroll_offset >= max_scroll;
                            continue;
                        }

                        if is_jump_to_top_key(&key) {
                            auto_scroll = false;
                            // Jump to the top of currently loaded history only.
                            // Older pages remain lazy-loaded by scroll-up actions.
                            scroll_offset = 0;
                            continue;
                        }

                        if is_jump_to_bottom_key(&key) {
                            let max_scroll =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
                                    &mut rendered_line_cache,
                                );
                            scroll_offset = max_scroll;
                            auto_scroll = true;
                            continue;
                        }

                        if is_newline_key(&key) {
                            textarea.insert_newline();
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
                            continue;
                        }

                        textarea.input(key);
                    }
                    Some(Ok(Event::Paste(data))) => {
                        textarea.insert_str(normalize_newlines(&data));
                        continue;
                    }
                    Some(Ok(Event::Resize(new_width, new_height))) => {
                        (view_height, view_width) = message_view_dimensions(
                            Rect::new(0, 0, new_width, new_height),
                            &textarea,
                        );
                        let max_scroll =
                            max_scroll_offset_for_view(
                                &display_lines,
                                view_height,
                                view_width,
                                &mut rendered_line_cache,
                            );
                        if auto_scroll {
                            scroll_offset = max_scroll;
                        } else {
                            scroll_offset = scroll_offset.min(max_scroll);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(error).context("failed reading crossterm event stream");
                    }
                    None => break 'main,
                }
            }
            maybe_line = msg_rx.recv() => {
                match maybe_line {
                    Some(line) => {
                        handle_display_line(
                            line,
                            &mut display_lines,
                            &mut rendered_line_cache,
                            &mut agent_statuses,
                        );
                        if auto_scroll {
                            scroll_offset =
                                max_scroll_offset_for_view(
                                    &display_lines,
                                    view_height,
                                    view_width,
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
) {
    let input_height = input_height_for_textarea(textarea, frame.area().width);
    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(STATUS_BAR_ROWS),
        Constraint::Length(input_height),
    ])
    .split(frame.area());

    let messages_block = Block::bordered();
    let messages = Paragraph::new(lines_to_text(lines))
        .block(messages_block)
        .scroll((scroll_offset, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);
    frame.render_widget(
        Paragraph::new(Text::from(build_status_line(agent_statuses))),
        chunks[1],
    );
    render_input(frame, chunks[2], textarea);
}

fn lines_to_text(lines: &[DisplayLine]) -> Text<'static> {
    Text::from(
        lines
            .iter()
            .flat_map(DisplayLine::to_styled_lines)
            .collect::<Vec<_>>(),
    )
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
    rendered_line_cache: &mut RenderedLineCache,
) -> u16 {
    rendered_line_cache
        .line_count(lines, view_width)
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

async fn prepend_older_history_page(
    history_pager: &mut SessionHistoryPager,
    runtime: &ChatRuntime,
    display_lines: &mut Vec<DisplayLine>,
    view_width: u16,
    scroll_offset: &mut u16,
    rendered_line_cache: &mut RenderedLineCache,
) -> Result<bool> {
    let older_lines = history_pager.load_older_page(runtime).await?;
    if older_lines.is_empty() {
        return Ok(false);
    }

    let added_rows = rendered_line_count(&older_lines, view_width).min(u16::MAX as usize) as u16;
    display_lines.splice(0..0, older_lines);
    rendered_line_cache.invalidate();
    *scroll_offset = scroll_offset.saturating_add(added_rows);
    Ok(true)
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

    for index in start..end {
        line.clear();
        reader
            .seek(SeekFrom::Start(line_offsets[index]))
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

fn rendered_line_count(lines: &[DisplayLine], view_width: u16) -> usize {
    Paragraph::new(lines_to_text(lines))
        .wrap(Wrap { trim: false })
        .line_count(view_width.max(1))
}

fn scroll_step_for_key(key: &KeyEvent) -> u16 {
    if is_page_up_alias(key) || is_page_down_alias(key) {
        KEY_SCROLL_STEP
    } else {
        1
    }
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
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
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
    fn ctrl_c_clears_and_ctrl_d_exits() {
        let ctrl_c = key_event(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_clear_input_key(&ctrl_c));
        assert!(!is_exit_key(&ctrl_c));

        let ctrl_d = key_event(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!is_clear_input_key(&ctrl_d));
        assert!(is_exit_key(&ctrl_d));
    }

    #[test]
    fn rendered_line_count_matches_wrapped_content() {
        let lines = vec![
            DisplayLine::System("1234567890".to_string()),
            DisplayLine::System("x".to_string()),
        ];
        assert_eq!(rendered_line_count(&lines, 5), 3);
    }

    #[test]
    fn rendered_line_count_respects_explicit_newlines() {
        let lines = vec![DisplayLine::System("alpha\nbeta".to_string())];
        assert_eq!(rendered_line_count(&lines, 80), 2);
    }

    #[test]
    fn max_scroll_offset_uses_rendered_rows_instead_of_message_count() {
        let lines = vec![
            DisplayLine::System("1234567890".to_string()),
            DisplayLine::System("x".to_string()),
        ];
        let mut cache = RenderedLineCache::default();
        assert_eq!(max_scroll_offset_for_view(&lines, 2, 5, &mut cache), 1);
    }

    #[test]
    fn page_up_down_use_fixed_small_step() {
        let page_up = key_event(KeyCode::PageUp, KeyModifiers::empty());
        let ctrl_p = key_event(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let ctrl_n = key_event(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let ctrl_up = key_event(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(scroll_step_for_key(&page_up), KEY_SCROLL_STEP);
        assert_eq!(scroll_step_for_key(&ctrl_p), KEY_SCROLL_STEP);
        assert_eq!(scroll_step_for_key(&ctrl_n), KEY_SCROLL_STEP);
        assert!(is_scroll_up(&ctrl_p));
        assert!(is_scroll_down(&ctrl_n));
        assert_eq!(scroll_step_for_key(&ctrl_up), 1);
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

        let line = build_status_line(&statuses).to_string();
        assert!(line.contains("Agents"));
        assert!(line.contains("Alice"));
        assert!(line.contains("provider timeout"));
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
