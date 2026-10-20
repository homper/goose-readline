// Streaming transcript renderer for one agent turn: agent message chunks
// (markdown lines, code fences, pipe tables), thought chunks with periodic
// tail display, tool call blocks with status transitions/diffs/output, usage
// lines, and a heartbeat. Port of the turn-loop rendering from
// kilo-readline src/index.ts.

use crate::app::render::*;
use crate::app::session_stats::UsageUpdate;
use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    ToolKind,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::Instant;

const KIND_ICONS: &[(&str, &str)] = &[
    ("read", "📖"),
    ("edit", "✏️"),
    ("delete", "🗑️"),
    ("move", "📦"),
    ("search", "🔍"),
    ("execute", "⚡"),
    ("think", "💭"),
    ("fetch", "🌐"),
    ("switch_mode", "🔀"),
    ("todowrite", "📋"),
    ("other", "🔧"),
];

fn kind_icon(kind: &str) -> &'static str {
    KIND_ICONS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, i)| *i)
        .unwrap_or("🔧")
}

fn kind_str(kind: ToolKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".to_string())
}

fn status_str(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending => "pending",
        ToolCallStatus::InProgress => "in_progress",
        ToolCallStatus::Completed => "completed",
        ToolCallStatus::Failed => "failed",
        _ => "other",
    }
}

fn status_color(status: &str) -> &'static str {
    match status {
        "completed" => GREEN,
        "failed" => RED,
        "in_progress" => YELLOW,
        _ => DIM,
    }
}

#[derive(Clone)]
pub struct ToolCallView {
    pub tool_call_id: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub locations: Option<Vec<ToolCallLocationView>>,
    pub content: Option<Vec<ToolCallContent>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
}

#[derive(Clone)]
pub struct ToolCallLocationView {
    pub path: String,
}

impl ToolCallView {
    fn from_call(call: &ToolCall) -> Self {
        Self {
            tool_call_id: Some(call.tool_call_id.to_string()),
            title: Some(call.title.clone()),
            kind: Some(kind_str(call.kind)),
            status: Some(status_str(call.status).to_string()),
            locations: Some(
                call.locations
                    .iter()
                    .map(|l| ToolCallLocationView { path: l.path.display().to_string() })
                    .collect(),
            ),
            content: Some(call.content.clone()),
            raw_input: call.raw_input.clone(),
            raw_output: call.raw_output.clone(),
        }
    }

    fn from_update(update: &ToolCallUpdate) -> Self {
        let f = &update.fields;
        Self {
            tool_call_id: Some(update.tool_call_id.to_string()),
            title: f.title.clone(),
            kind: f.kind.map(kind_str),
            status: f.status.map(|s| status_str(s).to_string()),
            locations: f.locations.as_ref().map(|ls| {
                ls.iter()
                    .map(|l| ToolCallLocationView { path: l.path.display().to_string() })
                    .collect()
            }),
            content: f.content.clone(),
            raw_input: f.raw_input.clone(),
            raw_output: f.raw_output.clone(),
        }
    }

