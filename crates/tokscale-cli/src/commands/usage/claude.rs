use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::Deserialize;
use tokscale_core::{parse_local_unified_messages, LocalParseOptions, UnifiedMessage};

use super::helpers::capitalize;
use super::{UsageMetric, UsageOutput};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA_HEADER: &str = "oauth-2025-04-20";
const CLAUDE_SESSION_WINDOW_MS: i64 = 5 * 60 * 60 * 1000;
const HOUR_MS: i64 = 60 * 60 * 1000;
const MINUTE_MS: i64 = 60 * 1000;
const RECENT_WINDOW_MS: i64 = 30 * 60 * 1000;

// Ring buffer: (timestamp_ms, five_hour_pct_used, seven_day_pct_used)
// Tracks utilization% history to compute %/min burn rate without knowing the absolute limit.
static UTIL_HIST: OnceLock<Mutex<VecDeque<(i64, f64, f64)>>> = OnceLock::new();

// Last known good window data — used as fallback when the API temporarily omits a window
// (prevents flickering between full metrics and orphaned Burn Rate-only display).
static LAST_5H: OnceLock<Mutex<Option<(f64, Option<String>)>>> = OnceLock::new();
static LAST_7D: OnceLock<Mutex<Option<(f64, Option<String>)>>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<Oauth>,
}

#[derive(Debug, Deserialize)]
struct Oauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageResponse {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
    seven_day_opus: Option<Window>,
}

#[derive(Debug, Clone, Deserialize)]
struct Window {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LocalActiveUsage {
    input_output_tokens: u64,
    cost: f64,
}


#[derive(Debug, Clone, Copy)]
struct ActiveBlock {
    end_ms: i64,
    last_message_ms: i64,
    input_output_tokens: u64,
    cost: f64,
}

impl ActiveBlock {
    fn new(timestamp_ms: i64) -> Self {
        let start_ms = floor_to_hour_ms(timestamp_ms);
        Self {
            end_ms: start_ms + CLAUDE_SESSION_WINDOW_MS,
            last_message_ms: timestamp_ms,
            input_output_tokens: 0,
            cost: 0.0,
        }
    }

    fn add_message(&mut self, message: &UnifiedMessage) {
        self.last_message_ms = message.timestamp;
        let input_output_tokens = message
            .tokens
            .input
            .saturating_add(message.tokens.output)
            .max(0) as u64;
        self.input_output_tokens = self.input_output_tokens.saturating_add(input_output_tokens);
        self.cost += message.cost.max(0.0);
    }

    fn into_usage(self) -> LocalActiveUsage {
        LocalActiveUsage {
            input_output_tokens: self.input_output_tokens,
            cost: self.cost,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenRefresh {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum CredentialSource {
    File,
    Keychain,
}

fn read_keychain() -> Result<String> {
    super::helpers::read_keychain("Claude Code-credentials")
}

pub fn has_credentials() -> bool {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".claude").join(".credentials.json").exists()
        || super::helpers::read_keychain("Claude Code-credentials").is_ok()
        || home.join(".claude").join("projects").exists()
        || home.join(".claude").join("transcripts").exists()
}

fn read_credentials() -> Result<(Credentials, CredentialSource)> {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = home.join(".claude").join(".credentials.json");
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(creds) = serde_json::from_str::<Credentials>(&content) {
                return Ok((creds, CredentialSource::File));
            }
        }
    }
    let content = read_keychain()?;
    let creds: Credentials = serde_json::from_str(&content)?;
    Ok((creds, CredentialSource::Keychain))
}

fn save_credentials(
    access_token: &str,
    refresh_token: &str,
    subscription_type: Option<&str>,
    rate_limit_tier: Option<&str>,
) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let path = home.join(".claude").join(".credentials.json");
    let mut oauth = serde_json::json!({
        "accessToken": access_token,
        "refreshToken": refresh_token,
    });
    if let Some(st) = subscription_type {
        oauth["subscriptionType"] = serde_json::Value::String(st.to_string());
    }
    if let Some(rlt) = rate_limit_tier {
        oauth["rateLimitTier"] = serde_json::Value::String(rlt.to_string());
    }
    let json = serde_json::json!({
        "claudeAiOauth": oauth
    });
    let content = match serde_json::to_string_pretty(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: failed to serialize Claude credentials: {e}");
            return;
        }
    };
    if let Err(e) = super::helpers::atomic_write_secret(&path, content.as_bytes()) {
        eprintln!("warning: failed to save Claude credentials: {e}");
    }
}

