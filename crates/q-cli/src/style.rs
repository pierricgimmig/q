//! Human-only styling. JSON and MCP never go through this module.
//!
//! `auto` asks [`anstream`] whether the stream should be colored. That honors
//! `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, and whether the stream is a TTY.
//! `--color always` forces ANSI even when those say no. `--color never` forces
//! plain text. Callers embed ANSI only when [`Paint`] is enabled, so a pipe
//! never sees escape bytes unless the user asked for them.

use anstyle::{AnsiColor, Style};
use time::OffsetDateTime;

use crate::cli::ColorMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paint {
    enabled: bool,
}

impl Paint {
    #[cfg(test)]
    pub const fn plain() -> Self {
        Self { enabled: false }
    }

    #[cfg(test)]
    pub const fn color() -> Self {
        Self { enabled: true }
    }

    pub fn paint(self, style: Style, text: &str) -> String {
        if !self.enabled || text.is_empty() || style.is_plain() {
            return text.to_string();
        }
        format!("{style}{text}{style:#}")
    }

    pub fn dim(self, text: &str) -> String {
        self.paint(Style::new().dimmed(), text)
    }

    pub fn bold(self, text: &str) -> String {
        self.paint(Style::new().bold(), text)
    }

    pub fn status(self, status: &str) -> String {
        self.paint(status_style(status), status)
    }

    pub fn warning_label(self) -> String {
        self.paint(
            Style::new().bold().fg_color(Some(AnsiColor::Yellow.into())),
            "warning:",
        )
    }

    pub fn error_label(self) -> String {
        self.paint(
            Style::new().bold().fg_color(Some(AnsiColor::Red.into())),
            "error:",
        )
    }
}

pub fn paint_for(mode: ColorMode, stream: Stream) -> Paint {
    let enabled = match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => match stream {
            Stream::Stdout => stream_allows_color(&std::io::stdout()),
            Stream::Stderr => stream_allows_color(&std::io::stderr()),
        },
    };
    Paint { enabled }
}

fn stream_allows_color(stream: &impl anstream::stream::RawStream) -> bool {
    matches!(
        anstream::AutoStream::choice(stream),
        anstream::ColorChoice::Always | anstream::ColorChoice::AlwaysAnsi
    )
}

/// Status colors. Unknown labels stay plain so a new status does not get a
/// misleading color.
pub fn status_style(status: &str) -> Style {
    match status {
        "inbox" => Style::new().fg_color(Some(AnsiColor::Blue.into())),
        "ready" => Style::new().fg_color(Some(AnsiColor::Green.into())),
        "claimed" => Style::new().fg_color(Some(AnsiColor::Yellow.into())),
        "in_progress" => Style::new().fg_color(Some(AnsiColor::Cyan.into())),
        "review" => Style::new().fg_color(Some(AnsiColor::Magenta.into())),
        "blocked" => Style::new().bold().fg_color(Some(AnsiColor::Red.into())),
        "done" => Style::new()
            .dimmed()
            .fg_color(Some(AnsiColor::Green.into())),
        "cancelled" => Style::new()
            .dimmed()
            .strikethrough()
            .fg_color(Some(AnsiColor::BrightBlack.into())),
        _ => Style::new(),
    }
}

pub fn dim_style() -> Style {
    Style::new().dimmed()
}

pub fn bold_style() -> Style {
    Style::new().bold()
}

const MINUTE: u64 = 60;
const HOUR: u64 = 60 * MINUTE;
const DAY: u64 = 24 * HOUR;
const MONTH: u64 = 30 * DAY;
const YEAR: u64 = 365 * DAY;

/// Compact relative time for list columns. `q show` keeps the full timestamp.
pub fn format_relative(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let delta = now.unix_timestamp().saturating_sub(then.unix_timestamp());
    let future = delta < 0;
    let seconds = delta.unsigned_abs();
    if seconds < 45 {
        return "just now".to_string();
    }
    let body = if seconds < MINUTE {
        format!("{seconds}s")
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else if seconds < MONTH {
        format!("{}d", seconds / DAY)
    } else if seconds < YEAR {
        format!("{}mo", seconds / MONTH)
    } else {
        format!("{}y", seconds / YEAR)
    };
    if future {
        format!("in {body}")
    } else {
        format!("{body} ago")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    #[test]
    fn relative_times_use_one_unit() {
        let now = at(1_700_000_000);
        assert_eq!(format_relative(now, now), "just now");
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 44), now),
            "just now"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 45), now),
            "45s ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 3 * 60), now),
            "3m ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 2 * 3600), now),
            "2h ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 6 * 86400), now),
            "6d ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 40 * 86400), now),
            "1mo ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() - 400 * 86400), now),
            "1y ago"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() + 120), now),
            "in 2m"
        );
        assert_eq!(
            format_relative(at(now.unix_timestamp() + 10), now),
            "just now"
        );
    }

    #[test]
    fn color_wraps_status_without_changing_visible_text() {
        for status in [
            "inbox",
            "ready",
            "claimed",
            "in_progress",
            "review",
            "blocked",
            "done",
            "cancelled",
        ] {
            let painted = Paint::color().status(status);
            assert!(painted.contains('\u{1b}'), "{status}: {painted:?}");
            assert_eq!(anstream::adapter::strip_str(&painted).to_string(), status);
            assert_eq!(Paint::plain().status(status), status);
        }
        assert!(Paint::color().status("ready").contains("32"));
        assert!(Paint::color().status("blocked").contains("31"));
        assert!(Paint::color().status("cancelled").contains('9'));
        assert_eq!(Paint::color().status("not-a-status"), "not-a-status");
    }
}