    fn merge(&mut self, update: ToolCallView) {
        if self.title.is_none() {
            self.title = update.title;
        }
        if self.kind.is_none() {
            self.kind = update.kind;
        }
        if self.status.is_none() {
            self.status = update.status;
        }
        if update.locations.is_some() {
            self.locations = update.locations;
        }
        if update.content.is_some() {
            self.content = update.content;
        }
        // Inputs can arrive incrementally (workdir first, command later).
        match (&mut self.raw_input, update.raw_input) {
            (Some(base), Some(inc)) if base.is_object() && inc.is_object() => {
                if let (Some(b), Some(i)) = (base.as_object_mut(), inc.as_object()) {
                    for (k, v) in i {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
            (_, Some(inc)) => self.raw_input = Some(inc),
            _ => {}
        }
        if update.raw_output.is_some() {
            self.raw_output = update.raw_output;
        }
    }
}

fn text_of(block: &ContentBlock) -> Option<&str> {
    match block {
        ContentBlock::Text(t) => Some(&t.text),
        _ => None,
    }
}

enum DeferredOutput {
    Static(String),
    Usage { used: u64, cost_amount: f64 },
}

pub struct Transcript {
    pub response_text: String,
    output_line_buffer: String,
    in_code_fence: bool,
    table_accum: TableAccumulator,
    pub got_text: bool,
    pub got_tool: bool,
    pub used_tokens: u64,

    // Thinking display state.
    thinking_header_shown: bool,
    thinking_active: bool,
    thinking_start: Option<Instant>,
    thought_text: String,
    last_thought_text: String,
    last_shown_thought_tail: String,
    last_thought_display_at: Option<Instant>,
    thought_display_count: u32,
    thought_529_shown: bool,

    // Tool-call render state (reset per turn).
    announced_tool_calls: HashSet<String>,
    rendered_content_count: HashMap<String, usize>,
    rendered_todo_count: HashMap<String, usize>,
    known_todowrite_ids: HashSet<String>,
    observed_tool_calls: HashMap<String, ToolCallView>,
    last_rendered_tool_call_id: Option<String>,
    last_status: HashMap<String, String>,
    rendered_input_summary_parts: HashMap<String, HashSet<String>>,
    rendered_edit_diff_ids: HashSet<String>,

    deferred_outputs: Vec<DeferredOutput>,
    pub latest_usage: Option<UsageUpdate>,
    pub usage_cost_amount: f64,

    // Heartbeat.
    turn_start: Instant,
    last_activity: Instant,
    active_tool_label: Option<String>,
    last_heartbeat_emit: Option<Instant>,
}

fn stdout_out(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
}

fn stderr_line(s: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(s.as_bytes());
    let _ = err.write_all(b"\n");
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            response_text: String::new(),
            output_line_buffer: String::new(),
            in_code_fence: false,
            table_accum: TableAccumulator::default(),
            got_text: false,
            got_tool: false,
            used_tokens: 0,
            thinking_header_shown: false,
            thinking_active: false,
            thinking_start: None,
            thought_text: String::new(),
            last_thought_text: String::new(),
            last_shown_thought_tail: String::new(),
            last_thought_display_at: None,
            thought_display_count: 0,
            thought_529_shown: false,
            announced_tool_calls: HashSet::new(),
            rendered_content_count: HashMap::new(),
            rendered_todo_count: HashMap::new(),
            known_todowrite_ids: HashSet::new(),
            observed_tool_calls: HashMap::new(),
            last_rendered_tool_call_id: None,
            last_status: HashMap::new(),
            rendered_input_summary_parts: HashMap::new(),
            rendered_edit_diff_ids: HashSet::new(),
            deferred_outputs: Vec::new(),
            latest_usage: None,
            usage_cost_amount: 0.0,
            turn_start: Instant::now(),
            last_activity: Instant::now(),
            active_tool_label: None,
            last_heartbeat_emit: None,
        }
    }

    pub fn touch_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    fn end_thinking(&mut self) {
        if self.thinking_active {
            self.thinking_active = false;
            self.last_thought_text = self.thought_text.trim().to_string();
        }
    }

    /// Echo the last few non-empty lines of the most recent thinking block.
    fn print_recent_thinking(&mut self, text: &str, elapsed: Option<u64>) -> bool {
        if text.is_empty() {
            return false;
        }
        const RECENT_LINES: usize = 3;
        const LINE_MAX: usize = 200;
        let mut lines: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| {
                if l.chars().count() > LINE_MAX {
                    let tail: String = l.chars().skip(l.chars().count() - (LINE_MAX - 1)).collect();
                    format!("…{tail}")
                } else {
                    l.to_string()
                }
            })
            .collect();
        if lines.len() > RECENT_LINES {
            lines = lines.split_off(lines.len() - RECENT_LINES);
        }
        if lines.is_empty() {
            return false;
        }
        let tail = lines.join("\n");
        if tail == self.last_shown_thought_tail {
            return false;
        }
        let timing = match elapsed {
            Some(e) => format!(" ({e}s)"),
            None => String::new(),
        };
        stderr_line(&format!("{GRAY}   💭 last thoughts{timing}:{RESET}"));
        for line in &lines {
            stderr_line(&format!("{GRAY}   {line}{RESET}"));
        }
        self.last_shown_thought_tail = tail;
        true
    }

    fn current_thought_display_gap(&self, short_gap_ms: u64, step_ms: u64, long_gap_ms: u64) -> u64 {
        let gap = short_gap_ms + self.thought_display_count as u64 * step_ms;
        gap.min(long_gap_ms)
    }

    /// Print agent response text held back because it didn't end in a
    /// newline, so it appears before a tool block instead of after the turn.
    pub fn flush_pending_output(&mut self) {
        if !self.output_line_buffer.is_empty() {
            let buf = std::mem::take(&mut self.output_line_buffer);
            if self.in_code_fence {
                stdout_out(&buf);
            } else {
                let is_table = is_table_row(&buf);
                if let Some(flushed) = self.table_accum.push(&buf, terminal_width()) {
                    stdout_out(&flushed);
                }
                if !is_table {
                    stdout_out(&format!("{}\n", format_markdown_line(&buf)));
                }
            }
        }
        if let Some(flushed) = self.table_accum.flush(terminal_width()) {
            stdout_out(&flushed);
        }
    }

