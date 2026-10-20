// Subscription quota usage reporting for /status and the /zai-balance,
// /codex-balance, /openrouter-balance commands. Ports of zaiUsage.ts,
// codexUsage.ts, and openrouterBalance.ts from kilo-readline (reqwest instead
// of fetch).

use crate::app::render::{BOLD, DIM, GREEN, GRAY, RED, RESET, YELLOW};
use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

const NETWORK_TIMEOUT_MS: u64 = 12_000;

pub fn kilo_auth_path() -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".local/share"));
    data_home.join("kilo/auth.json")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

fn read_auth_json() -> Option<Value> {
    std::fs::read_to_string(kilo_auth_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

async fn http_get_json(url: &str, headers: &[(&str, String)]) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(NETWORK_TIMEOUT_MS))
        .build()?;
    let mut req = client.get(url);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("usage fetch failed ({status}): {text}"));
    }
    Ok(serde_json::from_str(&text)?)
}

// --- shared format helpers (codexUsage.ts ports) ---

pub fn remaining_percent(used_percent: f64) -> f64 {
    (100.0 - used_percent).clamp(0.0, 100.0)
}

pub fn color_remaining_percent(p: f64) -> String {
    let remaining = p.clamp(0.0, 100.0);
    let code = if remaining <= 20.0 { RED } else if remaining <= 50.0 { YELLOW } else { GREEN };
    format!("{code}{}%{RESET}", remaining.round())
}

pub fn render_bar(percent: f64, width: usize) -> String {
    let filled = (((percent.clamp(0.0, 100.0)) / 100.0 * width as f64).round() as usize)
        .min(width);
    format!(
        "{GRAY}[{RESET}{}{GRAY}{}{GRAY}]{RESET}",
        "█".repeat(filled),
        "░".repeat(width - filled)
    )
}

pub fn window_label(seconds: u64) -> String {
    if seconds == 0 {
        return "window".to_string();
    }
    if seconds < 3600 {
        return format!("{}m", (seconds / 60).max(1));
    }
    if seconds < 86400 {
        return format!("{}h", seconds / 3600);
    }
    format!("{}d", seconds / 86400)
}

pub fn format_duration(sec: i64) -> String {
    if sec <= 0 {
        return "now".to_string();
    }
    let d = sec / 86400;
    let h = (sec % 86400) / 3600;
    let m = (sec % 3600) / 60;
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

const RESET_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn format_reset_clock(unix_sec: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_sec, 0) {
        chrono::LocalResult::Single(d) => format!("{:02}:{:02}", d.hour(), d.minute()),
        _ => String::new(),
    }
}

pub fn format_reset_date_time(unix_sec: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(unix_sec, 0) {
        chrono::LocalResult::Single(d) => {
            let mon = RESET_MONTHS[(d.month0() as usize).min(11)];
            format!("{} {} {}", mon, d.day(), format_reset_clock(unix_sec))
        }
        _ => String::new(),
    }
}