async fn refresh_token(client: &reqwest::Client, rt: &str) -> Result<TokenRefresh> {
    let resp = client
        .post("https://platform.claude.com/v1/oauth/token")
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": rt,
            "client_id": CLIENT_ID,
            "scope": "user:profile user:inference user:sessions:claude_code user:mcp_servers"
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("Claude token refresh failed (HTTP {})", resp.status());
    }
    Ok(resp.json().await?)
}

async fn fetch_usage(client: &reqwest::Client, token: &str) -> Result<UsageResponse> {
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("anthropic-beta", BETA_HEADER)
        .send()
        .await?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        anyhow::bail!("NEEDS_AUTH");
    }
    if !status.is_success() {
        anyhow::bail!("Claude usage request failed (HTTP {status})");
    }
    Ok(resp.json().await?)
}

fn window_metric(label: &str, w: &Window) -> UsageMetric {
    let used = w.utilization.clamp(0.0, 100.0);
    UsageMetric {
        label: label.into(),
        used_percent: used,
        remaining_percent: 100.0 - used,
        remaining_label: None,
        resets_at: w.resets_at.clone(),
    }
}

fn local_session_metric(_token_limit: Option<u64>, _tokens_used: Option<u64>) -> Option<UsageMetric> {
    None // removed: absolute token counts are unreliable; % comes from API utilization only
}

fn fmt_tokens(t: u64) -> String {
    if t >= 100_000 {
        format!("{:.0}k", t as f64 / 1_000.0)
    } else if t >= 1_000 {
        format!("{:.1}k", t as f64 / 1_000.0)
    } else {
        t.to_string()
    }
}

fn parse_resets_at_mins(resets_at: Option<&str>) -> Option<f64> {
    let s = resets_at?;
    let reset = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    let now = chrono::Utc::now();
    let secs = (reset.with_timezone(&chrono::Utc) - now).num_seconds();
    if secs > 0 { Some(secs as f64 / 60.0) } else { None }
}

fn push_util_sample(five_h_pct: f64, seven_d_pct: f64, now_ms: i64) {
    let hist = UTIL_HIST.get_or_init(|| Mutex::new(VecDeque::with_capacity(120)));
    let mut h = hist.lock().unwrap();
    if h.back().map_or(false, |&(ts, _, _)| now_ms - ts < 15_000) {
        return; // sample too recent, skip
    }
    // Significant drop → window reset, clear stale history
    if let Some(&(_, last_5h, last_7d)) = h.back() {
        if five_h_pct < last_5h - 20.0 || seven_d_pct < last_7d - 20.0 {
            h.clear();
        }
    }
    h.push_back((now_ms, five_h_pct, seven_d_pct));
    while h.len() > 120 {
        h.pop_front();
    }
}

fn compute_pct_burn_per_min(
    hist: &VecDeque<(i64, f64, f64)>,
    five_h: bool,
    now_ms: i64,
) -> Option<f64> {
    let window_start = now_ms - RECENT_WINDOW_MS;
    let recent: Vec<_> = hist.iter().filter(|&&(ts, _, _)| ts >= window_start).collect();
    if recent.len() < 2 {
        return None;
    }
    let &&(first_ts, f5h, f7d) = recent.first().unwrap();
    let &&(last_ts, l5h, l7d) = recent.last().unwrap();
    let delta = if five_h { l5h - f5h } else { l7d - f7d };
    if delta <= 0.0 {
        return None;
    }
    let mins = ((last_ts - first_ts) as f64 / MINUTE_MS as f64).max(1.0);
    Some(delta / mins)
}

fn util_pct_burn_per_min(five_h: bool, now_ms: i64) -> Option<f64> {
    let hist = UTIL_HIST.get()?.lock().ok()?;
    compute_pct_burn_per_min(&hist, five_h, now_ms)
}

