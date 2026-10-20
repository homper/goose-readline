// Pure helpers for /model's search view. Port of kilo-readline
// src/modelSearch.ts.

use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub value: String,
    pub name: String,
}

/// Split a model id/name into its constituent "words" (delimiters: "/",
/// "-", "_", ".", whitespace). Empty fragments are dropped.
pub fn extract_model_words(models: &[ModelOption]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for m in models {
        for src in [&m.value, &m.name] {
            for w in src.to_lowercase().split(|c: char| matches!(c, '/' | '-' | '_' | '.') || c.is_whitespace()) {
                if !w.is_empty() {
                    set.insert(w.to_string());
                }
            }
        }
    }
    set.into_iter().collect()
}

/// A token "looks like a full model path" when it already contains a
/// provider separator ("/").
pub fn is_full_path_token(token: &str) -> bool {
    token.contains('/')
}

/// Fuzzy subsequence matcher: true when every char of `query` appears in
/// `target` in order (case-insensitive). Score rewards compact, early,
/// boundary matches.
pub fn fuzzy_score(query: &str, target: &str) -> Option<f64> {
    let q = query.to_lowercase();
    let t = target.to_lowercase();
    if q.is_empty() {
        return Some(0.0);
    }
    let tq: Vec<char> = q.chars().collect();
    let tt: Vec<char> = t.chars().collect();
    let mut ti = 0usize;
    let mut prev_matched = false;
    let mut score = 0.0f64;
    for &ch in &tq {
        let mut found = false;
        while ti < tt.len() {
            if tt[ti] == ch {
                found = true;
                let boundary = ti == 0 || matches!(tt[ti - 1], '/' | '-' | '_' | '.' | ' ');
                score += if boundary {
                    10.0
                } else if prev_matched {
                    5.0
                } else {
                    1.0
                };
                prev_matched = true;
                ti += 1;
                break;
            }
            prev_matched = false;
            ti += 1;
        }
        if !found {
            return None;
        }
    }
    Some(score - (tt.len().saturating_sub(tq.len())) as f64 * 0.1)
}

/// Word-mode Tab completion candidates: words that start with the token
/// first (sorted by length then alpha), then fuzzy matches.
pub fn match_model_words(token: &str, words: &[String]) -> Vec<String> {
    let q = token.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut prefix: Vec<String> = Vec::new();
    let mut fuzzy: Vec<(String, f64)> = Vec::new();
    for w in words {
        if w.starts_with(&q) {
            prefix.push(w.clone());
        } else if let Some(s) = fuzzy_score(&q, w) {
            fuzzy.push((w.clone(), s));
        }
    }
    prefix.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    fuzzy.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
    prefix.into_iter().chain(fuzzy.into_iter().map(|(w, _)| w)).collect()
}

/// Full-path-mode Tab completion candidates: model values whose id starts
/// with the token (case-insensitive), shortest first.
pub fn match_model_paths(token: &str, models: &[ModelOption]) -> Vec<String> {
    let q = token.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<&ModelOption> = models
        .iter()
        .filter(|m| m.value.to_lowercase().starts_with(&q))
        .collect();
    hits.sort_by(|a, b| {
        a.value
            .len()
            .cmp(&b.value.len())
            .then_with(|| a.value.cmp(&b.value))
    });
    hits.into_iter().map(|m| m.value.clone()).collect()
}

/// Filter full models for Enter-time resolution. Splits the query on
/// whitespace and requires EVERY word to fuzzy-match (AND semantics). An
/// empty query returns the kilo/* models.
pub fn match_models(query: &str, models: &[ModelOption]) -> Vec<ModelOption> {
    let q_raw = query.trim().to_lowercase();
    if q_raw.is_empty() {
        return models
            .iter()
            .filter(|o| o.value.starts_with("kilo/"))
            .cloned()
            .collect();
    }
    if q_raw.starts_with("kilo/") || q_raw.starts_with("anthropic/") {
        let mut hits: Vec<ModelOption> = models
            .iter()
            .filter(|o| o.value.to_lowercase().starts_with(&q_raw))
            .cloned()
            .collect();
        hits.sort_by(|a, b| a.value.cmp(&b.value));
        return hits;
    }
    let words: Vec<&str> = q_raw.split_whitespace().collect();
    let mut scored: Vec<(ModelOption, f64)> = Vec::new();
    for o in models {
        let mut total = 0.0f64;
        let mut ok = true;
        for w in &words {
            let sv = fuzzy_score(w, &o.value);
            let sn = fuzzy_score(w, &o.name);
            let best = match (sv, sn) {
                (Some(v), Some(n)) => Some(v.max(n)),
                (Some(v), None) => Some(v),
                (None, Some(n)) => Some(n),
                (None, None) => None,
            };
            match best {
                Some(b) => total += b,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            scored.push((o.clone(), total));
        }
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap()
            .then_with(|| a.0.value.cmp(&b.0.value))
    });
    scored.into_iter().map(|(o, _)| o).collect()
}

/// True when `text` is exactly one full model id (case-insensitive).
pub fn exact_model_match(text: &str, models: &[ModelOption]) -> Option<ModelOption> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|o| o.value.to_lowercase() == t)
        .cloned()
}

// --- Thinking / reasoning (effort) level display ---

/// Canonical openrouter reasoning-effort names.
pub fn is_openrouter_effort_name(id: &str) -> bool {
    matches!(id.to_lowercase().as_str(), "none" | "low" | "medium" | "high")
}

