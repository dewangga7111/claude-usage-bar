//! Reads the Claude Code OAuth token and polls the plan usage endpoint.
//!
//! The endpoint (`/api/oauth/usage`) is the one Claude Code's `/usage` command uses. It is not a
//! public API, so every field is optional and unknown fields are ignored.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use serde::Deserialize;

use crate::timefmt::{now_unix, parse_rfc3339};

#[cfg_attr(windows, allow(dead_code))]
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub limits: Option<Vec<Limit>>,
    pub seven_day_breakdown: Option<Breakdown>,
}

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Window {
    pub utilization: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Limit {
    pub kind: Option<String>,
    pub percent: Option<f64>,
    pub resets_at: Option<String>,
}

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct Breakdown {
    pub rows: Vec<BreakdownRow>,
}

#[derive(Deserialize, Clone, Default, Debug)]
#[serde(default)]
pub struct BreakdownRow {
    pub display_name: Option<String>,
    pub percent: Option<f64>,
}

pub struct Meter {
    pub label: String,
    pub percent: f64,
    pub resets_at: Option<i64>,
}

impl Usage {
    pub fn session(&self) -> Option<f64> {
        self.five_hour.as_ref().and_then(|w| w.utilization).or_else(|| self.limit_percent("session"))
    }

    pub fn weekly(&self) -> Option<f64> {
        self.seven_day.as_ref().and_then(|w| w.utilization).or_else(|| self.limit_percent("weekly_all"))
    }

    fn limit_percent(&self, kind: &str) -> Option<f64> {
        self.limits.as_ref()?.iter().find(|l| l.kind.as_deref() == Some(kind))?.percent
    }

    /// Every limit the account has, preferring the generic `limits` list when present.
    pub fn meters(&self) -> Vec<Meter> {
        if let Some(limits) = self.limits.as_ref().filter(|l| !l.is_empty()) {
            return limits
                .iter()
                .filter_map(|l| {
                    Some(Meter {
                        label: limit_label(l.kind.as_deref()?),
                        percent: l.percent?,
                        resets_at: l.resets_at.as_deref().and_then(parse_rfc3339),
                    })
                })
                .collect();
        }
        [("Current session", &self.five_hour), ("Weekly · all models", &self.seven_day)]
            .into_iter()
            .filter_map(|(label, window)| {
                let window = window.as_ref()?;
                Some(Meter {
                    label: label.into(),
                    percent: window.utilization?,
                    resets_at: window.resets_at.as_deref().and_then(parse_rfc3339),
                })
            })
            .collect()
    }

    pub fn breakdown(&self) -> Vec<(String, f64)> {
        let Some(b) = &self.seven_day_breakdown else { return Vec::new() };
        b.rows
            .iter()
            .filter_map(|r| Some((r.display_name.clone()?, r.percent.filter(|p| *p > 0.0)?)))
            .collect()
    }
}

fn limit_label(kind: &str) -> String {
    match kind {
        "session" => "Current session".into(),
        "weekly_all" => "Weekly · all models".into(),
        other => match other.strip_prefix("weekly_") {
            Some(model) => format!("Weekly · {}", title_case(model)),
            None => title_case(other),
        },
    }
}

fn title_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug)]
pub enum FetchError {
    NoCredentials,
    Expired,
    Unauthorized,
    RateLimited,
    Http(u32),
    Network(String),
    Parse(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCredentials => write!(f, "Not signed in. Log in with Claude Code first."),
            Self::Expired => write!(f, "Login expired and Claude Code could not refresh it."),
            Self::Unauthorized => write!(f, "Not authorized. Log in again with Claude Code."),
            Self::RateLimited => write!(f, "Rate limited. Retrying in a few minutes."),
            Self::Http(status) => write!(f, "Server returned HTTP {status}."),
            Self::Network(e) => write!(f, "Network error: {e}"),
            Self::Parse(e) => write!(f, "Unexpected response: {e}"),
        }
    }
}

struct Credentials {
    access_token: String,
    expires_at_ms: Option<i64>,
    plan: Option<String>,
}

#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// The macOS Keychain first (where Claude Code keeps the token there), then
/// `.credentials.json` in the Claude config directory (Windows, Linux, older macOS installs).
fn credentials_json() -> Option<Vec<u8>> {
    #[cfg(target_os = "macos")]
    if let Some(json) = crate::platform::keychain_credentials() {
        return Some(json);
    }
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
            PathBuf::from(home).join(".claude")
        }
    };
    std::fs::read(dir.join(".credentials.json")).ok()
}

/// Re-read on every poll: Claude Code rewrites the token when it refreshes it.
fn read_credentials() -> Result<Credentials, FetchError> {
    let bytes = credentials_json().ok_or(FetchError::NoCredentials)?;
    let file: CredentialsFile = serde_json::from_slice(&bytes).map_err(|_| FetchError::NoCredentials)?;
    let oauth = file.oauth.ok_or(FetchError::NoCredentials)?;
    Ok(Credentials {
        access_token: oauth.access_token,
        expires_at_ms: oauth.expires_at,
        plan: oauth.subscription_type,
    })
}

