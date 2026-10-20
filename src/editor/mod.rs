// The raw-mode line editor: hand-port of kilo-readline src/rawinput.ts.
// Crossterm replaces only escape-sequence decoding / raw mode / bracketed
// paste (see keys.rs); all editing, layout, history, completion, and render
// logic below mirrors the TypeScript implementation, including:
//  - logical lines[] buffer with a char-index cursor (row, col)
//  - single-frame redraws (whole frames assembled into one write)
//  - cached-row diffing in short mode (pure cursor moves skip redraws)
//  - tall mode: in-memory hidden rows with a scrolling viewport
//  - bottom-pinned mode for search-style UIs with the completion overlay
//    drawn above the prompt
//  - history navigation with per-entry edit drafts
//  - Ctrl+R incremental reverse search with a preview box
//  - Tab-completion cycling with a 2-row bottom-pinned/below-input overlay

pub mod history;
pub mod keys;
pub mod wrap;

use history::HistoryEntry;
use keys::Key;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wrap::{char_prefix, char_suffix, display_width, display_width_prefix, wrap_plain};

const CSI: &str = "\x1b[";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const REV: &str = "\x1b[7m";

/// Terminal abstraction the editor renders to. `columns`/`rows` return 0 when
/// unknown (the editor falls back to 80x24, like a TTY without a size).
pub trait Terminal: Send + Sync {
    fn columns(&self) -> usize;
    fn rows(&self) -> usize;
    fn write_str(&mut self, s: &str);
    /// Consume the terminal as a capture sink (only CaptureTerminal returns
    /// non-empty; used by the editor's capture-render mechanism).
    fn into_sink(self: Box<Self>) -> String {
        String::new()
    }
}

/// Capture terminal mirroring a real terminal's size; writes accumulate into
/// a string instead of hitting the terminal (analog of captureRender's fake
/// stdout in rawinput.ts).
pub struct CaptureTerminal {
    pub columns: usize,
    pub rows: usize,
    pub sink: String,
}

impl CaptureTerminal {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self { columns, rows, sink: String::new() }
    }
}

impl Terminal for CaptureTerminal {
    fn columns(&self) -> usize {
        self.columns
    }
    fn rows(&self) -> usize {
        self.rows
    }
    fn write_str(&mut self, s: &str) {
        self.sink.push_str(s);
    }
    fn into_sink(self: Box<Self>) -> String {
        self.sink
    }
}

pub type Completer = Box<dyn Fn(&str, usize) -> (Vec<String>, String) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The user submitted the text (Enter / Ctrl+J / Ctrl+D on empty).
    Submitted(String),
    /// Ctrl+C / abort(): the input was abandoned ("" value).
    Aborted,
}

pub struct RawInput {
    term: Box<dyn Terminal>,
    prompt: String,
    pub history: Arc<Mutex<Vec<HistoryEntry>>>,
    completer: Option<Completer>,

    lines: Vec<String>,
    cursor_row: usize,
    /// Cursor column as a CHAR index into the logical line.
    cursor_col: usize,
    history_index: isize,
    saved_lines: Option<Vec<String>>,
    /// Edits made to a history entry while navigating. Temporary: cleared when
    /// the input is sent (finish) or cancelled with Ctrl+C (abort).
    drafts: HashMap<usize, Vec<String>>,
    rendered_rows: usize,
    /// A clear sequence deferred until the next render() so it can be flushed
    /// in the SAME write as the redraw (prevents the visible "prompt blink").
    pending_clear_prefix: String,
    last_phys_cursor_row: usize,
    /// Cache of the exact physical-row strings drawn by the last render_short,
    /// used to skip redrawing rows whose content hasn't changed.
    last_content_rows: Vec<String>,

    result: Option<ReadOutcome>,
    active: bool,
    multiline: bool,
    aborted: bool,
    /// When false, Up/Down and Ctrl+R never navigate prompt history.
    history_enabled: bool,

    search_active: bool,
    search_term: String,
    search_matches: Vec<usize>,
    search_cursor: usize,
    saved_before_search: Option<Vec<String>>,
    search_overlay_rows: usize,
    search_last_cursor_row: usize,

    in_paste: bool,

    // Tab-completion cycling state.
    completion_active: bool,
    completion_matches: Vec<String>,
    /// -1 = common-prefix state; 0..N-1 = selected full match.
    completion_index: isize,
    completion_token_start: usize,
    completion_token: String,
    completion_token_len: usize,
    completion_overlay_rows: usize,
    /// Distance (physical rows) from the top of the input region to the
    /// cursor, captured at the start of each completion render.
    completion_top_phys_row: usize,

    bottom_pinned_mode: bool,
    bottom_pinned_reserved_rows: usize,
    tall_latched: bool,
    view_top: usize,
}

fn common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let mut s = strings[0].clone();
    for o in &strings[1..] {
        let mut j = 0usize;
        let sb = s.as_bytes();
        let ob = o.as_bytes();
        while j < sb.len() && j < ob.len() && sb[j] == ob[j] {
            j += 1;
        }
        // Clamp to a char boundary.
        while j > 0 && !s.is_char_boundary(j) {
            j -= 1;
        }
        s.truncate(j);
        if s.is_empty() {
            break;
        }
    }
    s
}

/// Count occurrences of a needle in a haystack (for ``` fence detection).
fn count_occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

impl RawInput {
    pub fn new(
        term: Box<dyn Terminal>,
        prompt: &str,
        history: Arc<Mutex<Vec<HistoryEntry>>>,
        completer: Option<Completer>,
    ) -> Self {
        Self {
            term,
            prompt: prompt.to_string(),
            history,
            completer,
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
            history_index: -1,
            saved_lines: None,
            drafts: HashMap::new(),
            rendered_rows: 0,
            pending_clear_prefix: String::new(),
            last_phys_cursor_row: 0,
            last_content_rows: Vec::new(),
            result: None,
            active: false,
            multiline: true,
            aborted: false,
            history_enabled: true,
            search_active: false,
            search_term: String::new(),
            search_matches: Vec::new(),
            search_cursor: 0,
            saved_before_search: None,
            search_overlay_rows: 0,
            search_last_cursor_row: 0,
            in_paste: false,
            completion_active: false,
            completion_matches: Vec::new(),
            completion_index: -1,
            completion_token_start: 0,
            completion_token: String::new(),
            completion_token_len: 0,
            completion_overlay_rows: 0,
            completion_top_phys_row: 0,
            bottom_pinned_mode: false,
            bottom_pinned_reserved_rows: 0,
            tall_latched: false,
            view_top: 0,
        }
    }