// --- codex (ChatGPT subscription) ---

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexWindow {
    pub used_percent: f64,
    pub window_seconds: u64,
    pub reset_after_seconds: i64,
    pub reset_at: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexUsage {
    pub plan_type: Option<String>,
    pub allowed: bool,
    pub limit_reached: bool,
    pub primary: Option<CodexWindow>,
    pub secondary: Option<CodexWindow>,
    pub extra: Vec<CodexWindow>,
}

fn num_f(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn to_codex_window(raw: Option<&Value>) -> Option<CodexWindow> {
    let w = raw?.as_object()?;
    let used_percent = num_f(w.get("used_percent"))?;
    Some(CodexWindow {
        used_percent,
        window_seconds: num_f(w.get("limit_window_seconds")).unwrap_or(0.0) as u64,
        reset_after_seconds: num_f(w.get("reset_after_seconds")).unwrap_or(0.0) as i64,
        reset_at: num_f(w.get("reset_at")).map(|v| v as i64),
    })
}

pub fn parse_codex_usage(obj: &Value) -> Option<CodexUsage> {
    let o = obj.as_object()?;
    let plan_type = o.get("plan_type").and_then(Value::as_str).map(str::to_string);
    let rl = match o.get("rate_limit") {
        Some(rl) => rl.as_object()?,
        None => {
            return Some(CodexUsage {
                plan_type,
                allowed: true,
                limit_reached: false,
                primary: None,
                secondary: None,
                extra: Vec::new(),
            })
        }
    };
    let mut extra = Vec::new();
    if let Some(Value::Array(extra_raw)) = rl.get("additional_rate_limits") {
        for e in extra_raw {
            if let Some(w) = to_codex_window(Some(e)) {
                extra.push(w);
            }
        }
    }
    Some(CodexUsage {
        plan_type,
        allowed: rl.get("allowed").and_then(Value::as_bool) != Some(false),
        limit_reached: rl.get("limit_reached").and_then(Value::as_bool) == Some(true),
        primary: to_codex_window(rl.get("primary_window")),
        secondary: to_codex_window(rl.get("secondary_window")),
        extra,
    })
}

/// Pull the ChatGPT account id out of the access-token JWT.
fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    use base64_decode_compat::decode_url_safe_no_pad;
    let bytes = decode_url_safe_no_pad(parts[1])?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    if let Some(id) = claims.get("chatgpt_account_id").and_then(Value::as_str) {
        return Some(id.to_string());
    }
    if let Some(nested) = claims.get("https://api.openai.com/auth").and_then(Value::as_object) {
        if let Some(id) = nested.get("chatgpt_account_id").and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    if let Some(Value::Array(orgs)) = claims.get("organizations") {
        if let Some(first) = orgs.first().and_then(Value::as_object) {
            if let Some(id) = first.get("id").and_then(Value::as_str) {
                return Some(id.to_string());
            }
        }
    }
    None
}

// Minimal base64url-no-pad decode (kept local to avoid a dependency).
mod base64_decode_compat {
    pub fn decode_url_safe_no_pad(input: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            }
        }
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
        let mut chunk = [0u8; 4];
        let mut n = 0usize;
        for &b in bytes {
            chunk[n] = val(b)?;
            n += 1;
            if n == 4 {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
                out.push((chunk[1] << 4) | (chunk[2] >> 2));
                out.push((chunk[2] << 6) | chunk[3]);
                n = 0;
            }
        }
        match n {
            2 => {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
            }
            3 => {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
                out.push((chunk[1] << 4) | (chunk[2] >> 2));
            }
            _ => {}
        }
        Some(out)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CodexRemaining {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub five_hour_reset_sec: Option<i64>,
    pub seven_day_reset_sec: Option<i64>,
}

pub async fn fetch_kilo_codex_remaining() -> Option<CodexRemaining> {
    let auth = read_auth_json()?;
    let oauth = auth.get("openai")?.as_object()?;
    if oauth.get("type").and_then(Value::as_str) != Some("oauth") {
        return None;
    }
    let access = oauth.get("access")?.as_str()?;
    let account_id = oauth
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| extract_account_id_from_jwt(access))?;
    let payload = http_get_json(
        "https://chatgpt.com/backend-api/wham/usage",
        &[
            ("Authorization", format!("Bearer {access}")),
            ("Accept", "application/json".to_string()),
            ("ChatGPT-Account-Id", account_id),
        ],
    )
    .await
    .ok()?;
    let parsed = parse_codex_usage(&payload)?;
    let all: Vec<Option<CodexWindow>> = vec![parsed.primary, parsed.secondary]
        .into_iter()
        .chain(parsed.extra.into_iter().map(Some))
        .collect();
    let five = all
        .iter()
        .flatten()
        .find(|w| w.window_seconds == 5 * 60 * 60)
        .copied();
    let seven = all
        .iter()
        .flatten()
        .find(|w| w.window_seconds == 7 * 24 * 60 * 60)
        .copied();
    Some(CodexRemaining {
        five_hour: five.map(|w| remaining_percent(w.used_percent)),
        seven_day: seven.map(|w| remaining_percent(w.used_percent)),
        five_hour_reset_sec: five.and_then(|w| w.reset_at),
        seven_day_reset_sec: seven.and_then(|w| w.reset_at),
    })
}

fn format_codex_window_row(label: &str, w: &CodexWindow) -> String {
    let remaining = remaining_percent(w.used_percent);
    let bar = render_bar(remaining, 20);
    let pct = color_remaining_percent(remaining);
    let reset = match w.reset_at {
        Some(at) => {
            let clock = format_reset_clock(at);
            let dur = if w.reset_after_seconds > 0 {
                format_duration(w.reset_after_seconds)
            } else {
                String::new()
            };
            format!("  {DIM}resets in {dur}{}{RESET}", if clock.is_empty() { String::new() } else { format!(" ({clock})") })
        }
        None => String::new(),
    };
    format!("  {BOLD}{:<8}{RESET} {bar} {pct} left{reset}", label)
}

pub fn format_codex_usage(u: &CodexUsage) -> String {
    let mut lines = Vec::new();
    lines.push(format!("{BOLD}codex-balance{RESET}"));
    if let Some(plan) = &u.plan_type {
        lines.push(format!("  {DIM}plan{RESET}    : {plan}"));
    }
    if !u.allowed || u.limit_reached {
        lines.push(format!("  {RED}⚠️  rate limit reached{RESET}"));
    }
    if let Some(p) = &u.primary {
        lines.push(format_codex_window_row(&window_label(p.window_seconds), p));
    }
    if let Some(s) = &u.secondary {
        lines.push(format_codex_window_row(&window_label(s.window_seconds), s));
    }
    for e in &u.extra {
        lines.push(format_codex_window_row(&window_label(e.window_seconds), e));
    }
    lines.join("\n")
}

pub async fn handle_codex_usage() -> Result<String> {
    match fetch_kilo_codex_full().await? {
        Some(u) => Ok(format_codex_usage(&u)),
        None => Err(anyhow!(
            "No ChatGPT OAuth login found (~/.local/share/kilo/auth.json, `openai` section). Log in with kilo (Codex) and retry."
        )),
    }
}

async fn fetch_kilo_codex_full() -> Result<Option<CodexUsage>> {
    let auth = match read_auth_json() {
        Some(a) => a,
        None => return Ok(None),
    };
    let oauth = match auth.get("openai").and_then(Value::as_object) {
        Some(o) => o,
        None => return Ok(None),
    };
    if oauth.get("type").and_then(Value::as_str) != Some("oauth") {
        return Ok(None);
    }
    let access = match oauth.get("access").and_then(Value::as_str) {
        Some(a) => a,
        None => return Ok(None),
    };
    let account_id = oauth
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| extract_account_id_from_jwt(access))
        .ok_or_else(|| anyhow!("missing ChatGPT account id"))?;
    let payload = http_get_json(
        "https://chatgpt.com/backend-api/wham/usage",
        &[
            ("Authorization", format!("Bearer {access}")),
            ("Accept", "application/json".to_string()),
            ("ChatGPT-Account-Id", account_id),
        ],
    )
    .await?;
    Ok(parse_codex_usage(&payload))
}

