// The application: terminal setup, the crossterm/agent event loop, prompt
// reading, slash commands, the permission flow, session lifecycle, and
// status/usage rendering. Port of kilo-readline src/index.ts against the
// in-process goose ACP agent.

pub mod commands;
pub mod model_state;
pub mod permissions;
pub mod permissions_ui;
pub mod render;
pub mod session_stats;
pub mod single_line;
pub mod term;
pub mod transcript;
pub mod usage;

use crate::agent::{self, AgentCx, AgentEvent, Transport};
use crate::editor::history::{self, HistoryEntry};
use crate::editor::keys::{self, Key};
use crate::editor::{Completer, RawInput, ReadOutcome};
use anyhow::{anyhow, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream};
use futures::StreamExt;
use model_state::ModelState;
use permissions_ui::PermissionUiState;
use single_line::SingleLineReader;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use transcript::Transcript;
use usage::{CodexRemaining, ZaiRemaining};

pub mod model_search;
use crate::app::model_search::ModelOption;

pub struct Paths {
    pub history_dir: PathBuf,
    pub history_file: PathBuf,
    pub last_session_file: PathBuf,
}

impl Paths {
    pub fn new() -> Self {
        let history_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".kilo");
        let history_file = history_dir.join("history");
        let last_session_file = history_dir.join("last_session");
        Self { history_dir, history_file, last_session_file }
    }
}

pub struct HistoryStore {
    pub entries: Arc<Mutex<Vec<HistoryEntry>>>,
    file: PathBuf,
}

impl HistoryStore {
    pub fn load(paths: &Paths) -> Self {
        let _ = std::fs::create_dir_all(&paths.history_dir);
        let entries = std::fs::read_to_string(&paths.history_file)
            .map(|c| history::parse_history(&c))
            .unwrap_or_default();
        Self {
            entries: Arc::new(Mutex::new(entries)),
            file: paths.history_file.clone(),
        }
    }

    /// Append a prompt, deduping against the most recent entry; trims the
    /// file when it grows past the size limit.
    pub fn append(&self, text: &str) {
        {
            let mut entries = self.entries.lock().unwrap();
            if entries
                .last()
                .map(|e| e.text == text)
                .unwrap_or(false)
            {
                return;
            }
            let entry = history::serialize_history_entry(text);
            if std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.file)
                .and_then(|mut f| f.write_all(entry.as_bytes()))
                .is_err()
            {
                return;
            }
            entries.push(HistoryEntry {
                text: text.to_string(),
                is_multiline: text.contains('\n'),
            });
        }
        self.trim();
    }

    fn trim(&self) {
        let entries = self.entries.lock().unwrap();
        let byte_len = std::fs::metadata(&self.file).map(|m| m.len() as usize).unwrap_or(0);
        if let Some(keep) = history::trim_history_entries(&entries, byte_len) {
            let _ = std::fs::write(&self.file, history::serialize_history(&keep));
        }
    }
}

/// Where terminal input goes right now (port of setInputTarget).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTarget {
    Main,
    Confirm,
    Discard,
}

pub struct App {
    pub cx: AgentCx,
    pub events_rx: mpsc::UnboundedReceiver<AgentEvent>,
    pub editor: RawInput,
    pub history: HistoryStore,
    pub paths: Paths,

    pub session_id: String,
    pub session_names: BTreeMap<String, String>,
    pub agent_caps: serde_json::Value,
    pub active_session_provider: Option<String>,
    pub session_provider_options: Option<Vec<ModelOption>>,
    pub active_session_model: Option<String>,
    pub session_model_options: Option<Vec<ModelOption>>,
    pub active_effort: Option<String>,
    pub session_effort_options: Option<Vec<ModelOption>>,

    pub session_usage: BTreeMap<String, session_stats::SessionUsage>,
    pub main_usage: session_stats::SessionUsage,
    pub codex_remaining: CodexRemaining,
    pub zai_remaining: ZaiRemaining,

    pub model_state: ModelState,

    pub think_short_gap_ms: u64,
    pub think_long_gap_ms: u64,
    pub think_step_ms: u64,

    pub input_target: InputTarget,
    pub ctrl_c_exit_armed: bool,
    pub ctrl_c_exit_at: Option<Instant>,
    pub cancel_requested: bool,

    pub permission_state: PermissionUiState,
    pub global_kilo_config: Option<PathBuf>,
    pub project_kilo_configs: Vec<PathBuf>,
}

fn out_str(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
}

pub fn stdout_line(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.write_all(b"\n");
}

fn stderr_line(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.write_all(b"\n");
}

pub fn terminal_columns() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .max(1)
}

fn env_u64(names: &[&str], default: u64) -> u64 {
    for name in names {
        if let Ok(v) = std::env::var(name) {
            if let Ok(n) = v.trim().parse::<u64>() {
                return n;
            }
        }
    }
    default
}

impl App {
    pub async fn run(mut self) -> Result<()> {
        // Initialize + resolve initial session.
        self.agent_caps = agent::initialize(&self.cx).await?;
        self.resolve_initial_session().await?;
        self.refresh_quota().await;
        self.print_status();
        stdout_line("");

        let mut crossterm_events = EventStream::new();

        // Main loop.
        loop {
            // Read the main prompt.
            self.editor.start_read(true);
            self.input_target = InputTarget::Main;
            let prompt_text = loop {
                tokio::select! {
                    ev = crossterm_events.next() => {
                        let Some(Ok(ev)) = ev else { continue };
                        match self.handle_prompt_event(ev) {
                            PromptEvent::None => {}
                            PromptEvent::Submitted(text) => break text,
                            PromptEvent::Exit => return Ok(()),
                        }
                    }
                    agent_ev = self.events_rx.recv() => {
                        if self.handle_background_agent_event(agent_ev).is_break() {
                            return Ok(());
                        }
                    }
                }
            };
            self.input_target = InputTarget::Discard;

            if self.ctrl_c_exit_armed && !prompt_text.trim().is_empty() {
                self.ctrl_c_exit_armed = false;
                self.ctrl_c_exit_at = None;
            }
            if prompt_text.trim().is_empty() {
                continue;
            }

            self.history.append(&prompt_text);

            let trimmed = prompt_text.trim().to_lowercase();
            let invocation = commands::parse_slash_command_invocation(&prompt_text);
            if let Some(inv) = &invocation {
                if !inv.args.is_empty() && commands::is_no_arg_command(&inv.command) {
                    stdout_line(&format!(
                        "\n⚠️  \"{}\" does not accept arguments.\n",
                        inv.command
                    ));
                    continue;
                }
            }

            match trimmed.as_str() {
                "/exit" | "/quit" | "/q" => return Ok(()),
                "/help" | "/h" | "/?" => commands::print_command_help(),
                "/status" => {
                    self.refresh_session_title().await;
                    self.refresh_quota().await;
                    self.print_status();
                    stdout_line("");
                }
                "/compact" => {
                    if let Err(err) = self.handle_compact(&mut crossterm_events).await {
                        stdout_line(&format!("\n⚠️  Compact failed: {err:#}\n"));
                    }
                }
                "/model" | "/models" => {
                    self.handle_model(&mut crossterm_events).await;
                }
                "/provider" => {
                    self.handle_provider(&mut crossterm_events).await;
                }
                "/thinking" => {
                    self.handle_thinking(&mut crossterm_events).await;
                }
                "/zai-balance" | "/glm-balance" => self.run_balance_command(usage::handle_zai_usage()).await,
                "/codex-balance" => self.run_balance_command(usage::handle_codex_usage()).await,
                "/openrouter-balance" => self.run_balance_command(usage::handle_openrouter_balance()).await,
                "/sessions" => {
                    if let Err(err) = self.handle_sessions_list().await {
                        stdout_line(&format!("\n⚠️  {err:#}\n"));
                    }
                }
                "/resume" => {
                    if let Err(err) = self.handle_resume(&mut crossterm_events).await {
                        stdout_line(&format!("\n⚠️  {err:#}\n"));
                    }
                }
                "/new" | "/clear" => {
                    if let Err(err) = self.start_fresh_session().await {
                        stdout_line(&format!("\n⚠️  {err:#}\n"));
                    }
                }
                _ => {
                    if commands::is_stub_command(&trimmed) {
                        stdout_line(&format!(
                            "\n⚠️  \"{trimmed}\" is not implemented in this client yet. See /help.\n"
                        ));
                    } else {
                        // Regular prompt: run a turn.
                        if let Err(err) = self.run_turn(&mut crossterm_events, &prompt_text).await {
                            stdout_line(&format!("\n❌ Agent error: {err:#}\n"));
                        }
                    }
                }
            }
        }
    }