    /// Process one session update. Returns true when the turn should end
    /// (never for updates; the caller handles the prompt response).
    pub fn process_update(&mut self, update: &SessionUpdate) {
        self.touch_activity();
        match update {
            SessionUpdate::UserMessageChunk(_) => {}
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.end_thinking();
                if let Some(text) = text_of(&chunk.content) {
                    self.got_text = true;
                    self.response_text.push_str(text);
                    self.output_line_buffer.push_str(text);
                    let mut nl = self.output_line_buffer.find('\n');
                    while let Some(idx) = nl {
                        let line = self.output_line_buffer[..idx].to_string();
                        self.output_line_buffer = self.output_line_buffer[idx + 1..].to_string();
                        let is_fence = line.trim_start().starts_with("```");
                        if self.in_code_fence {
                            stdout_out(&format!("{line}\n"));
                            if is_fence {
                                self.in_code_fence = false;
                            }
                        } else {
                            if is_fence {
                                self.in_code_fence = true;
                            }
                            let is_table = is_table_row(&line);
                            if let Some(flushed) = self.table_accum.push(&line, terminal_width()) {
                                stdout_out(&flushed);
                            }
                            if !is_table {
                                stdout_out(&format!("{}\n", format_markdown_line(&line)));
                            }
                        }
                        nl = self.output_line_buffer.find('\n');
                    }
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if !self.thinking_header_shown {
                    self.thinking_header_shown = true;
                    stderr_line(&format!("{GRAY}💭 thinking…{RESET}"));
                }
                if !self.thinking_active {
                    self.thinking_active = true;
                    self.thinking_start = Some(Instant::now());
                    self.thought_text.clear();
                    self.thought_529_shown = false;
                    self.last_shown_thought_tail.clear();
                    self.last_thought_display_at = Some(Instant::now());
                    self.thought_display_count = 0;
                }
                if let Some(text) = text_of(&chunk.content) {
                    self.thought_text.push_str(text);
                    if !self.thought_529_shown && text.contains("529") {
                        self.thought_529_shown = true;
                        stderr_line(&format!("{RED}529{RESET}"));
                    }
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.end_thinking();
                let view = ToolCallView::from_call(call);
                if !self.is_unknown_or_invalid(&view) {
                    self.got_tool = true;
                    self.flush_pending_output();
                    let tail = self.last_thought_text.clone();
                    self.print_recent_thinking(&tail, None);
                    let what = plain_what(view.raw_input.as_ref());
                    let label = if !what.is_empty() {
                        format!("{} {}", view.title.clone().unwrap_or_default(), what)
                    } else {
                        format!(
                            "{} [{}]",
                            view.title.clone().unwrap_or_default(),
                            view.kind.clone().unwrap_or_else(|| "other".into())
                        )
                    };
                    self.active_tool_label = Some(label);
                    let id = view.tool_call_id.clone().unwrap_or_default();
                    self.observe(id.clone(), view.clone());
                    self.render_tool_call(&view, false, true, false);
                } else {
                    self.got_tool = true;
                }
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.end_thinking();
                let view = ToolCallView::from_update(update);
                if self.is_unknown_or_invalid(&view) {
                    self.active_tool_label = None;
                    return;
                }
                self.flush_pending_output();
                let terminal_status = matches!(
                    view.status.as_deref(),
                    Some("completed") | Some("failed")
                );
                if terminal_status {
                    self.active_tool_label = None;
                }
                let id = view.tool_call_id.clone().unwrap_or_default();
                self.observe(id, view.clone());
                self.render_tool_call(&view, true, true, false);
            }
            SessionUpdate::Plan(_) => {
                self.end_thinking();
                self.deferred_outputs
                    .push(DeferredOutput::Static("📋 [plan]".to_string()));
            }
            SessionUpdate::AvailableCommandsUpdate(u) => {
                self.end_thinking();
                self.deferred_outputs.push(DeferredOutput::Static(format!(
                    "📋 {} commands available",
                    u.available_commands.len()
                )));
            }
            SessionUpdate::UsageUpdate(u) => {
                self.end_thinking();
                self.used_tokens = u.used;
                let cost_amount = u.cost.as_ref().map(|c| c.amount).unwrap_or(0.0);
                self.usage_cost_amount = cost_amount;
                self.latest_usage = Some(UsageUpdate {
                    used: Some(u.used),
                    size: Some(u.size),
                    cost_amount: Some(cost_amount),
                    cost_currency: u.cost.as_ref().map(|c| c.currency.clone()),
                });
                self.deferred_outputs.push(DeferredOutput::Usage {
                    used: u.used,
                    cost_amount,
                });
            }
            SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_) => {}
            _ => {
                self.end_thinking();
                let raw = serde_json::to_string(update).unwrap_or_default();
                self.deferred_outputs
                    .push(DeferredOutput::Static(format!("[update] {raw}")));
            }
        }
    }

