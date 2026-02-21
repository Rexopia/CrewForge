use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use cliclack::{StringCursor, Theme, ThemeState, set_theme};
use console::Style;

static THEME_INIT: Once = Once::new();
static FILTER_INPUT_HIGHLIGHT_DEPTH: AtomicUsize = AtomicUsize::new(0);

pub fn install_prompt_theme() {
    THEME_INIT.call_once(|| {
        set_theme(CrewForgePromptTheme);
    });
}

pub fn filter_input_highlight_scope() -> FilterInputHighlightGuard {
    FILTER_INPUT_HIGHLIGHT_DEPTH.fetch_add(1, Ordering::Relaxed);
    FilterInputHighlightGuard
}

pub fn clear_filter_input_highlight() {
    FILTER_INPUT_HIGHLIGHT_DEPTH.store(0, Ordering::Relaxed);
}

pub struct FilterInputHighlightGuard;

impl Drop for FilterInputHighlightGuard {
    fn drop(&mut self) {
        let depth = FILTER_INPUT_HIGHLIGHT_DEPTH.load(Ordering::Relaxed);
        if depth > 0 {
            FILTER_INPUT_HIGHLIGHT_DEPTH.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct CrewForgePromptTheme;

impl Theme for CrewForgePromptTheme {
    fn bar_color(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active => Style::new().cyan(),
            ThemeState::Cancel => Style::new().red(),
            ThemeState::Submit => Style::new().green(),
            ThemeState::Error(_) => Style::new().yellow(),
        }
    }

    fn input_style(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active | ThemeState::Error(_) => Style::new().bold(),
            ThemeState::Submit => Style::new().dim(),
            ThemeState::Cancel => Style::new().dim().strikethrough(),
        }
    }

    fn placeholder_style(&self, state: &ThemeState) -> Style {
        match state {
            ThemeState::Active | ThemeState::Error(_) => Style::new().dim(),
            ThemeState::Submit => Style::new().dim(),
            ThemeState::Cancel => Style::new().hidden(),
        }
    }

    fn format_input(&self, state: &ThemeState, cursor: &StringCursor) -> String {
        if !should_highlight_filter_input() {
            let new_style = &self.input_style(state);
            let input = match state {
                ThemeState::Active | ThemeState::Error(_) => {
                    self.cursor_with_style(cursor, new_style)
                }
                _ => cursor.to_string(),
            };

            let mut rendered = input;
            if rendered.ends_with('\n') {
                rendered.push('\n');
            }

            return rendered.lines().fold(String::new(), |acc, line| {
                format!(
                    "{}{}  {}\n",
                    acc,
                    self.bar_color(state).apply_to("│"),
                    new_style.apply_to(line),
                )
            });
        }

        let value_style = match state {
            ThemeState::Active | ThemeState::Error(_) => Style::new().bold(),
            ThemeState::Submit => Style::new().dim(),
            ThemeState::Cancel => Style::new().dim().strikethrough(),
        };
        let border_style = filter_input_border_style(state);

        let input = match state {
            ThemeState::Active | ThemeState::Error(_) if cursor.is_empty() => {
                Style::new().dim().apply_to("type to search").to_string()
            }
            ThemeState::Active | ThemeState::Error(_) => {
                self.cursor_with_style(cursor, &value_style)
            }
            _ => cursor.to_string(),
        };

        let mut rendered = input;
        if rendered.ends_with('\n') {
            rendered.push('\n');
        }

        rendered.lines().fold(String::new(), |acc, line| {
            let body = value_style.apply_to(line).to_string();
            format!(
                "{}{}  {}{}{}\n",
                acc,
                self.bar_color(state).apply_to("│"),
                border_style.apply_to("│ "),
                body,
                border_style.apply_to(" │"),
            )
        })
    }

    fn format_placeholder(&self, state: &ThemeState, cursor: &StringCursor) -> String {
        if !should_highlight_filter_input() {
            let new_style = &self.placeholder_style(state);
            let placeholder = match state {
                ThemeState::Active | ThemeState::Error(_) => {
                    self.cursor_with_style(cursor, new_style)
                }
                ThemeState::Cancel => String::new(),
                _ => cursor.to_string(),
            };

            return placeholder.lines().fold(String::new(), |acc, line| {
                format!(
                    "{}{}  {}\n",
                    acc,
                    self.bar_color(state).apply_to("│"),
                    new_style.apply_to(line),
                )
            });
        }

        let value_style = match state {
            ThemeState::Active | ThemeState::Error(_) => Style::new().dim(),
            ThemeState::Submit => Style::new().dim(),
            ThemeState::Cancel => Style::new().hidden(),
        };
        let border_style = filter_input_border_style(state);

        let text = match state {
            ThemeState::Active | ThemeState::Error(_) => {
                self.cursor_with_style(cursor, &value_style)
            }
            ThemeState::Cancel => String::new(),
            _ => cursor.to_string(),
        };

        text.lines().fold(String::new(), |acc, line| {
            let body = value_style.apply_to(line).to_string();
            format!(
                "{}{}  {}{}{}\n",
                acc,
                self.bar_color(state).apply_to("│"),
                border_style.apply_to("│ "),
                body,
                border_style.apply_to(" │"),
            )
        })
    }
}

fn should_highlight_filter_input() -> bool {
    FILTER_INPUT_HIGHLIGHT_DEPTH.load(Ordering::Relaxed) > 0
}

fn filter_input_border_style(state: &ThemeState) -> Style {
    match state {
        ThemeState::Active => Style::new().cyan(),
        ThemeState::Cancel => Style::new().dim(),
        ThemeState::Submit => Style::new().green(),
        ThemeState::Error(_) => Style::new().yellow(),
    }
}
