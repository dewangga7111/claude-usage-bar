//! Windows services: WinHTTP, the registry (autostart, theme).

use std::ffi::c_void;

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::core::{PCWSTR, w};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("ClaudeUsageBar");

pub fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn set_autostart(enabled: bool) {
    unsafe {
        if !enabled {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(value.as_ptr().cast()),
            (value.len() * 2) as u32,
        );
    }
}

/// Whether the taskbar uses the light theme (Settings > Personalization > Colors > Windows mode).
pub fn light_taskbar() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    status == ERROR_SUCCESS && value != 0
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

fn checked(handle: *mut c_void, what: &str) -> Result<Handle, String> {
    if handle.is_null() {
        Err(format!("{what}: {}", windows::core::Error::from_thread().message()))
    } else {
        Ok(Handle(handle))
    }
}

/// GET the usage endpoint through WinHTTP: uses the system proxy and TLS stack.
pub fn http_get(token: &str) -> Result<(u32, Vec<u8>), String> {
    let headers: Vec<u16> = format!(
        "Authorization: Bearer {token}\r\nanthropic-beta: {}\r\nAccept: application/json\r\n",
        crate::api::OAUTH_BETA
    )
    .encode_utf16()
    .collect();

    unsafe {
        let session = checked(
            WinHttpOpen(
                w!("claude-usage-bar/0.1"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ),
            "WinHttpOpen",
        )?;
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 15_000);
        let connect = checked(
            WinHttpConnect(session.0, w!("api.anthropic.com"), INTERNET_DEFAULT_HTTPS_PORT, 0),
            "WinHttpConnect",
        )?;
        let request = checked(
            WinHttpOpenRequest(
                connect.0,
                w!("GET"),
                w!("/api/oauth/usage"),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ),
            "WinHttpOpenRequest",
        )?;

        let err = |e: windows::core::Error| e.message().trim().to_string();
        WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0).map_err(err)?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(err)?;

        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut status).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(err)?;

        let mut body = Vec::new();
        loop {
            let mut available = 0u32;
            WinHttpQueryDataAvailable(request.0, &mut available).map_err(err)?;
            if available == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + available as usize, 0);
            let mut read = 0u32;
            WinHttpReadData(request.0, body[start..].as_mut_ptr().cast(), available, &mut read)
                .map_err(err)?;
            body.truncate(start + read as usize);
        }
        Ok((status, body))
    }
}
