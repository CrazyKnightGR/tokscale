use anyhow::Result;
use serde::Deserialize;
use tokscale_core::{parse_local_unified_messages, LocalParseOptions};

use super::helpers::capitalize;
use super::{UsageMetric, UsageOutput};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const BETA_HEADER: &str = "oauth-2025-04-20";

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

#[derive(Debug, Deserialize)]
struct Window {
    utilization: f64,
    resets_at: Option<String>,
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

fn fmt_tokens(t: u64) -> String {
    if t >= 100_000 {
        format!("{:.0}k", t as f64 / 1_000.0)
    } else if t >= 1_000 {
        format!("{:.1}k", t as f64 / 1_000.0)
    } else {
        t.to_string()
    }
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
    include_tokens: bool,
    token_limit: Option<u64>,
    tokens_used: Option<u64>,
    burn: Option<(f64, f64)>, // (tokens_per_min, cost_per_min)
    include_runs_out: bool,
) -> Vec<UsageMetric> {
    let mut children = Vec::new();

    if include_tokens {
        if let (Some(used), Some(limit)) = (tokens_used, token_limit) {
            children.push(UsageMetric {
                label: "  Tokens".into(),
                used_percent: 0.0,
                remaining_percent: -1.0,
                remaining_label: Some(format!("{}/{} used", fmt_tokens(used), fmt_tokens(limit))),
                resets_at: None,
            });
        }
    }

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

        if include_runs_out && tokens_per_min > 0.0 {
            if let (Some(limit), Some(used)) = (token_limit, tokens_used) {
                if limit > used {
                    let remaining_tokens = limit - used;
                    let mins = remaining_tokens as f64 / tokens_per_min;
                    children.push(UsageMetric {
                        label: "  Runs Out".into(),
                        used_percent: 0.0,
                        remaining_percent: -1.0,
                        remaining_label: Some(fmt_duration(mins)),
                        resets_at: None,
                    });
                }
            }
        }
    }

    children
}

pub fn fetch() -> Result<UsageOutput> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (creds, _source) = read_credentials()?;
        let oauth = creds.claude_ai_oauth.ok_or_else(|| {
            anyhow::anyhow!("No Claude OAuth credentials. Run 'claude' to log in.")
        })?;
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

        // Determine session token limit from subscription type and rate_limit_tier
        let session_token_limit: Option<u64> = match (
            oauth.subscription_type.as_deref(),
            oauth
                .rate_limit_tier
                .as_deref()
                .and_then(|t| t.rsplit('_').next()),
        ) {
            (Some("pro"), _) => Some(19_000),
            (Some("max"), Some("5")) => Some(88_000),
            (Some("max"), Some("20")) => Some(220_000),
            _ => None,
        };

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
            Err(e) => return Err(e),
        };

        // Compute session tokens used
        let session_tokens_used: Option<u64> = resp.five_hour.as_ref().and_then(|w| {
            session_token_limit
                .map(|limit| (w.utilization / 100.0 * limit as f64) as u64)
        });

        // Local scan for burn rate (last 60 minutes)
        let burn: Option<(f64, f64)> = {
            let parse_result = parse_local_unified_messages(LocalParseOptions {
                clients: Some(vec!["claude".to_string()]),
                ..Default::default()
            })
            .await;

            match parse_result {
                Ok(messages) if !messages.is_empty() => {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    let one_hour_ago_ms = now_ms - 3_600_000;
                    let recent: Vec<_> = messages
                        .iter()
                        .filter(|m| m.timestamp >= one_hour_ago_ms)
                        .collect();

                    if recent.is_empty() {
                        None
                    } else {
                        let total_tokens: i64 = recent
                            .iter()
                            .map(|m| {
                                m.tokens.input
                                    + m.tokens.output
                                    + m.tokens.cache_read
                                    + m.tokens.cache_write
                            })
                            .sum();
                        let total_cost: f64 = recent.iter().map(|m| m.cost).sum();
                        let tokens_per_min = total_tokens as f64 / 60.0;
                        let cost_per_min = total_cost / 60.0;
                        Some((tokens_per_min, cost_per_min))
                    }
                }
                _ => None,
            }
        };

        let mut metrics = Vec::new();

        if let Some(ref w) = resp.five_hour {
            metrics.push(window_metric("Session", w));
            let children = build_child_metrics(
                true,
                session_token_limit,
                session_tokens_used,
                burn,
                true,
            );
            metrics.extend(children);
        }

        if let Some(ref w) = resp.seven_day {
            metrics.push(window_metric("Weekly", w));
            let children = build_child_metrics(false, None, None, burn, false);
            metrics.extend(children);
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