    async fn run_balance_command(&self, fut: impl std::future::Future<Output = Result<String>>) {
        match fut.await {
            Ok(out) => stdout_line(&format!("\n{out}\n")),
            Err(err) => stdout_line(&format!("\n⚠️  {err:#}\n")),
        }
    }

    // ---- prompt reading ----

    fn handle_prompt_event(&mut self, ev: CrosstermEvent) -> PromptEvent {
        if let CrosstermEvent::Resize(..) = ev {
            self.editor.on_resize();
            return PromptEvent::None;
        }
        let key = match ev {
            CrosstermEvent::Key(k) => k,
            CrosstermEvent::Paste(text) => {
                if self.input_target == InputTarget::Main {
                    self.editor.handle_paste(&text);
                    if let Some(outcome) = self.editor.take_result() {
                        return match outcome {
                            ReadOutcome::Submitted(t) => PromptEvent::Submitted(t),
                            ReadOutcome::Aborted => PromptEvent::None,
                        };
                    }
                }
                return PromptEvent::None;
            }
            _ => return PromptEvent::None,
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return PromptEvent::None;
        }
        let key = keys::translate(&key);

        if key == Key::CtrlC {
            if self.editor.is_searching() {
                self.editor.handle_key(key);
                return PromptEvent::None;
            }
            // Double-press to exit at the empty prompt.
            if self.editor.is_empty() {
                let armed_recently = self
                    .ctrl_c_exit_at
                    .map(|t| t.elapsed() < Duration::from_secs(3))
                    .unwrap_or(false);
                if self.ctrl_c_exit_armed && armed_recently {
                    return PromptEvent::Exit;
                }
                self.ctrl_c_exit_armed = true;
                self.ctrl_c_exit_at = Some(Instant::now());
                self.editor.abort();
                stderr_line(&format!("{}press ctrl-c again to exit{}", render::DIM, render::RESET));
                return PromptEvent::None;
            }
            self.editor.abort();
            return PromptEvent::None;
        }
        if key == Key::CtrlD && self.input_target == InputTarget::Main {
            if self.editor.is_empty() {
                return PromptEvent::Exit;
            }
            self.editor.handle_key(key);
            if let Some(outcome) = self.editor.take_result() {
                return match outcome {
                    ReadOutcome::Submitted(_) => PromptEvent::Exit,
                    ReadOutcome::Aborted => PromptEvent::None,
                };
            }
            return PromptEvent::None;
        }

        if self.input_target == InputTarget::Main {
            self.editor.handle_key(key);
            if let Some(outcome) = self.editor.take_result() {
                return match outcome {
                    ReadOutcome::Submitted(t) => PromptEvent::Submitted(t),
                    ReadOutcome::Aborted => PromptEvent::None,
                };
            }
        }
        PromptEvent::None
    }