    fn is_unknown_or_invalid(&self, view: &ToolCallView) -> bool {
        matches!(
            view.title.as_deref().map(str::to_lowercase).as_deref(),
            Some("unknown") | Some("invalid")
        )
    }

    fn observe(&mut self, id: String, view: ToolCallView) {
        self.observed_tool_calls
            .entry(id)
            .and_modify(|existing| existing.merge(ToolCallView {
                tool_call_id: None,
                title: view.title.clone(),
                kind: view.kind.clone(),
                status: view.status.clone(),
                locations: view.locations.clone(),
                content: view.content.clone(),
                raw_input: view.raw_input.clone(),
                raw_output: view.raw_output.clone(),
            }))
            .or_insert(view);
    }

    fn observed(&self, id: &str) -> Option<&ToolCallView> {
        self.observed_tool_calls.get(id)
    }

    /// Full tool-call render: header + input summary, status transitions,
    /// edit diffs, todo lists, and streamed content.
    fn render_tool_call(&mut self, view: &ToolCallView, is_update: bool, show_body: bool, force: bool) {
        let kind = view.kind.clone().unwrap_or_else(|| "other".into());
        let title = view.title.clone().unwrap_or_default();
        let display_title = if !title.is_empty() {
            title.clone()
        } else {
            view.tool_call_id.clone().unwrap_or_else(|| "(untitled)".into())
        };
        let id = view
            .tool_call_id
            .clone()
            .unwrap_or_else(|| title.to_lowercase());
        self.last_rendered_tool_call_id = Some(id.clone());
        let is_todowrite = kind == "todowrite" || title.to_lowercase() == "todowrite";
        if is_todowrite {
            self.known_todowrite_ids.insert(id.clone());
        }
        let icon = kind_icon(&kind);
        let status = view.status.clone().unwrap_or_else(|| {
            if is_update {
                String::new()
            } else {
                "pending".to_string()
            }
        });
        let terminal = status == "completed" || status == "failed";

        if force || !self.announced_tool_calls.contains(&id) {
            self.announced_tool_calls.insert(id.clone());
            let status_label = if !status.is_empty() {
                format!(" {}{}{}", status_color(&status), status, RESET)
            } else {
                String::new()
            };
            stdout_out(&format!(
                "\n{icon} {BOLD}{display_title}{RESET} {BLUE}[{kind}]{RESET}{status_label}\n"
            ));
            if force {
                self.render_input_parts_multiline(&id, view.raw_input.as_ref());
            } else {
                let summary = self.new_input_summary(&id, view.raw_input.as_ref());
                if !summary.is_empty() {
                    stdout_out(&format!("   {summary}\n"));
                }
            }
            if !status.is_empty() {
                self.last_status.insert(id.clone(), status.clone());
            }
        } else if !status.is_empty() {
            let summary = self.new_input_summary(&id, view.raw_input.as_ref());
            if !summary.is_empty() {
                stdout_out(&format!("   {summary}\n"));
            }
            if !terminal && self.last_status.get(&id).map(String::as_str) == Some(status.as_str()) {
                return;
            }
            self.last_status.insert(id.clone(), status.clone());
            let result = if terminal { self.summarize_result(view) } else { None };
            let result_label = result
                .map(|r| format!(" {DIM}({r}){RESET}"))
                .unwrap_or_default();
            stdout_out(&format!(
                "   {}↳ {status}{RESET}{result_label}\n",
                status_color(&status)
            ));
        }

        // Edit diffs: show before approval as soon as old/new text arrives.
        if kind == "edit" && !self.rendered_edit_diff_ids.contains(&id) {
            if self.render_edit_diff_from_raw(view.raw_input.as_ref()) {
                self.rendered_edit_diff_ids.insert(id.clone());
            }
        }

        if is_todowrite {
            if let Some(Value::Array(todos)) = view.raw_input.as_ref().map(|v| v.get("todos")).flatten() {
                let prev_count = self.rendered_todo_count.get(&id).copied().unwrap_or(0);
                if force || todos.len() != prev_count || !self.announced_tool_calls.contains(&id) {
                    self.rendered_todo_count.insert(id.clone(), todos.len());
                    for todo in todos {
                        let obj = todo.as_object();
                        let get = |k: &str| {
                            obj.and_then(|o| o.get(k))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        };
                        let status = get("status");
                        let priority = get("priority");
                        let content = get("content");
                        let icon = match status.as_str() {
                            "completed" => format!("{GREEN}✓{RESET}"),
                            "in_progress" => format!("{YELLOW}▶{RESET}"),
                            "cancelled" => format!("{RED}✗{RESET}"),
                            _ => format!("{DIM}☐{RESET}"),
                        };
                        let prio = match priority.as_str() {
                            "high" => format!(" {RED}[!]{RESET}"),
                            "medium" => format!(" {YELLOW}[·]{RESET}"),
                            _ => String::new(),
                        };
                        let text = if status == "completed" || status == "cancelled" {
                            format!("{DIM}{content}{RESET}")
                        } else {
                            content
                        };
                        stdout_out(&format!("   {icon}{prio} {text}\n"));
                    }
                }
            }
        }

        // Stream content items as they arrive; only new items are emitted.
        if show_body && !is_todowrite && !self.known_todowrite_ids.contains(&id) {
            if let Some(content) = &view.content {
                let prev_count = self.rendered_content_count.get(&id).copied().unwrap_or(0);
                let total = content.len();
                for item in content.iter().skip(prev_count) {
                    match item {
                        ToolCallContent::Diff(diff) => {
                            if kind == "edit" && self.rendered_edit_diff_ids.contains(&id) {
                                continue;
                            }
                            self.render_diff(&diff.path.display().to_string(), diff.old_text.as_deref(), &diff.new_text);
                        }
                        ToolCallContent::Content(c) => {
                            if let ContentBlock::Text(t) = &c.content {
                                let title_l = title.to_lowercase();
                                if kind == "search" {
                                    self.render_search_output(&t.text);
                                } else if kind == "read" || title_l == "read" {
                                    self.render_trimmed_output(&t.text, 20);
                                } else if kind == "compact"
                                    || title_l == "compact"
                                    || title_l == "compaction"
                                {
                                    self.render_trimmed_output(&t.text, 20);
                                } else if kind == "execute"
                                    || title_l == "bash"
                                    || title_l == "command"
                                    || title_l == "shell"
                                {
                                    self.render_trimmed_output(&t.text, 100);
                                } else if kind == "fetch" {
                                    self.render_trimmed_output(&t.text, 200);
                                } else {
                                    self.render_trimmed_output(&t.text, 100);
                                }
                            }
                        }
                        ToolCallContent::Terminal(_) => {}
                        _ => {}
                    }
                }
                if total > prev_count {
                    self.rendered_content_count.insert(id.clone(), total);
                }
            }
        }
    }

