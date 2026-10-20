// Slash-command table + parsing. Port of slashCommand.ts and the command
// table in index.ts.

use crate::app::render;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SlashCommandInvocation {
    pub command: String,
    pub args: String,
}

/// Split a slash-command line without treating ordinary prompts or absolute
/// paths as commands.
pub fn parse_slash_command_invocation(input: &str) -> Option<SlashCommandInvocation> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    match trimmed.find(char::is_whitespace) {
        None => Some(SlashCommandInvocation {
            command: trimmed.to_lowercase(),
            args: String::new(),
        }),
        Some(sep) => Some(SlashCommandInvocation {
            command: trimmed[..sep].to_lowercase(),
            args: trimmed[sep..].trim().to_string(),
        }),
    }
}

pub const COMMAND_HELP: &[(&str, &str)] = &[
    ("/exit", "Exit the interactive client"),
    ("/quit", "Exit the interactive client"),
    ("/sessions", "Show recent sessions (arg = page; offset = arg×10)"),
    ("/resume", "Resume / switch to a previous session"),
    ("/continue", "Continue the most recent session"),
    ("/new", "Start a new session (alias: /clear)"),
    ("/clear", "Clear and start a new session"),
    ("/compact", "Compact the session to save context"),
    ("/copy", "Copy the latest agent response"),
    ("/export", "Export the session transcript"),
    ("/models", "Switch the active model (alias: /model)"),
    ("/model", "Switch the active model (in place, from config + candidates)"),
    ("/thinking", "Choose the thinking/reasoning (effort) level for the active model"),
    ("/status", "Show sessions, models, and token/cost usage"),
    ("/codex-balance", "Show Codex (ChatGPT subscription) rate-limit usage"),
    ("/zai-balance", "Show Z.AI GLM Coding Plan quota usage, 5h + weekly (alias: /glm-balance)"),
    ("/glm-balance", "Show Z.AI GLM Coding Plan quota usage (alias of /zai-balance)"),
    ("/openrouter-balance", "Show OpenRouter credit balance and key usage (day/week/month)"),
    ("/help", "Show this list of commands and what they do"),
    ("/q", "Exit the client"),
];

pub const IMPLEMENTED_COMMANDS: &[&str] = &[
    "/exit",
    "/quit",
    "/q",
    "/help",
    "/status",
    "/sessions",
    "/compact",
    "/model",
    "/models",
    "/thinking",
    "/codex-balance",
    "/zai-balance",
    "/glm-balance",
    "/openrouter-balance",
    "/resume",
    "/new",
    "/clear",
];

/// Commands excluded from the no-arg rejection set (they take a page arg).
pub const PAGED_COMMANDS: &[&str] = &["/sessions"];

pub fn commands() -> Vec<&'static str> {
    COMMAND_HELP.iter().map(|(c, _)| *c).collect()
}

pub fn is_no_arg_command(command: &str) -> bool {
    // Everything except paged commands and /help aliases rejects args.
    let cmds = commands();
    if cmds.contains(&command) {
        return !PAGED_COMMANDS.contains(&command);
    }
    matches!(command, "/h" | "/?")
}

pub fn is_stub_command(trimmed: &str) -> bool {
    let cmds = commands();
    cmds.contains(&trimmed) && !IMPLEMENTED_COMMANDS.contains(&trimmed)
}

pub const COMPLETABLE_COMMANDS: &[&str] = &[
    "/exit", "/quit", "/q", "/help", "/status", "/sessions", "/compact", "/model", "/models",
    "/thinking", "/codex-balance", "/zai-balance", "/glm-balance", "/openrouter-balance",
    "/resume", "/new", "/clear",
];

pub fn print_command_help() {
    let items: Vec<(String, String, usize)> = COMMAND_HELP
        .iter()
        .map(|(c, desc)| {
            let implemented = IMPLEMENTED_COMMANDS.contains(c);
            let code = if implemented { render::GREEN } else { render::CYAN };
            let colored = format!("{code}{c}{}", render::RESET);
            let width = crate::editor::wrap::display_width(&colored);
            (colored, format!("{}{desc}{}", render::DIM, render::RESET), width)
        })
        .collect();
    let col_width = items.iter().map(|i| i.2).max().unwrap_or(0);
    let mut out = format!(
        "\n{}Commands{}  {DIM}({GREEN}green{RESET}{DIM}=available, {CYAN}cyan{RESET}{DIM}=agent-side){RESET}\n",
        render::BOLD,
        render::RESET,
        DIM = render::DIM,
        GREEN = render::GREEN,
        CYAN = render::CYAN,
        RESET = render::RESET
    );
    for (cmd, desc, w) in items {
        out.push_str(&format!("  {cmd}{}  {desc}\n", " ".repeat(col_width - w)));
    }
    eprint!("{out}");
    let _ = Value::Null; // keep serde_json import used in tests only builds
}
