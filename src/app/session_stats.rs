// Per-session usage accounting. Port of kilo-readline src/sessionStats.ts.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Main,
    #[allow(dead_code)]
    Summarizer,
}

#[derive(Debug, Clone)]
pub struct SessionUsage {
    pub role: SessionRole,
    /// Tokens currently in context (last reported).
    pub used: u64,
    /// Context window size (last reported).
    pub size: u64,
    /// Cumulative cost reported by the agent.
    pub cost_amount: f64,
    pub cost_currency: String,
}

#[derive(Debug, Clone, Default)]
pub struct UsageUpdate {
    pub used: Option<u64>,
    pub size: Option<u64>,
    pub cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
}

impl Default for SessionUsage {
    fn default() -> Self {
        Self {
            role: SessionRole::Main,
            used: 0,
            size: 0,
            cost_amount: 0.0,
            cost_currency: String::new(),
        }
    }
}

/// Merge a usage_update into an entry, creating it if needed. Entries are
/// never removed so compacted-away sessions still show up in /status.
pub fn apply_usage_update(
    map: &mut BTreeMap<String, SessionUsage>,
    session_id: &str,
    role: SessionRole,
    update: UsageUpdate,
) -> SessionUsage {
    let mut prev = map
        .get(session_id)
        .cloned()
        .unwrap_or(SessionUsage {
            role,
            ..Default::default()
        });
    prev.role = role;
    prev.used = update.used.unwrap_or(0);
    prev.size = update.size.unwrap_or(0);
    prev.cost_amount = update.cost_amount.unwrap_or(0.0);
    if let Some(cur) = update.cost_currency {
        if !cur.is_empty() {
            prev.cost_currency = cur;
        }
    }
    map.insert(session_id.to_string(), prev.clone());
    prev
}

pub struct SessionStatusRow {
    pub id: String,
    pub role: SessionRole,
    pub used: u64,
    pub size: u64,
    pub cost_amount: f64,
    pub cost_currency: String,
    pub current: bool,
}

/// Produce ordered rows plus a totals summary. `current_id` marks the active
/// main session.
pub fn summarize_sessions(
    map: &BTreeMap<String, SessionUsage>,
    current_id: Option<&str>,
) -> (Vec<SessionStatusRow>, f64) {
    let mut rows = Vec::new();
    let mut total_cost = 0.0;
    for (id, u) in map {
        rows.push(SessionStatusRow {
            id: id.clone(),
            role: u.role,
            used: u.used,
            size: u.size,
            cost_amount: u.cost_amount,
            cost_currency: u.cost_currency.clone(),
            current: Some(id.as_str()) == current_id && u.role == SessionRole::Main,
        });
        total_cost += u.cost_amount;
    }
    (rows, total_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_summarize() {
        let mut map = BTreeMap::new();
        apply_usage_update(
            &mut map,
            "s1",
            SessionRole::Main,
            UsageUpdate { used: Some(100), size: Some(1000), cost_amount: Some(0.5), cost_currency: Some("USD".into()) },
        );
        apply_usage_update(
            &mut map,
            "s2",
            SessionRole::Main,
            UsageUpdate { used: Some(10), size: None, cost_amount: Some(1.5), cost_currency: Some("USD".into()) },
        );
        let (rows, total) = summarize_sessions(&map, Some("s1"));
        assert_eq!(rows.len(), 2);
        assert!((total - 2.0).abs() < 1e-9);
        assert!(rows[0].current);
        assert!(!rows[1].current);
    }
}