// --- zai (GLM Coding Plan) ---

#[derive(Debug, Clone, Default)]
pub struct ZaiWindow {
    pub unit: u64, // 3=hour, 4=day, 5=month, 6=week
    pub number: u64,
    pub usage: f64,
    pub current_value: f64,
    pub used_percent: f64,
    pub next_reset_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct ZaiRenewal {
    pub auto_renew: Option<bool>,
    pub next_renew_time: Option<String>,
    pub valid_end: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ZaiUsage {
    pub plan_name: Option<String>,
    pub level: Option<String>,
    pub windows: Vec<ZaiWindow>,
    pub renewal: Option<ZaiRenewal>,
}

fn to_zai_window(raw: &Value) -> Option<ZaiWindow> {
    let w = raw.as_object()?;
    let used_percent = num_f(w.get("percentage"))?;
    Some(ZaiWindow {
        unit: num_f(w.get("unit")).unwrap_or(0.0) as u64,
        number: num_f(w.get("number")).unwrap_or(0.0) as u64,
        usage: num_f(w.get("usage")).unwrap_or(0.0),
        current_value: num_f(w.get("currentValue")).unwrap_or(0.0),
        used_percent,
        next_reset_ms: num_f(w.get("nextResetTime")).map(|v| v as i64),
    })
}

pub fn parse_zai_usage_json(quota: &Value, subscription: Option<&Value>) -> Option<ZaiUsage> {
    let qdata = quota.get("data")?.as_object()?;
    let mut windows = Vec::new();
    if let Some(Value::Array(limits)) = qdata.get("limits") {
        for l in limits {
            if let Some(w) = to_zai_window(l) {
                windows.push(w);
            }
        }
    }
    if windows.is_empty() {
        return None;
    }
    let level = qdata.get("level").and_then(Value::as_str).map(str::to_string);
    let mut plan_name = None;
    let mut renewal = None;
    if let Some(sub) = subscription {
        let subs_raw = sub.get("data");
        let first = match subs_raw {
            Some(Value::Array(arr)) => arr.first(),
            Some(v) => Some(v),
            None => None,
        };
        if let Some(first) = first.and_then(Value::as_object) {
            plan_name = first.get("productName").and_then(Value::as_str).map(str::to_string);
            let auto_renew = num_f(first.get("autoRenew")).map(|v| v != 0.0);
            renewal = Some(ZaiRenewal {
                auto_renew,
                next_renew_time: first
                    .get("nextRenewTime")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                valid_end: first
                    .get("valid")
                    .and_then(Value::as_str)
                    .and_then(parse_valid_end),
            });
        }
    }
    Some(ZaiUsage { plan_name, level, windows, renewal })
}

fn parse_valid_end(valid: &str) -> Option<String> {
    // "2026-10-25 22:09:20-2026-11-25 22:09:20" → end bound.
    let re = regex::Regex::new(
        r"^(\d{4}-\d{2}-\d{2}(?: \d{2}:\d{2}:\d{2})?)-(\d{4}-\d{2}-\d{2}(?: \d{2}:\d{2}:\d{2})?)$",
    )
    .ok()?;
    re.captures(valid.trim()).map(|c| c[2].to_string())
}

/// Auth-file sections that hold a Z.AI Coding Plan API key; first wins.
const ZAI_AUTH_KEYS: &[&str] = &["zai-coding-plan", "zhipuai-coding-plan"];

pub fn read_zai_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("ZAI_CODING_PLAN_API_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    let all = read_auth_json()?;
    for k in ZAI_AUTH_KEYS {
        if let Some(auth) = all.get(*k).and_then(Value::as_object) {
            if auth.get("type").and_then(Value::as_str) == Some("api") {
                if let Some(key) = auth.get("key").and_then(Value::as_str) {
                    return Some(key.to_string());
                }
            }
        }
    }
    None
}

async fn fetch_zai_quota(api_key: &str) -> Result<Value> {
    http_get_json(
        "https://api.z.ai/api/monitor/usage/quota/limit",
        &[
            ("Authorization", format!("Bearer {api_key}")),
            ("Accept", "application/json".to_string()),
        ],
    )
    .await
}

async fn fetch_zai_subscription(api_key: &str) -> Option<Value> {
    http_get_json(
        "https://api.z.ai/api/biz/subscription/list",
        &[
            ("Authorization", format!("Bearer {api_key}")),
            ("Accept", "application/json".to_string()),
        ],
    )
    .await
    .ok()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ZaiRemaining {
    pub five_hour: Option<f64>,
    pub weekly: Option<f64>,
    pub five_hour_reset_ms: Option<i64>,
    pub weekly_reset_ms: Option<i64>,
}

pub async fn fetch_kilo_zai_remaining() -> Option<ZaiRemaining> {
    let key = read_zai_api_key()?;
    let quota = fetch_zai_quota(&key).await.ok()?;
    let parsed = parse_zai_usage_json(&quota, None)?;
    let five = parsed.windows.iter().find(|w| w.unit == 3);
    let weekly = parsed.windows.iter().find(|w| w.unit == 6);
    Some(ZaiRemaining {
        five_hour: five.map(|w| remaining_percent(w.used_percent)),
        weekly: weekly.map(|w| remaining_percent(w.used_percent)),
        five_hour_reset_ms: five.and_then(|w| w.next_reset_ms),
        weekly_reset_ms: weekly.and_then(|w| w.next_reset_ms),
    })
}

pub fn zai_window_label(unit: u64, number: u64) -> String {
    match unit {
        3 => format!("{number}h"),
        6 => format!("{}d", number * 7),
        4 => format!("{number}d"),
        5 => "mo".to_string(),
        _ => "window".to_string(),
    }
}

const ZAI_MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn format_zai_date(raw: &str) -> String {
    let re = regex::Regex::new(r"^(\d{4})-(\d{2})-(\d{2})").unwrap();
    let Some(m) = re.captures(raw.trim()) else {
        return raw.to_string();
    };
    let year: i64 = m[1].parse().unwrap_or(0);
    let mon_idx: usize = m[2].parse::<usize>().unwrap_or(0);
    let Some(mon) = ZAI_MONTHS.get(mon_idx.wrapping_sub(1)) else {
        return raw.to_string();
    };
    let day: u64 = m[3].parse().unwrap_or(0);
    let now_year = chrono::Local::now().year();
    if year == now_year as i64 {
        format!("{mon} {day}")
    } else {
        format!("{mon} {day} {year}")
    }
}

fn format_zai_window_row(label: &str, w: &ZaiWindow) -> String {
    let rem = remaining_percent(w.used_percent);
    let bar = render_bar(rem, 20);
    let pct = color_remaining_percent(rem);
    let mut reset = String::new();
    if let Some(ms) = w.next_reset_ms {
        let clock = format_reset_clock(ms / 1000);
        let now_ms = chrono::Local::now().timestamp_millis();
        let dur_sec = (ms - now_ms) / 1000;
        let dur = if dur_sec > 0 { format_duration(dur_sec) } else { "now".to_string() };
        reset = format!("  {DIM}resets in {dur}{}{RESET}", if clock.is_empty() { String::new() } else { format!(" ({clock})") });
    }
    let credits = format!("  {DIM}{} / {} credits{RESET}", w.current_value.round(), w.usage.round());
    format!("  {BOLD}{:<8}{RESET} {bar} {pct} left{reset}{credits}", label)
}

pub fn format_zai_usage(u: &ZaiUsage) -> String {
    let mut lines = Vec::new();
    lines.push(format!("{BOLD}zai-balance{RESET}"));
    if u.plan_name.is_some() || u.level.is_some() {
        let plan = match (&u.plan_name, &u.level) {
            (Some(p), Some(l)) => format!("{p} ({l})"),
            (Some(p), None) => p.clone(),
            (None, Some(l)) => l.clone(),
            (None, None) => String::new(),
        };
        lines.push(format!("  {DIM}plan{RESET}    : {plan}"));
    }
    if let Some(r) = &u.renewal {
        if r.auto_renew == Some(false) {
            let end = r.valid_end.clone().or_else(|| r.next_renew_time.clone());
            if let Some(end) = end {
                lines.push(format!(
                    "  {DIM}ends{RESET}    : {} {DIM}(auto-renew off){RESET}",
                    format_zai_date(&end)
                ));
            }
        } else {
            let renew = r.next_renew_time.clone().or_else(|| r.valid_end.clone());
            if let Some(renew) = renew {
                lines.push(format!("  {DIM}renews{RESET}  : {}", format_zai_date(&renew)));
            }
        }
    }
    for w in &u.windows {
        lines.push(format_zai_window_row(&zai_window_label(w.unit, w.number), w));
    }
    if u.windows.is_empty() {
        lines.push(format!("  {DIM}(no usage windows reported){RESET}"));
    }
    lines.join("\n")
}

pub async fn handle_zai_usage() -> Result<String> {
    let key = read_zai_api_key().ok_or_else(|| {
        anyhow!(
            "No Z.AI Coding Plan API key found (~/.local/share/kilo/auth.json, `zai-coding-plan` section). Log in with kilo (Z.AI Coding Plan) and run a turn to establish it."
        )
    })?;
    let (quota, subscription) = tokio::join!(fetch_zai_quota(&key), async {
        fetch_zai_subscription(&key).await
    });
    let parsed = parse_zai_usage_json(&quota?, subscription.as_ref())
        .ok_or_else(|| anyhow!("zai quota endpoint returned an unexpected response shape"))?;
    Ok(format_zai_usage(&parsed))
}

// --- openrouter ---

#[derive(Debug, Clone, Default)]
pub struct OpenRouterKeyInfo {
    pub label: Option<String>,
    pub usage: f64,
    pub usage_daily: f64,
    pub usage_weekly: f64,
    pub usage_monthly: f64,
    pub limit: Option<f64>,
    pub limit_remaining: Option<f64>,
    pub limit_reset: Option<String>,
    pub is_free_tier: Option<bool>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenRouterCredits {
    pub total_credits: f64,
    pub total_usage: f64,
}

pub fn parse_openrouter_key_info(value: &Value) -> Option<OpenRouterKeyInfo> {
    let data = value.get("data")?.as_object()?;
    if data.get("usage").is_none() && data.get("usage_daily").is_none() {
        return None;
    }
    Some(OpenRouterKeyInfo {
        label: data.get("label").and_then(Value::as_str).map(str::to_string),
        usage: num_f(data.get("usage")).unwrap_or(0.0),
        usage_daily: num_f(data.get("usage_daily")).unwrap_or(0.0),
        usage_weekly: num_f(data.get("usage_weekly")).unwrap_or(0.0),
        usage_monthly: num_f(data.get("usage_monthly")).unwrap_or(0.0),
        limit: num_f(data.get("limit")),
        limit_remaining: num_f(data.get("limit_remaining")),
        limit_reset: data.get("limit_reset").and_then(Value::as_str).map(str::to_string),
        is_free_tier: data.get("is_free_tier").and_then(Value::as_bool),
        expires_at: data.get("expires_at").and_then(Value::as_str).map(str::to_string),
    })
}

pub fn parse_openrouter_credits(value: &Value) -> Option<OpenRouterCredits> {
    let data = value.get("data")?.as_object()?;
    if data.get("total_credits").is_none() && data.get("total_usage").is_none() {
        return None;
    }
    Some(OpenRouterCredits {
        total_credits: num_f(data.get("total_credits")).unwrap_or(0.0),
        total_usage: num_f(data.get("total_usage")).unwrap_or(0.0),
    })
}

fn read_openrouter_key() -> Option<String> {
    let all = read_auth_json()?;
    let entry = all.get("openrouter")?.as_object()?;
    if entry.get("type").and_then(Value::as_str) != Some("api") {
        return None;
    }
    entry.get("key").and_then(Value::as_str).map(str::to_string)
}

fn usd(v: f64) -> String {
    format!("${v:.2}")
}

pub fn openrouter_key_remaining(
    info: Option<&OpenRouterKeyInfo>,
    credits: Option<&OpenRouterCredits>,
) -> Option<f64> {
    if let Some(info) = info {
        if let Some(limit) = info.limit {
            return Some(info.limit_remaining.unwrap_or((limit - info.usage).max(0.0)));
        }
    }
    credits.map(|c| (c.total_credits - c.total_usage).max(0.0))
}

fn color_remaining_usd(remaining: f64, total: f64) -> String {
    let pct = if total > 0.0 { remaining / total * 100.0 } else { 0.0 };
    let code = if pct <= 20.0 { RED } else if pct <= 50.0 { YELLOW } else { GREEN };
    format!("{code}{}{RESET}", usd(remaining))
}

fn date_only(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| iso.chars().take(10).collect())
}

pub fn format_openrouter_balance(
    info: Option<&OpenRouterKeyInfo>,
    credits: Option<&OpenRouterCredits>,
) -> String {
    let mut lines = Vec::new();
    lines.push(format!("{BOLD}openrouter-balance{RESET}"));
    let label = |s: &str| format!("{s:<15}");
    if let Some(info) = info {
        let tag = if info.is_free_tier == Some(true) {
            format!("  {DIM}(free tier){RESET}")
        } else {
            String::new()
        };
        let expires = match &info.expires_at {
            Some(e) => format!("  {DIM}expires {}{RESET}", date_only(e)),
            None => String::new(),
        };
        lines.push(format!(
            "  {DIM}{}{RESET} {}{tag}{expires}",
            label("key"),
            info.label.clone().unwrap_or_else(|| "(unnamed key)".to_string())
        ));
        let remaining = openrouter_key_remaining(Some(info), credits);
        if let Some(limit) = info.limit {
            let reset = match &info.limit_reset {
                Some(r) => format!(" (resets {r})"),
                None => String::new(),
            };
            let colored = match remaining {
                Some(r) => color_remaining_usd(r, limit),
                None => format!("{RED}?{RESET}"),
            };
            lines.push(format!(
                "  {BOLD}{}{RESET} {colored} left of {} key limit{reset}",
                label("remaining"),
                usd(limit)
            ));
        } else if let Some(c) = credits {
            let colored = color_remaining_usd(remaining.unwrap_or(0.0), c.total_credits);
            lines.push(format!(
                "  {BOLD}{}{RESET} {colored} of account credits (no key limit)",
                label("remaining")
            ));
        } else {
            lines.push(format!(
                "  {BOLD}{}{RESET} {RED}unknown{RESET} (no key limit, no credit ledger)",
                label("remaining")
            ));
        }
        lines.push(format!("  {DIM}{}{RESET} {}", label("usage today"), usd(info.usage_daily)));
        lines.push(format!("  {DIM}{}{RESET} {}", label("usage week"), usd(info.usage_weekly)));
        lines.push(format!("  {DIM}{}{RESET} {}", label("usage month"), usd(info.usage_monthly)));
        lines.push(format!("  {DIM}{}{RESET} {}", label("usage total"), usd(info.usage)));
        if info.limit.is_some() {
            if let Some(c) = credits {
                let account_remaining = (c.total_credits - c.total_usage).max(0.0);
                lines.push(format!(
                    "  {DIM}{}{RESET} {} remaining of {} purchased",
                    label("account credits"),
                    usd(account_remaining),
                    usd(c.total_credits)
                ));
            }
        }
    } else {
        lines.push(format!("  {DIM}(no OpenRouter key info available){RESET}"));
    }
    lines.push(format!("  {DIM}per-request detail: https://openrouter.ai/activity{RESET}"));
    lines.join("\n")
}

pub async fn handle_openrouter_balance() -> Result<String> {
    let key = read_openrouter_key().ok_or_else(|| {
        anyhow!(
            "No OpenRouter API key found (~/.local/share/kilo/auth.json, `openrouter` section). Connect the openrouter provider in kilo and retry."
        )
    })?;
    let base = std::env::var("OPENROUTER_API_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
    let key_url = format!("{base}/key");
    let credits_url = format!("{base}/credits");
    let auth = ("Authorization", format!("Bearer {key}"));
    let content_type = ("Content-Type", "application/json".to_string());
    let info_headers = [auth.clone(), content_type.clone()];
    let credits_headers = [auth, content_type];
    let info_fut = http_get_json(&key_url, &info_headers);
    let credits_fut = http_get_json(&credits_url, &credits_headers);
    let (info_res, credits_res) = tokio::join!(info_fut, credits_fut);
    let info = parse_openrouter_key_info(&info_res?)
        .ok_or_else(|| anyhow!("openrouter /key returned an unexpected response shape"))?;
    let credits = credits_res.ok().and_then(|v| parse_openrouter_credits(&v));
    Ok(format_openrouter_balance(Some(&info), credits.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_parse_and_format() {
        let payload: Value = serde_json::json!({
            "plan_type": "plus",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 25.0,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 3600,
                    "reset_at": 1760000000
                },
                "secondary_window": {
                    "used_percent": 60.0,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 86400,
                    "reset_at": 1760500000
                }
            }
        });
        let u = parse_codex_usage(&payload).unwrap();
        assert_eq!(u.plan_name().as_deref(), Some("plus"));
        assert!(u.primary.is_some());
        let remaining = fetch_remaining_from(&u);
        assert_eq!(remaining.five_hour, Some(75.0));
        let out = format_codex_usage(&u);
        assert!(out.contains("codex-balance"));
        assert!(out.contains("5h"));
        assert!(out.contains("7d"));
    }

    fn fetch_remaining_from(u: &CodexUsage) -> CodexRemaining {
        let all: Vec<Option<CodexWindow>> = vec![u.primary, u.secondary]
            .into_iter()
            .chain(u.extra.clone().into_iter().map(Some))
            .collect();
        let five = all.iter().flatten().find(|w| w.window_seconds == 18000).copied();
        let seven = all.iter().flatten().find(|w| w.window_seconds == 604800).copied();
        CodexRemaining {
            five_hour: five.map(|w| remaining_percent(w.used_percent)),
            seven_day: seven.map(|w| remaining_percent(w.used_percent)),
            five_hour_reset_sec: five.and_then(|w| w.reset_at),
            seven_day_reset_sec: seven.and_then(|w| w.reset_at),
        }
    }

    #[test]
    fn zai_parse_and_format() {
        let quota: Value = serde_json::json!({
            "code": 0,
            "data": {
                "level": "lite",
                "limits": [
                    { "type": "CREDIT_LIMIT", "unit": 3, "number": 5, "usage": 120.0,
                      "currentValue": 30.0, "remaining": 90.0, "percentage": 25.0,
                      "nextResetTime": 1760000000000i64 },
                    { "type": "CREDIT_LIMIT", "unit": 6, "number": 1, "usage": 500.0,
                      "currentValue": 100.0, "remaining": 400.0, "percentage": 20.0 }
                ]
            }
        });
        let subscription: Value = serde_json::json!({
            "code": 0,
            "data": [{ "productName": "GLM Coding Lite", "autoRenew": 1,
                       "nextRenewTime": "2026-10-25",
                       "valid": "2026-09-25 22:09:20-2026-10-25 22:09:20" }]
        });
        let u = parse_zai_usage_json(&quota, Some(&subscription)).unwrap();
        assert_eq!(u.plan_name.as_deref(), Some("GLM Coding Lite"));
        assert_eq!(u.windows.len(), 2);
        let out = format_zai_usage(&u);
        assert!(out.contains("zai-balance"));
        assert!(out.contains("5h"));
        assert!(out.contains("7d"));
        assert!(out.contains("30 / 120 credits"));
    }

    #[test]
    fn openrouter_parse_and_format() {
        let key_payload: Value = serde_json::json!({
            "data": { "label": "my key", "usage": 1.5, "usage_daily": 0.25,
                      "usage_weekly": 1.0, "usage_monthly": 1.5,
                      "limit": 10.0, "limit_remaining": 8.5, "limit_reset": "daily" }
        });
        let info = parse_openrouter_key_info(&key_payload).unwrap();
        let credits_payload: Value = serde_json::json!({
            "data": { "total_credits": 50.0, "total_usage": 12.0 }
        });
        let credits = parse_openrouter_credits(&credits_payload).unwrap();
        assert_eq!(openrouter_key_remaining(Some(&info), Some(&credits)), Some(8.5));
        let out = format_openrouter_balance(Some(&info), Some(&credits));
        assert!(out.contains("openrouter-balance"));
        assert!(out.contains("$8.50"));
    }

    #[test]
    fn format_helpers() {
        assert_eq!(remaining_percent(25.0), 75.0);
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(90000), "1d 1h");
        let bar = render_bar(50.0, 4);
        assert_eq!(bar.matches('█').count(), 2);
    }
}

// Small helper used by tests.
#[cfg(test)]
impl CodexUsage {
    fn plan_name(&self) -> Option<String> {
        self.plan_type.clone()
    }
}

/// One quota segment: "5h: 23% (14:42)" / "7d: 41% (Oct 7 16:42)". The 5h
/// window shows a reset TIME; the 7d window a DATE+TIME. Empty when the
/// remaining figure is absent.
pub fn format_quota_segment(
    label: &str,
    remaining: Option<f64>,
    reset_sec: Option<i64>,
    date_mode: bool,
) -> Option<String> {
    let remaining = remaining?;
    let reset = match reset_sec {
        Some(sec) => {
            let fmt = if date_mode {
                format_reset_date_time(sec)
            } else {
                format_reset_clock(sec)
            };
            if fmt.is_empty() {
                String::new()
            } else {
                format!(" ({fmt})")
            }
        }
        None => String::new(),
    };
    Some(format!("{label}: {}{reset}", color_remaining_percent(remaining)))
}