pub struct Fetched {
    pub usage: Usage,
    pub plan: Option<String>,
}

pub fn fetch() -> Result<Fetched, FetchError> {
    let creds = read_credentials()?;
    if creds.expires_at_ms.is_some_and(|ms| ms <= now_unix() * 1000) {
        return Err(FetchError::Expired);
    }
    let (status, body) = crate::platform::http_get(&creds.access_token).map_err(FetchError::Network)?;
    match status {
        200 => {}
        401 | 403 => return Err(FetchError::Unauthorized),
        429 => return Err(FetchError::RateLimited),
        s => return Err(FetchError::Http(s)),
    }
    let usage = serde_json::from_slice(&body).map_err(|e| FetchError::Parse(e.to_string()))?;
    Ok(Fetched { usage, plan: creds.plan })
}

/// Between attempts to refresh the login through Claude Code, in seconds.
const LOGIN_REFRESH_COOLDOWN: i64 = 600;
const LOGIN_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// The Claude Code executable: `PATH` first, then the native installer's and Homebrew's
/// locations (a LaunchAgent or a login-started app may run with a minimal `PATH`).
fn claude_exe() -> Option<PathBuf> {
    let name = if cfg!(windows) { "claude.exe" } else { "claude" };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(home.map(|h| h.join(".local").join("bin")));
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Lets Claude Code refresh an expired token: any command that talks to claude.ai refreshes it
/// and writes the new one back. `claude mcp list` does that without using inference quota.
/// Refreshing here instead would rotate the refresh token behind Claude Code's back.
fn refresh_login() -> bool {
    let Some(exe) = claude_exe() else { return false };
    let mut command = std::process::Command::new(exe);
    command
        .args(["mcp", "list"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        command.current_dir(home);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else { return false };
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if started.elapsed() < LOGIN_REFRESH_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(250));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub usage: Option<Usage>,
    pub plan: Option<String>,
    pub updated_at: Option<i64>,
    pub error: Option<String>,
}

static SNAPSHOT: Mutex<Snapshot> =
    Mutex::new(Snapshot { usage: None, plan: None, updated_at: None, error: None });

pub fn snapshot() -> Snapshot {
    SNAPSHOT.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Starts the polling thread. Send `()` on the returned channel to refresh now.
pub fn spawn_worker(poll: Duration, notify: fn()) -> Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut last_login_refresh: Option<i64> = None;
        loop {
            let mut result = fetch();
            if matches!(result, Err(FetchError::Expired | FetchError::Unauthorized))
                && last_login_refresh.is_none_or(|t| now_unix() - t >= LOGIN_REFRESH_COOLDOWN)
            {
                last_login_refresh = Some(now_unix());
                if refresh_login() {
                    result = fetch();
                }
            }
            let wait = {
                let mut snap = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner());
                match result {
                    Ok(fetched) => {
                        snap.usage = Some(fetched.usage);
                        snap.plan = fetched.plan;
                        snap.updated_at = Some(now_unix());
                        snap.error = None;
                        poll
                    }
                    Err(e) => {
                        let wait = match e {
                            FetchError::RateLimited => poll.max(Duration::from_secs(300)),
                            _ => poll.min(Duration::from_secs(60)),
                        };
                        snap.error = Some(e.to_string());
                        wait
                    }
                }
            };
            notify();
            if let Err(RecvTimeoutError::Disconnected) = rx.recv_timeout(wait) {
                break;
            }
            while rx.try_recv().is_ok() {}
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_response_shape() {
        let json = r#"{"five_hour":{"utilization":2.0,"resets_at":"2026-10-24T07:40:00.137632+00:00"},
            "seven_day":{"utilization":46.0,"resets_at":"2026-10-27T00:00:00.137653+00:00"},
            "seven_day_opus":null,"extra_usage":{"is_enabled":false},
            "limits":[{"kind":"session","group":"session","percent":2,"resets_at":"2026-10-24T07:40:00+00:00"},
                      {"kind":"weekly_all","group":"weekly","percent":46,"resets_at":"2026-10-27T00:00:00+00:00"},
                      {"kind":"weekly_opus","percent":10,"resets_at":null}],
            "seven_day_breakdown":{"rows":[{"key":"claude_code","display_name":"Claude Code","percent":99},
                                           {"key":"chat","display_name":"Chats","percent":1}]}}"#;
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.session(), Some(2.0));
        assert_eq!(usage.weekly(), Some(46.0));
        let labels: Vec<_> = usage.meters().into_iter().map(|m| m.label).collect();
        assert_eq!(labels, ["Current session", "Weekly · all models", "Weekly · Opus"]);
        assert_eq!(usage.breakdown().len(), 2);
    }

    #[test]
    fn falls_back_without_limits() {
        let usage: Usage = serde_json::from_str(r#"{"five_hour":{"utilization":5},"limits":null}"#).unwrap();
        assert_eq!(usage.meters().len(), 1);
        assert_eq!(usage.weekly(), None);
    }
}
