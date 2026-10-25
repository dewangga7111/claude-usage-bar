//! Windows 11: an overlay on every taskbar.

mod gfx;
mod sys;
mod ui;

pub use sys::http_get;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::core::w;

pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Held for the life of the process; a second launch exits quietly.
        let Ok(_instance) = CreateMutexW(None, true, w!("Local\\ClaudeUsageBar")) else { return };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = ui::run();
    }
}
