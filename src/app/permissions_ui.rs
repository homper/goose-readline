// Permission prompts and client-side fs request handling. Port of the
// handleRequestPermission flow from index.ts (local-allow auto-approve,
// config-derived deny, numbered menu with "always allow locally" and
// confirm-before-persist for "always" options).

use crate::agent::AgentCx;
use crate::app::permissions as perm;
use crate::app::term;
use crate::app::transcript::{ToolCallView, Transcript};
use crate::app::App;
use crate::editor::keys::{self, Key};
use agent_client_protocol::schema::v1::{
    PermissionOption, ReadTextFileRequest, ReadTextFileResponse, RequestPermissionRequest,
    RequestPermissionResponse, RequestPermissionOutcome, SelectedPermissionOutcome,
    WriteTextFileResponse,
};
use agent_client_protocol::Responder;
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::StreamExt;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

#[derive(Default)]
pub struct PermissionUiState {
    awaiting_user_input: bool,
    cancel_requested: bool,
}

impl PermissionUiState {
    pub fn new(paths: &crate::app::Paths) -> Self {
        // Ensure the project config dir exists for local permission writes.
        let _ = std::fs::create_dir_all(&paths.history_dir);
        Self::default()
    }

    pub fn begin_awaiting(&mut self) {
        self.awaiting_user_input = true;
        self.cancel_requested = false;
    }

    pub fn end_awaiting(&mut self) {
        self.awaiting_user_input = false;
    }

    pub fn awaiting_user_input(&self) -> bool {
        self.awaiting_user_input
    }

    pub fn request_cancel(&mut self) {
        self.cancel_requested = true;
    }