    /// Handle agent events arriving outside a turn (usage info, session
    /// titles, permission requests queued between turns). Returns true on
    /// break (connection closed).
    fn handle_background_agent_event(&mut self, ev: Option<AgentEvent>) -> std::ops::ControlFlow<()> {
        match ev {
            None => std::ops::ControlFlow::Break(()),
            Some(AgentEvent::Update { session_id, update }) => {
                self.record_update_side_effects(&session_id, &update);
                std::ops::ControlFlow::Continue(())
            }
            Some(AgentEvent::Permission { request, responder }) => {
                // No turn is running; auto-cancel stray permission requests.
                let _ = responder.respond(
                    agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                        agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                    ),
                );
                let _ = request;
                std::ops::ControlFlow::Continue(())
            }
            Some(AgentEvent::ReadTextFile { request, responder }) => {
                permissions_ui::respond_read_text_file(request, responder);
                std::ops::ControlFlow::Continue(())
            }
            Some(AgentEvent::WriteTextFile { request, responder }) => {
                let _ = responder.respond(
                    agent_client_protocol::schema::v1::WriteTextFileResponse::new(),
                );
                let _ = request;
                std::ops::ControlFlow::Continue(())
            }
        }
    }

    fn record_update_side_effects(
        &mut self,
        session_id: &str,
        update: &agent_client_protocol::schema::v1::SessionUpdate,
    ) {
        use agent_client_protocol::schema::v1::SessionUpdate;
        match update {
            SessionUpdate::UsageUpdate(u) => {
                session_stats::apply_usage_update(
                    &mut self.session_usage,
                    session_id,
                    session_stats::SessionRole::Main,
                    session_stats::UsageUpdate {
                        used: Some(u.used),
                        size: Some(u.size),
                        cost_amount: u.cost.as_ref().map(|c| c.amount),
                        cost_currency: u.cost.as_ref().map(|c| c.currency.clone()),
                    },
                );
                if session_id == self.session_id {
                    self.main_usage = session_stats::SessionUsage {
                        role: session_stats::SessionRole::Main,
                        used: u.used,
                        size: u.size,
                        cost_amount: u.cost.as_ref().map(|c| c.amount).unwrap_or(0.0),
                        cost_currency: u
                            .cost
                            .as_ref()
                            .map(|c| c.currency.clone())
                            .unwrap_or_default(),
                    };
                }
            }
            SessionUpdate::SessionInfoUpdate(info) => {
                if let Some(title) = info.title.value() {
                    self.session_names.insert(session_id.to_string(), title.clone());
                }
            }
            _ => {}
        }
    }

    // ---- the turn loop ----

    async fn run_turn(
        &mut self,
        crossterm_events: &mut EventStream,
        prompt_text: &str,
    ) -> Result<()> {
        stderr_line(&format!("   {}↳ sent{}", render::GRAY, render::RESET));

        let mut transcript = Transcript::new();
        transcript.reset_for_turn();

        let sent = agent::send_prompt(&self.cx, &self.session_id, prompt_text);
        let (stop_tx, mut stop_rx) = mpsc::channel::<serde_json::Value>(1);
        self.cx
            .spawn(async move {
                let result = sent
                    .block_task()
                    .await
                    .map(|r| serde_json::to_value(&r).unwrap_or_default());
                let value = result.unwrap_or_else(|_| serde_json::json!({ "stopReason": "error" }));
                let _ = stop_tx.send(value).await;
                Ok(())
            })
            .map_err(|e| anyhow!("failed to spawn prompt task: {e}"))?;

        self.cancel_requested = false;
        let mut pending_updates: Vec<
            agent_client_protocol::schema::v1::SessionUpdate,
        > = Vec::new();
        let mut last_user_input_ended: Option<Instant> = None;
        let mut heartbeat = tokio::time::interval(Duration::from_millis(2000));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let stop_reason: Option<String>;
        'turn: loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    transcript.heartbeat_tick(
                        self.permission_state.awaiting_user_input(),
                        last_user_input_ended,
                        self.think_short_gap_ms,
                        self.think_step_ms,
                        self.think_long_gap_ms,
                    );
                }
                ev = crossterm_events.next() => {
                    let Some(Ok(ev)) = ev else { continue };
                    match ev {
                        CrosstermEvent::Resize(..) => {}
                        CrosstermEvent::Paste(_) => {}
                        CrosstermEvent::Key(k) => {
                            if k.kind == crossterm::event::KeyEventKind::Release { continue; }
                            let key = keys::translate(&k);
                            if key == Key::CtrlC {
                                if self.permission_state.awaiting_user_input() {
                                    self.permission_state.request_cancel();
                                    continue;
                                }
                                self.cancel_requested = true;
                                agent::cancel_session(&self.cx, &self.session_id);
                            }
                            // Everything else during a turn is filtered out.
                        }
                        _ => {}
                    }
                }
                stop = stop_rx.recv() => {
                    stop_reason = stop
                        .and_then(|v| v.get("stopReason").and_then(|s| s.as_str().map(str::to_string)))
                        .or(Some("error".to_string()));
                    break 'turn;
                }
                agent_ev = self.events_rx.recv() => {
                    match agent_ev {
                        None => { stop_reason = Some("connection-closed".into()); break 'turn; }
                        Some(AgentEvent::Update { session_id, update }) => {
                            if session_id != self.session_id {
                                self.record_update_side_effects(&session_id, &update);
                                continue;
                            }
                            self.record_update_side_effects(&session_id, &update);
                            if self.permission_state.awaiting_user_input() {
                                pending_updates.push(update);
                                continue;
                            }
                            transcript.process_update(&update);
                        }
                        Some(AgentEvent::Permission { request, responder }) => {
                            self.permission_state.begin_awaiting();
                            let outcome = permissions_ui::handle_permission_request(
                                self,
                                &mut transcript,
                                crossterm_events,
                                request,
                                responder,
                            )
                            .await;
                            self.permission_state.end_awaiting();
                            last_user_input_ended = Some(Instant::now());
                            if outcome == permissions_ui::PermissionOutcome::Cancelled {
                                self.cancel_requested = true;
                                agent::cancel_session(&self.cx, &self.session_id);
                            }
                            // Drain updates buffered while the prompt was up.
                            for upd in std::mem::take(&mut pending_updates) {
                                transcript.process_update(&upd);
                            }
                        }
                        Some(AgentEvent::ReadTextFile { request, responder }) => {
                            permissions_ui::respond_read_text_file(request, responder);
                        }
                        Some(AgentEvent::WriteTextFile { request, responder }) => {
                            let _ = responder.respond(agent_client_protocol::schema::v1::WriteTextFileResponse::new());
                            let _ = request;
                        }
                    }
                }
            }
        }

        // If the turn was cancelled, wait briefly for remaining updates.
        if self.cancel_requested && stop_reason.is_none() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                tokio::select! {
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => break,
                    agent_ev = self.events_rx.recv() => {
                        match agent_ev {
                            Some(AgentEvent::Update { session_id, update }) if session_id == self.session_id => {
                                self.record_update_side_effects(&session_id, &update);
                                transcript.process_update(&update);
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                    stop = stop_rx.recv() => {
                        let _ = stop;
                        break;
                    }
                }
            }
        }

        let quota = self.format_quota_lines();
        transcript.finish_turn(&quota);

        if !transcript.got_text && !transcript.got_tool && transcript.used_tokens == 0 {
            stdout_line(
                "\n⚠️  The agent ended the turn without producing any response.\n   \
                 This usually means no AI provider/model is configured, or it could not be reached.\n",
            );
        } else if let Some(reason) = &stop_reason {
            if reason != "end_turn" {
                stdout_line(&format!("\n⚠️  Agent stopped: {reason}"));
            }
        }

        if transcript.used_tokens > 0 {
            self.refresh_quota().await;
        }
        Ok(())
    }

    // ---- session lifecycle ----

    async fn resolve_initial_session(&mut self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let can_load = self
            .agent_caps
            .get("loadSession")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // -c / -s flags handled by main(); exposed via env-free globals here.
        let target = crate::app::CLI_SESSION_TARGET
            .lock()
            .unwrap()
            .clone()
            .filter(|_| can_load)
            .or_else(|| {
                if crate::app::CLI_CONTINUE.load(std::sync::atomic::Ordering::Relaxed) && can_load {
                    self.read_last_session()
                } else {
                    None
                }
            });

        if let Some(target) = target {
            match agent::load_session(&self.cx, &target, &cwd).await {
                Ok(resp) => {
                    self.apply_session_config_options(&resp);
                    self.session_id = target.clone();
                    let _ = std::fs::write(&self.paths.last_session_file, &target);
                    session_stats::apply_usage_update(
                        &mut self.session_usage,
                        &target,
                        session_stats::SessionRole::Main,
                        session_stats::UsageUpdate::default(),
                    );
                    self.drain_session_replay().await;
                    return Ok(());
                }
                Err(err) => {
                    stdout_line(&format!(
                        "\n⚠️  Could not resume session {target}: {err:#} — starting a new session instead."
                    ));
                }
            }
        }

        self.start_fresh_session().await
    }

    pub async fn start_fresh_session(&mut self) -> Result<()> {
        let cwd = std::env::current_dir()?;
        let resp = agent::new_session(&self.cx, &cwd).await?;
        let sid = resp
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("session/new response missing sessionId"))?
            .to_string();
        self.session_id = sid.clone();
        let _ = std::fs::write(&self.paths.last_session_file, &sid);
        session_stats::apply_usage_update(
            &mut self.session_usage,
            &sid,
            session_stats::SessionRole::Main,
            session_stats::UsageUpdate::default(),
        );
        self.apply_session_config_options(&resp);
        if let Some(model) = &self.active_session_model {
            self.model_state.record_recent_model(model);
        }
        Ok(())
    }

    fn apply_session_config_options(&mut self, resp: &serde_json::Value) {
        if let Some(psel) = model_search::extract_provider_select(resp.get("configOptions")) {
            self.active_session_provider = psel.current;
            if !psel.options.is_empty() {
                self.session_provider_options = Some(psel.options);
            }
        }
        if let Some(sel) = model_search::extract_model_select(resp.get("configOptions")) {
            self.active_session_model = sel.current;
            if !sel.options.is_empty() {
                self.session_model_options = Some(sel.options);
            }
        }
        if let Some(esel) = model_search::extract_effort_select(resp.get("configOptions")) {
            self.active_effort = esel.current;
            if !esel.options.is_empty() {
                self.session_effort_options = Some(esel.options);
            }
        }
    }

    pub fn read_last_session(&self) -> Option<String> {
        std::fs::read_to_string(&self.paths.last_session_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Drain replayed history after a session/load until the stream goes
    /// quiet (400ms), recording usage + names without printing the replay.
    async fn drain_session_replay(&mut self) {
        let quiet = Duration::from_millis(400);
        loop {
            let ev = tokio::select! {
                ev = self.events_rx.recv() => ev,
                _ = tokio::time::sleep(quiet) => return,
            };
            match ev {
                Some(AgentEvent::Update { session_id, update }) => {
                    self.record_update_side_effects(&session_id, &update);
                }
                Some(AgentEvent::ReadTextFile { request, responder }) => {
                    permissions_ui::respond_read_text_file(request, responder);
                }
                Some(AgentEvent::WriteTextFile { request, responder }) => {
                    let _ = responder.respond(
                        agent_client_protocol::schema::v1::WriteTextFileResponse::new(),
                    );
                    let _ = request;
                }
                Some(AgentEvent::Permission { request, responder }) => {
                    let _ = responder.respond(
                        agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                        ),
                    );
                    let _ = request;
                }
                None => return,
            }
        }
    }

    pub async fn refresh_session_title(&mut self) {
        let sessions = match agent::list_sessions(&self.cx).await {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Some(serde_json::Value::Array(list)) = sessions.get("sessions") {
            for s in list {
                if let (Some(id), Some(title)) = (
                    s.get("sessionId").and_then(|v| v.as_str()),
                    s.get("title").and_then(|v| v.as_str()),
                ) {
                    if !title.is_empty() {
                        self.session_names.insert(id.to_string(), title.to_string());
                    }
                }
            }
        }
    }

    pub async fn list_sessions(&self) -> Result<Vec<serde_json::Value>> {
        let resp = agent::list_sessions(&self.cx).await?;
        Ok(resp
            .get("sessions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    // ---- /compact ----

    async fn handle_compact(&mut self, crossterm_events: &mut EventStream) -> Result<()> {
        const COMPACT_REQUEST: &str = "Summarize the conversation so far into a concise context brief that a fresh \
agent session needs to continue the work. Include: the goal, decisions made, in-progress tasks, key files/paths \
touched, and open questions. Be terse and information-dense. Output only the brief, no preamble.";
        stdout_line(&format!("\n{}Compacting main session…{}", render::BOLD, render::RESET));

        let old_id = self.session_id.clone();
        let sent = agent::send_prompt(&self.cx, &old_id, COMPACT_REQUEST);
        let (stop_tx, mut stop_rx) = mpsc::channel::<serde_json::Value>(1);
        self.cx.spawn(async move {
            let v = sent
                .block_task()
                .await
                .map(|r| serde_json::to_value(&r).unwrap_or_default())
                .unwrap_or_default();
            let _ = stop_tx.send(v).await;
            Ok(())
        })?;

        let mut brief = String::new();
        loop {
            tokio::select! {
                stop = stop_rx.recv() => {
                    let _ = stop;
                    break;
                }
                agent_ev = self.events_rx.recv() => {
                    match agent_ev {
                        Some(AgentEvent::Update { session_id, update }) => {
                            use agent_client_protocol::schema::v1::SessionUpdate;
                            if session_id != old_id { continue; }
                            self.record_update_side_effects(&session_id, &update);
                            if let SessionUpdate::AgentMessageChunk(chunk) = &update {
                                if let agent_client_protocol::schema::v1::ContentBlock::Text(t) = &chunk.content {
                                    brief.push_str(&t.text);
                                }
                            }
                        }
                        Some(AgentEvent::ReadTextFile { request, responder }) => {
                            permissions_ui::respond_read_text_file(request, responder);
                        }
                        Some(AgentEvent::WriteTextFile { request, responder }) => {
                            let _ = responder.respond(agent_client_protocol::schema::v1::WriteTextFileResponse::new());
                            let _ = request;
                        }
                        Some(AgentEvent::Permission { .. }) => {
                            agent::cancel_session(&self.cx, &old_id);
                            stdout_line(&format!("\n{}Compact cancelled. Keeping current session.{}\n", render::DIM, render::RESET));
                            return Ok(());
                        }
                        None => break,
                    }
                }
                ev = crossterm_events.next() => {
                    if let Some(Ok(CrosstermEvent::Key(k))) = ev {
                        if keys::translate(&k) == Key::CtrlC {
                            agent::cancel_session(&self.cx, &old_id);
                        }
                    }
                }
            }
        }

        let brief = brief.trim().to_string();
        if brief.is_empty() {
            stdout_line(&format!(
                "\n⚠️  Compaction produced no summary; keeping current session {old_id}.\n"
            ));
            return Ok(());
        }

        self.start_fresh_session().await?;
        stdout_line(&format!("   seeding new session {}…", self.session_id));
        let fresh_id = self.session_id.clone();
        let seed = format!("Context brief from the previous (compacted) session — continue from here:\n\n{brief}");
        let sent = agent::send_prompt(&self.cx, &fresh_id, &seed);
        let (stop_tx, mut stop_rx) = mpsc::channel::<serde_json::Value>(1);
        self.cx.spawn(async move {
            let v = sent
                .block_task()
                .await
                .map(|r| serde_json::to_value(&r).unwrap_or_default())
                .unwrap_or_default();
            let _ = stop_tx.send(v).await;
            Ok(())
        })?;
        loop {
            tokio::select! {
                stop = stop_rx.recv() => { let _ = stop; break; }
                agent_ev = self.events_rx.recv() => {
                    match agent_ev {
                        Some(AgentEvent::Update { session_id, update }) => {
                            self.record_update_side_effects(&session_id, &update);
                        }
                        Some(AgentEvent::ReadTextFile { request, responder }) => {
                            permissions_ui::respond_read_text_file(request, responder);
                        }
                        Some(AgentEvent::WriteTextFile { request, responder }) => {
                            let _ = responder.respond(agent_client_protocol::schema::v1::WriteTextFileResponse::new());
                            let _ = request;
                        }
                        Some(AgentEvent::Permission { request, responder }) => {
                            let _ = responder.respond(agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                                agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                            ));
                            let _ = request;
                        }
                        None => break,
                    }
                }
                ev = crossterm_events.next() => {
                    if let Some(Ok(CrosstermEvent::Key(k))) = ev {
                        if keys::translate(&k) == Key::CtrlC {
                            agent::cancel_session(&self.cx, &fresh_id);
                        }
                    }
                }
            }
        }
        self.main_usage = session_stats::SessionUsage::default();
        stdout_line(&format!("\n✅ Compacted → new session {}\n", self.session_id));
        Ok(())
    }

    // ---- /sessions + /resume ----

    async fn handle_sessions_list(&mut self) -> Result<()> {
        let sessions = self.list_sessions().await?;
        if sessions.is_empty() {
            stdout_line("\n(no sessions)\n");
            return Ok(());
        }
        let mut lines = vec![format!("{}sessions{}", render::BOLD, render::RESET)];
        for s in sessions.iter().take(20) {
            let id = s.get("sessionId").and_then(|v| v.as_str()).unwrap_or("?");
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let modified = s
                .get("modifiedTime")
                .or_else(|| s.get("lastModified"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!(
                "  {}{id}{}  {DIM}{title} {modified}{RESET}",
                render::GREEN,
                render::RESET,
                DIM = render::DIM,
                RESET = render::RESET
            ));
        }
        stdout_line(&format!("\n{}\n", lines.join("\n")));
        Ok(())
    }

    async fn handle_resume(&mut self, crossterm_events: &mut EventStream) -> Result<()> {
        let sessions = self.list_sessions().await?;
        if sessions.is_empty() {
            stdout_line("\n(no sessions to resume)\n");
            return Ok(());
        }
        stdout_line(&format!("\n{}Resume session{}", render::BOLD, render::RESET));
        for (i, s) in sessions.iter().take(15).enumerate() {
            let id = s.get("sessionId").and_then(|v| v.as_str()).unwrap_or("?");
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
            stdout_line(&format!("   {}. {}{DIM}{title} {id}{RESET}", i + 1, "", DIM = render::DIM, RESET = render::RESET));
        }
        out_str("\nChoose (number, 'c' to cancel): ");
        let answer = self
            .read_single_line(crossterm_events, Some("0123456789cC"))
            .await;
        let Some(answer) = answer else {
            stdout_line("");
            return Ok(());
        };
        let t = answer.trim().to_lowercase();
        if t == "c" || t == "cancel" || t.is_empty() {
            stdout_line("");
            return Ok(());
        }
        let choice: usize = match t.parse::<usize>() {
            Ok(n) if n >= 1 && n <= sessions.len() => n - 1,
            _ => {
                stdout_line("Invalid choice.");
                return Ok(());
            }
        };
        let target = sessions[choice]
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if target.is_empty() {
            return Err(anyhow!("session entry missing id"));
        }
        let cwd = std::env::current_dir()?;
        let resp = agent::load_session(&self.cx, &target, &cwd).await?;
        self.session_id = target.clone();
        let _ = std::fs::write(&self.paths.last_session_file, &target);
        self.apply_session_config_options(&resp);
        self.drain_session_replay().await;
        let name = self.session_names.get(&target).cloned();
        stdout_line(&format!(
            "\n{}Resumed{}  {DIM}{}{RESET}\n",
            render::BOLD,
            render::RESET,
            name.unwrap_or_else(|| target.clone()),
            DIM = render::DIM,
            RESET = render::RESET
        ));
        Ok(())
    }

    // ---- /model + /thinking ----

    /// /provider: switch the provider, then pick a model from the new list.
    async fn handle_provider(&mut self, crossterm_events: &mut EventStream) {
        let opts = match &self.session_provider_options {
            Some(o) if o.len() > 1 => o.clone(),
            _ => {
                stdout_line(&format!(
                    "\nℹ️  Provider list not available for this session (current: {}).",
                    self.active_session_provider.as_deref().unwrap_or("(default)")
                ));
                return;
            }
        };
        loop {
            stdout_line(&format!("\n{}Switch provider{}", render::BOLD, render::RESET));
            for (i, o) in opts.iter().enumerate() {
                let star = if Some(o.value.clone()) == self.active_session_provider {
                    format!("{}★{} ", render::YELLOW, render::RESET)
                } else {
                    String::new()
                };
                let label = if o.name != o.value {
                    format!("  {DIM}{}{RESET}", o.name, DIM = render::DIM, RESET = render::RESET)
                } else {
                    String::new()
                };
                stdout_line(&format!("   {}. {star}{}{label}", i + 1, o.value));
            }
            out_str("\nChoose (number, 'c' to cancel): ");
            let answer = self
                .read_single_line(crossterm_events, Some("0123456789cC"))
                .await;
            let Some(answer) = answer else {
                stdout_line("");
                return;
            };
            let t = answer.trim().to_lowercase();
            if t == "c" || t == "cancel" {
                stdout_line("");
                return;
            }
            if t.is_empty() {
                continue;
            }
            let choice: usize = match t.parse::<usize>() {
                Ok(n) => n - 1,
                Err(_) => {
                    stdout_line("Invalid choice.");
                    continue;
                }
            };
            if choice >= opts.len() {
                stdout_line("Invalid choice.");
                continue;
            }
            let v = opts[choice].value.clone();
            if Some(v.clone()) == self.active_session_provider {
                stdout_line(&format!("{DIM}Already on {v}{RESET}", DIM = render::DIM, RESET = render::RESET));
                return;
            }
            match agent::set_config_option(&self.cx, &self.session_id, "provider", &v).await {
                Ok(resp) => {
                    self.apply_session_config_options(&resp);
                    stdout_line(&format!(
                        "{}✓{} Provider switched to {BOLD}{}{RESET}.",
                        render::GREEN,
                        render::RESET,
                        self.active_session_provider.as_deref().unwrap_or(&v),
                        BOLD = render::BOLD,
                        RESET = render::RESET
                    ));
                    stdout_line("");
                    self.handle_model(crossterm_events).await;
                    return;
                }
                Err(err) => {
                    stdout_line(&format!(
                        "{}⚠️  Could not switch provider:{} {err:#}",
                        render::RED,
                        render::RESET
                    ));
                }
            }
        }
    }

    async fn handle_model(&mut self, crossterm_events: &mut EventStream) {
        if let Err(err) = self.handle_model_inner(crossterm_events).await {
            stdout_line(&format!("\n⚠️  {err:#}\n"));
        }
    }

    async fn handle_model_inner(&mut self, crossterm_events: &mut EventStream) -> Result<()> {
        // Favourites: every "model": "..." in the kilo config (raw text,
        // including commented-out lines), deduped in order of appearance.
        let cfg_path = find_kilo_config_path();
        let raw = match &cfg_path {
            Some(p) => std::fs::read_to_string(p).unwrap_or_default(),
            None => String::new(),
        };
        let re = regex::Regex::new(r#""model"\s*:\s*"([^"]+)""#).unwrap();
        let mut fav_values: Vec<String> = Vec::new();
        for cap in re.captures_iter(&raw) {
            let v = cap[1].trim().to_string();
            if !v.is_empty() && !fav_values.contains(&v) {
                fav_values.push(v);
            }
        }
        const KILO_FREE: &str = "kilo/kilo-auto/free";
        if !fav_values.contains(&KILO_FREE.to_string()) {
            fav_values.insert(0, KILO_FREE.to_string());
        }

        let opts = self.session_model_options.clone().unwrap_or_default();
        let avail: std::collections::HashSet<String> =
            opts.iter().map(|o| o.value.clone()).collect();
        let have_agent_list = self.session_model_options.is_some();
        let current_model = self
            .active_session_model
            .clone()
            .or_else(|| fav_values.first().cloned());
        if let Some(cur) = &current_model {
            if !fav_values.contains(cur) {
                fav_values.push(cur.clone());
            }
        }

        loop {
            stdout_line(&format!(
                "\n{}Switch model{}{}{}",
                render::BOLD,
                render::RESET,
                match self.active_session_provider.as_deref() {
                    Some(p) => format!("  {DIM}(provider: {p}){RESET}", DIM = render::DIM, RESET = render::RESET),
                    None => String::new(),
                },
                match &cfg_path {
                    Some(p) => format!("  {DIM}(favourites from {}){RESET}", display_config_path(p), DIM = render::DIM, RESET = render::RESET),
                    None => String::new(),
                }
            ));
            for (i, v) in fav_values.iter().enumerate() {
                let star = if Some(v) == current_model.as_ref() {
                    format!("{}★{} ", render::YELLOW, render::RESET)
                } else {
                    String::new()
                };
                let unavail = if have_agent_list && !avail.contains(v) {
                    format!(" {}[unavailable]", render::RED)
                } else {
                    String::new()
                };
                stdout_line(&format!("   {}. {star}{v}{unavail}{}", i + 1, render::RESET));
            }
            stdout_line(&format!(
                "   {DIM}s. Search all available models ({} models){RESET}",
                opts.len(),
                DIM = render::DIM,
                RESET = render::RESET
            ));
            out_str("\nChoose (number, 's' to search, 'c' to cancel): ");
            let answer = self
                .read_single_line(crossterm_events, Some("0123456789sScC"))
                .await;
            let Some(answer) = answer else {
                stdout_line("");
                return Ok(());
            };
            let t = answer.trim().to_lowercase();
            if t == "c" || t == "cancel" {
                stdout_line("");
                return Ok(());
            }
            if t == "s" {
                if !have_agent_list {
                    stdout_line(&format!("{DIM}(The agent didn't publish a model list — nothing to search.){RESET}", DIM = render::DIM, RESET = render::RESET));
                    continue;
                }
                match self.model_search_view(crossterm_events, &opts).await {
                    ModelSearchOutcome::Cancel => {
                        stdout_line("");
                        return Ok(());
                    }
                    ModelSearchOutcome::Back => continue,
                    ModelSearchOutcome::Picked(v) => {
                        stdout_line("");
                        self.switch_with_thinking(crossterm_events, &v).await;
                        stdout_line("");
                        return Ok(());
                    }
                }
            }
            if t.is_empty() {
                continue;
            }
            let choice: usize = match t.parse::<usize>() {
                Ok(n) => (n - 1).min(usize::MAX - 1),
                Err(_) => {
                    stdout_line("Invalid choice.");
                    continue;
                }
            };
            if choice >= fav_values.len() {
                stdout_line("Invalid choice.");
                continue;
            }
            let v = fav_values[choice].clone();
            if have_agent_list && !avail.contains(&v) {
                stdout_line(&format!(
                    "{}{v} is not available on this session{} (its provider isn't loaded/authenticated). Try 's' to search available models.",
                    render::RED,
                    render::RESET
                ));
                continue;
            }
            stdout_line("");
            self.switch_with_thinking(crossterm_events, &v).await;
            stdout_line("");
            return Ok(());
        }
    }

    async fn switch_with_thinking(&mut self, crossterm_events: &mut EventStream, value: &str) {
        let previous_model = self.active_session_model.clone();
        let previous_effort = self.active_effort.clone();
        if !self.apply_model_switch(value).await {
            return;
        }
        if self.prompt_thinking_after_model_switch(crossterm_events).await {
            if let Some(model) = &self.active_session_model {
                let m = model.clone();
                self.model_state.record_recent_model(&m);
                stdout_line(&format!(
                    "{}✓{} Model switched to {}{BOLD}{m}{RESET} (session {}).",
                    render::GREEN,
                    render::RESET,
                    self.session_id,
                    BOLD = render::BOLD,
                    RESET = render::RESET
                ));
                let _ = &m;
            }
            return;
        }
        // Ctrl+C at the thinking picker rolls back the provisional switch.
        if let Some(prev) = previous_model {
            if self.active_session_model.as_deref() != Some(prev.as_str()) {
                if self.apply_model_switch(&prev).await {
                    if let Some(eff) = &previous_effort {
                        if self.active_effort.as_deref() != Some(eff.as_str())
                            && self
                                .session_effort_options
                                .as_ref()
                                .map(|o| o.iter().any(|x| x.value == *eff))
                                .unwrap_or(false)
                        {
                            self.apply_effort_level(eff).await;
                        }
                    }
                }
            }
        }
        stdout_line(&format!("{DIM}Model selection cancelled.{RESET}", DIM = render::DIM, RESET = render::RESET));
    }

    pub async fn apply_model_switch(&mut self, value: &str) -> bool {
        match agent::set_config_option(&self.cx, &self.session_id, "model", value).await
        {
            Ok(resp) => {
                self.apply_session_config_options(&resp);
                if self.active_session_model.is_none() {
                    self.active_session_model = Some(value.to_string());
                }
                true
            }
            Err(err) => {
                stdout_line(&format!(
                    "{}⚠️  Could not switch model:{} {err:#}",
                    render::RED,
                    render::RESET
                ));
                false
            }
        }
    }

    pub async fn apply_effort_level(&mut self, value: &str) -> bool {
        match agent::set_config_option(&self.cx, &self.session_id, "effort", value)
        .await
        {
            Ok(resp) => {
                self.apply_session_config_options(&resp);
                if self.active_effort.is_none() {
                    self.active_effort = Some(value.to_string());
                }
                if let Some(model) = self.active_session_model.clone() {
                    self.model_state.save_model_variant(&model, self.active_effort.as_deref());
                }
                true
            }
            Err(err) => {
                stdout_line(&format!(
                    "{}⚠️  Could not set thinking level:{} {err:#}",
                    render::RED,
                    render::RESET
                ));
                false
            }
        }
    }

    async fn prompt_thinking_after_model_switch(&mut self, crossterm_events: &mut EventStream) -> bool {
        let opts = match &self.session_effort_options {
            Some(o) if !o.is_empty() => o.clone(),
            _ => return true,
        };
        let saved = self
            .active_session_model
            .as_ref()
            .and_then(|m| self.model_state.variant_for(m));
        let mut default_idx = saved
            .as_ref()
            .and_then(|s| opts.iter().position(|o| o.value == *s))
            .unwrap_or(usize::MAX);
        if default_idx == usize::MAX {
            default_idx = self
                .active_effort
                .as_ref()
                .and_then(|e| opts.iter().position(|o| o.value == *e))
                .unwrap_or(0);
        }
        let default_value = opts[default_idx].value.clone();
        let default_display = model_search::format_effort_display(Some(&default_value), &opts);

        stdout_line(&format!(
            "\n{}Thinking level{}  {DIM}(for {}){RESET}",
            render::BOLD,
            render::RESET,
            self.active_session_model.as_deref().unwrap_or("this model"),
            DIM = render::DIM,
            RESET = render::RESET
        ));
        for (i, o) in opts.iter().enumerate() {
            let star = if i == default_idx {
                format!("{}★{} ", render::YELLOW, render::RESET)
            } else {
                String::new()
            };
            let display = model_search::format_effort_display(Some(&o.value), &opts);
            let name_suffix = if display != o.value {
                format!("  {DIM}{display}{RESET}", DIM = render::DIM, RESET = render::RESET)
            } else {
                String::new()
            };
            stdout_line(&format!("   {}. {star}{}{name_suffix}", i + 1, o.value));
        }

        loop {
            out_str(&format!(
                "\nChoose (number, Enter={default_display}, 'c' to skip): "
            ));
            let answer = self
                .read_single_line(crossterm_events, Some("0123456789cC"))
                .await;
            let Some(answer) = answer else {
                stdout_line("");
                return false; // Ctrl+C
            };
            let t = answer.trim().to_lowercase();
            if t == "c" || t == "cancel" {
                stdout_line("");
                return true;
            }
            if t.is_empty() {
                if self.active_effort.as_deref() == Some(default_value.as_str()) {
                    if let Some(model) = self.active_session_model.clone() {
                        self.model_state.save_model_variant(&model, Some(&default_value));
                    }
                    stdout_line(&format!("{DIM}Keeping {default_display}{RESET}", DIM = render::DIM, RESET = render::RESET));
                } else if self.apply_effort_level(&default_value).await {
                    stdout_line(&format!(
                        "{}✓{} Thinking level set to {BOLD}{default_display}{RESET}.",
                        render::GREEN,
                        render::RESET,
                        BOLD = render::BOLD,
                        RESET = render::RESET
                    ));
                }
                return true;
            }
            let choice: usize = match t.parse::<usize>() {
                Ok(n) => n - 1,
                Err(_) => {
                    stdout_line("Invalid choice.");
                    continue;
                }
            };
            if choice >= opts.len() {
                stdout_line("Invalid choice.");
                continue;
            }
            let v = opts[choice].value.clone();
            if self.active_effort.as_deref() == Some(v.as_str()) {
                if let Some(model) = self.active_session_model.clone() {
                    self.model_state.save_model_variant(&model, Some(&v));
                }
                stdout_line(&format!(
                    "{DIM}Already at {}{RESET}",
                    model_search::format_effort_display(Some(&v), &opts),
                    DIM = render::DIM,
                    RESET = render::RESET
                ));
                return true;
            }
            if self.apply_effort_level(&v).await {
                stdout_line(&format!(
                    "{}✓{} Thinking level set to {BOLD}{}{RESET}.",
                    render::GREEN,
                    render::RESET,
                    model_search::format_effort_display(Some(&v), &opts),
                    BOLD = render::BOLD,
                    RESET = render::RESET
                ));
            }
            return true;
        }
    }

    async fn handle_thinking(&mut self, crossterm_events: &mut EventStream) {
        let opts = match &self.session_effort_options {
            Some(o) if !o.is_empty() => o.clone(),
            _ => {
                let m = self.active_session_model.clone().unwrap_or_else(|| "(this model)".into());
                stdout_line(&format!("\nℹ️  {m} has no thinking/effort levels to choose."));
                return;
            }
        };
        loop {
            stdout_line(&format!(
                "\n{}Switch thinking level{}  {DIM}(effort levels for {}){RESET}",
                render::BOLD,
                render::RESET,
                self.active_session_model.as_deref().unwrap_or("(unknown model)"),
                DIM = render::DIM,
                RESET = render::RESET
            ));
            for (i, o) in opts.iter().enumerate() {
                let star = if Some(o.value.clone()) == self.active_effort {
                    format!("{}★{} ", render::YELLOW, render::RESET)
                } else {
                    String::new()
                };
                let display = model_search::format_effort_display(Some(&o.value), &opts);
                let name_suffix = if display != o.value {
                    format!("  {DIM}{display}{RESET}", DIM = render::DIM, RESET = render::RESET)
                } else {
                    String::new()
                };
                stdout_line(&format!("   {}. {star}{}{name_suffix}", i + 1, o.value));
            }
            out_str("\nChoose (number, 'c' to cancel): ");
            let answer = self
                .read_single_line(crossterm_events, Some("0123456789cC"))
                .await;
            let Some(answer) = answer else {
                stdout_line("");
                return;
            };
            let t = answer.trim().to_lowercase();
            if t == "c" || t == "cancel" {
                stdout_line("");
                return;
            }
            if t.is_empty() {
                continue;
            }
            let choice: usize = match t.parse::<usize>() {
                Ok(n) => n - 1,
                Err(_) => {
                    stdout_line("Invalid choice.");
                    continue;
                }
            };
            if choice >= opts.len() {
                stdout_line("Invalid choice.");
                continue;
            }
            let v = opts[choice].value.clone();
            if self.active_effort.as_deref() == Some(v.as_str()) {
                stdout_line(&format!(
                    "{DIM}Already at {}{RESET}",
                    model_search::format_effort_display(Some(&v), &opts),
                    DIM = render::DIM,
                    RESET = render::RESET
                ));
                return;
            }
            if self.apply_effort_level(&v).await {
                let display = model_search::format_effort_display(self.active_effort.as_deref(), &opts);
                stdout_line(&format!(
                    "{}✓{} Thinking level set to {BOLD}{display}{RESET} and saved as default.",
                    render::GREEN,
                    render::RESET,
                    BOLD = render::BOLD,
                    RESET = render::RESET
                ));
            }
            return;
        }
    }

    // ---- /model search view (bottom-pinned editor) ----

    async fn model_search_view(
        &mut self,
        crossterm_events: &mut EventStream,
        opts: &[ModelOption],
    ) -> ModelSearchOutcome {
        let word_list = model_search::extract_model_words(opts);
        let model_opts = opts.to_vec();
        let completer: Completer = Box::new(move |line: &str, cursor_col: usize| {
            let before = crate::editor::wrap::char_prefix(line, cursor_col);
            let token = before
                .split_whitespace()
                .next_back()
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                // Tab on an empty search: offer every available provider.
                return (model_search::provider_prefixes(&model_opts), line.to_string());
            }
            if model_search::is_full_path_token(&token) {
                (model_search::match_model_paths(&token, &model_opts), line.to_string())
            } else {
                (model_search::match_model_words(&token, &word_list), line.to_string())
            }
        });

        self.editor.set_prompt("search models (Tab=autocomplete, Ctrl+C=back)> ");
        self.editor.set_completer(Some(completer));
        self.editor.set_history_enabled(false);
        self.editor.set_bottom_pinned(true, 3);
        self.editor.start_read(false);

        let mut abort = false;
        let text = loop {
            tokio::select! {
                agent_ev = self.events_rx.recv() => {
                    // Auto-answer anything the agent asks while searching.
                    if let Some(AgentEvent::ReadTextFile { request, responder }) = agent_ev {
                        permissions_ui::respond_read_text_file(request, responder);
                    } else if let Some(AgentEvent::WriteTextFile { request, responder }) = agent_ev {
                        let _ = responder.respond(agent_client_protocol::schema::v1::WriteTextFileResponse::new());
                        let _ = request;
                    } else if let Some(AgentEvent::Permission { request, responder }) = agent_ev {
                        let _ = request;
                        let _ = responder.respond(agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                            agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                        ));
                    }
                }
                ev = crossterm_events.next() => {
                    let Some(Ok(ev)) = ev else { continue };
                    match ev {
                        CrosstermEvent::Resize(..) => self.editor.on_resize(),
                        CrosstermEvent::Paste(_) => {}
                        CrosstermEvent::Key(k) => {
                            if k.kind == crossterm::event::KeyEventKind::Release { continue; }
                            let key = keys::translate(&k);
                            if key == Key::CtrlC || key == Key::CtrlD {
                                self.editor.abort();
                                if let Some(ReadOutcome::Aborted) = self.editor.take_result() {
                                    abort = true;
                                    break String::new();
                                }
                            }
                            self.editor.handle_key(key);
                            if let Some(outcome) = self.editor.take_result() {
                                match outcome {
                                    ReadOutcome::Submitted(t) => break t,
                                    ReadOutcome::Aborted => { abort = true; break String::new(); }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        };

        self.editor.set_bottom_pinned(false, 0);
        self.editor.set_completer(None);
        self.editor.set_prompt("goose> ");
        self.editor.set_history_enabled(true);
        self.editor.close();

        if abort {
            return ModelSearchOutcome::Back;
        }
        let t = text.trim().to_lowercase();
        if t == "c" || t == "cancel" {
            return ModelSearchOutcome::Cancel;
        }
        if t == "m" {
            return ModelSearchOutcome::Back;
        }
        if t.is_empty() {
            return ModelSearchOutcome::Back;
        }
        if let Some(exact) = model_search::exact_model_match(&text, opts) {
            return ModelSearchOutcome::Picked(exact.value);
        }
        let matches = model_search::match_models(&text, opts);
        if matches.len() == 1 {
            return ModelSearchOutcome::Picked(matches[0].value.clone());
        }
        if matches.is_empty() {
            stdout_line(&format!("{DIM}No models match \"{}\" — refine.{RESET}", text.trim(), DIM = render::DIM, RESET = render::RESET));
        } else {
            stdout_line(&format!(
                "{DIM}{} models match \"{}\" — press Tab to cycle, or refine the query.{RESET}",
                matches.len(),
                text.trim(),
                DIM = render::DIM,
                RESET = render::RESET
            ));
        }
        ModelSearchOutcome::Back
    }

    // ---- shared single-line reader (menus, confirms) ----

    /// Read one line while keeping the agent event queue alive. Returns None
    /// on Ctrl+C. Agent events arriving during the read are handled
    /// conservatively (fs requests answered, permission requests answered
    /// with the UI queue later, updates recorded).
    pub async fn read_single_line(
        &mut self,
        crossterm_events: &mut EventStream,
        allow: Option<&str>,
    ) -> Option<String> {
        let mut reader = SingleLineReader::new(allow);
        loop {
            tokio::select! {
                ev = crossterm_events.next() => {
                    let Some(Ok(ev)) = ev else { continue };
                    match ev {
                        CrosstermEvent::Key(k) => {
                            if k.kind == crossterm::event::KeyEventKind::Release { continue; }
                            let key = keys::translate(&k);
                            if key == Key::CtrlC {
                                return reader.abort();
                            }
                            if let Some(line) = reader.handle_key(key) {
                                return Some(line);
                            }
                        }
                        CrosstermEvent::Paste(_) => {}
                        _ => {}
                    }
                }
                agent_ev = self.events_rx.recv() => {
                    match agent_ev {
                        Some(AgentEvent::ReadTextFile { request, responder }) => {
                            permissions_ui::respond_read_text_file(request, responder);
                        }
                        Some(AgentEvent::WriteTextFile { request, responder }) => {
                            let _ = responder.respond(agent_client_protocol::schema::v1::WriteTextFileResponse::new());
                            let _ = request;
                        }
                        Some(AgentEvent::Permission { request, responder }) => {
                            // No turn is on screen to show a prompt; cancel.
                            let _ = request;
                            let _ = responder.respond(agent_client_protocol::schema::v1::RequestPermissionResponse::new(
                                agent_client_protocol::schema::v1::RequestPermissionOutcome::Cancelled,
                            ));
                        }
                        Some(AgentEvent::Update { session_id, update }) => {
                            self.record_update_side_effects(&session_id, &update);
                        }
                        None => return reader.abort(),
                    }
                }
            }
        }
    }

    // ---- status ----

    pub async fn refresh_quota(&mut self) {
        let (codex, zai) = tokio::join!(usage::fetch_kilo_codex_remaining(), usage::fetch_kilo_zai_remaining());
        if let Some(c) = codex {
            self.codex_remaining = c;
        }
        if let Some(z) = zai {
            self.zai_remaining = z;
        }
    }

    pub fn format_quota_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let c = &self.codex_remaining;
        if c.five_hour.is_some() || c.seven_day.is_some() {
            let segs = [
                usage::format_quota_segment("5h", c.five_hour, c.five_hour_reset_sec, false),
                usage::format_quota_segment("7d", c.seven_day, c.seven_day_reset_sec, true),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !segs.is_empty() {
                lines.push(format!("codex  {}", segs.join("  ")));
            }
        }
        let z = &self.zai_remaining;
        if z.five_hour.is_some() || z.weekly.is_some() {
            let segs = [
                usage::format_quota_segment("5h", z.five_hour, z.five_hour_reset_ms.map(|ms| ms / 1000), false),
                usage::format_quota_segment("7d", z.weekly, z.weekly_reset_ms.map(|ms| ms / 1000), true),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            if !segs.is_empty() {
                lines.push(format!("zai    {}", segs.join("  ")));
            }
        }
        lines
    }

    pub fn print_status(&self) {
        let main_model = self
            .active_session_model
            .clone()
            .or_else(|| read_config_model())
            .unwrap_or_else(|| "(goose default)".to_string());
        stdout_line(&format!("\n  {}main{}", render::BOLD, render::RESET));
        if let Some(name) = self.session_names.get(&self.session_id) {
            stdout_line(&format!("     name    : {name}"));
        }
        stdout_line(&format!("     session : {}", self.session_id));
        if let Some(provider) = &self.active_session_provider {
            stdout_line(&format!("     provider: {provider}"));
        }
        stdout_line(&format!("     model   : {main_model}"));
        if let Some(opts) = &self.session_effort_options {
            if !opts.is_empty() {
                stdout_line(&format!(
                    "     thinking: {}",
                    model_search::format_effort_display(self.active_effort.as_deref(), opts)
                ));
            }
        }
        if self.main_usage.size > 0 {
            stdout_line(&format!(
                "     context : {} / {} tokens",
                self.main_usage.used, self.main_usage.size
            ));
        } else if self.main_usage.used > 0 {
            stdout_line(&format!("     context : {} tokens", self.main_usage.used));
        }
        stdout_line(&format!(
            "     cost    : {} (cumulative)",
            format_cost(self.main_usage.cost_amount, &self.main_usage.cost_currency)
        ));
        let prev: Vec<(&String, &session_stats::SessionUsage)> = self
            .session_usage
            .iter()
            .filter(|(id, u)| u.role == session_stats::SessionRole::Main && *id != &self.session_id)
            .collect();
        if !prev.is_empty() {
            stdout_line(&format!("     {}previous:{}", render::DIM, render::RESET));
            for (id, u) in prev {
                stdout_line(&format!(
                    "       {DIM}{id}  ctx {}  cost {}{RESET}",
                    if u.size > 0 {
                        format!("{} / {}", u.used, u.size)
                    } else if u.used > 0 {
                        format!("{}", u.used)
                    } else {
                        "-".to_string()
                    },
                    if u.cost_amount > 0.0 {
                        format_cost(u.cost_amount, &u.cost_currency)
                    } else {
                        "-".to_string()
                    },
                    DIM = render::DIM,
                    RESET = render::RESET
                ));
            }
        }
        let quota = self.format_quota_lines();
        if !quota.is_empty() {
            stdout_line(&format!("  {}quota{}", render::BOLD, render::RESET));
            for l in &quota {
                stdout_line(&format!("     {l}"));
            }
        }
    }
}

pub enum ModelSearchOutcome {
    Cancel,
    Back,
    Picked(String),
}

enum PromptEvent {
    None,
    Submitted(String),
    Exit,
}

fn format_cost(amount: f64, currency: &str) -> String {
    let sym = if currency.eq_ignore_ascii_case("USD") {
        "$".to_string()
    } else if currency.is_empty() {
        "$".to_string()
    } else {
        format!("{currency} ")
    };
    format!("{sym}{amount:.6}")
}

pub fn find_kilo_config_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(|_| PathBuf::from(".config"))
        });
    let p = base.join("kilo/kilo.jsonc");
    p.exists().then_some(p)
}

fn read_config_model() -> Option<String> {
    let path = find_kilo_config_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed = permissions::parse_jsonc(&raw).ok()?;
    parsed
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub fn display_config_path(abs: &std::path::Path) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let s = abs.display().to_string();
    if s == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    s.strip_prefix(&prefix)
        .map(|rest| format!("~/{rest}"))
        .unwrap_or(s)
}

// ---- entry point ----

pub static CLI_CONTINUE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static CLI_SESSION_TARGET: Mutex<Option<String>> = Mutex::new(None);

pub async fn main_main() -> Result<()> {
    // Parse CLI flags.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--continue" => CLI_CONTINUE.store(true, std::sync::atomic::Ordering::Relaxed),
            "-s" | "--session" => {
                if let Some(target) = args.get(i + 1) {
                    *CLI_SESSION_TARGET.lock().unwrap() = Some(target.clone());
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }

    term::install_panic_hook();
    term::setup()?;

    let spawn_cmd = std::env::var("GOOSE_READLINE_SPAWN").ok().filter(|s| !s.is_empty());
    let transport = match &spawn_cmd {
        Some(cmd) => Transport::Spawn { command: cmd.clone() },
        None => Transport::InProcess {
            builtins: std::env::var("GOOSE_BUILTINS")
                .unwrap_or_else(|_| "developer".to_string())
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        },
    };

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let history_paths = Paths::new();
    let result = agent::connect_and_run(transport, events_tx, move |cx| async move {
        let history = HistoryStore::load(&history_paths);
        let editor = RawInput::new(
            Box::new(term::CrosstermTerm),
            "goose> ",
            history.entries.clone(),
            Some(default_completer()),
        );
        let think_short_gap_ms = env_u64(
            &[
                "KILO_THINK_DISPLAY_SHORT_GAP",
                "KILO_THINK_SUMMARY_SHORT_GAP",
                "KILO_THINK_SUMMARY_MIN_GAP",
            ],
            15000,
        );
        let think_long_gap_ms = env_u64(
            &["KILO_THINK_DISPLAY_LONG_GAP", "KILO_THINK_SUMMARY_LONG_GAP"],
            120000,
        );
        let think_step_ms = env_u64(
            &["KILO_THINK_DISPLAY_GAP_STEP", "KILO_THINK_SUMMARY_GAP_STEP"],
            10000,
        );
        let permission_state = permissions_ui::PermissionUiState::new(&history_paths);
        let app = App {
            cx,
            events_rx,
            editor,
            history,
            paths: history_paths,
            session_id: String::new(),
            session_names: BTreeMap::new(),
            agent_caps: serde_json::Value::Null,
            active_session_provider: None,
            session_provider_options: None,
            active_session_model: None,
            session_model_options: None,
            active_effort: None,
            session_effort_options: None,
            session_usage: BTreeMap::new(),
            main_usage: session_stats::SessionUsage::default(),
            codex_remaining: CodexRemaining::default(),
            zai_remaining: ZaiRemaining::default(),
            model_state: ModelState::load(),
            think_short_gap_ms,
            think_long_gap_ms,
            think_step_ms,
            input_target: InputTarget::Discard,
            ctrl_c_exit_armed: false,
            ctrl_c_exit_at: None,
            cancel_requested: false,
            permission_state,
            global_kilo_config: find_kilo_config_path(),
            project_kilo_configs: default_project_configs(),
        };
        app.run().await
    })
    .await;

    term::teardown();
    result
}

pub fn default_project_configs() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    vec![
        cwd.join("kilo.json"),
        cwd.join("kilo.jsonc"),
        cwd.join(".kilo/kilo.json"),
        cwd.join(".kilo/kilo.jsonc"),
    ]
}

/// Default completer: slash commands + filesystem paths (port of index.ts's
/// completer).
fn default_completer() -> Completer {
    Box::new(|line: &str, cursor_col: usize| {
        let before = crate::editor::wrap::char_prefix(line, cursor_col);
        let token = before.split_whitespace().next_back().unwrap_or("").to_string();
        if token.is_empty() {
            return (Vec::new(), String::new());
        }

        // Slash commands when the token is the sole content.
        if token.starts_with('/') && before.trim() == token {
            let prefix = token.to_lowercase();
            let hits: Vec<String> = commands::COMPLETABLE_COMMANDS
                .iter()
                .filter(|c| c.starts_with(&prefix))
                .map(|c| c.to_string())
                .collect();
            if !hits.is_empty() {
                return (hits, token);
            }
        }

        // Path completion for explicit relative/home prefixes or any token
        // containing '/'.
        if token.starts_with("./")
            || token.starts_with("../")
            || token == "~"
            || token.starts_with("~/")
            || token.contains('/')
        {
            let (hits, matched) = complete_path(&token);
            if !hits.is_empty() {
                return (hits, matched);
            }
        }

        (Vec::new(), token)
    })
}

/// Complete a path token against the filesystem, returning hits in the same
/// notation the user typed. Directories are listed WITHOUT a trailing '/'.
fn complete_path(token: &str) -> (Vec<String>, String) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();

    let (fs_dir, prefix, file_prefix): (PathBuf, String, String) = if token == "~" || token.starts_with("~/") {
        let rest = if token == "~" { "" } else { &token[2..] };
        match rest.rfind('/') {
            Some(idx) => (
                home.join(&rest[..idx]),
                format!("~/{}/", &rest[..idx]),
                rest[idx + 1..].to_string(),
            ),
            None => (home, "~/".to_string(), rest.to_string()),
        }
    } else {
        let Some(idx) = token.rfind('/') else {
            return (Vec::new(), token.to_string());
        };
        let dir_part = &token[..idx];
        let file_part = token[idx + 1..].to_string();
        if dir_part.is_empty() {
            (PathBuf::from("/"), "/".to_string(), file_part)
        } else if dir_part.starts_with('/') {
            (PathBuf::from(dir_part), format!("{dir_part}/"), file_part)
        } else {
            let resolved = resolve_path(&cwd, dir_part);
            (resolved, format!("{dir_part}/"), file_part)
        }
    };

    let Ok(entries) = std::fs::read_dir(&fs_dir) else {
        return (Vec::new(), token.to_string());
    };
    let want_hidden = file_prefix.starts_with('.');
    let mut hits: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !want_hidden {
            continue;
        }
        if !name.starts_with(&file_prefix) {
            continue;
        }
        hits.push(format!("{prefix}{name}"));
    }
    hits.sort();
    (hits, token.to_string())
}

fn resolve_path(base: &std::path::Path, rel: &str) -> PathBuf {
    let mut out = base.to_path_buf();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out
}