fn fmt_duration(mins: f64) -> String {
    if mins < 60.0 {
        format!("in {:.0}m", mins)
    } else if mins < 1440.0 {
        format!("in {:.1}h", mins / 60.0)
    } else {
        format!("in {:.1}d", mins / 1440.0)
    }
}

fn build_child_metrics(
    burn: Option<(f64, f64)>, // (tokens_per_min, cost_per_min) from local messages
    remaining_pct: f64,       // % remaining in the window (0–100)
    pct_burn_per_min: Option<f64>, // % consumed per minute, derived from utilization history
    mins_until_reset: Option<f64>,
) -> Vec<UsageMetric> {
    let mut children = Vec::new();

    if let Some((tokens_per_min, cost_per_min)) = burn {
        children.push(UsageMetric {
            label: "  Burn Rate".into(),
            used_percent: 0.0,
            remaining_percent: -1.0,
            remaining_label: Some(format!("{:.1}k tok/min", tokens_per_min / 1000.0)),
            resets_at: None,
        });
        children.push(UsageMetric {
            label: "  Cost Rate".into(),
            used_percent: 0.0,
            remaining_percent: -1.0,
            remaining_label: Some(format!("${:.4}/min", cost_per_min)),
            resets_at: None,
        });
    }

    if let Some(pct_per_min) = pct_burn_per_min {
        if pct_per_min > 0.0 && remaining_pct > 0.0 {
            let runs_out_mins = remaining_pct / pct_per_min;
            let shows_before_reset = mins_until_reset.map_or(true, |r| runs_out_mins < r);
            if shows_before_reset {
                children.push(UsageMetric {
                    label: "  Runs Out".into(),
                    used_percent: 0.0,
                    remaining_percent: -1.0,
                    remaining_label: Some(fmt_duration(runs_out_mins)),
                    resets_at: None,
                });
            }
        }
    }

    children
}

fn floor_to_hour_ms(timestamp_ms: i64) -> i64 {
    timestamp_ms.div_euclid(HOUR_MS) * HOUR_MS
}

// Burn rate from messages sent in the last RECENT_WINDOW_MS.
// Returns (tokens_per_min, cost_per_min), or None if no activity in the window.
// Using a short recent window prevents a historical burst from inflating the
// projected "runs out" time when the user is currently idle or working slowly.
fn compute_recent_burn_rate(
    messages: &[UnifiedMessage],
    now_ms: i64,
) -> Option<(f64, f64)> {
    let window_start = now_ms - RECENT_WINDOW_MS;
    let mut recent: Vec<&UnifiedMessage> = messages
        .iter()
        .filter(|m| m.timestamp > 0 && m.timestamp >= window_start)
        .collect();
    if recent.is_empty() {
        return None;
    }
    recent.sort_by_key(|m| m.timestamp);

    let tokens: u64 = recent
        .iter()
        .map(|m| m.tokens.input.saturating_add(m.tokens.output).max(0) as u64)
        .sum();
    let cost: f64 = recent.iter().map(|m| m.cost.max(0.0)).sum();
    if tokens == 0 {
        return None;
    }

    // Span from the first to the last message in the window; minimum 1 minute
    // so a single-message burst doesn't produce an infinite rate.
    let first_ts = recent.first().unwrap().timestamp;
    let last_ts = recent.last().unwrap().timestamp;
    let duration_mins = ((last_ts - first_ts) as f64 / MINUTE_MS as f64).max(1.0);

    Some((tokens as f64 / duration_mins, cost / duration_mins))
}

