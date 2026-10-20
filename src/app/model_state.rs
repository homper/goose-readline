// Kilo's own model-state file (~/.local/state/kilo/model.json): the saved
// default thinking level per model (`variant` map) and the global
// recently-used models list (`recent` array). Port of the read/write helpers
// in index.ts.

use crate::editor::history::update_recent_list;
use serde_json::{Map, Value};
use std::path::PathBuf;

pub const RECENT_MODELS_STORE_CAP: usize = 5;

#[derive(Debug, Default)]
pub struct ModelState {
    raw: Value,
    path: PathBuf,
}

fn model_state_path() -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|_| PathBuf::from(".local/state"))
        });
    state_home.join("kilo").join("model.json")
}

impl ModelState {
    pub fn load() -> Self {
        let path = model_state_path();
        let raw = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(|v: &Value| v.is_object())
            .unwrap_or_else(|| Value::Object(Map::new()));
        Self { raw, path }
    }

    fn object_mut(&mut self) -> &mut Map<String, Value> {
        if !self.raw.is_object() {
            self.raw = Value::Object(Map::new());
        }
        self.raw.as_object_mut().unwrap()
    }

    pub fn variant_for(&self, model_id: &str) -> Option<String> {
        self.raw
            .get("variant")
            .and_then(|v| v.get(model_id))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    pub fn save_model_variant(&mut self, model_id: &str, variant: Option<&str>) {
        let obj = self.object_mut();
        let variants = obj
            .entry("variant".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(map) = variants.as_object_mut() {
            match variant {
                Some(v) => {
                    map.insert(model_id.to_string(), Value::String(v.to_string()));
                }
                None => {
                    map.remove(model_id);
                }
            }
        }
        self.write();
    }

    pub fn read_recent_models(&self) -> Vec<String> {
        let Some(Value::Array(recent)) = self.raw.get("recent") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for r in recent {
            if let (Some(p), Some(m)) = (
                r.get("providerID").and_then(Value::as_str),
                r.get("modelID").and_then(Value::as_str),
            ) {
                let id = format!("{p}/{m}");
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Record a model as just-used: move it to the front of the recent list
    /// and persist (best-effort).
    pub fn record_recent_model(&mut self, value: &str) {
        let v = value.trim();
        if v.is_empty() {
            return;
        }
        let existing = self.read_recent_models();
        let next = update_recent_list(&existing, v, RECENT_MODELS_STORE_CAP);
        let refs: Vec<Value> = next
            .iter()
            .map(|id| match id.find('/') {
                Some(sep) if sep > 0 => serde_json::json!({
                    "providerID": id[..sep],
                    "modelID": id[sep + 1..],
                }),
                _ => serde_json::json!({ "providerID": id, "modelID": "" }),
            })
            .collect();
        let obj = self.object_mut();
        obj.insert("recent".to_string(), Value::Array(refs));
        self.write();
    }

    fn write(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(text) = serde_json::to_string_pretty(&self.raw) {
            let _ = std::fs::write(&self.path, format!("{text}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_and_variant_roundtrip() {
        let dir = tempfile_dir();
        std::env::set_var("XDG_STATE_HOME", &dir);
        let mut state = ModelState::load();
        state.record_recent_model("zai/glm-5.3");
        state.record_recent_model("anthropic/claude-sonnet-5");
        state.record_recent_model("zai/glm-5.3");
        let recent = state.read_recent_models();
        assert_eq!(
            recent,
            vec![
                "zai/glm-5.3".to_string(),
                "anthropic/claude-sonnet-5".to_string()
            ]
        );
        state.save_model_variant("zai/glm-5.3", Some("high"));
        assert_eq!(state.variant_for("zai/glm-5.3").as_deref(), Some("high"));
        state.save_model_variant("zai/glm-5.3", None);
        assert_eq!(state.variant_for("zai/glm-5.3"), None);
        std::env::remove_var("XDG_STATE_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("goose-readline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
