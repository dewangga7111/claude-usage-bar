//! macOS services through the system tools: Keychain via `security`, HTTPS via `curl`, and a
//! LaunchAgent for starting at login. No extra crates and no C build steps.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Keychain item where Claude Code stores its OAuth credentials (same JSON as `.credentials.json`).
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const AGENT_LABEL: &str = "com.claude-usage-bar";

pub fn keychain_credentials() -> Option<Vec<u8>> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

pub fn http_get(token: &str) -> Result<(u32, Vec<u8>), String> {
    let fail = |e: std::io::Error| format!("curl: {e}");
    // Headers go in through stdin (`--config -`) so the token never shows up in `ps`.
    let mut child = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--max-time", "20", "--config", "-"])
        .args(["--write-out", "\n%{http_code}", crate::api::USAGE_URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(fail)?;
    let config = format!(
        "header = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: {}\"\nheader = \"Accept: application/json\"\n",
        crate::api::OAUTH_BETA
    );
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(config.as_bytes()).map_err(fail)?;
    }
    let out = child.wait_with_output().map_err(fail)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }

    // `--write-out` appends "\n<status>" after the body.
    let split = out.stdout.iter().rposition(|&b| b == b'\n').ok_or("curl: missing status code")?;
    let status = std::str::from_utf8(&out.stdout[split + 1..])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or("curl: bad status code")?;
    let mut body = out.stdout;
    body.truncate(split);
    Ok((status, body))
}

fn agent_path() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join("Library").join("LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
}

pub fn launch_at_login() -> bool {
    agent_path().is_some_and(|p| p.exists())
}

pub fn set_launch_at_login(enabled: bool) {
    let Some(path) = agent_path() else { return };
    if !enabled {
        let _ = std::fs::remove_file(path);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let exe = exe.display().to_string().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, plist);
}