// Returns (block_usage, recent_burn_rate).
// block_usage  — aggregate token/cost totals for the active 5h window.
// recent_burn  — tokens/min and cost/min from the last RECENT_WINDOW_MS only;
//                None when no messages were sent in that window (idle user).
fn local_active_usage_from_messages(
    messages: &[UnifiedMessage],
    now_ms: i64,
) -> (Option<LocalActiveUsage>, Option<(f64, f64)>) {
    let mut sorted: Vec<&UnifiedMessage> = messages
        .iter()
        .filter(|message| message.timestamp > 0)
        .collect();
    sorted.sort_by_key(|message| message.timestamp);

    let mut current: Option<ActiveBlock> = None;
    let mut latest_active: Option<LocalActiveUsage> = None;

    for message in &sorted {
        let starts_new_block = current
            .as_ref()
            .map(|block| {
                message.timestamp >= block.end_ms
                    || message.timestamp - block.last_message_ms >= CLAUDE_SESSION_WINDOW_MS
            })
            .unwrap_or(true);

        if starts_new_block {
            if let Some(block) = current.take() {
                if block.end_ms > now_ms {
                    latest_active = Some(block.into_usage());
                }
            }
            current = Some(ActiveBlock::new(message.timestamp));
        }

        if let Some(block) = current.as_mut() {
            block.add_message(message);
        }
    }

    if let Some(block) = current {
        if block.end_ms > now_ms {
            latest_active = Some(block.into_usage());
        }
    }

    let recent_burn = if latest_active.is_some() {
        compute_recent_burn_rate(messages, now_ms)
    } else {
        None
    };

    (latest_active, recent_burn)
}

