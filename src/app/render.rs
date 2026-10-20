// Transcript rendering helpers: ANSI color map, lightweight markdown line
// formatting, pipe-table layout, and unified diffs. Port of the pure
// rendering helpers from kilo-readline src/index.ts.

use crate::editor::wrap::display_width;
use regex::Regex;

pub const RESET: &str = "\x1b[0m";
pub const DIM: &str = "\x1b[2m";
pub const BOLD: &str = "\x1b[1m";
pub const ITALIC: &str = "\x1b[3m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const REVERSE: &str = "\x1b[7m";
pub const RED: &str = "\x1b[31m";
pub const GREEN: &str = "\x1b[32m";
pub const YELLOW: &str = "\x1b[33m";
pub const BLUE: &str = "\x1b[34m";
pub const CYAN: &str = "\x1b[36m";
pub const GRAY: &str = "\x1b[90m";

pub fn colorize(text: &str, code: &str) -> String {
    format!("{code}{text}{RESET}")
}

pub fn visible_width(text: &str) -> usize {
    display_width(text)
}

pub fn pad_visible(s: &str, width: usize) -> String {
    let current = display_width(s);
    if current >= width {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(width - current))
}

/// Apply lightweight inline markdown to a single line: headers render bold,
/// **bold** / __bold__, *italic* and _underline_ (word-boundary rules), and
/// `code spans` render in reverse video (protected from the other rules).
pub fn format_markdown_line(line: &str) -> String {
    let header_re = Regex::new(r"^(#{1,6})\s+(.*)").unwrap();
    if header_re.is_match(line) {
        return format!("{BOLD}{line}{RESET}");
    }

    // Protect inline code spans: extract, then restore after formatting.
    let mut code_spans: Vec<String> = Vec::new();
    let protected = protect_code_spans(line, &mut code_spans);

    let mut result = apply_bold(&protected);
    result = apply_italic(&result);
    result = apply_underline(&result);
    restore_code_spans(&result, &code_spans)
}

/// Replace `span` occurrences with \u{0}idx\u{0} placeholders.
fn protect_code_spans(line: &str, spans: &mut Vec<String>) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '`' {
            // Find the closing backtick.
            if let Some(close) = chars[i + 1..].iter().position(|&c| c == '`') {
                let span: String = chars[i + 1..i + 1 + close].iter().collect();
                if !span.is_empty() {
                    spans.push(span);
                    out.push('\u{0}');
                    out.push_str(&spans.len().to_string());
                    out.push('\u{0}');
                    i = i + close + 2;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn restore_code_spans(line: &str, spans: &[String]) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\u{0}' {
            let mut j = i + 1;
            let mut digits = String::new();
            while j < chars.len() && chars[j].is_ascii_digit() {
                digits.push(chars[j]);
                j += 1;
            }
            if j < chars.len() && chars[j] == '\u{0}' && !digits.is_empty() {
                if let Ok(idx) = digits.parse::<usize>() {
                    if idx > 0 && idx <= spans.len() {
                        out.push_str(&format!("{REVERSE}{}{RESET}", spans[idx - 1]));
                        i = j + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn apply_bold(line: &str) -> String {
    // **bold** (lazy) then __bold__
    let re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let once = re.replace_all(line, format!("{BOLD}$1{RESET}").as_str()).to_string();
    let re2 = Regex::new(r"__([^_].*?)__").unwrap();
    re2.replace_all(&once, format!("{BOLD}$1{RESET}").as_str()).to_string()
}

/// *italic*: a `*` that is not adjacent to another `*`, with non-empty
/// content that doesn't start/end with `*`.
fn apply_italic(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '*' {
            let prev_ok = i == 0 || chars[i - 1] != '*';
            if prev_ok {
                // Find closing single *.
                if let Some(close) = chars[i + 1..].iter().position(|&c| c == '*') {
                    let end = i + 1 + close;
                    let content: String = chars[i + 1..end].iter().collect();
                    let next_ok = end + 1 >= chars.len() || chars[end + 1] != '*';
                    let content_ok =
                        !content.is_empty() && !content.starts_with('*') && !content.ends_with('*');
                    if content_ok && next_ok && close > 0 {
                        out.push_str(&format!("{ITALIC}{content}{RESET}"));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// _underline_: the opening `_` must start a word (not preceded by a letter
/// or digit), avoiding snake_case identifiers.
fn apply_underline(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '_' {
            let prev_ok = i == 0 || !(chars[i - 1].is_ascii_alphanumeric());
            let next_not_underscore = i + 1 < chars.len() && chars[i + 1] != '_';
            if prev_ok && next_not_underscore {
                if let Some(close) = chars[i + 1..].iter().position(|&c| c == '_') {
                    let end = i + 1 + close;
                    let content: String = chars[i + 1..end].iter().collect();
                    let content_ok =
                        !content.is_empty() && !content.starts_with('_') && !content.ends_with('_');
                    let after_ok = end + 1 >= chars.len() || chars[end + 1] != '_';
                    if content_ok && after_ok {
                        out.push_str(&format!("{UNDERLINE}{content}{RESET}"));
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.ends_with('|')
}

pub fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    is_table_row(t)
        && t[1..t.len() - 1]
            .chars()
            .all(|c| c.is_whitespace() || c == '-' || c == ':')
}

pub fn parse_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = &t[1..t.len() - 1];
    inner
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

pub fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur = word.to_string();
            continue;
        }
        let candidate = format!("{cur} {word}");
        if display_width(&candidate) <= max_width {
            cur = candidate;
        } else {
            lines.push(cur);
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Render markdown pipe-table rows into a bordered ASCII table with column
/// sizing and cell wrapping. Port of renderTable from index.ts.
pub fn render_table(rows: &[String], term_width: usize) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let parsed: Vec<Vec<String>> = rows.iter().map(|r| parse_table_row(r)).collect();
    let sep_idx = rows.iter().position(|r| is_table_separator(r));
    let has_header = matches!(sep_idx, Some(idx) if idx > 0);

    let (header_rows, body_rows): (&[Vec<String>], &[Vec<String>]) = match sep_idx {
        Some(idx) if idx > 0 => (&parsed[..idx], &parsed[idx + 1..]),
        _ => (&[], &parsed[..]),
    };

    let col_count = parsed.iter().map(|r| r.len()).max().unwrap_or(1).max(1);

    let pad_row = |r: &[String]| -> Vec<String> {
        let mut padded = r.to_vec();
        while padded.len() < col_count {
            padded.push(String::new());
        }
        padded
    };

    let all_rows: Vec<Vec<String>> = header_rows.iter().chain(body_rows.iter()).cloned().collect();

    let mut cols: Vec<Vec<String>> = vec![Vec::new(); col_count];
    for r in &all_rows {
        for (i, c) in pad_row(r).iter().enumerate() {
            cols[i].push(c.clone());
        }
    }

    let overhead = 3 * col_count + 1;
    let available = term_width.saturating_sub(overhead);

    let natural_widths: Vec<usize> = cols
        .iter()
        .map(|c| c.iter().map(|v| display_width(v)).max().unwrap_or(3).max(3))
        .collect();
    let total_natural: usize = natural_widths.iter().sum();

    let widths;
    let mut needs_wrap = false;

    if available == 0 || total_natural <= available {
        widths = natural_widths.clone();
    } else {
        needs_wrap = true;
        let min_widths: Vec<usize> = cols
            .iter()
            .enumerate()
            .map(|(ci, c)| {
                let mut min = 3usize;
                for cell in c {
                    for word in cell.split_whitespace() {
                        min = min.max(display_width(word));
                    }
                }
                min.min(natural_widths[ci])
            })
            .collect();

        let total_min: usize = min_widths.iter().sum();
        if total_min >= available {
            widths = natural_widths.clone();
            needs_wrap = false;
        } else {
            let slack = available - total_min;
            let extra: Vec<usize> = natural_widths
                .iter()
                .enumerate()
                .map(|(i, w)| w.saturating_sub(min_widths[i]))
                .collect();
            let total_extra: usize = extra.iter().sum();
            if total_extra == 0 {
                widths = natural_widths.clone();
                needs_wrap = false;
            } else {
                let mut w: Vec<usize> = min_widths
                    .iter()
                    .enumerate()
                    .map(|(i, min)| min + (slack * extra[i] + total_extra / 2) / total_extra)
                    .collect();
                let mut diff = available as isize - w.iter().sum::<usize>() as isize;
                let mut order: Vec<usize> = (0..col_count).collect();
                order.sort_by_key(|&i| std::cmp::Reverse(natural_widths[i]));
                let mut oi = 0usize;
                while diff > 0 {
                    w[order[oi % col_count]] += 1;
                    diff -= 1;
                    oi += 1;
                }
                let mut oi = 0usize;
                while diff < 0 && oi < col_count * 16 {
                    let ci = order[oi % col_count];
                    if w[ci] > 3 {
                        w[ci] -= 1;
                        diff += 1;
                    }
                    oi += 1;
                }
                widths = w;
            }
        }
    }

    let sep = format!(
        "+{}+",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );

    let mut out = String::new();

    let render_header_row = |out: &mut String, r: &[String]| {
        let padded: Vec<String> = pad_row(r).iter().map(|c| format_markdown_line(c)).collect();
        out.push_str(&sep);
        out.push('\n');
        let cells: Vec<String> = padded
            .iter()
            .enumerate()
            .map(|(i, c)| pad_visible(c, widths[i]))
            .collect();
        out.push_str(&format!(
            "| {} |\n",
            cells.join(&format!(" {RESET}|{BOLD} "))
        ));
    };

    if !needs_wrap {
        for r in header_rows {
            render_header_row(&mut out, r);
        }
        if !header_rows.is_empty() {
            out.push_str(&sep);
            out.push('\n');
        }
        for r in body_rows {
            let padded: Vec<String> = pad_row(r).iter().map(|c| format_markdown_line(c)).collect();
            let cells: Vec<String> = padded
                .iter()
                .enumerate()
                .map(|(i, c)| pad_visible(c, widths[i]))
                .collect();
            out.push_str(&format!(
                "| {} {DIM}|{RESET}\n",
                cells.join(&format!(" {DIM}|{RESET} "))
            ));
        }
        if !body_rows.is_empty() {
            out.push_str(&sep);
            out.push('\n');
        }
    } else {
        for r in header_rows {
            render_header_row(&mut out, r);
        }
        if !header_rows.is_empty() {
            out.push_str(&sep);
            out.push('\n');
        }

        let wrapped_body: Vec<Vec<Vec<String>>> = body_rows
            .iter()
            .map(|row| {
                pad_row(row)
                    .iter()
                    .enumerate()
                    .map(|(ci, cell)| {
                        wrap_text(cell, widths[ci])
                            .iter()
                            .map(|l| format_markdown_line(l))
                            .collect()
                    })
                    .collect()
            })
            .collect();

        for (ri, wrapped) in wrapped_body.iter().enumerate() {
            if !(ri == 0 && has_header) {
                out.push_str(&sep);
                out.push('\n');
            }
            let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(0);
            for si in 0..height {
                let mut row_out = String::from("| ");
                for ci in 0..col_count {
                    let line = if si < wrapped[ci].len() {
                        &wrapped[ci][si]
                    } else {
                        ""
                    };
                    row_out.push_str(&pad_visible(line, widths[ci]));
                    if ci < col_count - 1 {
                        row_out.push_str(&format!(" {DIM}|{RESET} "));
                    } else {
                        row_out.push_str(&format!(" {DIM}|{RESET}"));
                    }
                }
                row_out.push('\n');
                out.push_str(&row_out);
            }
        }
        if !body_rows.is_empty() {
            out.push_str(&sep);
            out.push('\n');
        }
    }

    out
}

/// Accumulates consecutive markdown table rows and flushes them through
/// render_table when a non-table line arrives.
#[derive(Default)]
pub struct TableAccumulator {
    rows: Vec<String>,
}

impl TableAccumulator {
    /// Feed a line; returns Some(rendered table) when this line ends a table.
    pub fn push(&mut self, line: &str, term_width: usize) -> Option<String> {
        if is_table_row(line) {
            self.rows.push(line.to_string());
            return None;
        }
        self.flush(term_width)
    }

    pub fn flush(&mut self, term_width: usize) -> Option<String> {
        if self.rows.is_empty() {
            return None;
        }
        let table = render_table(&self.rows, term_width);
        self.rows.clear();
        Some(table)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Removed,
    Added,
}

pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

/// LCS line diff (port of diffLines from index.ts).
pub fn diff_lines(old_lines: &[&str], new_lines: &[&str]) -> Vec<DiffLine> {
    let m = old_lines.len();
    let n = new_lines.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut result = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if old_lines[i] == new_lines[j] {
            result.push(DiffLine { kind: DiffLineKind::Context, text: old_lines[i].to_string() });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            result.push(DiffLine { kind: DiffLineKind::Removed, text: old_lines[i].to_string() });
            i += 1;
        } else {
            result.push(DiffLine { kind: DiffLineKind::Added, text: new_lines[j].to_string() });
            j += 1;
        }
    }
    while i < m {
        result.push(DiffLine { kind: DiffLineKind::Removed, text: old_lines[i].to_string() });
        i += 1;
    }
    while j < n {
        result.push(DiffLine { kind: DiffLineKind::Added, text: new_lines[j].to_string() });
        j += 1;
    }
    result
}

/// Cap a single over-long line, preserving both edges.
pub const MAX_LINE_CHARS: usize = 1000;

pub fn trim_line(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let edge = (MAX_LINE_CHARS - 40) / 2;
    let omitted = chars.len() - edge * 2;
    let head: String = chars[..edge].iter().collect();
    let tail: String = chars[chars.len() - edge..].iter().collect();
    format!("{head} ... ({omitted} chars truncated) ... {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_bold_italic_code() {
        assert_eq!(format_markdown_line("**bold**"), format!("{BOLD}bold{RESET}"));
        assert_eq!(format_markdown_line("*it*"), format!("{ITALIC}it{RESET}"));
        assert_eq!(format_markdown_line("a_b_c"), "a_b_c", "snake_case untouched");
        assert_eq!(
            format_markdown_line("some _under_ text"),
            format!("some {UNDERLINE}under{RESET} text")
        );
        // Code spans protect markdown inside.
        let out = format_markdown_line("`**not bold**`");
        assert!(out.contains(REVERSE) && !out.contains(BOLD));
    }

    #[test]
    fn table_render() {
        let rows = vec![
            "| a | b |".to_string(),
            "|---|---|".to_string(),
            "| 1 | 2 |".to_string(),
        ];
        let out = render_table(&rows, 80);
        assert!(out.contains("+-----+-----+"), "sep: {out}");
        assert!(out.contains("| a"));
        assert!(out.contains("| 1"));
    }

    #[test]
    fn diff_lcs() {
        let old = vec!["a", "b", "c"];
        let new = vec!["a", "x", "c"];
        let d = diff_lines(&old, &new);
        let kinds: Vec<DiffLineKind> = d.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffLineKind::Context,
                DiffLineKind::Removed,
                DiffLineKind::Added,
                DiffLineKind::Context
            ]
        );
    }

    #[test]
    fn trim_long_line() {
        let long = "x".repeat(2000);
        let t = trim_line(&long);
        assert!(t.contains("chars truncated"));
        assert!(display_width(&t) < 1100);
    }
}
