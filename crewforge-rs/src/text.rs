use chrono::{DateTime, Local};

pub fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

pub fn to_single_line_error(message: &str) -> String {
    normalize_text(message)
        .lines()
        .next()
        .unwrap_or("unknown error")
        .to_string()
}

pub fn format_time(iso_ts: &str) -> String {
    match DateTime::parse_from_rfc3339(iso_ts) {
        Ok(ts) => ts.with_timezone(&Local).format("%H:%M:%S").to_string(),
        Err(_) => "--:--:--".to_string(),
    }
}