    pub fn take_cancel(&mut self) -> bool {
        std::mem::take(&mut self.cancel_requested)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOutcome {
    Answered,
    Cancelled,
}

fn option_kind(option: &PermissionOption) -> String {
    serde_json::to_value(&option.kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Answer a client-side fs/read_text_file request (workspace-rooted read
/// with optional line/limit slice).
pub fn respond_read_text_file(
    request: ReadTextFileRequest,
    responder: Responder<ReadTextFileResponse>,
) {
    let content = match std::fs::read_to_string(&request.path) {
        Ok(raw) => {
            let lines: Vec<&str> = raw.split('\n').collect();
            let start = request.line.filter(|l| *l > 1).map(|l| (l - 1) as usize).unwrap_or(0);
            let end = request
                .limit
                .filter(|l| *l > 0)
                .map(|l| start + l as usize)
                .unwrap_or(lines.len());
            lines[start.min(lines.len())..end.min(lines.len())].join("\n")
        }
        Err(err) => format!("Error reading {}: {err}", request.path.display()),
    };
    let _ = responder.respond(ReadTextFileResponse::new(content));
}

/// Config-key-style name for a tool (port of permissionToolName).
pub fn permission_tool_name(view: &ToolCallView, transcript: &Transcript) -> String {
    let raw = view.raw_input.as_ref().and_then(Value::as_object);
    if let Some(raw) = raw {
        if raw.get("parentDir").and_then(Value::as_str).is_some()
            || matches!(raw.get("directories"), Some(Value::Array(_)))
        {
            return "external_directory".to_string();
        }
    }
    let known = [
        "bash", "shell", "read", "edit", "write", "apply_patch", "grep", "glob", "webfetch",
        "websearch", "task", "skill", "recall", "todowrite",
    ];
    let id = view.tool_call_id.clone().unwrap_or_default();
    let original = transcript.observed_view(&id);
    for title in [
        original.and_then(|o| o.title.clone()),
        view.title.clone(),
    ] {
        if let Some(t) = title {
            let normalized = t.trim().to_lowercase();
            if known.contains(&normalized.as_str()) {
                return normalized;
            }
        }
    }

    let kind = view
        .kind
        .clone()
        .or_else(|| original.and_then(|o| o.kind.clone()))
        .unwrap_or_default()
        .to_lowercase();
    let has = |k: &str| raw.map(|r| r.get(k).and_then(Value::as_str).is_some()).unwrap_or(false);
    if kind == "execute" || has("command") || has("cmd") {
        return "bash".to_string();
    }
    if kind == "read" {
        return "read".to_string();
    }
    if kind == "edit" {
        if has("content") || has("contents") {
            return "write".to_string();
        }
        return "edit".to_string();
    }
    if kind == "fetch" {
        return "webfetch".to_string();
    }
    if kind == "search" {
        if has("query") {
            return "websearch".to_string();
        }
        return "grep".to_string();
    }

    if let Some(title) = &view.title {
        let t = title.trim();
        if !t.is_empty() && !t.contains('/') && !t.contains(char::is_whitespace) {
            return t.to_lowercase();
        }
    }
    "this_tool".to_string()
}

pub fn rule_tool_name(tool_name: &str) -> String {
    tool_name.to_lowercase().replace(char::is_whitespace, "_")
}

fn print_stdout(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.write_all(b"\n");
}

/// The full permission flow. Renders the tool context, applies config-derived
/// decisions, and shows the numbered menu when a manual choice is needed.
pub async fn handle_permission_request(
    app: &mut App,
    transcript: &mut Transcript,
    crossterm_events: &mut EventStream,
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
) -> PermissionOutcome {
    let respond_cancel = |responder: Responder<RequestPermissionResponse>| {
        let _ = responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ));
    };

    if app.permission_state.take_cancel() {
        respond_cancel(responder);
        return PermissionOutcome::Cancelled;
    }

    let view = ToolCallView {
        tool_call_id: Some(request.tool_call.tool_call_id.to_string()),
        title: request.tool_call.fields.title.clone(),
        kind: request
            .tool_call
            .fields
            .kind
            .map(|k| {
                serde_json::to_value(k)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "other".to_string())
            }),
        status: request
            .tool_call
            .fields
            .status
            .map(|s| {
                serde_json::to_value(s)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            }),
        locations: None,
        content: None,
        raw_input: request.tool_call.fields.raw_input.clone(),
        raw_output: None,
    };

    // Unknown/invalid tool calls are auto-cancelled (port of the head check).
    let title_l = view.title.clone().unwrap_or_default().to_lowercase();
    if title_l == "unknown" || title_l == "invalid" {
        respond_cancel(responder);
        return PermissionOutcome::Answered;
    }

    let tool_name = permission_tool_name(&view, transcript);
    let options = &request.options;
    let find_kind = |kind: &str| options.iter().find(|o| option_kind(o) == kind);

    // Config-derived decisions.
    let ruleset =
        perm::get_merged_permission(app.global_kilo_config.as_deref(), &app.project_kilo_configs);
    let decision = perm::decide(&tool_name, view.raw_input.as_ref(), &ruleset);

    if decision.action == perm::Action::Allow && find_kind("allow_always").is_some() {
        if let Some(allow_once) = find_kind("allow_once") {
            transcript.render_permission_tool_context(&view, false);
            let _ = responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    allow_once.option_id.clone(),
                )),
            ));
            return PermissionOutcome::Answered;
        }
    }
    if decision.action == perm::Action::Deny {
        if let Some(reject) = find_kind("reject_once").or_else(|| find_kind("reject_always")) {
            transcript.render_permission_tool_context(&view, false);
            let rule_desc = decision
                .matched
                .as_ref()
                .map(|r| format!("{} {}", rule_tool_name(&tool_name), r.pattern))
                .unwrap_or_else(|| rule_tool_name(&tool_name));
            print_stdout(&format!(
                "   {DIM}(denied by config: {rule_desc}){RESET}",
                DIM = crate::app::render::DIM,
                RESET = crate::app::render::RESET
            ));
            let _ = responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    reject.option_id.clone(),
                )),
            ));
            return PermissionOutcome::Answered;
        }
    }

    transcript.render_permission_tool_context(&view, true);
    print_stdout(&format!(
        "   {}🔐 Permission requested: {}{}",
        crate::app::render::YELLOW,
        rule_tool_name(&tool_name),
        crate::app::render::RESET
    ));

    let computed_always = perm::always_patterns(&tool_name, view.raw_input.as_ref());
    let allow_once_opt = find_kind("allow_once");

    // Build the menu: server options with the local "always allow locally"
    // entry inserted just before the first reject.
    enum MenuItem {
        Server(usize),
        Local,
    }
    let first_reject_idx = options
        .iter()
        .position(|o| matches!(option_kind(o).as_str(), "reject_once" | "reject_always"));
    let local_insert_at: Option<usize> = allow_once_opt.map(|_| {
        first_reject_idx.unwrap_or(options.len())
    });
    let mut menu_items: Vec<MenuItem> = Vec::new();
    for i in 0..options.len() {
        if Some(i) == local_insert_at {
            menu_items.push(MenuItem::Local);
        }
        menu_items.push(MenuItem::Server(i));
    }
    if local_insert_at == Some(options.len()) {
        menu_items.push(MenuItem::Local);
    }

    let local_pats: Vec<String> = allow_once_opt
        .map(|o| resolve_always_patterns(o, &computed_always))
        .unwrap_or_default();

    for (n, item) in menu_items.iter().enumerate() {
        match item {
            MenuItem::Local => {
                let clause = if !local_pats.is_empty() {
                    format!(" {DIM}({}){RESET}", local_pats.join(", "), DIM = crate::app::render::DIM, RESET = crate::app::render::RESET)
                } else {
                    String::new()
                };
                print_stdout(&format!(
                    "   {}. Always allow locally: {tool_name}{clause}",
                    n + 1
                ));
            }
            MenuItem::Server(idx) => {
                let opt = &options[*idx];
                let kind = option_kind(opt);
                let is_always =
                    kind == "allow_always" || kind == "reject_always";
                if !is_always {
                    print_stdout(&format!("   {}. {}", n + 1, opt.name));
                } else {
                    let pats = resolve_always_patterns(opt, &computed_always);
                    let clause = if !pats.is_empty() {
                        format!(" {DIM}({}){RESET}", pats.join(", "), DIM = crate::app::render::DIM, RESET = crate::app::render::RESET)
                    } else {
                        String::new()
                    };
                    print_stdout(&format!(
                        "   {}. {}: {tool_name}{clause}",
                        n + 1,
                        opt.name
                    ));
                }
            }
        }
    }

    // Choice loop.
    print_stdout("\nChoose an option (or 'c' to cancel): ");
    loop {
        if app.permission_state.take_cancel() {
            cancel_and_notify(app);
            respond_cancel(responder);
            return PermissionOutcome::Cancelled;
        }
        let Some(answer) = read_menu_line(app, crossterm_events, "0123456789cC").await else {
            cancel_and_notify(app);
            respond_cancel(responder);
            return PermissionOutcome::Cancelled;
        };
        let trimmed = answer.trim().to_lowercase();
        if trimmed == "c" || trimmed == "cancel" {
            print_stdout("");
            respond_cancel(responder);
            return PermissionOutcome::Answered;
        }
        if trimmed.is_empty() {
            print_stdout("\nChoose another option (or 'c' to cancel): ");
            continue;
        }
        let Ok(choice) = trimmed.parse::<usize>() else {
            print_stdout("Invalid option. Please try again.");
            print_stdout("Choose another option (or 'c' to cancel): ");
            continue;
        };
        if choice == 0 || choice > menu_items.len() {
            print_stdout("Invalid option. Please try again.");
            print_stdout("Choose another option (or 'c' to cancel): ");
            continue;
        }
        let item = &menu_items[choice - 1];
        match item {
            MenuItem::Local => {
                let rule_tool = rule_tool_name(&tool_name);
                let rules = if !local_pats.is_empty() {
                    local_pats
                        .iter()
                        .map(|p| format!("allow {rule_tool} {p}"))
                        .collect::<Vec<_>>()
                } else {
                    vec![format!("allow {rule_tool}")]
                };
                let local_path = app
                    .project_kilo_configs
                    .last()
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from(".kilo/kilo.jsonc"));
                print_stdout(&format!(
                    "\nSaves to {BOLD}{}{RESET}:",
                    crate::app::display_config_path(&local_path),
                    BOLD = crate::app::render::BOLD,
                    RESET = crate::app::render::RESET
                ));
                for r in &rules {
                    print_stdout(&format!("   {DIM}{r}{RESET}", DIM = crate::app::render::DIM, RESET = crate::app::render::RESET));
                }
                print_stdout("Confirm? (y/N): ");
                let Some(confirm) = read_menu_line(app, crossterm_events, "ynYN").await else {
                    cancel_and_notify(app);
                    respond_cancel(responder);
                    return PermissionOutcome::Cancelled;
                };
                if confirm.trim().to_lowercase() != "y" {
                    print_stdout("\nChoose another option (or 'c' to cancel): ");
                    continue;
                }
                if let Err(err) = perm::patch_permission_rule(
                    &local_path,
                    &rule_tool,
                    &local_pats,
                    "allow",
                    Some("// goose-readline local permissions — managed by the client.\n"),
                ) {
                    print_stdout(&format!(
                        "\n{}⚠️  Could not write local config: {err}{}",
                        crate::app::render::RED,
                        crate::app::render::RESET
                    ));
                    print_stdout("\nChoose another option (or 'c' to cancel): ");
                    continue;
                }
                print_stdout("");
                let allow_once = allow_once_opt.expect("local entry implies allow_once");
                let _ = responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        allow_once.option_id.clone(),
                    )),
                ));
                return PermissionOutcome::Answered;
            }
            MenuItem::Server(idx) => {
                let opt = &options[*idx];
                let kind = option_kind(opt);
                let is_always = kind == "allow_always" || kind == "reject_always";
                if is_always {
                    let verb = if kind == "allow_always" { "allow" } else { "reject" };
                    let config_path = app
                        .global_kilo_config
                        .clone()
                        .unwrap_or_else(default_global_config_path);
                    let pats = resolve_always_patterns(opt, &computed_always);
                    let rule_tool = rule_tool_name(&tool_name);
                    let rules = if !pats.is_empty() {
                        pats.iter()
                            .map(|p| format!("{verb} {rule_tool} {p}"))
                            .collect::<Vec<_>>()
                    } else {
                        vec![format!("{verb} {rule_tool}")]
                    };
                    print_stdout(&format!(
                        "\nSaves to {BOLD}{}{RESET}:",
                        crate::app::display_config_path(&config_path),
                        BOLD = crate::app::render::BOLD,
                        RESET = crate::app::render::RESET
                    ));
                    for r in &rules {
                        print_stdout(&format!("   {DIM}{r}{RESET}", DIM = crate::app::render::DIM, RESET = crate::app::render::RESET));
                    }
                    print_stdout("Confirm? (y/N): ");
                    let Some(confirm) = read_menu_line(app, crossterm_events, "ynYN").await else {
                        cancel_and_notify(app);
                        respond_cancel(responder);
                        return PermissionOutcome::Cancelled;
                    };
                    if confirm.trim().to_lowercase() != "y" {
                        print_stdout("\nChoose another option (or 'c' to cancel): ");
                        continue;
                    }
                    let decision = if kind == "allow_always" { "allow" } else { "reject" };
                    let _ = perm::patch_permission_rule(&config_path, &rule_tool, &pats, decision, None);
                }
                let once_kind = if kind == "allow_always" {
                    "allow_once"
                } else if kind == "reject_always" {
                    "reject_once"
                } else {
                    ""
                };
                let respond_id = if is_always {
                    find_kind(once_kind).map(|o| o.option_id.clone()).unwrap_or_else(|| opt.option_id.clone())
                } else {
                    opt.option_id.clone()
                };
                print_stdout("");
                let _ = responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(respond_id)),
                ));
                return PermissionOutcome::Answered;
            }
        }
    }
}

