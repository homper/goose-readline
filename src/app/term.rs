// Terminal lifecycle: raw mode, bracketed paste, kitty keyboard-enhancement
// flags, and a restore guard that also covers panics (port of the setup and
// cleanup paths in kilo-readline src/index.ts, on crossterm).

use crate::editor::Terminal;
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType},
};
use std::io::{self, Write};

/// crossterm-backed Terminal for the editor: sizes come from the terminal,
/// writes go straight to stdout.
pub struct CrosstermTerm;

impl Terminal for CrosstermTerm {
    fn columns(&self) -> usize {
        crossterm::terminal::size().map(|(c, _)| c as usize).unwrap_or(0)
    }
    fn rows(&self) -> usize {
        crossterm::terminal::size().map(|(_, r)| r as usize).unwrap_or(0)
    }
    fn write_str(&mut self, s: &str) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(s.as_bytes());
        let _ = out.flush();
    }
}

pub fn terminal_columns() -> usize {
    crossterm::terminal::size().map(|(c, _)| c as usize).unwrap_or(80).max(1)
}

/// Enter raw mode with bracketed paste + disambiguated escape codes.
pub fn setup() -> Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout().lock();
    execute!(
        out,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok(())
}

/// Restore the terminal: paste off, kitty flags popped, cursor shown, cooked
/// mode. Idempotent.
pub fn teardown() {
    let _ = disable_raw_mode();
    let mut out = io::stdout().lock();
    let _ = execute!(
        out,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        crossterm::cursor::Show
    );
    let _ = out.flush();
}

/// Clear from the cursor to the end of the screen (used when abandoning
/// editor overlays).
pub fn clear_to_end() {
    let mut out = io::stdout().lock();
    let _ = execute!(out, Clear(ClearType::FromCursorDown));
}

/// Install a panic hook that restores the terminal before printing the
/// panic, so a crash never leaves the shell in raw mode.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        teardown();
        default(info);
    }));
}