    fn summarize_result(&self, view: &ToolCallView) -> Option<String> {
        if let Some(out) = &view.raw_output {
            if let Some(obj) = out.as_object() {
                if let Some(n) = obj.get("matchCount").and_then(Value::as_f64) {
                    return Some(format!("{} match(es)", n as u64));
                }
                if let Some(Value::Array(m)) = obj.get("matches") {
                    return Some(format!("{} match(es)", m.len()));
                }
                if let Some(Value::Array(f)) = obj.get("files") {
                    return Some(format!("{} file(s)", f.len()));
                }
                if let Some(Value::Array(r)) = obj.get("results") {
                    return Some(format!("{} result(s)", r.len()));
                }
            }
        }
        if let Some(content) = &view.content {
            for item in content {
                if let ToolCallContent::Content(c) = item {
                    if let ContentBlock::Text(t) = &c.content {
                        let lines = t.text.lines().filter(|l| !l.trim().is_empty()).count();
                        if lines > 0 {
                            return Some(format!("{} line(s) of output", lines));
                        }
                    }
                }
            }
        }
        None
    }

    fn render_diff(&self, path: &str, old_text: Option<&str>, new_text: &str) {
        stdout_out(&format!("{CYAN}   📝 diff {path}{RESET}\n"));
        let old_lines: Vec<&str> = old_text.unwrap_or("").split('\n').collect();
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        for line in diff_lines(&old_lines, &new_lines) {
            match line.kind {
                DiffLineKind::Removed => stdout_out(&format!("{RED}   - {}{RESET}\n", line.text)),
                DiffLineKind::Added => stdout_out(&format!("{GREEN}   + {}{RESET}\n", line.text)),
                DiffLineKind::Context => stdout_out(&format!("     {}\n", line.text)),
            }
        }
    }

    fn render_edit_diff_from_raw(&mut self, raw: Option<&Value>) -> bool {
        let Some(obj) = raw.and_then(Value::as_object) else {
            return false;
        };
        let get = |k: &str| obj.get(k).and_then(Value::as_str);
        let path = get("filePath")
            .or_else(|| get("filepath"))
            .or_else(|| get("path"))
            .unwrap_or("(file)");
        let old_str = get("oldString").or_else(|| get("old_str")).or_else(|| get("oldText"));
        let new_str = get("newString").or_else(|| get("new_str")).or_else(|| get("newText"));
        if let Some(new_str) = new_str {
            self.render_diff(path, old_str, new_str);
            return true;
        }
        if let Some(full) = get("content").or_else(|| get("contents")) {
            let old = std::fs::read_to_string(path).ok();
            self.render_diff(path, old.as_deref(), full);
            return true;
        }
        if let Some(diff_str) = get("diff") {
            if !diff_str.trim().is_empty() {
                stdout_out(&format!("{CYAN}   📝 diff {path}{RESET}\n"));
                for line in diff_str.split('\n') {
                    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("Index:") || line.starts_with("===") || line.starts_with("diff ") {
                        stdout_out(&format!("{DIM}   {line}{RESET}\n"));
                    } else if line.starts_with("@@") {
                        stdout_out(&format!("{CYAN}   {line}{RESET}\n"));
                    } else if line.starts_with('+') {
                        stdout_out(&format!("{GREEN}   {line}{RESET}\n"));
                    } else if line.starts_with('-') {
                        stdout_out(&format!("{RED}   {line}{RESET}\n"));
                    } else {
                        stdout_out(&format!("   {line}\n"));
                    }
                }
                return true;
            }
        }
        false
    }

