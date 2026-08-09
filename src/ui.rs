//! Minimal terminal UI helpers — ANSI colors, TTY detection.
//!
//! Colors are only emitted when stdout/stderr is a TTY, so pipe mode
//! and protocol modes (acp/mcp) never get escape-code pollution.

use std::io::IsTerminal;

/// A tiny ANSI color wrapper that no-ops off a TTY.
#[derive(Clone, Copy)]
pub struct Color {
    code: &'static str,
}

impl Color {
    const RESET: &'static str = "\x1b[0m";

    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// Wrap `s` in this color if `out` is a TTY, else return it bare.
    pub fn paint(self, s: &str, out: &impl IsTerminal) -> String {
        if out.is_terminal() {
            format!("{}{s}{}", self.code, Self::RESET)
        } else {
            s.to_string()
        }
    }
}

pub const CYAN: Color = Color::new("\x1b[36m");
pub const GREEN: Color = Color::new("\x1b[32m");
pub const YELLOW: Color = Color::new("\x1b[33m");
pub const RED: Color = Color::new("\x1b[31m");
pub const BOLD: Color = Color::new("\x1b[1m");