    // ---- accessors ----

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn is_searching(&self) -> bool {
        self.search_active
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn at_end(&self) -> bool {
        self.cursor_row == self.lines.len() - 1
            && self.cursor_col == self.lines[self.cursor_row].chars().count()
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, value: &str) {
        if value.is_empty() {
            self.lines = vec![String::new()];
            self.cursor_row = 0;
            self.cursor_col = 0;
        } else {
            self.lines = value.split('\n').map(str::to_string).collect();
            self.cursor_row = self.lines.len() - 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.to_string();
    }

    pub fn set_completer(&mut self, c: Option<Completer>) {
        self.completer = c;
    }

    pub fn set_bottom_pinned(&mut self, enabled: bool, reserved_rows: usize) {
        self.bottom_pinned_mode = enabled;
        self.bottom_pinned_reserved_rows = reserved_rows;
        if !enabled {
            self.rendered_rows = 0;
            self.last_phys_cursor_row = 0;
            self.completion_overlay_rows = 0;
            self.completion_top_phys_row = 0;
        }
    }

    pub fn set_history_enabled(&mut self, enabled: bool) {
        self.history_enabled = enabled;
    }

    pub fn set_paste_mode(&mut self, mode: bool) {
        self.in_paste = mode;
    }

    pub fn take_result(&mut self) -> Option<ReadOutcome> {
        self.result.take()
    }

    fn line_char_len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn write(&mut self, s: &str) {
        self.term.write_str(s);
    }

    fn columns(&self) -> usize {
        let c = self.term.columns();
        if c == 0 {
            80
        } else {
            c
        }
    }

    fn screen_height(&self) -> usize {
        let r = self.term.rows();
        if r == 0 {
            24
        } else {
            r
        }
    }

    /// Prefix drawn in front of every continuation line: a dim ">" plus enough
    /// spaces to match the prompt's visible width.
    fn continuation_prefix(&self) -> String {
        let prompt_len = display_width(&self.prompt);
        format!("{DIM}>{RESET}{}", " ".repeat(prompt_len.saturating_sub(1)))
    }

    // ---- read lifecycle ----

    /// Begin a read. Feed subsequent crossterm events to handle_key /
    /// handle_paste and poll take_result() for completion.
    pub fn start_read(&mut self, multiline: bool) {
        self.multiline = multiline;
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.history_index = -1;
        self.saved_lines = None;
        self.drafts.clear();
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;
        self.result = None;
        self.active = true;
        self.aborted = false;
        self.search_active = false;
        self.search_term.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
        self.saved_before_search = None;
        self.search_overlay_rows = 0;
        self.search_last_cursor_row = 0;
        self.tall_latched = false;
        self.view_top = 0;
        self.last_content_rows.clear();
        self.completion_active = false;
        self.completion_matches.clear();
        self.completion_index = -1;
        self.completion_token.clear();
        self.completion_token_len = 0;
        self.completion_overlay_rows = 0;
        self.completion_top_phys_row = 0;

        self.write("\x1b[?25h");
        if self.bottom_pinned_mode {
            // Reserve rows at the bottom: move to the last row and emit
            // newlines so the terminal scrolls to make room.
            let screen_h = self.screen_height();
            let reserved = self.bottom_pinned_reserved_rows.max(1);
            self.write(&format!("{CSI}{screen_h};1H"));
            self.write(&"\n".repeat(reserved - 1));
        }
        self.render();
    }

    pub fn abort(&mut self) {
        if !self.active {
            return;
        }
        let had_completion_overlay = self.completion_overlay_rows > 0;
        let was_bottom_pinned = self.bottom_pinned_mode;
        self.aborted = true;
        self.active = false;
        self.drafts.clear();
        self.completion_active = false;
        self.completion_matches.clear();
        self.completion_index = -1;
        self.completion_token.clear();
        self.completion_token_len = 0;
        self.completion_overlay_rows = 0;
        self.completion_top_phys_row = 0;
        self.search_active = false;
        self.search_term.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
        self.saved_before_search = None;
        if was_bottom_pinned {
            let screen_h = self.screen_height();
            let reserved = self.bottom_pinned_reserved_rows.max(1);
            let top_row = screen_h - reserved + 1;
            let mut buf = String::new();
            for r in top_row..=screen_h {
                buf.push_str(&format!("{CSI}{r};1H{CSI}2K"));
            }
            buf.push_str(&format!("{CSI}{top_row};1H"));
            self.write(&buf);
            self.reset_render_state();
            self.write("\x1b[?25h");
            self.result = Some(ReadOutcome::Aborted);
            return;
        }
        // Print the full input with "^C" stamped at the cursor, mirroring a
        // regular shell.
        self.print_full_input(Some("^C"));
        if had_completion_overlay {
            self.write(&format!("{CSI}0J"));
        }
        self.reset_render_state();
        self.write("\x1b[?25h");
        self.result = Some(ReadOutcome::Aborted);
    }

    /// Clear all bookkeeping that tracks the on-screen region so the next
    /// render starts fresh.
    fn reset_render_state(&mut self) {
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;
        self.tall_latched = false;
        self.view_top = 0;
        self.last_content_rows.clear();
    }

    /// Reprint the whole input from the top of the rendered region, so the
    /// full text is committed to the terminal scrollback. If `marker` is
    /// given it is spliced in at the cursor position.
    fn print_full_input(&mut self, marker: Option<&str>) {
        if self.tall_latched {
            self.write(&format!("{CSI}1;1H{CSI}0J"));
        } else if self.rendered_rows > 0 {
            self.clear();
        }
        let mut buf = String::new();
        for i in 0..self.lines.len() {
            let mut line = self.lines[i].clone();
            if let Some(marker) = marker {
                if i == self.cursor_row {
                    let at_end = self.cursor_col >= line.chars().count();
                    let before = char_prefix(&line, self.cursor_col).to_string();
                    let after = char_suffix(&line, if at_end { self.cursor_col } else { self.cursor_col + 1 }).to_string();
                    line = format!("{before}{marker}{after}");
                }
            }
            if i == 0 {
                buf.push_str(&self.prompt);
                buf.push_str(&line);
            } else {
                buf.push_str("\r\n");
                buf.push_str(&self.continuation_prefix());
                buf.push_str(&line);
            }
        }
        buf.push_str("\r\n");
        self.write(&buf);
    }

    pub fn close(&mut self) {
        self.active = false;
        self.result = None;
    }

    pub fn on_resize(&mut self) {
        if !self.search_active && self.rendered_rows > 0 {
            if self.last_phys_cursor_row > 0 {
                self.write(&format!("{CSI}{}A", self.last_phys_cursor_row));
            }
            self.write("\r");
            self.write(&format!("{CSI}0J"));
        }
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;
        self.search_overlay_rows = 0;
        self.search_last_cursor_row = 0;
        self.tall_latched = false;
        self.view_top = 0;
        self.last_content_rows.clear();
        if self.search_active {
            self.search_active = false;
            self.search_term.clear();
            self.search_matches.clear();
            self.search_cursor = 0;
            self.saved_before_search = None;
            self.write("\r");
            self.write(&format!("{CSI}0J"));
            self.render();
        } else {
            self.render();
        }
    }

    // ---- key dispatch ----

    pub fn handle_key(&mut self, key: Key) {
        if !self.active {
            return;
        }
        if self.search_active {
            self.handle_search_key(key);
            return;
        }
        // Any key other than Tab/Shift-Tab (which drive cycling) and Escape
        // (which reverts it) breaks out of completion cycling.
        if !matches!(key, Key::Tab | Key::BackTab | Key::Escape) && self.completion_active {
            self.break_completion();
        }
        match key {
            Key::Char(c) => self.insert_char(c),
            Key::Enter => self.handle_enter(),
            Key::CtrlEnter => self.finish_value(),
            Key::AltEnter => {
                if self.multiline {
                    self.insert_newline();
                } else {
                    self.finish_value();
                }
            }
            Key::Backspace => self.backspace(),
            Key::CtrlBackspace => self.delete_word_before(),
            Key::AltBackspace => self.delete_bash_word_before(),
            Key::Delete => self.delete_fwd(),
            Key::CtrlDelete => self.delete_word_after(),
            Key::Tab => self.do_tab(),
            Key::BackTab => self.do_shift_tab(),
            Key::Escape => {
                if self.completion_active {
                    self.revert_completion();
                }
            }
            Key::Left(n) => self.move_left(n as usize),
            Key::Right(n) => self.move_right(n as usize),
            Key::Up(n) => {
                if self.multiline && self.cursor_row > 0 {
                    self.move_up(n as usize);
                } else if self.history_enabled {
                    self.history_up();
                }
            }
            Key::Down(n) => {
                if self.multiline && self.cursor_row < self.lines.len() - 1 {
                    self.move_down(n as usize);
                } else if self.history_enabled {
                    self.history_down();
                }
            }
            Key::CtrlLeft => self.move_word_left(),
            Key::CtrlRight => self.move_word_right(),
            Key::Home => self.move_home(),
            Key::End => self.move_end(),
            Key::CtrlHome => self.move_doc_start(),
            Key::CtrlEnd => self.move_doc_end(),
            Key::CtrlC => {
                self.abort();
                return;
            }
            Key::CtrlD => {
                if self.is_empty() {
                    self.finish("");
                    return;
                }
            }
            Key::CtrlR => {
                if self.history_enabled {
                    self.start_search();
                }
            }
            Key::Other => {}
        }
        if self.aborted {
            return;
        }
        if !self.search_active {
            self.render();
        }
    }

    /// Bracketed-paste content (crossterm Event::Paste). Newlines insert as
    /// newlines (paste mode); tabs drive completion, mirroring the TS byte
    /// path. In search mode pastes are dropped.
    pub fn handle_paste(&mut self, raw: &str) {
        if !self.active || self.search_active {
            return;
        }
        let converted = raw.replace("\r\n", "\n").replace('\r', "\n");
        if !converted.is_empty() && self.completion_active {
            self.break_completion();
        }
        for ch in converted.chars() {
            if self.search_active || !self.active {
                break;
            }
            match ch {
                '\n' => self.insert_newline(),
                '\t' => self.do_tab(),
                _ => self.insert_char(ch),
            }
        }
        if !self.search_active {
            self.render();
        }
    }

    // ---- editing ops ----

    fn insert_char(&mut self, ch: char) {
        let line = self.lines[self.cursor_row].clone();
        let before = char_prefix(&line, self.cursor_col).to_string();
        let after = char_suffix(&line, self.cursor_col).to_string();
        self.lines[self.cursor_row] = format!("{before}{ch}{after}");
        self.cursor_col += 1;
    }

    fn has_unclosed_code_block_in_text(&self, text: &str) -> bool {
        count_occurrences(text, "```") % 2 != 0
    }

    fn has_unclosed_code_block(&self) -> bool {
        self.has_unclosed_code_block_in_text(&self.text())
    }

    fn is_closing_code_block_line(&self) -> bool {
        self.lines[self.cursor_row].trim() == "```"
    }

    fn handle_enter(&mut self) {
        if self.in_paste {
            self.insert_newline();
            return;
        }
        // Single-line mode (e.g. a search box): Enter submits.
        if !self.multiline {
            self.finish_value();
            return;
        }
        if !self.at_end() {
            self.insert_newline();
            return;
        }

        if self.lines.len() > 1
            && self.lines[self.cursor_row].is_empty()
            && !self.has_unclosed_code_block()
        {
            self.finish_value();
        } else if self.lines.len() > 1 && self.is_closing_code_block_line() {
            self.finish_value();
        } else if self.lines.len() > 1 {
            self.insert_newline();
        } else if self.has_unclosed_code_block() {
            self.insert_newline();
        } else {
            self.finish_value();
        }
    }

    fn insert_newline(&mut self) {
        let line = self.lines[self.cursor_row].clone();
        let before = char_prefix(&line, self.cursor_col).to_string();
        let after = char_suffix(&line, self.cursor_col).to_string();

        if after.trim_start().starts_with("```")
            && self.has_unclosed_code_block_in_text(&before)
        {
            self.lines[self.cursor_row] = before;
            self.lines
                .insert(self.cursor_row + 1, after.trim_start().to_string());
        } else {
            self.lines[self.cursor_row] = before;
            self.lines.insert(self.cursor_row + 1, after);
        }
        self.cursor_row += 1;
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = self.lines[self.cursor_row].clone();
            let before = char_prefix(&line, self.cursor_col - 1).to_string();
            let after = char_suffix(&line, self.cursor_col).to_string();
            self.lines[self.cursor_row] = format!("{before}{after}");
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let prev_len = self.line_char_len(self.cursor_row - 1);
            let cur = self.lines.remove(self.cursor_row);
            self.lines[self.cursor_row - 1].push_str(&cur);
            self.cursor_row -= 1;
            self.cursor_col = prev_len;
        }
    }

    fn delete_fwd(&mut self) {
        let line_len = self.line_char_len(self.cursor_row);
        if self.cursor_col < line_len {
            let line = self.lines[self.cursor_row].clone();
            let before = char_prefix(&line, self.cursor_col).to_string();
            let after = char_suffix(&line, self.cursor_col + 1).to_string();
            self.lines[self.cursor_row] = format!("{before}{after}");
        } else if self.cursor_row < self.lines.len() - 1 {
            let next = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next);
        }
    }

