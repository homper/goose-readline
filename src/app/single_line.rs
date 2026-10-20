// readSingleLine analog: a tiny line reader for permission menus, /model
// choices, and y/N confirms. Only printable typed characters form the answer
// (arrow keys and other escape sequences are dropped); Backspace edits.
// Driven by the app's event loop, one Key at a time.

use crate::editor::keys::Key;

pub struct SingleLineReader {
    buf: String,
    allow: Option<Vec<char>>,
    done: Option<String>,
}

impl SingleLineReader {
    pub fn new(allow: Option<&str>) -> Self {
        Self {
            buf: String::new(),
            allow: allow.map(|a| a.chars().collect()),
            done: None,
        }
    }

    /// Feed one key; returns Some(line) when Enter completes the read.
    pub fn handle_key(&mut self, key: Key) -> Option<String> {
        match key {
            Key::Char(c) => {
                if c.is_control() {
                    return None;
                }
                if let Some(allow) = &self.allow {
                    if !allow.contains(&c) {
                        return None;
                    }
                }
                print!("{c}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                self.buf.push(c);
            }
            Key::Enter => {
                println!();
                self.done = Some(self.buf.clone());
                return self.done.clone();
            }
            Key::Backspace => {
                if self.buf.pop().is_some() {
                    print!("\x08 \x08");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
            }
            _ => {}
        }
        None
    }

    /// Abort (Ctrl+C / external cancel): completes with an empty line.
    pub fn abort(&mut self) -> Option<String> {
        if self.done.is_none() {
            self.done = Some(String::new());
        }
        self.done.clone()
    }

    pub fn is_done(&self) -> bool {
        self.done.is_some()
    }
}
