// Display-width helpers: count terminal columns a string occupies, accounting
// for ANSI/OSC escape sequences, zero-width combining marks, and East-Asian
// wide/fullwidth characters. Port of kilo-readline src/width.ts, using the
// unicode-width crate for per-character widths.

use unicode_width::UnicodeWidthChar;

/// Strip ANSI/OSC escape sequences (SGR color sequences the renderer emits,
/// other CSI sequences, and OSC sequences terminated by BEL or ST).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() {
                if bytes[i + 1] == b'[' {
                    // CSI: params are digits and ';' (':' accepted too).
                    let mut j = i + 2;
                    while j < bytes.len()
                        && (bytes[j].is_ascii_digit() || bytes[j] == b';' || bytes[j] == b':')
                    {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                        i = j + 1;
                        continue;
                    }
                } else if bytes[i + 1] == b']' {
                    // OSC: terminated by BEL or ST (ESC \).
                    let mut j = i + 2;
                    let mut end = None;
                    while j < bytes.len() {
                        if bytes[j] == 0x07 {
                            end = Some(j);
                            break;
                        }
                        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                            end = Some(j + 1);
                            break;
                        }
                        j += 1;
                    }
                    if let Some(end) = end {
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        // Advance one full character (UTF-8 safe).
        let ch_end = text[i..]
            .chars()
            .next()
            .map(|c| i + c.len_utf8())
            .unwrap_or(text.len());
        out.push_str(&text[i..ch_end]);
        i = ch_end;
    }
    out
}

fn char_width(cp: char) -> usize {
    if cp == '\0' {
        return 0;
    }
    cp.width().unwrap_or(0)
}

/// Terminal column width of `s` after stripping escape sequences. Wide
/// (CJK/emoji) chars count 2, combining/format marks count 0.
pub fn display_width(s: &str) -> usize {
    strip_ansi(s).chars().map(char_width).sum()
}

/// Display width of the first `chars` chars of `s` (used for the cursor
/// column, where the cursor position is a char index into the line).
pub fn display_width_prefix(s: &str, chars: usize) -> usize {
    s.chars().take(chars).map(char_width).sum()
}

/// Byte index of the `char_idx`-th char of `line` (or line.len() when the
/// index is at/past the end).
pub fn byte_index_at_char(line: &str, char_idx: usize) -> usize {
    match line.char_indices().nth(char_idx) {
        Some((b, _)) => b,
        None => line.len(),
    }
}

/// Prefix of `s` (in char count) as a &str.
pub fn char_prefix(s: &str, char_idx: usize) -> &str {
    &s[..byte_index_at_char(s, char_idx)]
}

/// Suffix of `s` starting at char index `char_idx`.
pub fn char_suffix(s: &str, char_idx: usize) -> &str {
    &s[byte_index_at_char(s, char_idx)..]
}

/// Longest prefix of `s` whose display width is <= `cols`. Returns the byte
/// index into the *unstripped* string so ANSI codes between visible chars are
/// preserved when slicing (port of displaySlice; returns index not string).
/// Escape sequences take no columns and are skipped as units.
pub fn display_slice_index(s: &str, cols: usize) -> usize {
    let bytes = s.as_bytes();
    let mut w = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b';' || bytes[j] == b':')
            {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                i = j + 1;
                continue;
            }
        }
        let (ch_end, ch) = match s[i..].chars().next() {
            Some(c) => (i + c.len_utf8(), c),
            None => break,
        };
        let cw = char_width(ch);
        if w + cw > cols {
            return i;
        }
        w += cw;
        i = ch_end;
    }
    s.len()
}

/// Wrap a plain (no ANSI) string into chunks whose display widths fit
/// `first_width` for the first chunk and `rest_width` for subsequent chunks.
/// Wide chars are never split across rows: one that would straddle a boundary
/// moves to the next row.
pub fn wrap_plain(content: &str, first_width: usize, rest_width: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut limit = first_width;
    for ch in content.chars() {
        let cw = char_width(ch);
        if cw > 0 && cur_w + cw > limit && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_w = 0;
            limit = rest_width;
        }
        cur.push(ch);
        cur_w += cw;
    }
    chunks.push(cur);
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("\x1b[2m>\x1b[0mabc"), ">abc");
        assert_eq!(strip_ansi("a\x1b]0;title\x07b"), "ab");
        assert_eq!(display_width("\x1b[2m>\x1b[0m  "), 3);
    }

    #[test]
    fn wide_and_combining() {
        assert_eq!(display_width("abc"), 3);
        // CJK wide char counts 2.
        assert_eq!(display_width("a中b"), 4);
        // Combining mark is zero width.
        assert_eq!(display_width("e\u{0301}"), 1);
        // Emoji pictograph is wide.
        assert_eq!(display_width("💡"), 2);
    }

    #[test]
    fn display_slice_keeps_ansi_prefix() {
        let s = "\x1b[2m>\x1b[0mabc";
        let i = display_slice_index(s, 1);
        assert_eq!(&s[..i], "\x1b[2m>\x1b[0m");
        let i2 = display_slice_index(s, 2);
        assert_eq!(&s[..i2], "\x1b[2m>\x1b[0ma");
    }

    #[test]
    fn wrap_splits_on_width() {
        let chunks = wrap_plain("abcdefgh", 3, 4);
        assert_eq!(chunks, vec!["abc", "defg", "h"]);
        // Wide char straddling a boundary moves down whole ("中b" is width 3
        // and doesn't fit a 2-column row either, so each lands on its own).
        let chunks = wrap_plain("a中b", 2, 2);
        assert_eq!(chunks, vec!["a", "中", "b"]);
    }
}

/// Public re-export for the editor's cursor-math.
pub fn char_display_width(ch: char) -> usize {
    if ch == '\0' {
        return 0;
    }
    ch.width().unwrap_or(0)
}