    fn move_up(&mut self, n: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(n);
        let len = self.line_char_len(self.cursor_row);
        self.cursor_col = self.cursor_col.min(len);
    }

    fn move_down(&mut self, n: usize) {
        self.cursor_row = (self.cursor_row + n).min(self.lines.len() - 1);
        let len = self.line_char_len(self.cursor_row);
        self.cursor_col = self.cursor_col.min(len);
    }

    fn move_left(&mut self, n: usize) {
        let mut n = n;
        while n > 0 && (self.cursor_col > 0 || self.cursor_row > 0) {
            if self.cursor_col > 0 {
                self.cursor_col -= 1;
            } else {
                self.cursor_row -= 1;
                self.cursor_col = self.line_char_len(self.cursor_row);
            }
            n -= 1;
        }
    }

    fn move_right(&mut self, n: usize) {
        let mut n = n;
        while n > 0
            && (self.cursor_col < self.line_char_len(self.cursor_row)
                || self.cursor_row < self.lines.len() - 1)
        {
            if self.cursor_col < self.line_char_len(self.cursor_row) {
                self.cursor_col += 1;
            } else {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            n -= 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    fn move_end(&mut self) {
        self.cursor_col = self.line_char_len(self.cursor_row);
    }

    fn move_doc_start(&mut self) {
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn move_doc_end(&mut self) {
        self.cursor_row = self.lines.len() - 1;
        self.cursor_col = self.line_char_len(self.cursor_row);
    }

    /// Bash/readline-style word char test: letters, digits, underscore.
    fn is_word_char(ch: Option<char>) -> bool {
        matches!(ch, Some(c) if c.is_ascii_alphanumeric() || c == '_')
    }

    fn char_at(line: &str, char_idx: usize) -> Option<char> {
        line.chars().nth(char_idx)
    }

    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_row > 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.line_char_len(self.cursor_row);
            }
            return;
        }
        let line = self.lines[self.cursor_row].clone();
        while self.cursor_col > 0 && Self::char_at(&line, self.cursor_col - 1) == Some(' ') {
            self.cursor_col -= 1;
        }
        while self.cursor_col > 0 && Self::char_at(&line, self.cursor_col - 1) != Some(' ') {
            self.cursor_col -= 1;
        }
    }

    fn move_word_right(&mut self) {
        let line_len = self.line_char_len(self.cursor_row);
        if self.cursor_col >= line_len {
            if self.cursor_row < self.lines.len() - 1 {
                self.cursor_row += 1;
                self.cursor_col = 0;
            }
            return;
        }
        let line = self.lines[self.cursor_row].clone();
        while self.cursor_col < line_len && Self::char_at(&line, self.cursor_col) != Some(' ') {
            self.cursor_col += 1;
        }
        while self.cursor_col < line_len && Self::char_at(&line, self.cursor_col) == Some(' ') {
            self.cursor_col += 1;
        }
    }

    fn delete_word_before(&mut self) {
        if self.cursor_col == 0 && self.cursor_row == 0 {
            return;
        }
        let saved_row = self.cursor_row;
        let saved_col = self.cursor_col;

        if self.cursor_col == 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.line_char_len(self.cursor_row);
        }

        let line = self.lines[self.cursor_row].clone();
        while self.cursor_col > 0 && Self::char_at(&line, self.cursor_col - 1) == Some(' ') {
            self.cursor_col -= 1;
        }
        while self.cursor_col > 0 && Self::char_at(&line, self.cursor_col - 1) != Some(' ') {
            self.cursor_col -= 1;
        }

        self.splice_deleted(saved_row, saved_col);
    }

    /// bash/readline `backward-kill-word` (Alt+Backspace): word characters are
    /// [A-Za-z0-9_]; other non-whitespace characters are their own word.
    fn delete_bash_word_before(&mut self) {
        if self.cursor_col == 0 && self.cursor_row == 0 {
            return;
        }
        let saved_row = self.cursor_row;
        let saved_col = self.cursor_col;

        if self.cursor_col == 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.line_char_len(self.cursor_row);
        }

        let line = self.lines[self.cursor_row].clone();

        while self.cursor_col > 0 && Self::char_at(&line, self.cursor_col - 1) == Some(' ') {
            self.cursor_col -= 1;
        }

        if self.cursor_col > 0 {
            let word_char = Self::is_word_char(Self::char_at(&line, self.cursor_col - 1));
            while self.cursor_col > 0
                && Self::char_at(&line, self.cursor_col - 1) != Some(' ')
                && Self::is_word_char(Self::char_at(&line, self.cursor_col - 1)) == word_char
            {
                self.cursor_col -= 1;
            }
        }

