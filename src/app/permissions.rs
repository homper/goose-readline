// Client-side permission evaluation and "always allow" pattern derivation.
// Port of kilo-readline src/permissionEval.ts + src/permission.ts (the regex
// fallback path — no tree-sitter in the Rust port; the quote-aware tokenizer
// mirrors its behavior for common commands).
//
// Divergence from the TS version: config writes regenerate the JSON without
// preserving comments (jsonc-parser's comment-preserving modify is not
// available); parsing still accepts comments and trailing commas.

use regex::Regex;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// --- JSONC parsing (strip comments + trailing commas, then serde_json) ---

pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    let mut result = String::with_capacity(text.len());
    let mut in_string = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        let next = chars.get(i + 1).copied();
        if in_line_comment {
            if ch == '\n' {
                in_line_comment = false;
                result.push(ch);
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if ch == '*' && next == Some('/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_string {
            result.push(ch);
            if ch == '\\' {
                if let Some(n) = next {
                    result.push(n);
                    i += 2;
                    continue;
                }
            } else if ch == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_string = true;
            result.push(ch);
            i += 1;
            continue;
        }
        if ch == '/' && next == Some('/') {
            in_line_comment = true;
            i += 2;
            continue;
        }
        if ch == '/' && next == Some('*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        result.push(ch);
        i += 1;
    }
    // Strip trailing commas before } or ].
    let re = Regex::new(r",(\s*[}\]])").unwrap();
    let stripped = re.replace_all(&result, "$1").to_string();
    serde_json::from_str(&stripped).map_err(|e| e.to_string())
}

// --- Wildcard match (port of permissionEval.wildcardMatch) ---

pub fn wildcard_match(input: &str, pattern: &str) -> bool {
    let normalized = input.replace('\\', "/");
    let escaped_base = pattern.replace('\\', "/");
    // Escape regex specials except * and ?.
    let mut escaped = String::new();
    for c in escaped_base.chars() {
        match c {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            _ => escaped.push(c),
        }
    }
    let escaped = escaped.replace('*', ".*").replace('?', ".");
    // A trailing ` *` becomes optional so `npm install *` matches `npm install`.
    let escaped = if let Some(stripped) = escaped.strip_suffix(" .*") {
        format!("{stripped}( .*)?")
    } else {
        escaped
    };
    let re = match Regex::new(&format!("(?s)^{escaped}$")) {
        Ok(re) => re,
        Err(_) => return false,
    };
    re.is_match(&normalized)
}

// --- expand ~/ ~ $HOME ---

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn expand_pattern(pattern: &str) -> String {
    let home = home_dir();
    if let Some(rest) = pattern.strip_prefix("~/") {
        return format!("{}/{rest}", home.display());
    }
    if pattern == "~" {
        return home.display().to_string();
    }
    if let Some(rest) = pattern.strip_prefix("$HOME/") {
        return format!("{}/{rest}", home.display());
    }
    if let Some(rest) = pattern.strip_prefix("$HOME") {
        return format!("{}{rest}", home.display());
    }
    pattern.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    pub fn from_str(s: &str) -> Action {
        match s {
            "allow" => Action::Allow,
            "deny" | "reject" => Action::Deny,
            _ => Action::Ask,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Action::Allow => "allow",
            Action::Ask => "ask",
            Action::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub permission: String,
    pub pattern: String,
    pub action: Action,
}

/// Build a ruleset (insertion order preserved) from a kilo `permission`
/// object: scalar → one `{*, action}` rule; object → one rule per entry
/// (null entries are delete sentinels and are skipped).
pub fn from_config(permission: &Map<String, Value>) -> Vec<Rule> {
    let mut ruleset = Vec::new();
    for (key, value) in permission {
        if let Some(action) = value.as_str() {
            ruleset.push(Rule {
                permission: key.clone(),
                action: Action::from_str(action),
                pattern: "*".to_string(),
            });
            continue;
        }
        if let Some(map) = value.as_object() {
            for (pat, act) in map {
                if let Some(action) = act.as_str() {
                    ruleset.push(Rule {
                        permission: key.clone(),
                        pattern: expand_pattern(pat),
                        action: Action::from_str(action),
                    });
                }
            }
        }
    }
    ruleset
}

/// Canonical config key for a permission entry (legacy alias normalization).
fn canonical_key(key: &str) -> &str {
    if key == "external_dir" {
        "external_directory"
    } else {
        key
    }
}

/// Deep-merge a higher-precedence `permission` object into an accumulator,
/// reproducing kilo's concatenation + findLast semantics.
pub fn merge_permission_into(acc: &mut Map<String, Value>, perm: &Map<String, Value>) {
    for (raw_key, val) in perm {
        let key = canonical_key(raw_key).to_string();
        if val.is_null() {
            acc.remove(&key);
            continue;
        }
        if val.is_string() {
            acc.insert(key, val.clone());
            continue;
        }
        if let Some(incoming) = val.as_object() {
            let mut base: Map<String, Value> = match acc.get(&key) {
                Some(Value::Object(existing)) => existing.clone(),
                Some(Value::String(s)) => {
                    let mut m = Map::new();
                    m.insert("*".to_string(), Value::String(s.clone()));
                    m
                }
                _ => Map::new(),
            };
            for (p, a) in incoming {
                if a.is_null() {
                    base.remove(p);
                } else {
                    base.insert(p.clone(), a.clone());
                }
            }
            acc.insert(key, Value::Object(base));
        }
    }
}

// --- mtime-cached config file reader ---

struct FileCacheEntry {
    mtime_ms: i64,
    perm: Option<Map<String, Value>>,
}

static FILE_CACHE: Mutex<Option<HashMap<PathBuf, FileCacheEntry>>> = Mutex::new(None);

fn load_permission_file(p: &Path) -> Option<Map<String, Value>> {
    let mtime = match std::fs::metadata(p) {
        Ok(md) => md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64),
        Err(_) => {
            let mut cache = FILE_CACHE.lock().unwrap();
            cache.get_or_insert_with(HashMap::new).remove(p);
            return None;
        }
    };
    let mtime = mtime.unwrap_or(0);
    {
        let mut cache = FILE_CACHE.lock().unwrap();
        let cache = cache.get_or_insert_with(HashMap::new);
        if let Some(entry) = cache.get(p) {
            if entry.mtime_ms == mtime {
                return entry.perm.clone();
            }
        }
    }
    let perm = std::fs::read_to_string(p)
        .ok()
        .and_then(|text| parse_jsonc(&text).ok())
        .and_then(|obj| match obj {
            Value::Object(o) => match o.get("permission") {
                Some(Value::Object(pp)) => Some(pp.clone()),
                _ => None,
            },
            _ => None,
        });
    let mut cache = FILE_CACHE.lock().unwrap();
    cache.get_or_insert_with(HashMap::new).insert(
        p.to_path_buf(),
        FileCacheEntry { mtime_ms: mtime, perm: perm.clone() },
    );
    perm
}

/// Read + merge the `permission` objects from `global_path` (lowest) then
/// each project path in order (later wins), returning a single ruleset.
pub fn get_merged_permission(
    global_path: Option<&Path>,
    project_paths: &[PathBuf],
) -> Vec<Rule> {
    let mut acc = Map::new();
    if let Some(g) = global_path {
        if let Some(perm) = load_permission_file(g) {
            merge_permission_into(&mut acc, &perm);
        }
    }
    for p in project_paths {
        if let Some(perm) = load_permission_file(p) {
            merge_permission_into(&mut acc, &perm);
        }
    }
    from_config(&acc)
}

// --- bash pattern derivation (regex fallback port of permission.ts) ---

const CWD_CMDS: &[&str] = &[
    "cd", "chdir", "popd", "pushd", "push-location", "set-location",
];

fn arity_for(prefix: &str) -> Option<usize> {
    arity_lookup(prefix)
}

fn arity_prefix(tokens: &[String]) -> Vec<String> {
    for len in (1..=tokens.len()).rev() {
        let joined = tokens[..len].join(" ");
        if let Some(arity) = arity_for(&joined) {
            return tokens[..arity.min(tokens.len())].to_vec();
        }
    }
    if tokens.is_empty() {
        return Vec::new();
    }
    vec![tokens[0].clone()]
}

/// Quote-aware whitespace tokenizer.
fn tokenize(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(qc) = q {
            cur.push(ch);
            if ch == '\\' {
                if let Some(&n) = chars.get(i + 1) {
                    cur.push(n);
                    i += 1;
                }
            } else if ch == qc {
                q = None;
            }
        } else if ch == '"' || ch == '\'' {
            q = Some(ch);
            cur.push(ch);
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
        i += 1;
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Split a shell command into sub-commands on top-level `&&`, `||`, `;`,
/// `|`, and `&`, respecting quotes and parentheses.
fn split_shell_commands(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut depth = 0i32;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(qc) = q {
            cur.push(ch);
            if ch == '\\' {
                if let Some(&n) = chars.get(i + 1) {
                    cur.push(n);
                    i += 1;
                }
            } else if ch == qc {
                q = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            q = Some(ch);
            cur.push(ch);
            i += 1;
            continue;
        }
        if ch == '(' {
            depth += 1;
            cur.push(ch);
            i += 1;
            continue;
        }
        if ch == ')' {
            depth = depth.saturating_sub(1).max(0);
            cur.push(ch);
            i += 1;
            continue;
        }
        if depth == 0 {
            let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
            if two == "&&" || two == "||" {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                i += 2;
                continue;
            }
            if ch == '|' || ch == ';' {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
            if ch == '&' {
                let prev = cur.chars().last().unwrap_or(' ');
                let next_ch = chars.get(i + 1).copied().unwrap_or(' ');
                if prev == '>' || prev == '<' || next_ch == '>' {
                    cur.push(ch);
                    i += 1;
                    continue;
                }
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                i += 1;
                continue;
            }
        }
        cur.push(ch);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn is_assignment(token: &str) -> bool {
    let re = Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap();
    re.is_match(token)
}

/// The arity-prefix shape an "always allow" persists for a bash command.
pub fn bash_always_patterns(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for sub in split_shell_commands(command) {
        let tokens = tokenize(&sub);
        let mut start = 0usize;
        while start < tokens.len() && is_assignment(&tokens[start]) {
            start += 1;
        }
        if start >= tokens.len() {
            continue;
        }
        if CWD_CMDS.contains(&tokens[start].to_lowercase().as_str()) {
            continue;
        }
        let pref = arity_prefix(&tokens[start..]).join(" ");
        let pat = if pref.is_empty() { "*".to_string() } else { format!("{pref} *") };
        if !out.contains(&pat) {
            out.push(pat);
        }
    }
    out
}

/// The *call's target* patterns (the source text of each non-CWD
/// sub-command), which permission rules are matched against.
pub fn bash_call_patterns(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    for sub in split_shell_commands(command) {
        let tokens = tokenize(&sub);
        let mut start = 0usize;
        while start < tokens.len() && is_assignment(&tokens[start]) {
            start += 1;
        }
        if start >= tokens.len() {
            continue;
        }
        if CWD_CMDS.contains(&tokens[start].to_lowercase().as_str()) {
            continue;
        }
        let src = tokens[start..].join(" ");
        if !src.is_empty() && !out.contains(&src) {
            out.push(src);
        }
    }
    out
}

// --- per-tool call-pattern derivation ---

fn as_str(v: Option<&Value>) -> &str {
    v.and_then(Value::as_str).unwrap_or("")
}

fn rel_to_cwd(worktree: &Path, p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let abs = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        worktree.join(p)
    };
    let r = pathdiff_rel(worktree, &abs);
    if r.is_empty() {
        ".".to_string()
    } else {
        r.replace('\\', "/")
    }
}

fn pathdiff_rel(base: &Path, target: &Path) -> String {
    // Minimal path-relative implementation (both absolute or both relative).
    let base_c: Vec<_> = base.components().collect();
    let target_c: Vec<_> = target.components().collect();
    let mut common = 0usize;
    while common < base_c.len() && common < target_c.len() && base_c[common] == target_c[common] {
        common += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in common..base_c.len() {
        parts.push("..".to_string());
    }
    for c in &target_c[common..] {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }
    parts.join("/")
}

fn dir_glob(dir: &str) -> String {
    let dir = dir.replace('\\', "/");
    if dir.ends_with('/') {
        format!("{dir}*")
    } else {
        format!("{dir}/*")
    }
}

/// The patterns the evaluator checks for this call, per tool.
pub fn call_patterns(permission: &str, raw_input: Option<&Value>) -> Vec<String> {
    let t = permission.to_lowercase();
    let obj = raw_input.and_then(Value::as_object);

    let get = |k: &str| -> String { as_str(obj.map(|o| o.get(k)).flatten()).to_string() };

    match t.as_str() {
        "bash" | "shell" => {
            let pats = bash_call_patterns(&get("command"));
            if pats.is_empty() {
                vec!["*".to_string()]
            } else {
                pats
            }
        }
        "external_directory" => {
            let dir = {
                let d = get("parentDir");
                if d.is_empty() {
                    get("directory")
                } else {
                    d
                }
            };
            if !dir.is_empty() {
                return vec![dir_glob(&dir)];
            }
            let fp = first_non_empty(obj, &["filePath", "filepath", "path"]);
            if let Some(fp) = fp {
                let parent = Path::new(&fp).parent().map(|p| p.to_path_buf()).unwrap_or_default();
                return vec![dir_glob(&parent.display().to_string())];
            }
            if let Some(Value::Array(pats)) = obj.map(|o| o.get("patterns")).flatten() {
                let out: Vec<String> = pats
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect();
                if !out.is_empty() {
                    return out;
                }
            }
            vec!["*".to_string()]
        }
        "read" | "edit" | "write" | "apply_patch" => {
            let worktree = std::env::current_dir().unwrap_or_default();
            if let Some(Value::Array(files)) = obj.map(|o| o.get("files")).flatten() {
                let mut out: Vec<String> = Vec::new();
                for f in files {
                    let fo = f.as_object();
                    let fp = first_non_empty(fo, &["filePath", "filepath", "path"]);
                    if let Some(fp) = fp {
                        let rel = rel_to_cwd(&worktree, &fp);
                        if !out.contains(&rel) {
                            out.push(rel);
                        }
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
            let fp = first_non_empty(obj, &["filePath", "filepath", "path"]);
            if let Some(fp) = fp {
                let rel = rel_to_cwd(&worktree, &fp);
                if t == "read" {
                    let parent =
                        Path::new(&worktree.join(&fp)).parent().map(|p| p.display().to_string()).unwrap_or_default();
                    let parent_rel = rel_to_cwd(&worktree, &parent);
                    let mut out = Vec::new();
                    if !rel.is_empty() {
                        out.push(rel);
                    }
                    if !parent_rel.is_empty() {
                        out.push(parent_rel);
                    }
                    return out;
                }
                return vec![rel];
            }
            vec!["*".to_string()]
        }
        "glob" | "grep" | "warpgrep" | "codebase_search" => {
            let p = first_non_empty(obj, &["pattern", "query", "regex"]).unwrap_or_default();
            if p.is_empty() {
                vec!["*".to_string()]
            } else {
                vec![p]
            }
        }
        "skill" => {
            let n = get("name");
            if n.is_empty() {
                vec!["*".to_string()]
            } else {
                vec![n]
            }
        }
        "webfetch" => {
            let u = get("url");
            if u.is_empty() {
                vec!["*".to_string()]
            } else {
                vec![u]
            }
        }
        "websearch" => {
            let q = get("query");
            if q.is_empty() {
                vec!["*".to_string()]
            } else {
                vec![q]
            }
        }
        "repo_clone" => {
            let r = {
                let v = get("repository");
                if v.is_empty() {
                    get("url")
                } else {
                    v
                }
            };
            if r.is_empty() {
                vec!["*".to_string()]
            } else {
                vec![r]
            }
        }
        "recall" => {
            let d = {
                let v = get("directory");
                if v.is_empty() {
                    get("path")
                } else {
                    v
                }
            };
            if d.is_empty() {
                vec!["search".to_string()]
            } else {
                vec![d]
            }
        }
        _ => vec!["*".to_string()],
    }
}

fn first_non_empty(obj: Option<&Map<String, Value>>, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(v) = obj.map(|o| o.get(*k)).flatten() {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// findLast over the ruleset; default "ask" when no rule matches.
pub fn evaluate(permission: &str, pattern: &str, ruleset: &[Rule]) -> Rule {
    let mut matched: Option<&Rule> = None;
    for rule in ruleset {
        if wildcard_match(permission, &rule.permission) && wildcard_match(pattern, &rule.pattern) {
            matched = Some(rule);
        }
    }
    matched
        .cloned()
        .unwrap_or(Rule {
            permission: permission.to_string(),
            pattern: "*".to_string(),
            action: Action::Ask,
        })
}

pub struct Decision {
    pub action: Action,
    pub matched: Option<Rule>,
}

/// Evaluate every call pattern: any deny → deny; else any ask → ask; else
/// allow.
pub fn decide(permission: &str, raw_input: Option<&Value>, ruleset: &[Rule]) -> Decision {
    let patterns = call_patterns(permission, raw_input);
    let mut result = Action::Allow;
    let mut allow_rule: Option<Rule> = None;
    for p in &patterns {
        let rule = evaluate(permission, p, ruleset);
        if rule.action == Action::Deny {
            return Decision { action: Action::Deny, matched: Some(rule) };
        }
        if rule.action == Action::Ask {
            result = Action::Ask;
            continue;
        }
        if allow_rule.is_none() {
            allow_rule = Some(rule);
        }
    }
    Decision {
        action: result,
        matched: if result == Action::Allow { allow_rule } else { None },
    }
}

/// The patterns the agent will persist for an "always" reply to this tool
/// call, per tool kind.
pub fn always_patterns(tool_name: &str, raw_input: Option<&Value>) -> Vec<String> {
    let t = tool_name.to_lowercase();
    let obj = raw_input.and_then(Value::as_object);
    let get = |k: &str| -> String { as_str(obj.map(|o| o.get(k)).flatten()).to_string() };

    if t == "bash" || t == "shell" {
        let pats = bash_always_patterns(&get("command"));
        if pats.is_empty() {
            vec!["*".to_string()]
        } else {
            pats
        }
    } else if t == "external_directory" {
        let dir = {
            let d = get("parentDir");
            if d.is_empty() {
                get("directory")
            } else {
                d
            }
        };
        if !dir.is_empty() {
            return vec![dir_glob(&dir)];
        }
        if let Some(fp) = first_non_empty(obj, &["filePath", "filepath", "path"]) {
            let parent = Path::new(&fp).parent().map(|p| p.display().to_string()).unwrap_or_default();
            return vec![dir_glob(&parent)];
        }
        if let Some(Value::Array(pats)) = obj.map(|o| o.get("patterns")).flatten() {
            let out: Vec<String> = pats
                .iter()
                .filter_map(|p| p.as_str().map(str::to_string))
                .collect();
            if !out.is_empty() {
                return out;
            }
        }
        vec!["*".to_string()]
    } else if t == "skill" {
        let n = get("name");
        if n.is_empty() {
            vec!["*".to_string()]
        } else {
            vec![n]
        }
    } else if t == "recall" {
        let d = {
            let v = get("directory");
            if v.is_empty() {
                get("path")
            } else {
                v
            }
        };
        if d.is_empty() {
            vec!["search".to_string()]
        } else {
            vec![d]
        }
    } else if t == "repo_clone" {
        let r = {
            let v = get("repository");
            if v.is_empty() {
                get("url")
            } else {
                v
            }
        };
        if r.is_empty() {
            vec!["*".to_string()]
        } else {
            vec![r]
        }
    } else if t.starts_with("mcp_")
        || t == "read_mcp_resource"
        || t == "list_mcp_resources"
        || t == "list_mcp_resource_templates"
    {
        let server = get("server");
        if server.is_empty() {
            vec!["*".to_string()]
        } else {
            vec![format!("mcp:{server}:*")]
        }
    } else {
        vec!["*".to_string()]
    }
}

/// Merge one "always" rule into a kilo `permission` object, returning a NEW
/// object.
pub fn apply_permission_rule(
    perm: &Map<String, Value>,
    tool: &str,
    patterns: &[String],
    decision: &str,
) -> Map<String, Value> {
    let mut out = perm.clone();
    if patterns.len() == 1 && patterns[0] == "*" {
        out.insert(tool.to_string(), Value::String(decision.to_string()));
        return out;
    }
    let mut map: Map<String, Value> = match out.get(tool) {
        Some(Value::Object(existing)) => existing.clone(),
        _ => Map::new(),
    };
    for p in patterns {
        if !p.is_empty() {
            map.insert(p.clone(), Value::String(decision.to_string()));
        }
    }
    out.insert(tool.to_string(), Value::Object(map));
    out
}

/// Write one permission rule to a config file. If the file exists and parses
/// (comments tolerated), the rule is merged into the parsed object and
/// rewritten as pretty JSON; unparseable files are backed up to `.bak` and
/// recreated. Returns the path written.
pub fn patch_permission_rule(
    file_path: &Path,
    tool: &str,
    patterns: &[String],
    decision: &str,
    fresh_header: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut text: Option<String> = None;
    let mut had_file = false;
    if let Ok(raw) = std::fs::read_to_string(file_path) {
        had_file = true;
        text = Some(raw);
    }
    let mut next = String::new();
    match text {
        Some(raw) if !raw.trim().is_empty() => match parse_jsonc(&raw) {
            Ok(Value::Object(mut obj)) => {
                let perm = obj
                    .get("permission")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let patched = apply_permission_rule(&perm, tool, patterns, decision);
                obj.insert("permission".to_string(), Value::Object(patched));
                next = serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default();
                next.push('\n');
            }
            _ => {
                if had_file {
                    let _ = std::fs::copy(file_path, format!("{}.bak", file_path.display()));
                }
                eprintln!("⚠️  Config at {} was unparseable; backed up and rewriting.", file_path.display());
                let perm = apply_permission_rule(&Map::new(), tool, patterns, decision);
                let mut obj = Map::new();
                obj.insert("permission".to_string(), Value::Object(perm));
                let body = serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default();
                next = match fresh_header {
                    Some(h) => format!("{h}{body}\n"),
                    None => format!("{body}\n"),
                };
            }
        },
        _ => {
            let perm = apply_permission_rule(&Map::new(), tool, patterns, decision);
            let mut obj = Map::new();
            obj.insert("permission".to_string(), Value::Object(perm));
            let body = serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default();
            next = match fresh_header {
                Some(h) => format!("{h}{body}\n"),
                None => format!("{body}\n"),
            };
        }
    }
    std::fs::write(file_path, next).map_err(|e| e.to_string())?;
    Ok(file_path.to_path_buf())
}

include!("arity_table.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jsonc_parses_comments_and_trailing_commas() {
        let text = r#"{
          // comment
          "permission": {
            "bash": "allow", /* block */
            "read": { "src/*": "allow", },
          },
        }"#;
        let v = parse_jsonc(text).unwrap();
        assert_eq!(v["permission"]["bash"], "allow");
        assert_eq!(v["permission"]["read"]["src/*"], "allow");
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard_match("npm install", "npm install *"));
        assert!(wildcard_match("npm install lodash", "npm install *"));
        assert!(wildcard_match("anything", "*"));
        assert!(!wildcard_match("npm installx", "npm install"));
        assert!(wildcard_match("/tmp/foo/bar", "/tmp/*"));
        assert!(wildcard_match("aXb", "a?b"));
    }

    #[test]
    fn bash_patterns() {
        let pats = bash_always_patterns("FOO=bar npm install lodash && ls -la | grep foo");
        assert!(pats.contains(&"npm install *".to_string()));
        assert!(pats.contains(&"ls *".to_string()));
        let calls = bash_call_patterns("cd /tmp && rm -rf build");
        assert_eq!(calls, vec!["rm -rf build".to_string()], "cd is skipped");
        // Redirects don't become bogus sub-commands.
        let pats = bash_always_patterns("npm run build 2>&1 | head");
        assert!(pats.iter().all(|p| p != "1 *"), "no phantom `1 *`: {pats:?}");
    }

    #[test]
    fn evaluate_findlast_and_decide() {
        let ruleset = vec![
            Rule { permission: "bash".into(), pattern: "npm *".into(), action: Action::Allow },
            Rule { permission: "bash".into(), pattern: "npm install *".into(), action: Action::Ask },
        ];
        let input = json!({"command": "npm install lodash"});
        let d = decide("bash", Some(&input), &ruleset);
        assert_eq!(d.action, Action::Ask);
        let input2 = json!({"command": "npm run build"});
        let d2 = decide("bash", Some(&input2), &ruleset);
        assert_eq!(d2.action, Action::Allow);
        // Default ask.
        let d3 = decide("todowrite", None, &ruleset);
        assert_eq!(d3.action, Action::Ask);
    }

    #[test]
    fn always_patterns_shapes() {
        let p = always_patterns("bash", Some(&json!({"command": "cargo build --release"})));
        assert_eq!(p, vec!["cargo build *".to_string()]);
        let p = always_patterns("read", Some(&json!({"filePath": "/x/y"})));
        assert_eq!(p, vec!["*".to_string()]);
    }
}