fn default_global_config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from(".config"))
        });
    base.join("kilo").join("kilo.jsonc")
}

fn resolve_always_patterns(opt: &PermissionOption, computed: &[String]) -> Vec<String> {
    let meta = opt
        .meta
        .as_ref()
        .and_then(|m| serde_json::to_value(m).ok())
        .and_then(|v| v.get("patterns").cloned());
    if let Some(Value::Array(pats)) = meta {
        let out: Vec<String> = pats
            .iter()
            .filter_map(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    computed.to_vec()
}

fn cancel_and_notify(app: &App) {
    let cx: &AgentCx = &app.cx;
    crate::agent::cancel_session(cx, &app.session_id);
}

/// Read one menu line: digits/letters + Enter; Ctrl+C aborts (returns None).
/// Agent events during the read are answered conservatively.
async fn read_menu_line(
    app: &mut App,
    crossterm_events: &mut EventStream,
    allow: &str,
) -> Option<String> {
    let mut reader = crate::app::single_line::SingleLineReader::new(Some(allow));
    loop {
        tokio::select! {
            ev = crossterm_events.next() => {
                let Some(Ok(ev)) = ev else { continue };
                if let CrosstermEvent::Key(k) = ev {
                    if k.kind == crossterm::event::KeyEventKind::Release { continue; }
                    let key = keys::translate(&k);
                    if key == Key::CtrlC {
                        return None;
                    }
                    if let Some(line) = reader.handle_key(key) {
                        return Some(line);
                    }
                }
            }
            agent_ev = app.events_rx.recv() => {
                use crate::agent::AgentEvent;
                match agent_ev {
                    Some(AgentEvent::ReadTextFile { request, responder }) => {
                        respond_read_text_file(request, responder);
                    }
                    Some(AgentEvent::WriteTextFile { request, responder }) => {
                        let _ = responder.respond(WriteTextFileResponse::new());
                        let _ = request;
                    }
                    Some(AgentEvent::Update { session_id, update }) => {
                        app.record_update_side_effects(&session_id, &update);
                    }
                    // A second permission request while the first menu is up:
                    // the agent is blocked on this one, so this only happens
                    // with parallel calls; cancel it (the user can re-run).
                    Some(AgentEvent::Permission { request, responder }) => {
                        let _ = request;
                        let _ = responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ));
                    }
                    None => return None,
                }
            }
        }
    }
}

// Keep unused imports referenced.
#[allow(unused)]
fn _unused(_: &dyn std::any::Any, _: Option<term::CrosstermTerm>) {}