        self.splice_deleted(saved_row, saved_col);
    }

    fn splice_deleted(&mut self, saved_row: usize, saved_col: usize) {
        if self.cursor_row == saved_row {
            let line = self.lines[self.cursor_row].clone();
            let head = char_prefix(&line, self.cursor_col).to_string();
            let tail = char_suffix(&line, saved_col).to_string();
            self.lines[self.cursor_row] = format!("{head}{tail}");
        } else {
            // Keep the text before the cursor on the current row, drop the
            // rows spanned by the deletion, and re-attach the tail.
            let line = self.lines[self.cursor_row].clone();
            self.lines[self.cursor_row] = char_prefix(&line, self.cursor_col).to_string();
            let tail_src = self.lines[saved_row].clone();
            let remaining = char_suffix(&tail_src, saved_col).to_string();
            self.lines.drain(self.cursor_row + 1..=saved_row);
            self.lines[self.cursor_row].push_str(&remaining);
        }
    }

    fn delete_word_after(&mut self) {
        if self.cursor_row == self.lines.len() - 1
            && self.cursor_col >= self.line_char_len(self.cursor_row)
        {
            return;
        }

        let saved_row = self.cursor_row;
        let saved_col = self.cursor_col;

        if self.cursor_col >= self.line_char_len(self.cursor_row) {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }

        let line_len = self.line_char_len(self.cursor_row);
        let line = self.lines[self.cursor_row].clone();
        while self.cursor_col < line_len && Self::char_at(&line, self.cursor_col) != Some(' ') {
            self.cursor_col += 1;
        }
        while self.cursor_col < line_len && Self::char_at(&line, self.cursor_col) == Some(' ') {
            self.cursor_col += 1;
        }

        if self.cursor_row == saved_row {
            let line = self.lines[self.cursor_row].clone();
            let head = char_prefix(&line, saved_col).to_string();
            let tail = char_suffix(&line, self.cursor_col).to_string();
            self.lines[self.cursor_row] = format!("{head}{tail}");
            self.cursor_col = saved_col;
        } else {
            let end = char_suffix(&self.lines[self.cursor_row], self.cursor_col).to_string();
            let head = char_prefix(&self.lines[saved_row], saved_col).to_string();
            self.lines.drain(saved_row + 1..=self.cursor_row);
            self.lines[saved_row] = format!("{head}{end}");
            self.cursor_row = saved_row;
            self.cursor_col = saved_col;
        }
    }

    // ---- history navigation ----

    fn save_current_draft(&mut self) {
        if self.history_index >= 0 {
            self.drafts.insert(self.history_index as usize, self.lines.clone());
        }
    }

    /// Wipe the currently-rendered region and reset tall-mode state so the
    /// next render() redraws from scratch. The actual clear bytes are deferred
    /// into pending_clear_prefix so the wipe + redraw land in a single write.
    fn clear_for_rerender(&mut self) {
        if self.tall_latched {
            self.pending_clear_prefix
                .push_str(&format!("{CSI}1;1H{CSI}0J"));
        } else if self.rendered_rows > 0 {
            if self.last_phys_cursor_row > 0 {
                self.pending_clear_prefix
                    .push_str(&format!("\r{CSI}{}A", self.last_phys_cursor_row));
            } else {
                self.pending_clear_prefix.push('\r');
            }
            self.pending_clear_prefix.push_str(&format!("{CSI}0J"));
        }
        self.reset_render_state();
    }

    fn load_history_entry(&mut self, index: usize) {
        self.clear_for_rerender();
        if let Some(draft) = self.drafts.get(&index) {
            self.lines = draft.clone();
            self.cursor_row = self.lines.len() - 1;
            self.cursor_col = self.line_char_len(self.cursor_row);
        } else {
            let text = {
                let hist = self.history.lock().unwrap();
                hist[index].text.clone()
            };
            self.set_text(&text);
        }
    }

    fn history_up(&mut self) {
        let hist_len = self.history.lock().unwrap().len();
        if hist_len == 0 {
            return;
        }
        if self.history_index == -1 {
            self.saved_lines = Some(self.lines.clone());
            self.history_index = hist_len as isize - 1;
            self.load_history_entry(self.history_index as usize);
        } else if self.history_index > 0 {
            self.save_current_draft();
            self.history_index -= 1;
            self.load_history_entry(self.history_index as usize);
        } else {
            // Already at the top: jump the cursor to the top as a boundary cue.
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
    }

    fn history_down(&mut self) {
        if self.history_index == -1 {
            // Already at the live draft: jump the cursor to the bottom.
            self.cursor_row = self.lines.len() - 1;
            self.cursor_col = self.line_char_len(self.cursor_row);
            return;
        }
        self.save_current_draft();
        let hist_len = self.history.lock().unwrap().len() as isize;
        if self.history_index < hist_len - 1 {
            self.history_index += 1;
            self.load_history_entry(self.history_index as usize);
        } else {
            self.history_index = -1;
            self.clear_for_rerender();
            if let Some(saved) = self.saved_lines.take() {
                self.lines = saved;
                self.cursor_row = self.lines.len() - 1;
                self.cursor_col = self.line_char_len(self.cursor_row);
            } else {
                self.set_text("");
            }
        }
    }

    // ---- submit / finish ----

    #[allow(dead_code)]
    fn submit(&mut self) {
        let text = self.text();
        if text.is_empty() {
            self.write("\r\n");
            self.rendered_rows = 0;
            self.render();
            return;
        }
        self.finish(&text);
    }

    fn finish_value(&mut self) {
        let text = self.text();
        self.finish(&text);
    }

    #[allow(dead_code)]
    fn finish_empty(&mut self) {
        self.finish("");
    }

    fn finish(&mut self, value: &str) {
        self.active = false;
        self.drafts.clear();
        if self.bottom_pinned_mode {
            let screen_h = self.screen_height();
            let reserved = self.bottom_pinned_reserved_rows.max(1);
            let top_row = screen_h - reserved + 1;
            let mut buf = String::new();
            for r in top_row..=screen_h {
                buf.push_str(&format!("{CSI}{r};1H{CSI}2K"));
            }
            buf.push_str(&format!("{CSI}{top_row};1H"));
            self.write(&buf);
            self.reset_render_state();
            self.result = Some(ReadOutcome::Submitted(value.to_string()));
            return;
        }
        if self.tall_latched {
            self.print_full_input(None);
        } else {
            // If the cursor is mid-block, move down to the last content row so
            // the trailing newline starts below the full input.
            let last_content_row = self.rendered_rows as isize - 2;
            if self.rendered_rows > 0 && (self.last_phys_cursor_row as isize) < last_content_row {
                let down = last_content_row - self.last_phys_cursor_row as isize;
                self.write(&format!("{CSI}{down}B"));
            }
            self.write("\r\n");
        }
        self.reset_render_state();
        self.result = Some(ReadOutcome::Submitted(value.to_string()));
    }

    // ---- reverse search ----

    fn start_search(&mut self) {
        if self.history.lock().unwrap().is_empty() {
            return;
        }
        self.search_active = true;
        self.search_term.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
        self.saved_before_search = Some(self.lines.clone());
        self.search_overlay_rows = 0;
        self.search_last_cursor_row = self.last_phys_cursor_row;
        self.render_all();
    }

    fn handle_search_key(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                self.search_term.push(c);
                self.do_search();
            }
            Key::CtrlC => {
                self.cancel_search();
                return;
            }
            Key::Escape => {
                self.cancel_search();
                return;
            }
            // Left/Right arrows (incl. Ctrl variants) accept the search.
            Key::Left(_) | Key::Right(_) | Key::CtrlLeft | Key::CtrlRight => {
                self.accept_search();
                return;
            }
            Key::Enter => {
                self.accept_search();
                return;
            }
            Key::Backspace => {
                if !self.search_term.is_empty() {
                    self.search_term.pop();
                    self.do_search();
                } else {
                    self.cancel_search();
                    return;
                }
            }
            Key::CtrlR => self.next_search_match(),
            _ => {}
        }
        self.render_all();
    }

    fn do_search(&mut self) {
        if self.search_term.is_empty() {
            self.search_matches.clear();
            self.search_cursor = 0;
            return;
        }
        let term = self.search_term.to_lowercase();
        let mut matches = Vec::new();
        let hist = self.history.lock().unwrap();
        for i in (0..hist.len()).rev() {
            if hist[i].text.to_lowercase().contains(&term) {
                matches.push(i);
            }
        }
        drop(hist);
        self.search_matches = matches;
        self.search_cursor = 0;
    }

    fn next_search_match(&mut self) {
        if self.search_matches.len() > 1 {
            self.search_cursor = (self.search_cursor + 1) % self.search_matches.len();
        }
    }

    fn get_current_match(&self) -> String {
        if !self.search_matches.is_empty() && self.search_cursor < self.search_matches.len() {
            let hist = self.history.lock().unwrap();
            hist[self.search_matches[self.search_cursor]].text.clone()
        } else {
            String::new()
        }
    }

    fn accept_search(&mut self) {
        if !self.search_matches.is_empty() && self.search_cursor < self.search_matches.len() {
            let idx = self.search_matches[self.search_cursor];
            if self.saved_lines.is_none() {
                self.saved_lines = self.saved_before_search.clone();
            }
            self.history_index = idx as isize;
            self.load_history_entry(idx);
            // load_history_entry deferred a clear; accept_search performs its
            // own explicit clear, so drop the deferred one.
            self.pending_clear_prefix.clear();
        }
        self.search_active = false;
        self.search_term.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
        self.saved_before_search = None;
        self.search_overlay_rows = 0;

        if self.search_last_cursor_row > 0 {
            self.write(&format!("{CSI}{}A", self.search_last_cursor_row));
        }
        self.write("\r");
        self.write(&format!("{CSI}0J"));
        self.search_last_cursor_row = 0;
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;
        self.view_top = 0;
        let saved_active = self.active;
        self.active = true;
        self.render();
        self.active = saved_active;
    }

    fn cancel_search(&mut self) {
        self.search_active = false;
        self.search_term.clear();
        self.search_matches.clear();
        self.search_cursor = 0;
        self.saved_before_search = None;
        self.search_overlay_rows = 0;
        self.search_last_cursor_row = 0;

        let phys_rows = self.rendered_rows.saturating_sub(1);
        let phys_cursor = self.last_phys_cursor_row;
        let down = phys_rows.saturating_sub(phys_cursor);
        if down > 0 {
            self.write(&format!("{CSI}{down}B"));
        }
        self.write("\r");
        self.write(&format!("{CSI}0J"));
        if down > 0 {
            self.write(&format!("{CSI}{down}A"));
        }
        self.write("\r");
        let col = self.input_cursor_col();
        if col > 0 {
            self.write(&format!("{CSI}{col}C"));
        }
    }

    fn build_box_top(&self, header_text: &str, width: usize) -> String {
        let inner_width = width.saturating_sub(2);
        let mut hdr = header_text.to_string();
        if hdr.chars().count() > inner_width {
            let keep = inner_width.saturating_sub(1);
            hdr = format!("{}…", char_prefix(&hdr, keep));
        }
        let pad = inner_width.saturating_sub(hdr.chars().count());
        format!("┌{hdr}{}┐", "─".repeat(pad))
    }

    /// Physical cursor column within the input's current row.
    fn input_cursor_col(&self) -> usize {
        let columns = self.columns().max(1);
        let prefix = if self.cursor_row == 0 {
            self.prompt.clone()
        } else {
            " ".repeat(display_width(&self.prompt))
        };
        let prefix_len = display_width(&prefix);
        let line = &self.lines[self.cursor_row];
        (prefix_len + display_width_prefix(line, self.cursor_col)) % columns
    }

    /// Draw the search overlay: the frozen input plus a preview box for the
    /// current match below it.
    fn render_all(&mut self) {
        if !self.search_active {
            return;
        }
        let columns = self.columns();
        let box_width = columns.max(4);

        let matched = self.get_current_match();
        let matched_lines: Vec<&str> = if matched.is_empty() {
            Vec::new()
        } else {
            matched.split('\n').collect()
        };

        let mut preview_lines: Vec<String> = Vec::new();
        if !matched_lines.is_empty() {
            if matched_lines.len() <= 5 {
                preview_lines = matched_lines.iter().map(|s| s.to_string()).collect();
            } else {
                preview_lines = matched_lines[..2].iter().map(|s| s.to_string()).collect();
                preview_lines.push("…".to_string());
                preview_lines.extend(matched_lines[matched_lines.len() - 2..].iter().map(|s| s.to_string()).collect::<Vec<_>>());
            }
        }

        let header_text = format!(
            " search `{}`'  {}",
            self.search_term,
            if !self.search_matches.is_empty() {
                format!("{}/{}", self.search_cursor + 1, self.search_matches.len())
            } else {
                "0/0".to_string()
            }
        );
        let box_top = self.build_box_top(&header_text, box_width);
        let inner_width = box_width - 2;
        let content_width = inner_width.saturating_sub(1).max(1);
        let mut wrapped_lines: Vec<String> = Vec::new();
        for line in &preview_lines {
            if line.is_empty() {
                wrapped_lines.push(String::new());
                continue;
            }
            let mut off = 0usize;
            let chars: Vec<char> = line.chars().collect();
            while off < chars.len() {
                let end = (off + content_width).min(chars.len());
                wrapped_lines.push(chars[off..end].iter().collect());
                off = end;
            }
        }
        let new_overlay_rows = 2 + wrapped_lines.len();

        // 1. Move to the top of the region and clear it.
        if self.search_last_cursor_row > 0 {
            self.write(&format!("{CSI}{}A", self.search_last_cursor_row));
        }
        self.write("\r");
        self.write(&format!("{CSI}0J"));

        // 2. Redraw the (frozen) input fresh.
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;
        let saved_active = self.active;
        self.active = true;
        self.render();
        self.active = saved_active;

        // 3. Move down to the overlay top and draw the box.
        let phys_rows = self.rendered_rows.saturating_sub(1);
        let phys_cursor = self.last_phys_cursor_row;
        let down = phys_rows.saturating_sub(phys_cursor);
        if down > 0 {
            self.write(&format!("{CSI}{down}B"));
        }
        self.write("\r");

        self.write(&format!("{DIM}{box_top}{RESET}\r\n"));
        for wl in &wrapped_lines {
            let pad = content_width.saturating_sub(display_width(wl));
            self.write(&format!("{DIM}│{RESET} {wl}{}{DIM}│{RESET}\r\n", " ".repeat(pad)));
        }
        self.write(&format!("{DIM}└{}┘{RESET}\r\n", "─".repeat(inner_width)));

        // 4. Move the cursor back into the input.
        let up = phys_rows + new_overlay_rows - phys_cursor;
        if up > 0 {
            self.write(&format!("{CSI}{up}A"));
        }
        self.write("\r");
        let col = self.input_cursor_col();
        if col > 0 {
            self.write(&format!("{CSI}{col}C"));
        }

        self.search_overlay_rows = new_overlay_rows;
        self.search_last_cursor_row = phys_cursor;
    }

    // ---- completion ----

    fn do_tab(&mut self) {
        if self.completer.is_none() {
            return;
        }
        if self.completion_active {
            self.cycle_completion(1);
            return;
        }
        self.start_completion(false);
    }

    fn do_shift_tab(&mut self) {
        if self.completer.is_none() {
            return;
        }
        if self.completion_active {
            self.cycle_completion(-1);
            return;
        }
        self.start_completion(true);
    }

    fn start_completion(&mut self, backward: bool) {
        let columns = self.columns();
        let line = self.lines[self.cursor_row].clone();
        let cursor_col = self.cursor_col;
        let before = char_prefix(&line, cursor_col).to_string();
        // Token = trailing non-whitespace run.
        let token_start_char = before
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        let token = before[token_start_char..].to_string();
        let token_start = before[..token_start_char].chars().count();

        let hits = {
            let completer = self.completer.as_ref().unwrap();
            completer(&line, cursor_col).0
        };
        if hits.is_empty() {
            return;
        }

        if hits.len() == 1 {
            let trail = char_suffix(&hits[0], token.chars().count()).to_string();
            let head = char_prefix(&line, cursor_col).to_string();
            let tail = char_suffix(&line, cursor_col).to_string();
            self.lines[self.cursor_row] = format!("{head}{trail}{tail}");
            self.cursor_col += trail.chars().count();
            return;
        }

        // Tall mode has no room for the overlay: just complete the common
        // prefix without entering cycling.
        if self.tall_latched {
            let common = common_prefix(&hits);
            let trail = char_suffix(&common, token.chars().count()).to_string();
            let head = char_prefix(&line, cursor_col).to_string();
            let tail = char_suffix(&line, cursor_col).to_string();
            self.lines[self.cursor_row] = format!("{head}{trail}{tail}");
            self.cursor_col += trail.chars().count();
            return;
        }

        let common = common_prefix(&hits);
        self.completion_active = true;
        self.completion_matches = hits.clone();
        self.completion_token_start = token_start;
        self.completion_token = token.clone();
        self.completion_token_len = token.chars().count();
        self.completion_index = if backward { hits.len() as isize - 1 } else { -1 };
        let value: String = if self.completion_index == -1 {
            if common.chars().count() >= token.chars().count() {
                common
            } else {
                token
            }
        } else {
            hits[self.completion_index as usize].clone()
        };
        let head = char_prefix(&line, token_start).to_string();
        let tail = char_suffix(&line, cursor_col).to_string();
        self.lines[self.cursor_row] = format!("{head}{value}{tail}");
        self.cursor_col = token_start + value.chars().count();
        self.completion_top_phys_row = self.last_phys_cursor_row;
        let _ = columns;
        self.render_completion();
    }

    fn cycle_completion(&mut self, dir: i32) {
        let n = self.completion_matches.len();
        let states = n + 1;
        let pos = (self.completion_index + 1) as i32;
        let pos = ((pos + dir + states as i32) % states as i32) as usize;
        self.completion_index = pos as isize - 1;
        let value: String = if self.completion_index == -1 {
            let c = common_prefix(&self.completion_matches);
            if c.chars().count() >= self.completion_token_len {
                c
            } else {
                self.completion_token.clone()
            }
        } else {
            self.completion_matches[self.completion_index as usize].clone()
        };
        let line = self.lines[self.cursor_row].clone();
        let ts = self.completion_token_start;
        let tail = char_suffix(&line, self.cursor_col).to_string();
        let head = char_prefix(&line, ts).to_string();
        self.lines[self.cursor_row] = format!("{head}{value}{tail}");
        self.cursor_col = ts + value.chars().count();
        self.render_completion();
    }

    /// Exit completion cycling and clear the variant overlay below the input.
    fn break_completion(&mut self) {
        if !self.completion_active {
            return;
        }
        self.completion_active = false;
        self.completion_matches.clear();
        self.completion_index = -1;
        self.completion_token.clear();
        self.completion_token_len = 0;
        if self.bottom_pinned_mode {
            self.completion_overlay_rows = 0;
            return;
        }
        if self.completion_overlay_rows > 0 {
            let phys_rows = self.rendered_rows.saturating_sub(1);
            let down = phys_rows.saturating_sub(self.last_phys_cursor_row);
            let mut buf = String::new();
            if down > 0 {
                buf.push_str(&format!("{CSI}{down}B"));
            }
            buf.push_str(&format!("\r{CSI}0J"));
            if down > 0 {
                buf.push_str(&format!("{CSI}{down}A"));
            }
            buf.push('\r');
            let col = self.input_cursor_col();
            if col > 0 {
                buf.push_str(&format!("{CSI}{col}C"));
            }
            self.write(&buf);
            self.completion_overlay_rows = 0;
        }
    }

    /// Like break_completion, but restores the originally typed token.
    fn revert_completion(&mut self) {
        if !self.completion_active {
            return;
        }
        let line = self.lines[self.cursor_row].clone();
        let ts = self.completion_token_start;
        let trailing = char_suffix(&line, self.cursor_col).to_string();
        let head = char_prefix(&line, ts).to_string();
        self.lines[self.cursor_row] = format!("{head}{}{trailing}", self.completion_token);
        self.cursor_col = ts + self.completion_token.chars().count();
        self.completion_active = false;
        self.completion_matches.clear();
        self.completion_index = -1;
        self.completion_token.clear();
        self.completion_token_len = 0;
        self.completion_overlay_rows = 0;
        self.completion_top_phys_row = 0;
        if self.bottom_pinned_mode {
            return;
        }
        self.clear_for_rerender();
    }

    /// Redraw the input region plus the variant overlay below it (at most 2
    /// rows), with the selected match highlighted in reverse video.
    fn render_completion(&mut self) {
        if self.bottom_pinned_mode {
            let columns = self.columns();
            let screen_h = self.screen_height();
            self.render_bottom_pinned(columns, screen_h);
            return;
        }
        let columns = self.columns();

        let hits = self.completion_matches.clone();
        let widths: Vec<usize> = hits.iter().map(|h| display_width(h)).collect();
        let max_w = widths.iter().copied().max().unwrap_or(1).max(1);
        let col_w = (max_w + 2).min(columns);
        let per_row = (columns / col_w).max(1);
        const MAX_ROWS: usize = 2;

        let cur = self.completion_index;
        let focus = if cur == -1 { 0 } else { cur as usize };
        let focus_row = focus / per_row;
        let total_rows = hits.len().div_ceil(per_row);
        let mut start_row = focus_row.saturating_sub(MAX_ROWS - 1);
        let max_start_row = total_rows.saturating_sub(MAX_ROWS);
        if start_row > max_start_row {
            start_row = max_start_row;
        }
        let start_idx = start_row * per_row;
        let show_end = (start_idx + per_row * MAX_ROWS).min(hits.len());
        let show = &hits[start_idx..show_end];
        let more_before = start_idx;
        let more_after = hits.len() - show_end;

        let mut rows: Vec<String> = Vec::new();
        let mut i = 0usize;
        while i < show.len() {
            let row_hits = &show[i..(i + per_row).min(show.len())];
            let mut row_str = String::new();
            for (j, cell) in row_hits.iter().enumerate() {
                let abs_idx = start_idx + i + j;
                let cell_str = if abs_idx as isize == cur {
                    format!("{REV}{cell}{RESET}")
                } else {
                    cell.clone()
                };
                let pad = col_w.saturating_sub(display_width(cell));
                row_str.push_str(&cell_str);
                row_str.push_str(&" ".repeat(pad));
            }
            rows.push(row_str);
            i += per_row;
        }
        let more_line = if more_before > 0 || more_after > 0 {
            Some(format!("{DIM}... ({more_before} above, {more_after} below){RESET}"))
        } else {
            None
        };
        let overlay_rows = rows.len() + more_line.is_some() as usize;

        // 1. Move to the top of the region and clear it.
        let mut pre = String::new();
        if self.completion_top_phys_row > 0 {
            pre.push_str(&format!("{CSI}{}A", self.completion_top_phys_row));
        }
        pre.push_str(&format!("\r{CSI}0J"));
        self.rendered_rows = 0;
        self.last_phys_cursor_row = 0;

        // 2. Redraw the input with its output captured.
        let input_buf = self.capture_render();
        let _ = input_buf.clone();

        // 3. Build the overlay + cursor-restore suffix.
        let phys_rows = self.rendered_rows.saturating_sub(1);
        let down = phys_rows.saturating_sub(self.last_phys_cursor_row);
        let mut post = String::new();
        if down > 0 {
            post.push_str(&format!("{CSI}{down}B"));
        }
        post.push('\r');
        if !rows.is_empty() {
            post.push_str(&rows.join("\r\n"));
            post.push_str("\r\n");
        }
        if let Some(ml) = &more_line {
            post.push_str(ml);
            post.push_str("\r\n");
        }
        let up = (phys_rows - self.last_phys_cursor_row) + overlay_rows;
        if up > 0 {
            post.push_str(&format!("{CSI}{up}A"));
        }
        post.push('\r');
        let col = self.input_cursor_col();
        if col > 0 {
            post.push_str(&format!("{CSI}{col}C"));
        }

        // 4. One flush: clear + input + overlay + cursor.
        let frame = format!("{pre}{input_buf}{post}");
        if !frame.is_empty() {
            self.write(&frame);
        }

        self.completion_overlay_rows = overlay_rows;
        self.completion_top_phys_row = self.last_phys_cursor_row;
    }

    /// Run render() with output captured into a string (analog of
    /// captureRender): the editor's terminal is swapped for a capture sink of
    /// the same size.
    fn capture_render(&mut self) -> String {
        let (cols, rows) = (self.term.columns(), self.term.rows());
        let real = std::mem::replace(&mut self.term, Box::new(CaptureTerminal::new(cols, rows)));
        let saved_active = self.active;
        self.active = true;
        self.render();
        self.active = saved_active;
        let cap = std::mem::replace(&mut self.term, real);
        cap.into_sink()
    }

    // ---- layout & render ----

    fn clear(&mut self) {
        if self.rendered_rows == 0 {
            return;
        }
        let rows = self.rendered_rows;
        if self.last_phys_cursor_row > 0 {
            self.write(&format!("\r{CSI}{}A", self.last_phys_cursor_row));
        } else {
            self.write("\r");
        }
        for _ in 0..rows {
            self.write(&format!("\r{CSI}K\r\n"));
        }
        // Move back up to the top of the cleared region so callers reprint in
        // place.
        self.write(&format!("\r{CSI}{rows}A"));
        self.rendered_rows = 0;
    }

    /// Wrap each logical line into physical screen rows. Returns all row
    /// strings (first row of each line carries its prefix) and the per-line
    /// row counts.
    fn build_rows(&self, columns: usize) -> (Vec<String>, Vec<usize>) {
        let cont = self.continuation_prefix();
        let mut rows: Vec<String> = Vec::new();
        let mut line_rows: Vec<usize> = Vec::new();
        for (i, content) in self.lines.iter().enumerate() {
            let prefix = if i == 0 { self.prompt.clone() } else { cont.clone() };
            let prefix_vis = display_width(&prefix);
            let avail = columns.saturating_sub(prefix_vis);
            if content.is_empty() || avail == 0 {
                rows.push(format!("{prefix}{content}"));
                line_rows.push(1);
                continue;
            }
            let chunks = wrap_plain(content, avail, columns);
            let count = chunks.len();
            for (k, chunk) in chunks.iter().enumerate() {
                if k == 0 {
                    rows.push(format!("{prefix}{chunk}"));
                } else {
                    rows.push(chunk.clone());
                }
            }
            line_rows.push(count);
        }
        (rows, line_rows)
    }

    fn layout(&self, columns: usize) -> Layout {
        let (rows, line_rows) = self.build_rows(columns);
        let phys_rows = rows.len();
        let mut phys_cursor = 0usize;
        for i in 0..self.lines.len() {
            if i < self.cursor_row {
                phys_cursor += line_rows[i];
            } else if i == self.cursor_row {
                phys_cursor += self.cursor_phys_in_line(i, columns);
            }
        }
        Layout {
            phys_rows,
            phys_cursor,
            total_physical: phys_rows + 1,
            rows,
        }
    }

    /// Physical row (0-based, within the cursor's logical line) where the
    /// cursor falls, computed by walking the same width-based wrap boundaries
    /// build_rows uses — guaranteeing layout row counts match rendered rows
    /// even with wide chars or a prompt as wide as the terminal.
    fn cursor_phys_in_line(&self, line_idx: usize, columns: usize) -> usize {
        let prefix = if line_idx == 0 {
            self.prompt.clone()
        } else {
            self.continuation_prefix()
        };
        let prefix_w = display_width(&prefix);
        let line = &self.lines[line_idx];
        let avail = columns.saturating_sub(prefix_w);
        if line.is_empty() || avail == 0 {
            return 0;
        }
        let mut row = 0usize;
        let mut row_w = 0usize;
        let mut limit = avail;
        for ch in line.chars().take(self.cursor_col) {
            let cw = wrap::char_display_width(ch);
            if cw > 0 && row_w + cw > limit && row_w > 0 {
                row += 1;
                row_w = 0;
                limit = columns;
            }
            row_w += cw;
        }
        row
    }

    pub fn render(&mut self) {
        if !self.active {
            return;
        }
        let columns = self.columns();
        let screen_h = self.screen_height();

        if self.bottom_pinned_mode {
            self.render_bottom_pinned(columns, screen_h);
            return;
        }

        let lay = self.layout(columns);
        let tall = lay.total_physical > screen_h;
        let entering_tall = tall && !self.tall_latched;
        if tall {
            self.tall_latched = true;
        }

        if self.tall_latched && !self.search_active {
            if entering_tall {
                self.scroll_previous_output_into_scrollback(screen_h);
            }
            self.render_tall(columns, screen_h, &lay);
            return;
        }

        self.render_short(columns, &lay);
        self.view_top = 0;
    }

    fn render_bottom_pinned(&mut self, columns: usize, screen_h: usize) {
        const MAX_OVERLAY_ROWS: usize = 2;
        let prompt_row = screen_h; // 1-indexed last row
        let line = self.lines.first().cloned().unwrap_or_default();
        let prompt_content = format!("{}{}", self.prompt, line);

        let mut buf = String::new();

        if self.completion_active && !self.completion_matches.is_empty() {
            let hits = self.completion_matches.clone();
            let widths: Vec<usize> = hits.iter().map(|h| display_width(h)).collect();
            let max_w = widths.iter().copied().max().unwrap_or(1).max(1);
            let col_w = (max_w + 2).min(columns);
            let per_row = (columns / col_w).max(1);

            let cur = self.completion_index;
            let focus = if cur == -1 { 0 } else { cur as usize };
            let focus_row = focus / per_row;
            let total_rows = hits.len().div_ceil(per_row);
            let mut start_row = focus_row.saturating_sub(MAX_OVERLAY_ROWS - 1);
            let max_start_row = total_rows.saturating_sub(MAX_OVERLAY_ROWS);
            if start_row > max_start_row {
                start_row = max_start_row;
            }
            let start_idx = start_row * per_row;
            let show_end = (start_idx + per_row * MAX_OVERLAY_ROWS).min(hits.len());
            let show = &hits[start_idx..show_end];
            let more_before = start_idx;
            let more_after = hits.len() - show_end;

            let mut rows: Vec<String> = Vec::new();
            let mut i = 0usize;
            while i < show.len() {
                let row_hits = &show[i..(i + per_row).min(show.len())];
                let mut row_str = String::new();
                for (j, cell) in row_hits.iter().enumerate() {
                    let abs_idx = start_idx + i + j;
                    let cell_str = if abs_idx as isize == cur {
                        format!("{REV}{cell}{RESET}")
                    } else {
                        format!("{DIM}{cell}{RESET}")
                    };
                    let pad = col_w.saturating_sub(display_width(cell));
                    row_str.push_str(&cell_str);
                    row_str.push_str(&" ".repeat(pad));
                }
                rows.push(row_str);
                i += per_row;
            }
            let more_line = if more_before > 0 || more_after > 0 {
                Some(format!("{DIM}... ({more_before} above, {more_after} below){RESET}"))
            } else {
                None
            };

            let overlay_row_count = rows.len() + more_line.is_some() as usize;
            let overlay_start_row = prompt_row.saturating_sub(overlay_row_count);
            for (i, row) in rows.iter().enumerate() {
                buf.push_str(&format!("{CSI}{};1H{CSI}2K{}", overlay_start_row + i, row));
            }
            if let Some(ml) = &more_line {
                buf.push_str(&format!(
                    "{CSI}{};1H{CSI}2K{ml}",
                    overlay_start_row + rows.len()
                ));
            }
            self.completion_overlay_rows = overlay_row_count;
        } else {
            let reserved = self.bottom_pinned_reserved_rows.max(1);
            for i in 0..reserved.saturating_sub(1) {
                buf.push_str(&format!(
                    "{CSI}{};1H{CSI}2K",
                    prompt_row - (reserved - 1) + i
                ));
            }
            self.completion_overlay_rows = 0;
        }

        // Prompt line at the last row.
        buf.push_str(&format!("{CSI}{prompt_row};1H{CSI}2K{prompt_content}"));

        let prefix_len = display_width(&self.prompt);
        let cursor_abs_col = prefix_len + display_width_prefix(&line, self.cursor_col) + 1;
        buf.push_str(&format!("{CSI}{prompt_row};{cursor_abs_col}H"));

        self.write(&buf);
        self.rendered_rows = 1;
        self.last_phys_cursor_row = 0;
    }

    /// Resolve the tall-mode window for the current cursor position.
    fn resolve_tall_window(
        &self,
        screen_h: usize,
        phys_cursor: usize,
        total_physical: usize,
    ) -> (usize, usize, usize, usize) {
        let mut view_top = self.view_top;
        let mut header_rows = 0usize;
        let mut footer_rows = 0usize;
        let mut content_rows;
        for _ in 0..6 {
            header_rows = if view_top > 0 { 1 } else { 0 };
            content_rows = screen_h - header_rows;
            let bottom_hidden = total_physical.saturating_sub(view_top + content_rows);
            footer_rows = if bottom_hidden > 0 { 1 } else { 0 };
            content_rows = screen_h - header_rows - footer_rows;
            if content_rows < 1 {
                footer_rows = 0;
                content_rows = screen_h.saturating_sub(header_rows).max(1);
            }
            let max_view_top = total_physical.saturating_sub(content_rows);
            let mut nv = view_top.min(max_view_top);
            if phys_cursor < nv {
                nv = phys_cursor;
            }
            if phys_cursor > nv + content_rows - 1 {
                nv = phys_cursor - content_rows + 1;
            }
            nv = nv.min(max_view_top);
            if nv == view_top {
                break;
            }
            view_top = nv;
        }
        header_rows = if view_top > 0 { 1 } else { 0 };
        content_rows = screen_h - header_rows;
        let bottom_hidden = total_physical.saturating_sub(view_top + content_rows);
        footer_rows = if bottom_hidden > 0 { 1 } else { 0 };
        content_rows = screen_h - header_rows - footer_rows;
        if content_rows < 1 {
            footer_rows = 0;
            content_rows = screen_h.saturating_sub(header_rows).max(1);
        }
        (view_top, header_rows, footer_rows, content_rows)
    }

    fn render_short(&mut self, columns: usize, lay: &Layout) {
        let phys_cursor = lay.phys_cursor;
        let total_physical = lay.total_physical;
        let new_rows = &lay.rows;

        let had_region = self.rendered_rows > 0;
        let old_rows: &[String] = if had_region {
            &self.last_content_rows
        } else {
            &[]
        };

        // First physical row whose content differs from what's on screen.
        let mut d = 0usize;
        while d < new_rows.len() && d < old_rows.len() && new_rows[d] == old_rows[d] {
            d += 1;
        }
        let content_changed = d < new_rows.len() || new_rows.len() != old_rows.len();

        // Whole frame in one write, with any deferred clear prepended.
        let mut buf = std::mem::take(&mut self.pending_clear_prefix);

        if had_region && !content_changed {
            // Pure cursor move: just reposition.
            let delta = phys_cursor as isize - self.last_phys_cursor_row as isize;
            if delta > 0 {
                buf.push_str(&format!("{CSI}{delta}B"));
            } else if delta < 0 {
                buf.push_str(&format!("{CSI}{}A", -delta));
            }
            buf.push('\r');
        } else {
            if had_region {
                if self.last_phys_cursor_row > 0 {
                    buf.push_str(&format!("{CSI}{}A", self.last_phys_cursor_row));
                }
                buf.push('\r');
                if d > 0 {
                    buf.push_str(&format!("{CSI}{d}B"));
                }
            }
            for row in &new_rows[d..] {
                buf.push_str(&format!("\r{CSI}K{row}\r\n"));
            }
            if had_region && new_rows.len() < old_rows.len() {
                buf.push_str(&format!("{CSI}0J"));
            }
            let up_rows = new_rows.len() - phys_cursor;
            if up_rows > 0 {
                buf.push_str(&format!("{CSI}{up_rows}A"));
            }
            buf.push('\r');
        }

        self.last_content_rows = new_rows.clone();
        self.rendered_rows = total_physical;
        self.last_phys_cursor_row = phys_cursor;

        let prefix = if self.cursor_row == 0 {
            self.prompt.clone()
        } else {
            self.continuation_prefix()
        };
        let prefix_len = display_width(&prefix);
        let before = display_width_prefix(&self.lines[self.cursor_row], self.cursor_col);
        let col = (prefix_len + before) % columns.max(1);
        if col > 0 {
            buf.push_str(&format!("{CSI}{col}C"));
        }

        if !buf.is_empty() {
            self.write(&buf);
        }
    }

    fn scroll_previous_output_into_scrollback(&mut self, screen_h: usize) {
        let k = screen_h.saturating_sub(1 + self.last_phys_cursor_row);
        if k > 0 {
            self.write(&"\n".repeat(k));
        }
    }

    fn render_tall(&mut self, columns: usize, screen_h: usize, lay: &Layout) {
        let phys_cursor = lay.phys_cursor;
        let total_physical = lay.total_physical;

        let (view_top, header_rows, footer_rows, content_rows) =
            self.resolve_tall_window(screen_h, phys_cursor, total_physical);
        self.view_top = view_top;

        let start_phys = view_top;
        let end_phys = total_physical.min(view_top + content_rows);

        let mut buf = std::mem::take(&mut self.pending_clear_prefix);
        let mut screen_row = 0usize;

        let hidden_hint = |hidden: usize| {
            format!(
                "{DIM}… {RESET}{DIM}({hidden} row{} hidden){RESET}",
                if hidden == 1 { "" } else { "s" }
            )
        };

        if header_rows > 0 {
            buf.push_str(&format!("{CSI}1;1H{CSI}2K{}", hidden_hint(view_top)));
            screen_row = header_rows;
        }

        // Visible content rows from the prebuilt physical rows.
        let mut phys_row = 0usize;
        for row in &lay.rows {
            if phys_row >= end_phys {
                break;
            }
            if phys_row >= start_phys && screen_row < screen_h {
                buf.push_str(&format!("{CSI}{};1H{CSI}2K{row}", screen_row + 1));
                screen_row += 1;
            }
            phys_row += 1;
        }
        // Trailing rest rows + blank rows inside the window.
        while phys_row < end_phys && screen_row < screen_h {
            buf.push_str(&format!("{CSI}{};1H{CSI}2K", screen_row + 1));
            screen_row += 1;
            phys_row += 1;
        }

        if footer_rows > 0 && screen_row < screen_h {
            let hidden = total_physical.saturating_sub(view_top + content_rows);
            buf.push_str(&format!("{CSI}{};1H{CSI}2K{}", screen_row + 1, hidden_hint(hidden)));
            screen_row += 1;
        }

        // Clear previously-drawn rows below the new window.
        let mut r = screen_row;
        while r < self.rendered_rows && r < screen_h {
            buf.push_str(&format!("{CSI}{};1H{CSI}2K", r + 1));
            r += 1;
        }

        self.rendered_rows = screen_row;
        self.last_phys_cursor_row = header_rows + (phys_cursor - view_top);

        let prefix = if self.cursor_row == 0 {
            self.prompt.clone()
        } else {
            self.continuation_prefix()
        };
        let prefix_len = display_width(&prefix);
        let before = display_width_prefix(&self.lines[self.cursor_row], self.cursor_col);
        let col = (prefix_len + before) % columns.max(1);
        let cursor_screen_row = header_rows + (phys_cursor - view_top);
        buf.push_str(&format!("{CSI}{};1H", cursor_screen_row + 1));
        if col > 0 {
            buf.push_str(&format!("{CSI}{}G", col + 1));
        }

        if !buf.is_empty() {
            self.write(&buf);
        }
    }
}