async fn fetch_local_active_usage() -> (Option<LocalActiveUsage>, Option<(f64, f64)>) {
    let messages = match parse_local_unified_messages(LocalParseOptions {
        clients: Some(vec!["claude".to_string()]),
        ..Default::default()
    })
    .await
    {
        Ok(m) => m,
        Err(_) => return (None, None),
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    local_active_usage_from_messages(&messages, now_ms)
}

fn local_only_output(
    plan: Option<String>,
    local_active: LocalActiveUsage,
    recent_burn: Option<(f64, f64)>,
) -> UsageOutput {
    let _ = local_active; // no % known without API; only burn rate is shown
    let mut metrics = Vec::new();
    if recent_burn.is_some() {
        // remaining_pct=0 → Runs Out won't show (no % data without API)
        metrics.extend(build_child_metrics(recent_burn, 0.0, None, None));
    }
    UsageOutput {
        provider: "Claude".into(),
        account: None,
        plan,
        email: None,
        metrics,
    }
}

pub fn fetch() -> Result<UsageOutput> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (local_active, recent_burn) = fetch_local_active_usage().await;
        let (creds, _source) = match read_credentials() {
            Ok(creds) => creds,
            Err(error) => {
                if let Some(local_active) = local_active {
                    return Ok(local_only_output(None, local_active, recent_burn));
                }
                return Err(error);
            }
        };
        let oauth = match creds.claude_ai_oauth {
            Some(oauth) => oauth,
            None => {
                if let Some(local_active) = local_active {
                    return Ok(local_only_output(None, local_active, recent_burn));
                }
                return Err(anyhow::anyhow!(
                    "No Claude OAuth credentials. Run 'claude' to log in."
                ));
            }
        };
        let access_token = oauth
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No Claude access token."))?;
        let plan = oauth.subscription_type.as_ref().map(|s| {
            let tier = oauth
                .rate_limit_tier
                .as_deref()
                .and_then(|t| t.rsplit('_').next());
            match tier {
                Some(mult) => format!("{} {}", capitalize(s), mult),
                None => capitalize(s),
            }
        });

        let client = reqwest::Client::new();
        let resp = match fetch_usage(&client, &access_token).await {
            Ok(r) => r,
            Err(e) if e.to_string().contains("NEEDS_AUTH") => {
                let rt = oauth
                    .refresh_token
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No refresh token."))?;
                let refreshed = refresh_token(&client, rt).await?;
                let new = refreshed
                    .access_token
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Refresh returned no token."))?;
                if let Some(new_rt) = refreshed.refresh_token.as_deref() {
                    save_credentials(
                        &new,
                        new_rt,
                        oauth.subscription_type.as_deref(),
                        oauth.rate_limit_tier.as_deref(),
                    );
                }
                fetch_usage(&client, &new).await?
            }
            Err(e) => {
                if let Some(local_active) = local_active {
                    return Ok(local_only_output(plan, local_active, recent_burn));
                }
                return Err(e);
            }
        };

        // Record utilization% history so we can compute %/min burn rate
        // without needing to know the absolute token limit.
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Update window caches when the API provides fresh data; fall back to
        // the last known-good values when a window is temporarily absent so
        // the display doesn't flicker between full metrics and burn-rate-only.
        let five_h_window: Option<(f64, Option<String>)> = match &resp.five_hour {
            Some(w) => {
                let v = (w.utilization, w.resets_at.clone());
                if let Ok(mut g) = LAST_5H.get_or_init(|| Mutex::new(None)).lock() {
                    *g = Some(v.clone());
                }
                Some(v)
            }
            None => LAST_5H.get().and_then(|m| m.lock().ok()).and_then(|g| g.clone()),
        };
        let seven_d_window: Option<(f64, Option<String>)> = match &resp.seven_day {
            Some(w) => {
                let v = (w.utilization, w.resets_at.clone());
                if let Ok(mut g) = LAST_7D.get_or_init(|| Mutex::new(None)).lock() {
                    *g = Some(v.clone());
                }
                Some(v)
            }
            None => LAST_7D.get().and_then(|m| m.lock().ok()).and_then(|g| g.clone()),
        };

        let five_h_pct = five_h_window.as_ref().map(|(u, _)| u.clamp(0.0, 100.0)).unwrap_or(0.0);
        let seven_d_pct = seven_d_window.as_ref().map(|(u, _)| u.clamp(0.0, 100.0)).unwrap_or(0.0);
        if five_h_window.is_some() || seven_d_window.is_some() {
            push_util_sample(five_h_pct, seven_d_pct, now_ms);
        }
        let pct_burn_5h = util_pct_burn_per_min(true, now_ms);
        let pct_burn_7d = util_pct_burn_per_min(false, now_ms);

        let burn = recent_burn;
        let mins_until_reset = parse_resets_at_mins(
            five_h_window.as_ref().and_then(|(_, r)| r.as_deref()),
        );

        let mut metrics = Vec::new();

        if let Some((utilization, resets_at)) = five_h_window {
            let w = Window { utilization, resets_at };
            let remaining_pct = (100.0 - utilization.clamp(0.0, 100.0)).max(0.0);
            metrics.push(window_metric("5h Limit", &w));
            metrics.extend(build_child_metrics(burn, remaining_pct, pct_burn_5h, mins_until_reset));
        }

        if let Some((utilization, resets_at)) = seven_d_window {
            let w = Window { utilization, resets_at };
            let remaining_pct = (100.0 - utilization.clamp(0.0, 100.0)).max(0.0);
            metrics.push(window_metric("Week Limit", &w));
            if burn.is_some() || pct_burn_7d.is_some() {
                metrics.extend(build_child_metrics(burn, remaining_pct, pct_burn_7d, None));
            }
        }

        if let Some(ref w) = resp.seven_day_opus {
            metrics.push(window_metric("Opus", w));
        }

        Ok(UsageOutput {
            provider: "Claude".into(),
            account: None,
            plan,
            email: None,
            metrics,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokscale_core::TokenBreakdown;

    fn message(
        timestamp: i64,
        input: i64,
        output: i64,
        cache_read: i64,
        cost: f64,
    ) -> UnifiedMessage {
        UnifiedMessage {
            client: "claude".to_string(),
            model_id: "claude-sonnet-4".to_string(),
            provider_id: "anthropic".to_string(),
            session_id: "session".to_string(),
            workspace_key: None,
            workspace_label: None,
            timestamp,
            date: "2026-06-27".to_string(),
            tokens: TokenBreakdown {
                input,
                output,
                cache_read,
                cache_write: 0,
                reasoning: 0,
            },
            cost,
            duration_ms: None,
            message_count: 1,
            agent: None,
            dedup_key: None,
            is_turn_start: true,
        }
    }

    #[test]
    fn local_active_usage_block_total_excludes_cache_tokens() {
        // The block-level token count must use only input+output, never cache.
        let start = 1_800_000_000_000;
        let messages = vec![
            message(start, 1_000, 200, 500_000, 0.10),
            message(start + 30 * MINUTE_MS, 1_000, 100, 7_000_000, 0.20),
        ];

        let now_ms = start + 40 * MINUTE_MS;
        let (usage, _) = local_active_usage_from_messages(&messages, now_ms);
        let usage = usage.expect("active block");

        // 500k and 7M cache tokens must NOT be counted — only 1200+1100 = 2300
        assert_eq!(usage.input_output_tokens, 2_300);
    }

    #[test]
    fn recent_burn_rate_span_covers_messages_within_30_min_window() {
        let start = 1_800_000_000_000;
        // Both messages within the 30-min window of now_ms (now - 25min, now - 5min)
        let msg1_ms = start;
        let msg2_ms = start + 20 * MINUTE_MS;
        let now_ms = start + 25 * MINUTE_MS; // window_start = start - 5min → both in window
        let messages = vec![
            message(msg1_ms, 1_000, 200, 500_000, 0.10),
            message(msg2_ms, 1_000, 100, 7_000_000, 0.20),
        ];

        let (_, recent_burn) = local_active_usage_from_messages(&messages, now_ms);
        let (tokens_per_min, cost_per_min) = recent_burn.expect("recent burn");

        // span = 20 min; tokens = 2300 (no cache); cost = 0.30
        assert!((tokens_per_min - (2_300.0 / 20.0)).abs() < 0.001);
        assert!((cost_per_min - (0.30 / 20.0)).abs() < 0.001);
    }

    #[test]
    fn local_active_usage_returns_none_after_block_expires() {
        let start = 1_800_000_000_000;
        let messages = vec![message(start, 1_000, 200, 0, 0.10)];

        let (usage, _) = local_active_usage_from_messages(
            &messages,
            floor_to_hour_ms(start) + CLAUDE_SESSION_WINDOW_MS,
        );
        assert!(usage.is_none());
    }

    #[test]
    fn local_active_usage_starts_new_block_after_five_hour_gap() {
        let start = 1_800_000_000_000;
        let later = start + CLAUDE_SESSION_WINDOW_MS + MINUTE_MS;
        let messages = vec![
            message(start, 10_000, 0, 0, 1.0),
            message(later, 500, 100, 0, 0.1),
        ];

        let (usage, _) =
            local_active_usage_from_messages(&messages, later + MINUTE_MS);
        let usage = usage.expect("latest active block");

        assert_eq!(usage.input_output_tokens, 600);
    }

    #[test]
    fn recent_burn_rate_uses_only_last_30_minutes() {
        let now_ms = 1_800_000_000_000i64;
        let old_msg_ms = now_ms - 60 * MINUTE_MS; // 60 min ago — outside window
        let new_msg_ms = now_ms - 10 * MINUTE_MS; // 10 min ago — inside window

        let messages = vec![
            message(old_msg_ms, 50_000, 0, 0, 5.0), // old burst — must be ignored
            message(new_msg_ms, 300, 100, 0, 0.04), // recent work
        ];

        // Active block started at old_msg_ms; both messages are within 5h.
        let (usage, recent_burn) = local_active_usage_from_messages(&messages, now_ms);
        let usage = usage.expect("active block");

        // Block total includes both messages
        assert_eq!(usage.input_output_tokens, 50_400);

        // Recent burn should only count the last message (300+100 = 400 tokens)
        // span = 1 min minimum (single message in window)
        let (tokens_per_min, _) = recent_burn.expect("recent burn");
        assert!(
            tokens_per_min <= 400.0 + 0.1,
            "recent burn must not include the 60-min-old burst (got {tokens_per_min:.1})"
        );
    }

    #[test]
    fn recent_burn_rate_is_none_when_user_is_idle() {
        let now_ms = 1_800_000_000_000i64;
        let old_msg_ms = now_ms - 45 * MINUTE_MS; // 45 min ago — outside 30-min window

        let messages = vec![message(old_msg_ms, 1_000, 200, 0, 0.10)];

        let (usage, recent_burn) = local_active_usage_from_messages(&messages, now_ms);
        // Block is still active (< 5h from start)
        assert!(usage.is_some(), "block should still be active");
        // But no recent activity → no burn rate shown
        assert!(
            recent_burn.is_none(),
            "burn rate must be None when last message was > 30 min ago"
        );
    }

    #[test]
    fn utilization_pct_drives_runs_out_not_token_count() {
        // Remaining 40% at 2%/min burn → runs out in 20 min
        let children = build_child_metrics(Some((500.0, 0.01)), 40.0, Some(2.0), None);
        assert!(
            children.iter().any(|c| c.label.contains("Runs Out")),
            "Runs Out must appear when pct burn > 0 and remaining > 0"
        );
    }

    #[test]
    fn compute_pct_burn_returns_none_with_single_sample() {
        let mut hist: VecDeque<(i64, f64, f64)> = VecDeque::new();
        hist.push_back((1_000_000, 10.0, 5.0));
        assert!(compute_pct_burn_per_min(&hist, true, 2_000_000).is_none());
    }

    #[test]
    fn compute_pct_burn_calculates_rate_from_history() {
        let now_ms = 1_800_000_000_000i64;
        let mut hist: VecDeque<(i64, f64, f64)> = VecDeque::new();
        // 10 min ago: 50% used; now: 60% used → 10%/10min = 1%/min
        hist.push_back((now_ms - 10 * MINUTE_MS, 50.0, 20.0));
        hist.push_back((now_ms, 60.0, 22.0));
        let rate = compute_pct_burn_per_min(&hist, true, now_ms).unwrap();
        assert!((rate - 1.0).abs() < 0.01, "expected ~1%/min, got {rate:.3}");
        let rate_7d = compute_pct_burn_per_min(&hist, false, now_ms).unwrap();
        assert!((rate_7d - 0.2).abs() < 0.01, "expected ~0.2%/min for 7d, got {rate_7d:.3}");
    }

    #[test]
    fn weekly_window_metric_has_correct_label_and_utilization() {
        let week = Window {
            utilization: 35.0,
            resets_at: None,
        };
        let metric = window_metric("Week Limit", &week);

        assert_eq!(metric.label, "Week Limit");
        assert!((metric.used_percent - 35.0).abs() < 0.01);
        assert!((metric.remaining_percent - 65.0).abs() < 0.01);
    }

    #[test]
    fn weekly_burn_children_show_rate_but_not_runs_out_when_no_pct_burn() {
        // Week window with no pct burn history → no Runs Out
        let children = build_child_metrics(Some((50.0, 0.005)), 65.0, None, None);

        assert!(
            children.iter().any(|c| c.label.contains("Burn Rate")),
            "Burn Rate must appear for week display"
        );
        assert!(
            children.iter().any(|c| c.label.contains("Cost Rate")),
            "Cost Rate must appear for week display"
        );
        assert!(
            !children.iter().any(|c| c.label.contains("Runs Out")),
            "Runs Out must not appear when pct burn history is absent"
        );
    }

    #[test]
    fn runs_out_hidden_when_reset_comes_before_exhaustion() {
        // 5% remaining, 0.1%/min burn → runs out in 50 min; reset in 30 min → hidden
        let children = build_child_metrics(Some((10.0, 0.001)), 5.0, Some(0.1), Some(30.0));

        assert!(
            !children.iter().any(|c| c.label.contains("Runs Out")),
            "Runs Out must not appear when reset comes before exhaustion"
        );
    }

    #[test]
    fn runs_out_shown_when_exhaustion_comes_before_reset() {
        // 5% remaining, 1%/min burn → runs out in 5 min; reset in 30 min → shown
        let children = build_child_metrics(Some((100.0, 0.01)), 5.0, Some(1.0), Some(30.0));

        assert!(
            children.iter().any(|c| c.label.contains("Runs Out")),
            "Runs Out must appear when exhaustion comes before reset"
        );
    }

    #[test]
    fn runs_out_shown_when_no_reset_time_known() {
        // no resets_at → conservative: always show "Runs Out" when pct burn known
        let children = build_child_metrics(Some((10.0, 0.001)), 5.0, Some(0.1), None);

        assert!(
            children.iter().any(|c| c.label.contains("Runs Out")),
            "Runs Out must appear when reset time is unknown"
        );
    }
}
