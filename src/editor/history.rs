// Pure helpers for the on-disk prompt history file. Port of
// kilo-readline src/history.ts.
//
// History file format: a sequence of NUL-separated blocks, each prefixed with
// \x01 (single-line entry) or \x02 (multiline entry). Trailing entries without
// a marker (legacy files) are tolerated and classified by whether they contain
// a newline.

/// Once the history file grows past this many bytes it is trimmed down to the
/// most recent entries so it does not grow without bound.
pub const HISTORY_MAX_BYTES: usize = 5 * 1024 * 1024;
pub const HISTORY_KEEP_ON_TRIM: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub text: String,
    pub is_multiline: bool,
}

pub fn serialize_history_entry(text: &str) -> String {
    let marker = if text.contains('\n') { "\x02" } else { "\x01" };
    format!("{marker}{text}\x00")
}

pub fn serialize_history(entries: &[HistoryEntry]) -> String {
    entries
        .iter()
        .map(|e| serialize_history_entry(&e.text))
        .collect()
}

/// Parse a history file's raw content into entries.
pub fn parse_history(content: &str) -> Vec<HistoryEntry> {
    let mut out = Vec::new();
    for block in content.split('\x00') {
        if block.trim().is_empty() {
            continue;
        }
        let mut chars = block.chars();
        let (text, is_multiline) = match chars.next() {
            Some('\x01') => (chars.as_str().to_string(), false),
            Some('\x02') => (chars.as_str().to_string(), true),
            _ => {
                let text = block.to_string();
                let ml = text.contains('\n');
                (text, ml)
            }
        };
        out.push(HistoryEntry { text, is_multiline });
    }
    out
}

/// Decide whether the history needs trimming given its current byte length,
/// and if so return the entries to keep (the last HISTORY_KEEP_ON_TRIM).
/// Returns None when no trim is needed.
pub fn trim_history_entries(entries: &[HistoryEntry], byte_length: usize) -> Option<Vec<HistoryEntry>> {
    if byte_length <= HISTORY_MAX_BYTES {
        return None;
    }
    if entries.len() <= HISTORY_KEEP_ON_TRIM {
        return None;
    }
    Some(
        entries
            .iter()
            .rev()
            .take(HISTORY_KEEP_ON_TRIM)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect(),
    )
}

/// Update a most-recently-used list: remove any existing occurrence of
/// `value`, move it to the front, and cap the list to `cap` entries
/// (most-recent first).
pub fn update_recent_list(existing: &[String], value: &str, cap: usize) -> Vec<String> {
    let v = value.trim();
    if v.is_empty() {
        return existing.to_vec();
    }
    let mut next: Vec<String> = vec![v.to_string()];
    for x in existing {
        if x != v {
            next.push(x.clone());
        }
    }
    if cap > 0 && next.len() > cap {
        next.truncate(cap);
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let entries = vec![
            HistoryEntry { text: "hello".into(), is_multiline: false },
            HistoryEntry { text: "a\nb".into(), is_multiline: true },
        ];
        let s = serialize_history(&entries);
        assert_eq!(parse_history(&s), entries);
    }

    #[test]
    fn legacy_entries_classified() {
        let parsed = parse_history("plain\nmulti\x00");
        assert_eq!(parsed.len(), 1);
        assert!(parsed[0].is_multiline);
        assert_eq!(parsed[0].text, "plain\nmulti");
    }

    #[test]
    fn trim_keeps_recent() {
        let entries: Vec<HistoryEntry> = (0..20)
            .map(|i| HistoryEntry { text: format!("e{i}"), is_multiline: false })
            .collect();
        let keep = trim_history_entries(&entries, HISTORY_MAX_BYTES + 1).unwrap();
        assert_eq!(keep.len(), HISTORY_KEEP_ON_TRIM);
        assert_eq!(keep[0].text, "e10");
        assert_eq!(keep.last().unwrap().text, "e19");
        assert!(trim_history_entries(&entries, 100).is_none());
    }

    #[test]
    fn recent_list_dedup_and_cap() {
        let existing = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let next = update_recent_list(&existing, "b", 3);
        assert_eq!(next, vec!["b", "a", "c"]);
        let next = update_recent_list(&existing, "z", 2);
        assert_eq!(next, vec!["z", "a"]);
    }
}
