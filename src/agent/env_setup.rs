// Env parity with scripts/run-goose.sh: derive the GLM Coding Plan API key
// from kilo's auth.json ($XDG_DATA_HOME/kilo/auth.json, `zai-coding-plan`
// section) and point goose's zai_coding_plan provider at it. Nothing is
// written to disk; the key only travels via environment variables.

use serde_json::Value;
use std::path::PathBuf;

pub fn kilo_auth_path() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"));
    data_home.join("kilo").join("auth.json")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

/// Read the GLM Coding Plan API key from kilo's auth file. Checks the
/// `zai-coding-plan` and `zai` sections for a type "api" entry with a `key`.
pub fn read_zai_key_from_kilo_auth() -> Option<String> {
    let raw = std::fs::read_to_string(kilo_auth_path()).ok()?;
    let all: Value = serde_json::from_str(&raw).ok()?;
    for name in ["zai-coding-plan", "zai"] {
        if let Some(section) = all.get(name).and_then(Value::as_object) {
            if section.get("type").and_then(Value::as_str) == Some("api") {
                if let Some(key) = section.get("key").and_then(Value::as_str) {
                    if !key.is_empty() {
                        return Some(key.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Apply the run-goose.sh env defaults: GOOSE_PROVIDER=zai_coding_plan,
/// GOOSE_MODEL=glm-5.3, and ZAI_CODING_PLAN_API_KEY derived from kilo's
/// auth.json (only when not already set). Must run before the goose Config
/// is first touched.
pub fn apply_defaults() {
    if std::env::var("GOOSE_PROVIDER").is_err() {
        std::env::set_var("GOOSE_PROVIDER", "zai_coding_plan");
    }
    if std::env::var("GOOSE_MODEL").is_err() {
        std::env::set_var("GOOSE_MODEL", "glm-5.3");
    }
    if std::env::var("ZAI_CODING_PLAN_API_KEY").is_err() {
        if let Some(key) = read_zai_key_from_kilo_auth() {
            std::env::set_var("ZAI_CODING_PLAN_API_KEY", key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_zai_key_shape() {
        // Parse-only check against a synthetic file (no fs dependency):
        let raw = r#"{"zai-coding-plan": {"type": "api", "key": "k"},
                      "zai": {"type": "oauth", "access": "x"}}"#;
        let all: Value = serde_json::from_str(raw).unwrap();
        let mut found = None;
        for name in ["zai-coding-plan", "zai"] {
            if let Some(section) = all.get(name).and_then(Value::as_object) {
                if section.get("type").and_then(Value::as_str) == Some("api") {
                    if let Some(key) = section.get("key").and_then(Value::as_str) {
                        found = Some(key.to_string());
                        break;
                    }
                }
            }
        }
        assert_eq!(found.as_deref(), Some("k"));
    }
}
