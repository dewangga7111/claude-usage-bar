# claude-usage-bar

A small widget that shows your Claude plan usage: the current 5-hour session and the weekly limit.

- **Windows 11:** every taskbar (primary and secondary monitors) shows `5h` and `7d` bars with percentages. Left-click opens a details popup with every limit, reset countdowns, the weekly split by product, and a Refresh link. Right-click opens a menu: refresh now, position (next to the clock or on the left), start with Windows, quit.
- **macOS:** a menu bar item shows `5h 9% · 7d 49%`. Clicking it opens a menu with the same details, plus Refresh Now, Launch at Login and Quit.

## How it works

- **Data:** reads the OAuth token that Claude Code stores and calls `GET https://api.anthropic.com/api/oauth/usage`, the endpoint behind Claude Code's `/usage`. It polls every 2 minutes. The endpoint is undocumented and may change without notice. It also rate-limits: after a `429` the widget waits 5 minutes before trying again.
- **Token location:**
  - Windows: `%USERPROFILE%\.claude\.credentials.json` (or `%CLAUDE_CONFIG_DIR%`).
  - macOS: the Keychain item `Claude Code-credentials`, read with `/usr/bin/security`. Falls back to `~/.claude/.credentials.json`.
- **Token refresh:** the widget never refreshes the token itself. Claude Code does that when you use it. If the token expires while Claude Code is idle, the widget shows a "Login expired" message until Claude Code runs again.
- **Windows taskbar placement:** Windows 11 has no taskbar widget API. Each widget is a topmost, layered tool window placed over the free space of one taskbar (`Shell_TrayWnd` or `Shell_SecondaryTrayWnd`). Widgets are added and removed as monitors connect and disconnect. Each one follows its taskbar's size and DPI, hides when a full-screen app covers its monitor or its taskbar auto-hides, and puts itself back above the taskbar when the taskbar is clicked. The popup opens on the monitor whose widget you clicked.
- **Windows secondary taskbars:** these have no tray window to measure, so "next to the clock" keeps a fixed 116 px (logical) gap from the right edge for the clock and bell.
- **macOS:** a standard `NSStatusItem` with an `NSMenu`, no Dock icon. HTTPS goes through `/usr/bin/curl`, with the token passed on stdin so it never appears in `ps`. Launch at Login writes `~/Library/LaunchAgents/com.claude-usage-bar.plist`.

## Code layout

| Path | Contents |
|---|---|
| `src/api.rs` | Usage model, credentials lookup, polling thread (shared) |
| `src/config.rs`, `src/timefmt.rs` | Config file, time parsing and formatting (shared) |
| `src/platform/win/` | Taskbar overlays, popup, software rendering, WinHTTP, registry |
| `src/platform/mac/` | Menu bar item and menu, Keychain, curl, LaunchAgent |

## Build

```sh
cargo build --release
```

- **Windows:** the `stable-x86_64-pc-windows-gnu` toolchain with MinGW-w64 (`dlltool` and `as`) on `PATH`, or the MSVC toolchain. Run `target\release\claude-usage-bar.exe`.
- **macOS:** Rust from rustup and the Xcode Command Line Tools (`xcode-select --install`). Run `./target/release/claude-usage-bar`. For a universal binary:

  ```sh
  rustup target add aarch64-apple-darwin x86_64-apple-darwin
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  lipo -create -output claude-usage-bar \
    target/aarch64-apple-darwin/release/claude-usage-bar \
    target/x86_64-apple-darwin/release/claude-usage-bar
  ```

`.github/workflows/build.yml` builds and tests both platforms on GitHub Actions and uploads the binaries.

## Config

`%APPDATA%\claude-usage-bar\config.json` on Windows, `~/Library/Application Support/claude-usage-bar/config.json` on macOS:

```json
{
  "position": "right",
  "offset": 0,
  "poll_seconds": 120
}
```

- `position` (Windows only): `right` (next to the clock) or `left`. Changing it from the menu writes this file.
- `offset` (Windows only): extra horizontal shift in logical pixels. Negative values move left.
- `poll_seconds`: poll interval, minimum 30.

Restart the widget after editing the file by hand.