    fn render_search_output(&self, text: &str) {
        const MAX: usize = 30;
        let mut found_total: Option<usize> = None;
        let mut current: Option<String> = None;
        let mut lines: Vec<String> = Vec::new();
        for raw in text.split('\n') {
            if raw.is_empty() {
                continue;
            }
            if let Some(rest) = raw.strip_prefix("Found ") {
                if let Some(n) = rest.split_whitespace().next().and_then(|w| w.parse::<usize>().ok()) {
                    found_total = Some(n);
                    continue;
                }
            }
            let indented = raw.starts_with(' ') && raw.trim_start().len() < raw.len();
            if !indented && raw.ends_with(':') {
                current = Some(raw[..raw.len() - 1].to_string());
            } else if indented {
                let m: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
                if !m.is_empty() {
                    match &current {
                        Some(c) => lines.push(format!("{c}:{m}")),
                        None => lines.push(m),
                    }
                }
            } else {
                lines.push(raw.to_string());
                current = None;
            }
        }
        let shown: Vec<String> = if lines.len() > MAX {
            let head = MAX.div_ceil(2);
            let tail = MAX - head;
            let mut v = lines[..head].to_vec();
            v.push(format!("... ({} lines truncated) ...", lines.len() - MAX));
            v.extend(lines[lines.len() - tail..].iter().cloned());
            v
        } else {
            lines.clone()
        };
        for l in &shown {
            stdout_out(&format!("   {DIM}{}{RESET}\n", trim_line(l)));
        }
        if let Some(total) = found_total {
            if total > lines.len() {
                stdout_out(&format!("   {DIM}... ({total} total matches){RESET}\n"));
            }
        }
    }

    fn render_trimmed_output(&self, text: &str, max_lines: usize) {
        let all_lines: Vec<&str> = text.split('\n').collect();
        let lines: Vec<String> = if all_lines.len() > max_lines {
            let head = max_lines.div_ceil(2);
            let tail = max_lines - head;
            let mut v: Vec<String> = all_lines[..head].iter().map(|s| s.to_string()).collect();
            v.push(format!("... ({} lines truncated) ...", all_lines.len() - max_lines));
            v.extend(all_lines[all_lines.len() - tail..].iter().map(|s| s.to_string()));
            v
        } else {
            all_lines.iter().map(|s| s.to_string()).collect()
        };
        let joined: Vec<String> = lines.iter().map(|l| trim_line(l)).collect();
        stdout_out(&format!("   {DIM}{}{RESET}\n", joined.join("\n   ")));
    }

    /// Render a permission-context tool call, repeating the originating
    /// header when another call printed in between.
    pub fn render_permission_tool_context(&mut self, view: &ToolCallView, repeat: bool) {
        let id = view
            .tool_call_id
            .clone()
            .or_else(|| view.title.clone())
            .unwrap_or_default();
        let display = match self.observed(&id) {
            Some(orig) => {
                let mut merged = ToolCallView {
                    tool_call_id: Some(id.clone()),
                    title: Some(view.title.clone().unwrap_or_default()),
                    kind: view.kind.clone(),
                    status: view.status.clone(),
                    locations: None,
                    content: None,
                    raw_input: view.raw_input.clone(),
                    raw_output: None,
                };
                if merged.title.as_deref() == Some("") {
                    merged.title = orig.title.clone();
                }
                if merged.kind.is_none() {
                    merged.kind = orig.kind.clone();
                }
                if let Some(orig_input) = &orig.raw_input {
                    match (&mut merged.raw_input, orig_input) {
                        (Some(base), inc) if base.is_object() && inc.is_object() => {
                            if let (Some(b), Some(i)) = (base.as_object_mut(), inc.as_object()) {
                                for (k, v) in i {
                                    b.entry(k.clone()).or_insert(v.clone());
                                }
                            }
                        }
                        (None, inc) => merged.raw_input = Some(inc.clone()),
                        _ => {}
                    }
                }
                merged.locations = view.locations.clone().or_else(|| orig.locations.clone());
                merged.content = orig.content.clone();
                merged
            }
            None => view.clone(),
        };
        let disp_id = display
            .tool_call_id
            .clone()
            .or_else(|| display.title.clone())
            .unwrap_or_default();
        if repeat {
            self.rendered_input_summary_parts.remove(&disp_id);
            self.render_tool_call(&display, false, true, true);
            return;
        }
        if !self.announced_tool_calls.contains(&disp_id)
            || self.last_rendered_tool_call_id.as_deref() != Some(disp_id.as_str())
        {
            self.render_tool_call(&display, false, true, true);
            return;
        }
        let summary = self.new_input_summary(&disp_id, display.raw_input.as_ref());
        if !summary.is_empty() {
            stdout_out(&format!("   {summary}\n"));
        }
        let kind = display.kind.clone().unwrap_or_else(|| "other".into());
        if kind == "edit" && !self.rendered_edit_diff_ids.contains(&disp_id) {
            if self.render_edit_diff_from_raw(display.raw_input.as_ref()) {
                self.rendered_edit_diff_ids.insert(disp_id.clone());
            }
        }
        self.last_rendered_tool_call_id = Some(disp_id);
    }