struct Layout {
    #[allow(dead_code)]
    phys_rows: usize,
    phys_cursor: usize,
    total_physical: usize,
    rows: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::wrap::display_slice_index;
    use keys::Key;

    struct TestTerm {
        columns: usize,
        rows: usize,
        out: String,
    }

    impl TestTerm {
        fn new(columns: usize, rows: usize) -> Self {
            Self { columns, rows, out: String::new() }
        }
    }

    impl Terminal for TestTerm {
        fn columns(&self) -> usize {
            self.columns
        }
        fn rows(&self) -> usize {
            self.rows
        }
        fn write_str(&mut self, s: &str) {
            self.out.push_str(s);
        }
    }

    fn editor_with_history(cols: usize, rows: usize, hist: Vec<&str>) -> (RawInput, Arc<Mutex<Vec<HistoryEntry>>>) {
        let history = Arc::new(Mutex::new(
            hist.into_iter()
                .map(|t| HistoryEntry { text: t.to_string(), is_multiline: t.contains('\n') })
                .collect(),
        ));
        let ed = RawInput::new(
            Box::new(TestTerm::new(cols, rows)),
            "kilo> ",
            history.clone(),
            None,
        );
        (ed, history)
    }

    #[test]
    fn type_and_submit() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        for ch in "hello".chars() {
            ed.handle_key(Key::Char(ch));
        }
        assert_eq!(ed.text(), "hello");
        ed.handle_key(Key::Enter);
        assert_eq!(
            ed.take_result(),
            Some(ReadOutcome::Submitted("hello".to_string()))
        );
    }

    #[test]
    fn multiline_enter_rules() {
        // Enter on empty continuation line submits.
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.handle_key(Key::AltEnter); // shift/alt-enter inserts a newline
        for ch in "abc".chars() {
            ed.handle_key(Key::Char(ch));
        }
        // Cursor on line 1 at end; Enter at end on last line with >1 lines and
        // empty line? line is "abc", not empty → inserts newline.
        ed.handle_key(Key::Enter);
        assert_eq!(ed.lines.len(), 3);
        // Now on empty line 2: Enter submits (the leading empty line from the
        // initial Alt+Enter is part of the text).
        ed.handle_key(Key::Enter);
        assert_eq!(
            ed.take_result(),
            Some(ReadOutcome::Submitted("\nabc\n".to_string()))
        );
    }

    #[test]
    fn single_line_enter_submits() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(false);
        ed.handle_key(Key::AltEnter);
        assert_eq!(ed.lines.len(), 1, "single-line mode never grows");
        ed.handle_key(Key::Enter);
        assert!(ed.take_result().is_some());
    }

    #[test]
    fn word_ops() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        for ch in "foo bar baz".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::CtrlBackspace); // delete "baz"
        assert_eq!(ed.text(), "foo bar ");
        ed.handle_key(Key::AltBackspace); // bash-style kill "bar"
        assert_eq!(ed.text(), "foo ");
        ed.handle_key(Key::CtrlRight); // skip space, land in "foo"
        ed.handle_key(Key::CtrlLeft);
        assert_eq!(ed.cursor_col, 0);
    }

    #[test]
    fn history_drafts_preserved_and_restored() {
        let (mut ed, _) = editor_with_history(80, 24, vec!["one", "two", "three"]);
        ed.start_read(true);
        ed.handle_key(Key::Up(1)); // load "three"
        assert_eq!(ed.text(), "three");
        for ch in "X".chars() {
            ed.handle_key(Key::Char(ch));
        }
        assert_eq!(ed.text(), "threeX");
        ed.handle_key(Key::Up(1)); // draft saved; load "two"
        assert_eq!(ed.text(), "two");
        ed.handle_key(Key::Down(1)); // restore draft of "three"
        assert_eq!(ed.text(), "threeX");
        ed.handle_key(Key::Down(1)); // back to live draft (empty)
        assert_eq!(ed.text(), "");
        // Drafts persist until submit/abort: Up returns to the edited draft.
        ed.handle_key(Key::Up(1));
        assert_eq!(ed.text(), "threeX");
        // Submitting clears the drafts.
        ed.handle_key(Key::Enter);
        assert!(ed.take_result().is_some());
        ed.start_read(true);
        ed.handle_key(Key::Up(1));
        assert_eq!(ed.text(), "three");
    }

    #[test]
    fn ctrl_c_aborts_and_clears_drafts() {
        let (mut ed, _) = editor_with_history(80, 24, vec!["one"]);
        ed.start_read(true);
        ed.handle_key(Key::Up(1));
        for ch in "edit".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::CtrlC);
        assert_eq!(ed.take_result(), Some(ReadOutcome::Aborted));
        // The aborted draft must not persist.
        ed.start_read(true);
        ed.handle_key(Key::Up(1));
        assert_eq!(ed.text(), "one");
    }

    #[test]
    fn ctrl_d_on_empty_finishes() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.handle_key(Key::CtrlD);
        assert_eq!(ed.take_result(), Some(ReadOutcome::Submitted(String::new())));
    }

    #[test]
    fn search_flow() {
        let (mut ed, _) = editor_with_history(80, 24, vec!["alpha", "beta", "gamma"]);
        ed.start_read(true);
        ed.handle_key(Key::CtrlR);
        assert!(ed.is_searching());
        for ch in "alp".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::Enter); // accept
        assert!(!ed.is_searching());
        assert_eq!(ed.text(), "alpha");
    }

    #[test]
    fn search_cancel_restores() {
        let (mut ed, _) = editor_with_history(80, 24, vec!["alpha"]);
        ed.start_read(true);
        ed.handle_key(Key::CtrlR);
        ed.handle_key(Key::Char('a'));
        ed.handle_key(Key::CtrlC);
        assert!(!ed.is_searching());
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn paste_inserts_newlines() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.handle_paste("line1\nline2\r\nline3");
        assert_eq!(ed.text(), "line1\nline2\nline3");
        // Paste mode: Enter inside paste content inserted, not submitted.
        assert!(ed.is_active());
    }

    #[test]
    fn completion_single_match_inserts() {
        let completer: Completer = Box::new(|line: &str, _col: usize| {
            let _ = line;
            (vec!["src/".to_string()], String::new())
        });
        let history = Arc::new(Mutex::new(vec![]));
        let mut ed = RawInput::new(
            Box::new(TestTerm::new(80, 24)),
            "kilo> ",
            history,
            Some(completer),
        );
        ed.start_read(true);
        for ch in "sr".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::Tab);
        assert_eq!(ed.text(), "src/");
        assert!(!ed.completion_active);
    }

    #[test]
    fn completion_cycling_and_break() {
        let completer: Completer = Box::new(|_line: &str, _col: usize| {
            (vec!["foo".to_string(), "far".to_string(), "fab".to_string()], String::new())
        });
        let history = Arc::new(Mutex::new(vec![]));
        let mut ed = RawInput::new(
            Box::new(TestTerm::new(80, 24)),
            "kilo> ",
            history,
            Some(completer),
        );
        ed.start_read(true);
        ed.handle_key(Key::Char('f'));
        ed.handle_key(Key::Tab); // common prefix "f" — cycling at -1
        assert!(ed.completion_active);
        assert_eq!(ed.text(), "f");
        ed.handle_key(Key::Tab); // -> "foo"
        assert_eq!(ed.text(), "foo");
        ed.handle_key(Key::Tab); // -> "far"
        assert_eq!(ed.text(), "far");
        ed.handle_key(Key::BackTab); // -> "foo"
        assert_eq!(ed.text(), "foo");
        ed.handle_key(Key::BackTab); // -> "f" common prefix
        assert_eq!(ed.text(), "f");
        ed.handle_key(Key::Char('x')); // break cycling, insert
        assert!(!ed.completion_active);
        assert_eq!(ed.text(), "fx");
    }

    #[test]
    fn completion_escape_reverts_token() {
        let completer: Completer = Box::new(|_line: &str, _col: usize| {
            (vec!["model-a".to_string(), "model-b".to_string()], String::new())
        });
        let history = Arc::new(Mutex::new(vec![]));
        let mut ed = RawInput::new(
            Box::new(TestTerm::new(80, 24)),
            "kilo> ",
            history,
            Some(completer),
        );
        ed.start_read(true);
        for ch in "mod".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::Tab); // common prefix "model-" extends typed text
        assert_eq!(ed.text(), "model-");
        ed.handle_key(Key::Tab); // select first match
        assert_eq!(ed.text(), "model-a");
        ed.handle_key(Key::Escape); // revert to originally typed token
        assert_eq!(ed.text(), "mod");
    }

    #[test]
    fn tall_mode_latches_and_scrolls() {
        // 5-row screen; type 8 lines to force tall mode.
        let (mut ed, _) = editor_with_history(20, 5, vec![]);
        ed.start_read(true);
        for i in 0..8 {
            for ch in format!("line{i}").chars() {
                ed.handle_key(Key::Char(ch));
            }
            if i < 7 {
                ed.handle_key(Key::AltEnter);
            }
        }
        assert!(ed.tall_latched, "8 lines on a 5-row screen must latch tall mode");
        // Cursor at the end must be visible: view follows the cursor.
        let cursor_phys = ed.last_phys_cursor_row;
        assert!(cursor_phys < 5, "cursor row {cursor_phys} must be on screen");
        // Submitting from tall mode prints the full input.
        ed.handle_key(Key::CtrlEnter);
        let outcome = ed.take_result().unwrap();
        let text = match outcome {
            ReadOutcome::Submitted(t) => t,
            ReadOutcome::Aborted => panic!("expected submit"),
        };
        assert_eq!(text.lines().count(), 8);
    }

    #[test]
    fn ctrl_home_end_navigation() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.handle_paste("a\nb\nc");
        ed.handle_key(Key::CtrlHome);
        assert_eq!((ed.cursor_row, ed.cursor_col), (0, 0));
        ed.handle_key(Key::CtrlEnd);
        assert_eq!(ed.cursor_row, 2);
        assert_eq!(ed.cursor_col, 1);
    }

    #[test]
    fn boundary_cues_at_history_ends() {
        let (mut ed, _) = editor_with_history(80, 24, vec!["one"]);
        ed.start_read(true);
        for ch in "xy".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::Up(1)); // history: "one"
        ed.handle_key(Key::Up(1)); // top boundary: cursor to doc top
        assert_eq!((ed.cursor_row, ed.cursor_col), (0, 0));
        ed.handle_key(Key::Down(1)); // back down into "one"
        ed.handle_key(Key::Down(1)); // bottom: restore live draft, cursor at end
        assert_eq!(ed.text(), "xy");
        assert_eq!(ed.cursor_col, 2);
        ed.handle_key(Key::Down(1)); // at live draft bottom: boundary cue again
        assert_eq!(ed.cursor_col, 2);
    }

    #[test]
    fn wide_char_wrapping_keeps_cursor_sane() {
        // A wide char at a column boundary must not corrupt the layout:
        // render twice and make sure the editor doesn't panic and the cursor
        // math lands on a valid row.
        let (mut ed, _) = editor_with_history(6, 24, vec![]);
        ed.start_read(true);
        for ch in "a中b中c".chars() {
            ed.handle_key(Key::Char(ch));
        }
        ed.handle_key(Key::Left(1));
        assert_eq!(ed.text(), "a中b中c");
    }

    #[test]
    fn code_fence_enter_rules() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.handle_paste("```\ncode");
        // Unclosed fence: Enter inserts a newline rather than submitting.
        ed.handle_key(Key::Enter);
        assert!(ed.is_active());
        for ch in "```".chars() {
            ed.handle_key(Key::Char(ch));
        }
        // Closing fence line submits.
        ed.handle_key(Key::Enter);
        assert_eq!(ed.take_result().map(|o| match o {
            ReadOutcome::Submitted(t) => t,
            _ => String::new(),
        }), Some("```\ncode\n```".to_string()));
    }

    #[test]
    fn set_text_positions_cursor_at_end() {
        let (mut ed, _) = editor_with_history(80, 24, vec![]);
        ed.start_read(true);
        ed.set_text("a\nb");
        assert_eq!(ed.cursor_row, 1);
        assert_eq!(ed.cursor_col, 1);
    }

    #[test]
    fn display_slice_index_mid_grapheme() {
        let s = "ab😀cd";
        let i = display_slice_index(s, 3);
        assert_eq!(&s[..i], "ab");
        let i = display_slice_index(s, 4);
        assert_eq!(&s[..i], "ab😀");
    }
}