/// Model-specific variant ids that are NOT openrouter names, mapped to the
/// openrouter canonical name they correspond to.
pub fn effort_alias_to_openrouter(id: &str) -> Option<&'static str> {
    match id.to_lowercase().as_str() {
        "max" => Some("high"),
        _ => None,
    }
}

/// Format the display name for a single effort/variant level.
pub fn effort_display_name(variant_id: &str, agent_name: &str) -> String {
    let id = variant_id.to_lowercase();
    if is_openrouter_effort_name(&id) {
        return agent_name.to_string();
    }
    match effort_alias_to_openrouter(&id) {
        Some(mapped) => format!("{agent_name} ({mapped})"),
        None => agent_name.to_string(),
    }
}

/// Format the thinking level shown in /status: resolve `current` to its
/// option name, then apply the openrouter-alias rule.
pub fn format_effort_display(current: Option<&str>, options: &[ModelOption]) -> String {
    if options.is_empty() {
        return "(none)".to_string();
    }
    let cur = match current {
        Some(c) if !c.is_empty() => c,
        _ => return "(none)".to_string(),
    };
    let name = options
        .iter()
        .find(|o| o.value == cur)
        .map(|o| o.name.clone())
        .unwrap_or_else(|| cur.to_string());
    effort_display_name(cur, &name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<ModelOption> {
        vec![
            ModelOption { value: "anthropic/claude-sonnet-5".into(), name: "Claude Sonnet 5".into() },
            ModelOption { value: "zai/glm-5.3".into(), name: "GLM 5.3".into() },
            ModelOption { value: "kilo/kilo-auto/free".into(), name: "Kilo Free".into() },
        ]
    }

    #[test]
    fn words_and_paths() {
        let words = extract_model_words(&opts());
        assert!(words.contains(&"sonnet".to_string()));
        assert!(words.contains(&"glm".to_string()));

        let paths = match_model_paths("zai/", &opts());
        assert_eq!(paths, vec!["zai/glm-5.3".to_string()]);

        let words_hits = match_model_words("son", &words);
        assert_eq!(words_hits.first().map(String::as_str), Some("sonnet"));
    }

    #[test]
    fn fuzzy_and_match() {
        assert!(fuzzy_score("cs", "claude-sonnet").unwrap() > 0.0);
        assert!(fuzzy_score("zzz", "claude").is_none());

        let m = match_models("claude sonnet", &opts());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].value, "anthropic/claude-sonnet-5");

        let blank = match_models("", &opts());
        assert_eq!(blank.len(), 1);
        assert_eq!(blank[0].value, "kilo/kilo-auto/free");

        let exact = exact_model_match("  ZAI/GLM-5.3  ", &opts());
        assert_eq!(exact.unwrap().value, "zai/glm-5.3");
    }

    #[test]
    fn effort_display() {
        let options = vec![
            ModelOption { value: "high".into(), name: "High".into() },
            ModelOption { value: "max".into(), name: "Max".into() },
        ];
        assert_eq!(format_effort_display(Some("high"), &options), "High");
        assert_eq!(format_effort_display(Some("max"), &options), "Max (high)");
        assert_eq!(format_effort_display(None, &options), "(none)");
    }
}

// --- configOptions extraction (port of extractModelSelect/extractEffortSelect) ---

/// A select option extracted from a session response's `configOptions`.
pub struct SelectExtract {
    pub current: Option<String>,
    pub options: Vec<ModelOption>,
}

fn extract_select(
    config_options: Option<&serde_json::Value>,
    id_match: impl Fn(&str) -> bool,
) -> Option<SelectExtract> {
    let arr = config_options?.as_array()?;
    for opt in arr {
        let Some(o) = opt.as_object() else { continue };
        if o.get("type").and_then(|v| v.as_str()) != Some("select") {
            continue;
        }
        let id = o.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !id_match(id) {
            continue;
        }
        let current = o
            .get("currentValue")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut options = Vec::new();
        if let Some(serde_json::Value::Array(opts)) = o.get("options") {
            for item in opts {
                let Some(it) = item.as_object() else { continue };
                // Flat option: { value, name }
                if let Some(value) = it.get("value").and_then(|v| v.as_str()) {
                    if it.contains_key("name") {
                        options.push(ModelOption {
                            value: value.to_string(),
                            name: it
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or(value)
                                .to_string(),
                        });
                        continue;
                    }
                }
                // Grouped: { group, name, options: [...] }
                if let Some(serde_json::Value::Array(sub)) = it.get("options") {
                    for s in sub {
                        let Some(sv) = s.as_object() else { continue };
                        if let Some(value) = sv.get("value").and_then(|v| v.as_str()) {
                            options.push(ModelOption {
                                value: value.to_string(),
                                name: sv
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(value)
                                    .to_string(),
                            });
                        }
                    }
                }
            }
        }
        return Some(SelectExtract { current, options });
    }
    None
}

/// Extract the model select (id == "model") from a session response's
/// configOptions.
pub fn extract_model_select(config_options: Option<&serde_json::Value>) -> Option<SelectExtract> {
    extract_select(config_options, |id| id == "model")
}

/// Extract the thinking/effort select (id "effort", fallback "thinking"/
/// "reasoning") from a session response's configOptions.
pub fn extract_effort_select(config_options: Option<&serde_json::Value>) -> Option<SelectExtract> {
    extract_select(config_options, |id| {
        id == "effort" || id == "thinking" || id == "reasoning"
    })
}