    pub fn observed_view(&self, id: &str) -> Option<&ToolCallView> {
        self.observed_tool_calls.get(id)
    }

    fn new_input_summary(&mut self, id: &str, input: Option<&Value>) -> String {
        let parts = summarize_input_parts(input);
        let rendered = self
            .rendered_input_summary_parts
            .entry(id.to_string())
            .or_default();
        let new_parts: Vec<String> = parts
            .into_iter()
            .filter(|p| !rendered.contains(p))
            .collect();
        for p in &new_parts {
            rendered.insert(p.clone());
        }
        new_parts.join(&format!("  {DIM}|{RESET}  "))
    }

    fn render_input_parts_multiline(&mut self, id: &str, input: Option<&Value>) {
        let parts = summarize_input_parts(input);
        if parts.is_empty() {
            return;
        }
        let rendered = self
            .rendered_input_summary_parts
            .entry(id.to_string())
            .or_default();
        for (i, part) in parts.iter().enumerate() {
            rendered.insert(part.clone());
            let prefix = if i == 0 { "   " } else { &format!("   {DIM}|{RESET}  ") };
            stdout_out(&format!("{prefix}{part}\n"));
        }
    }

    /// End-of-turn flush: trailing buffer, tables, deferred outputs.
    pub fn finish_turn(&mut self, quota_lines: &[String]) {
        if !self.output_line_buffer.is_empty() {
            let buf = std::mem::take(&mut self.output_line_buffer);
            if self.in_code_fence {
                stdout_out(&buf);
            } else {
                let is_table = is_table_row(&buf);
                if let Some(flushed) = self.table_accum.push(&buf, terminal_width()) {
                    stdout_out(&flushed);
                }
                if !is_table {
                    stdout_out(&format_markdown_line(&buf));
                }
            }
        }
        if let Some(flushed) = self.table_accum.flush(terminal_width()) {
            stdout_out(&flushed);
        }
        if !self.response_text.is_empty() && !self.response_text.ends_with('\n') {
            println!();
        }
        for out in std::mem::take(&mut self.deferred_outputs) {
            match out {
                DeferredOutput::Static(s) => println!("\n{s}"),
                DeferredOutput::Usage { used, cost_amount } => {
                    let head = format!("🔢 {used} tokens used, cost ${cost_amount:.6}");
                    if quota_lines.is_empty() {
                        println!("\n{head}");
                    } else {
                        println!(
                            "\n{head}\n{}",
                            quota_lines
                                .iter()
                                .map(|l| format!("     {l}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
                    }
                }
            }
        }
        println!("\n------------------------------------------------");
    }

    pub fn reset_for_turn(&mut self) {
        self.announced_tool_calls.clear();
        self.rendered_content_count.clear();
        self.rendered_todo_count.clear();
        self.known_todowrite_ids.clear();
        self.observed_tool_calls.clear();
        self.last_rendered_tool_call_id = None;
        self.last_status.clear();
        self.rendered_input_summary_parts.clear();
        self.rendered_edit_diff_ids.clear();
        self.response_text.clear();
        self.got_text = false;
        self.got_tool = false;
        self.used_tokens = 0;
        self.usage_cost_amount = 0.0;
        self.thinking_header_shown = false;
        self.active_tool_label = None;
        self.turn_start = Instant::now();
        self.last_activity = Instant::now();
        self.last_heartbeat_emit = None;
    }

    /// Heartbeat tick: periodic "still working…" lines and adaptive
    /// thought-tail display. Returns normally; prints to stderr.
    pub fn heartbeat_tick(
        &mut self,
        awaiting_user_input: bool,
        last_user_input_ended: Option<Instant>,
        short_gap_ms: u64,
        step_ms: u64,
        long_gap_ms: u64,
    ) {
        if awaiting_user_input {
            return;
        }
        if let Some(ended) = last_user_input_ended {
            if ended.elapsed().as_millis() < 5000 {
                return;
            }
        }

        if self.thinking_active
            && self
                .last_thought_display_at
                .map(|t| t.elapsed().as_millis() as u64 >= self.current_thought_display_gap(short_gap_ms, step_ms, long_gap_ms))
                .unwrap_or(false)
        {
            let dur = self
                .thinking_start
                .map(|t| (t.elapsed().as_secs() as u64).max(1))
                .unwrap_or(1);
            let text = self.thought_text.trim().to_string();
            if self.print_recent_thinking(&text, Some(dur)) {
                self.last_thought_display_at = Some(Instant::now());
                self.thought_display_count += 1;
            }
        }

        let idle_ms = self.last_activity.elapsed().as_millis() as u64;
        if idle_ms < 3000 {
            return;
        }
        let min_gap = (10_000u64).min(3000 + idle_ms / 3);
        if let Some(last) = self.last_heartbeat_emit {
            if (last.elapsed().as_millis() as u64) < min_gap {
                return;
            }
        }

        let elapsed = (self.turn_start.elapsed().as_secs() as u64).max(0);
        let what = if let Some(label) = &self.active_tool_label {
            format!("running {label}")
        } else if self.thinking_active {
            "thinking".to_string()
        } else {
            "working".to_string()
        };
        let hint = if idle_ms >= 10000 {
            format!("  {DIM}(Ctrl+C to cancel){RESET}")
        } else {
            String::new()
        };
        stderr_line(&format!(
            "{GRAY}   ⏳ still {what}… ({}s idle, {elapsed}s total){RESET}{hint}",
            (idle_ms / 1000).max(1)
        ));
        self.last_heartbeat_emit = Some(Instant::now());
    }
}

/// Fields we know how to show nicely, in priority order.
const INPUT_FIELD_LABELS: &[(&str, &str)] = &[
    ("command", ""),
    ("cmd", ""),
    ("workdir", "workdir"),
    ("cwd", "workdir"),
    ("pattern", "pattern"),
    ("query", "query"),
    ("regex", "regex"),
    ("include", "include"),
    ("glob", "glob"),
    ("filePath", "file"),
    ("filepath", "file"),
    ("path", "path"),
    ("parentDir", "dir"),
    ("url", "url"),
    ("name", "name"),
    ("description", ""),
];

fn summarize_input_parts(input: Option<&Value>) -> Vec<String> {
    let Some(obj) = input.and_then(Value::as_object) else {
        return Vec::new();
    };
    let get = |k: &str| obj.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let mut parts = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (key, label) in INPUT_FIELD_LABELS {
        if *key == "cmd" && seen.contains(&"command") {
            continue;
        }
        if *key == "cwd" && seen.contains(&"workdir") {
            continue;
        }
        if *key == "path" && seen.contains(&"filePath") {
            continue;
        }
        if *key == "filepath" && (seen.contains(&"filePath") || seen.contains(&"path")) {
            continue;
        }
        if let Some(val) = get(key) {
            seen.push(key);
            if *key == "command" || *key == "cmd" {
                parts.push(format!("{GREEN}$ {val}{RESET}"));
            } else if !label.is_empty() {
                parts.push(format!("{DIM}{label}:{RESET} {YELLOW}{val}{RESET}"));
            } else {
                parts.push(val.to_string());
            }
        }
    }
    if let Some(_fp) = get("filePath") {
        let offset = obj.get("offset").and_then(Value::as_f64);
        let limit = obj.get("limit").and_then(Value::as_f64);
        if let Some(offset) = offset.filter(|o| *o > 0.0) {
            let start = offset as u64;
            let end = limit.filter(|l| *l > 0.0).map(|l| start + l as u64 - 1);
            let range = match end {
                Some(e) => format!("{start}-{e}"),
                None => format!("from {start}"),
            };
            parts.push(format!("{DIM}lines:{RESET} {YELLOW}{range}{RESET}"));
        } else if let Some(limit) = limit.filter(|l| *l > 0.0) {
            parts.push(format!("{DIM}lines:{RESET} {YELLOW}1-{}{RESET}", limit as u64));
        }
    }
    parts
}

fn plain_what(input: Option<&Value>) -> String {
    let Some(obj) = input.and_then(Value::as_object) else {
        return String::new();
    };
    for key in [
        "command", "cmd", "pattern", "query", "regex", "include", "glob", "filePath",
        "filepath", "path", "parentDir", "url", "description",
    ] {
        if let Some(v) = obj.get(key).and_then(Value::as_str) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

fn terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .max(1)
}
