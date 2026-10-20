// Crossterm KeyEvent → editor key dispatch. This replaces rawinput.ts's
// hand-rolled byte parser (handleBytes/parseEscape/parseCSI/parseSS3):
// crossterm already decodes escape sequences, modifiers, and bracketed paste,
// so only the keymap itself is ported.
//
// Parity notes vs the TS parser:
//  - Ctrl+C (raw 0x03 or kitty CSI 3;5u) → Key::CtrlC → abort
//  - Ctrl+D (raw 0x04) → Key::CtrlD → finish("") when the buffer is empty
//  - Backspace (0x7f) → backspace; Ctrl+Backspace (0x08 / CSI 127;5u) →
//    deleteWordBefore; Alt+Backspace (ESC DEL) → deleteBashWordBefore
//  - Enter → handleEnter; Ctrl+Enter (or Ctrl+J) → submit; Shift/Alt+Enter →
//    newline in multiline mode
//  - Tab / Shift-Tab drive completion cycling and never break it; every other
//    key breaks cycling first (Escape reverts it)
//  - Ctrl+R starts reverse history search (when history navigation is enabled)
//  - Ctrl+Home/End move to doc start/end; Home/End to line start/end
//  - Ctrl+Delete deletes the word after the cursor

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Printable character insertion.
    Char(char),
    Enter,
    /// Submit regardless of cursor position (Ctrl+Enter / Ctrl+J).
    CtrlEnter,
    /// Shift/Alt+Enter: newline in multiline mode, submit in single-line.
    AltEnter,
    Backspace,
    /// Ctrl+Backspace / Ctrl+H: whitespace-rubout backward-kill-word.
    CtrlBackspace,
    /// Alt+Backspace: bash-style backward-kill-word (splits on punctuation).
    AltBackspace,
    Delete,
    CtrlDelete,
    Tab,
    BackTab,
    Up(u16),
    Down(u16),
    Left(u16),
    Right(u16),
    CtrlLeft,
    CtrlRight,
    Home,
    End,
    CtrlHome,
    CtrlEnd,
    Escape,
    CtrlC,
    CtrlD,
    CtrlR,
    /// Recognized but unmapped: no-op.
    Other,
}

pub fn translate(ev: &KeyEvent) -> Key {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    match ev.code {
        KeyCode::Enter => {
            if ctrl {
                Key::CtrlEnter
            } else if alt || shift {
                Key::AltEnter
            } else {
                Key::Enter
            }
        }
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => {
            if ctrl {
                Key::CtrlBackspace
            } else if alt {
                Key::AltBackspace
            } else {
                Key::Backspace
            }
        }
        KeyCode::Delete => {
            if ctrl {
                Key::CtrlDelete
            } else {
                Key::Delete
            }
        }
        KeyCode::Esc => Key::Escape,
        KeyCode::Left => {
            if ctrl {
                Key::CtrlLeft
            } else {
                Key::Left(1)
            }
        }
        KeyCode::Right => {
            if ctrl {
                Key::CtrlRight
            } else {
                Key::Right(1)
            }
        }
        KeyCode::Up => Key::Up(1),
        KeyCode::Down => Key::Down(1),
        KeyCode::Home => {
            if ctrl {
                Key::CtrlHome
            } else {
                Key::Home
            }
        }
        KeyCode::End => {
            if ctrl {
                Key::CtrlEnd
            } else {
                Key::End
            }
        }
        KeyCode::Char(c) => {
            if ctrl {
                match c.to_ascii_lowercase() {
                    'c' => Key::CtrlC,
                    'd' => Key::CtrlD,
                    'r' => Key::CtrlR,
                    // Ctrl+J is a raw line feed: submit from anywhere.
                    'j' => Key::CtrlEnter,
                    // Ctrl+M is carriage return: same as Enter.
                    'm' => Key::Enter,
                    // Terminals send Ctrl+H as 0x08, which the TS editor treats
                    // as backward-kill-word.
                    'h' => Key::CtrlBackspace,
                    _ => Key::Other,
                }
            } else if alt {
                Key::Other
            } else {
                Key::Char(c)
            }
        }
        _ => Key::Other,
    }
}
