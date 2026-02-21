use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::task::JoinHandle;
use tui_textarea::TextArea;

use crate::chat::{ChatRuntime, handle_user_input};

const INPUT_MIN_ROWS: u16 = 1;
const INPUT_MAX_ROWS: u16 = 5;
const INPUT_CHROME_ROWS: u16 = 2;

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
        }
    }

    fn to_styled_line(&self) -> Line<'static> {
        match self {
            DisplayLine::System(text) => Line::from(Span::styled(
                text.clone(),
                Style::default().add_modifier(Modifier::DIM),
            )),
            DisplayLine::Human { ts, speaker, text } => Line::from(vec![
                Span::styled(
                    format!("[{ts}] {speaker}"),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(format!(": {text}")),
            ]),
            DisplayLine::Agent {
                ts,
                speaker,
                text,
                agent_idx,
            } => Line::from(vec![
                Span::styled(format!("[{ts}] {speaker}"), agent_style(*agent_idx)),
                Span::raw(format!(": {text}")),
            ]),
        }
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

pub async fn run_tui_loop(
    runtime: Arc<ChatRuntime>,
    mut msg_rx: UnboundedReceiver<DisplayLine>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let _guard = TerminalGuard;
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(std::io::stdout(), EnterAlternateScreen)
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

    let prompt = format!("└─ {}>", runtime.human_name());
    let mut textarea = build_textarea(&prompt);
    let mut display_lines: Vec<DisplayLine> = Vec::new();
    let mut scroll_offset: u16 = 0;
    let mut view_height: u16 = 1;
    let mut auto_scroll = true;
    let mut event_stream = EventStream::new();
    let mut watchdog_handle: Option<JoinHandle<()>> = None;
    let mut seen_human_message = false;

    'main: loop {
        let mut channel_closed = false;
        let mut drained_any = false;
        loop {
            match msg_rx.try_recv() {
                Ok(line) => {
                    display_lines.push(line);
                    drained_any = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    channel_closed = true;
                    break;
                }
            }
        }

        if drained_any && auto_scroll {
            scroll_offset = max_scroll_offset(display_lines.len(), view_height);
        }

        terminal
            .draw(|frame| {
                view_height = render(frame, &display_lines, &textarea, scroll_offset);
            })
            .context("failed to draw ratatui frame")?;

        if auto_scroll {
            scroll_offset = max_scroll_offset(display_lines.len(), view_height);
        }

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
                            scroll_offset = scroll_offset.saturating_sub(1);
                            continue;
                        }

                        if is_scroll_down(&key) {
                            let max_scroll = max_scroll_offset(display_lines.len(), view_height);
                            scroll_offset = scroll_offset.saturating_add(1).min(max_scroll);
                            if scroll_offset >= max_scroll {
                                auto_scroll = true;
                            }
                            continue;
                        }

                        if is_newline_key(&key) {
                            textarea.insert_newline();
                            continue;
                        }

                        if is_submit_key(&key) {
                            let submitted = textarea.lines().join("\n").trim().to_string();
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
                    Some(Ok(Event::Resize(_, _))) => {
                        if auto_scroll {
                            scroll_offset = max_scroll_offset(display_lines.len(), view_height);
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
                        display_lines.push(line);
                        if auto_scroll {
                            scroll_offset = max_scroll_offset(display_lines.len(), view_height);
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
    textarea: &TextArea,
    scroll_offset: u16,
) -> u16 {
    let input_height = input_height_for_textarea(textarea);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(input_height)])
        .split(frame.area());

    let messages_block = Block::bordered();
    let messages = Paragraph::new(lines_to_text(lines))
        .block(messages_block)
        .scroll((scroll_offset, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(messages, chunks[0]);
    frame.render_widget(textarea, chunks[1]);

    chunks[0].height.saturating_sub(2).max(1)
}

fn lines_to_text(lines: &[DisplayLine]) -> Text<'static> {
    Text::from(
        lines
            .iter()
            .map(DisplayLine::to_styled_line)
            .collect::<Vec<_>>(),
    )
}

fn build_textarea(prompt: &str) -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(Block::bordered().title(prompt.to_string()));
    textarea.set_cursor_line_style(Style::default());
    textarea
}

fn input_height_for_textarea(textarea: &TextArea) -> u16 {
    let rows = textarea
        .lines()
        .len()
        .clamp(INPUT_MIN_ROWS as usize, INPUT_MAX_ROWS as usize) as u16;
    rows + INPUT_CHROME_ROWS
}

fn max_scroll_offset(line_count: usize, view_height: u16) -> u16 {
    line_count.saturating_sub(view_height.max(1) as usize) as u16
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
    key.code == KeyCode::PageUp
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Up)
}

fn is_scroll_down(key: &KeyEvent) -> bool {
    key.code == KeyCode::PageDown
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Down)
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
        assert_eq!(input_height_for_textarea(&textarea), 3);
    }

    #[test]
    fn input_height_grows_with_newlines_and_caps_at_five_rows() {
        let mut textarea = TextArea::default();
        textarea.insert_str("line1\nline2\nline3\nline4\nline5\nline6");
        assert_eq!(input_height_for_textarea(&textarea), 7);
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
}
