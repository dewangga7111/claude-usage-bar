//! Platform front ends. Each exposes `run()` and `http_get()`; macOS also `keychain_credentials()`.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("claude-usage-bar supports Windows and macOS only");
