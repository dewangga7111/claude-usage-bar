//! Layered windows: one small overlay per taskbar (primary and secondary monitors), and a details
//! popup above whichever overlay was clicked. A hidden host window owns the timer and messages.
//!
//! Windows 11 has no API for taskbar widgets, so each overlay is a topmost tool window placed over
//! its taskbar's empty space. A 250 ms timer keeps them aligned with their taskbars and above them.

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::gfx::{self, Canvas, Fonts, Rgba};
use super::sys;
use crate::api::{self, Snapshot};
use crate::config::{self, Config, Position};
use crate::timefmt;

const WM_APP_DATA: u32 = WM_APP + 1;
const WM_APP_RAISE: u32 = WM_APP + 2;
const WM_APP_HIDE: u32 = WM_APP + 3;
// Lives in `Win32_UI_Controls`; defined here to avoid pulling in that feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

const TIMER_TICK: usize = 1;
const TICK_MS: u32 = 250;

const OVERLAY_CLASS: PCWSTR = w!("ClaudeUsageBarOverlay");
const OVERLAY_W: f32 = 116.0;
/// Secondary taskbars have no tray window to measure; this clears their clock and bell.
const SECONDARY_CLOCK_W: f32 = 116.0;
const POPUP_W: f32 = 300.0;
const POPUP_GAP: f32 = 12.0;

const CMD_REFRESH: i32 = 1;
const CMD_POS_RIGHT: i32 = 2;
const CMD_POS_LEFT: i32 = 3;
const CMD_AUTOSTART: i32 = 4;
const CMD_QUIT: i32 = 5;

const CLAUDE: Rgba = Rgba(217, 119, 87, 255);
const CRITICAL: Rgba = Rgba(232, 72, 85, 255);

static HOST: AtomicIsize = AtomicIsize::new(0);

struct Palette {
    fg: Rgba,
    dim: Rgba,
    track: Rgba,
    hover: Rgba,
    surface: Rgba,
    border: Rgba,
}

impl Palette {
    fn new(light: bool) -> Palette {
        if light {
            Palette {
                fg: Rgba(0, 0, 0, 230),
                dim: Rgba(0, 0, 0, 150),
                track: Rgba(0, 0, 0, 38),
                hover: Rgba(0, 0, 0, 16),
                surface: Rgba(249, 249, 249, 255),
                border: Rgba(0, 0, 0, 40),
            }
        } else {
            Palette {
                fg: Rgba(255, 255, 255, 235),
                dim: Rgba(255, 255, 255, 160),
                track: Rgba(255, 255, 255, 45),
                hover: Rgba(255, 255, 255, 20),
                surface: Rgba(36, 36, 36, 255),
                border: Rgba(255, 255, 255, 30),
            }
        }
    }
}

fn level_color(percent: f64) -> Rgba {
    if percent >= 90.0 { CRITICAL } else { CLAUDE }
}

#[derive(Clone, Copy, PartialEq)]
struct Placement {
    rect: RECT,
    work: RECT,
    dpi: u32,
    taskbar: isize,
}

impl Placement {
    fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }
}

/// The overlay on one taskbar.
struct Bar {
    hwnd: HWND,
    placed: Placement,
    hover: bool,
}

struct Ui {
    instance: HINSTANCE,
    popup: HWND,
    bars: Vec<Bar>,
    fonts: Fonts,
    config: Config,
    refresh: Sender<()>,
    light: bool,
    /// Taskbar whose overlay the popup is open for.
    popup_for: Option<isize>,
    /// Screen-space hit box of the popup's "Refresh" link.
    refresh_hit: RECT,
    refreshing: bool,
    ticks: u32,
}

impl Ui {
    fn popup_anchor(&self) -> Option<Placement> {
        let taskbar = self.popup_for?;
        self.bars.iter().find(|b| b.placed.taskbar == taskbar).map(|b| b.placed)
    }
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Runs `f` on the UI state. Returns `None` when re-entered from a nested message (the outer
/// call still holds the state); those messages fall through to `DefWindowProcW`.
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|cell| cell.try_borrow_mut().ok().and_then(|mut ui| ui.as_mut().map(f)))
}

fn post_to_host(msg: u32) {
    let hwnd = HOST.load(Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn notify_data() {
    post_to_host(WM_APP_DATA);
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    post_to_host(WM_APP_RAISE);
}

pub fn run() -> windows::core::Result<()> {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        let classes: [(PCWSTR, WNDPROC); 3] = [
            (w!("ClaudeUsageBarHost"), Some(host_proc)),
            (OVERLAY_CLASS, Some(overlay_proc)),
            (w!("ClaudeUsageBarPopup"), Some(popup_proc)),
        ];
        for (name, proc) in classes {
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: proc,
                hInstance: instance,
                hCursor: cursor,
                lpszClassName: name,
                ..Default::default()
            };
            if RegisterClassExW(&class) == 0 {
                return Err(windows::core::Error::from_thread());
            }
        }

        // A hidden top-level window (not message-only) so it also gets broadcasts such as
        // WM_SETTINGCHANGE and WM_POWERBROADCAST.
        let host = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("ClaudeUsageBarHost"),
            w!("Claude usage"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?;
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
            w!("ClaudeUsageBarPopup"),
            w!("Claude usage details"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )?;
        HOST.store(host.0 as isize, Ordering::Relaxed);

        let config = config::load();
        let refresh = api::spawn_worker(Duration::from_secs(config.poll_seconds.max(30)), notify_data);
        UI.with(|cell| {
            *cell.borrow_mut() = Some(Ui {
                instance,
                popup,
                bars: Vec::new(),
                fonts: Fonts::load(),
                config,
                refresh,
                light: sys::light_taskbar(),
                popup_for: None,
                refresh_hit: RECT::default(),
                refreshing: false,
                ticks: 0,
            })
        });

        SetTimer(Some(host), TIMER_TICK, TICK_MS, None);
        SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT);
        with_ui(tick);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// The primary taskbar first, then one per secondary monitor.
fn taskbars() -> Vec<HWND> {
    let mut found = Vec::new();
    unsafe {
        if let Ok(primary) = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) {
            found.push(primary);
        }
        let mut after = None;
        while let Ok(next) = FindWindowExW(None, after, w!("Shell_SecondaryTrayWnd"), PCWSTR::null()) {
            found.push(next);
            after = Some(next);
        }
    }
    found
}

fn find_placement(taskbar: HWND, primary: bool, config: &Config) -> Option<Placement> {
    unsafe {
        let mut tb = RECT::default();
        GetWindowRect(taskbar, &mut tb).ok()?;
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(MonitorFromWindow(taskbar, MONITOR_DEFAULTTONEAREST), &mut info).as_bool() {
            return None;
        }
        let mon = info.rcMonitor;
        let height = tb.bottom - tb.top;
        let visible = tb.bottom.min(mon.bottom) - tb.top.max(mon.top);
        // Auto-hidden (slid off screen) or not a horizontal bar.
        if height <= 0 || visible < height / 2 || tb.right - tb.left < height * 4 {
            return None;
        }
        if fullscreen_on(mon) {
            return None;
        }

        let dpi = GetDpiForWindow(taskbar).max(96);
        let scale = dpi as f32 / 96.0;
        let width = (OVERLAY_W * scale).round() as i32;
        let gap = (4.0 * scale) as i32;
        let x = match config.position {
            Position::Right => {
                let tray = primary
                    .then(|| FindWindowExW(Some(taskbar), None, w!("TrayNotifyWnd"), PCWSTR::null()).ok())
                    .flatten()
                    .and_then(|t| {
                        let mut r = RECT::default();
                        GetWindowRect(t, &mut r).ok().map(|_| r)
                    });
                match tray {
                    Some(r) if r.left > tb.left && r.right > r.left => r.left - width - gap,
                    _ if primary => tb.right - width - (320.0 * scale) as i32,
                    _ => tb.right - width - (SECONDARY_CLOCK_W * scale) as i32 - gap,
                }
            }
            Position::Left => tb.left + (8.0 * scale) as i32,
        } + (config.offset as f32 * scale).round() as i32;

        Some(Placement {
            rect: RECT { left: x, top: tb.top, right: x + width, bottom: tb.bottom },
            work: info.rcWork,
            dpi,
            taskbar: taskbar.0 as isize,
        })
    }
}

/// A full-screen app (game, video, slideshow) owns the foreground and covers its whole monitor.
fn fullscreen_on(monitor: RECT) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_invalid() {
            return false;
        }
        let mut buf = [0u16; 64];
        let len = GetClassNameW(fg, &mut buf).max(0) as usize;
        // The desktop and the taskbars span the monitor too.
        if matches!(
            String::from_utf16_lossy(&buf[..len]).as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
        ) {
            return false;
        }
        let mut r = RECT::default();
        GetWindowRect(fg, &mut r).is_ok()
            && r.left <= monitor.left
            && r.top <= monitor.top
            && r.right >= monitor.right
            && r.bottom >= monitor.bottom
    }
}

/// The taskbar re-raises itself when clicked; put the overlay back on top of it.
fn keep_above_taskbar(overlay: HWND, taskbar: HWND) {
    unsafe {
        let mut h = overlay;
        for _ in 0..512 {
            match GetWindow(h, GW_HWNDPREV) {
                Ok(prev) if !prev.is_invalid() => {
                    if prev == taskbar {
                        let _ = SetWindowPos(
                            overlay,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                        );
                        return;
                    }
                    h = prev;
                }
                _ => return,
            }
        }
    }
}

fn create_overlay(instance: HINSTANCE) -> Option<HWND> {
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            OVERLAY_CLASS,
            w!("Claude usage"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )
        .ok()
    }
}

fn tick(ui: &mut Ui) {
    ui.ticks = ui.ticks.wrapping_add(1);
    let wanted: Vec<Placement> = taskbars()
        .into_iter()
        .enumerate()
        .filter_map(|(i, tb)| find_placement(tb, i == 0, &ui.config))
        .collect();

    // Drop overlays whose taskbar is gone, auto-hidden or under a full-screen app.
    ui.bars.retain(|bar| {
        let keep = wanted.iter().any(|p| p.taskbar == bar.placed.taskbar);
        if !keep {
            unsafe {
                let _ = DestroyWindow(bar.hwnd);
            }
        }
        keep
    });
    if ui.popup_for.is_some() && ui.popup_anchor().is_none() {
        hide_popup(ui);
    }

    let mut anchor_moved = false;
    for p in wanted {
        let i = match ui.bars.iter().position(|b| b.placed.taskbar == p.taskbar) {
            Some(i) => {
                if ui.bars[i].placed == p {
                    keep_above_taskbar(ui.bars[i].hwnd, HWND(p.taskbar as _));
                    continue;
                }
                ui.bars[i].placed = p;
                anchor_moved |= ui.popup_for == Some(p.taskbar);
                i
            }
            None => {
                let Some(hwnd) = create_overlay(ui.instance) else { continue };
                ui.bars.push(Bar { hwnd, placed: p, hover: false });
                ui.bars.len() - 1
            }
        };
        draw_bar(ui, &ui.bars[i]);
        unsafe {
            let _ = ShowWindow(ui.bars[i].hwnd, SW_SHOWNOACTIVATE);
        }
        keep_above_taskbar(ui.bars[i].hwnd, HWND(p.taskbar as _));
    }

    // Countdowns and "updated … ago" change every second.
    if ui.popup_for.is_some() && (anchor_moved || ui.ticks % (1000 / TICK_MS) == 0) {
        draw_popup(ui);
    }
}

fn draw_bars(ui: &Ui) {
    for bar in &ui.bars {
        draw_bar(ui, bar);
    }
}

fn draw_bar(ui: &Ui, bar: &Bar) {
    let p = bar.placed;
    let (w, h) = (p.rect.right - p.rect.left, p.rect.bottom - p.rect.top);
    let mut c = Canvas::new(w as u32, h as u32, p.scale());
    let (lw, lh) = (c.logical_width(), h as f32 / p.scale());
    let pal = Palette::new(ui.light);
    let fonts = &ui.fonts;

    // Alpha 0 pixels are click-through, so the idle background keeps alpha 1.
    let active = bar.hover || ui.popup_for == Some(p.taskbar);
    let bg = if active { pal.hover } else { Rgba(0, 0, 0, 1) };
    c.rrect(2.0, 4.0, lw - 4.0, lh - 8.0, 4.0, bg);

    let snap = api::snapshot();
    let usage = snap.usage.as_ref();
    let stale = snap.error.is_some();
    let rows = [("5h", usage.and_then(|u| u.session())), ("7d", usage.and_then(|u| u.weekly()))];
    let (bar_x, bar_w, bar_h) = (30.0, 42.0, 4.0);
    for (i, (label, percent)) in rows.into_iter().enumerate() {
        let mid = lh / 2.0 + if i == 0 { -8.5 } else { 8.5 };
        let baseline = mid + 4.0;
        c.text(&fonts.regular, 11.0, 10.0, baseline, label, pal.dim);
        c.rrect(bar_x, mid - bar_h / 2.0, bar_w, bar_h, bar_h / 2.0, pal.track);
        if let Some(pct) = percent.filter(|p| *p > 0.0) {
            let fill = (bar_w * (pct / 100.0).clamp(0.0, 1.0) as f32).max(bar_h);
            c.rrect(bar_x, mid - bar_h / 2.0, fill, bar_h, bar_h / 2.0, level_color(pct));
        }
        let text = percent.map_or("–".into(), |p| format!("{p:.0}%"));
        let tw = c.measure(&fonts.bold, 11.5, &text);
        c.text(&fonts.bold, 11.5, lw - 9.0 - tw, baseline, &text, if stale { pal.dim } else { pal.fg });
    }
    gfx::present(bar.hwnd, &c, p.rect.left, p.rect.top);
}

fn draw_popup(ui: &mut Ui) {
    let Some(p) = ui.popup_anchor() else { return };
    let scale = p.scale();
    let pal = Palette::new(ui.light);
    let fonts = &ui.fonts;
    let snap: Snapshot = api::snapshot();
    let now = timefmt::now_unix();

    let pad = 16.0;
    let inner = POPUP_W - pad * 2.0;
    let right = POPUP_W - pad;
    let mut c = Canvas::new((POPUP_W * scale).round() as u32, (900.0 * scale) as u32, scale);
    let mut y = pad;

    c.text(&fonts.bold, 15.0, pad, y + 14.0, "Claude usage", pal.fg);
    if let Some(plan) = snap.plan.as_deref().filter(|p| !p.is_empty()) {
        let label = plan[..1].to_uppercase() + &plan[1..];
        let tw = c.measure(&fonts.bold, 11.0, &label);
        c.rrect(right - tw - 16.0, y + 1.0, tw + 16.0, 20.0, 10.0, CLAUDE.with_alpha(48));
        c.text(&fonts.bold, 11.0, right - tw - 8.0, y + 15.0, &label, CLAUDE);
    }
    y += 36.0;

    let meters = snap.usage.as_ref().map(|u| u.meters()).unwrap_or_default();
    if meters.is_empty() && snap.error.is_none() {
        c.text(&fonts.regular, 12.5, pad, y + 12.0, "Loading…", pal.dim);
        y += 28.0;
    }
    for m in &meters {
        c.text_fit(&fonts.regular, 12.5, pad, y + 12.0, &m.label, inner - 50.0, pal.fg);
        let pct = format!("{:.0}%", m.percent);
        let tw = c.measure(&fonts.bold, 12.5, &pct);
        c.text(&fonts.bold, 12.5, right - tw, y + 12.0, &pct, pal.fg);
        y += 20.0;
        c.rrect(pad, y, inner, 6.0, 3.0, pal.track);
        if m.percent > 0.0 {
            let fill = (inner * (m.percent / 100.0).clamp(0.0, 1.0) as f32).max(6.0);
            c.rrect(pad, y, fill, 6.0, 3.0, level_color(m.percent));
        }
        y += 12.0;
        if let Some(reset) = m.resets_at {
            let mut line = format!("Resets in {}", timefmt::duration_short(reset - now));
            if let Some(local) = timefmt::local_label(reset, now) {
                line += &format!(" · {local}");
            }
            c.text(&fonts.regular, 11.0, pad, y + 11.0, &line, pal.dim);
        }
        y += 30.0;
    }

    let breakdown = snap.usage.as_ref().map(|u| u.breakdown()).unwrap_or_default();
    if !breakdown.is_empty() {
        c.rrect(pad, y - 4.0, inner, 1.0, 0.0, pal.track);
        y += 10.0;
        c.text(&fonts.regular, 11.0, pad, y + 11.0, "This week by product", pal.dim);
        y += 22.0;
        for (name, pct) in &breakdown {
            c.text_fit(&fonts.regular, 12.5, pad, y + 12.0, name, inner - 50.0, pal.fg);
            let text = format!("{pct:.0}%");
            let tw = c.measure(&fonts.regular, 12.5, &text);
            c.text(&fonts.regular, 12.5, right - tw, y + 12.0, &text, pal.dim);
            y += 21.0;
        }
        y += 8.0;
    }

    c.rrect(pad, y - 4.0, inner, 1.0, 0.0, pal.track);
    y += 10.0;
    if let Some(err) = &snap.error {
        c.text_fit(&fonts.regular, 11.0, pad, y + 11.0, err, inner, CRITICAL);
        y += 20.0;
    }
    let status = if ui.refreshing {
        "Refreshing…".to_string()
    } else {
        snap.updated_at.map_or("Not updated yet".into(), |t| format!("Updated {}", timefmt::ago(now - t)))
    };
    c.text(&fonts.regular, 11.0, pad, y + 11.0, &status, pal.dim);
    let link = "Refresh";
    let lw = c.measure(&fonts.bold, 11.0, link);
    c.text(&fonts.bold, 11.0, right - lw, y + 11.0, link, CLAUDE);
    let link_box = (right - lw - 6.0, y - 4.0, lw + 12.0, 22.0);
    y += 14.0 + pad;

    let out = c.crop_onto(y, |bg| {
        bg.rrect(0.0, 0.0, POPUP_W, y, 8.0, pal.border);
        bg.rrect(1.0, 1.0, POPUP_W - 2.0, y - 2.0, 7.0, pal.surface);
    });

    let (w, h) = (out.pix.width() as i32, out.pix.height() as i32);
    let margin = (POPUP_GAP * scale) as i32;
    let center = (p.rect.left + p.rect.right) / 2;
    let x = (center - w / 2).clamp(p.work.left + margin, (p.work.right - w - margin).max(p.work.left + margin));
    let taskbar_at_top = p.rect.top < p.work.top;
    let top = if taskbar_at_top { p.work.top + margin } else { p.work.bottom - h - margin };

    let px = |v: f32| (v * scale).round() as i32;
    ui.refresh_hit = RECT {
        left: x + px(link_box.0),
        top: top + px(link_box.1),
        right: x + px(link_box.0 + link_box.2),
        bottom: top + px(link_box.1 + link_box.3),
    };
    gfx::present(ui.popup, &out, x, top);
}

fn show_popup(ui: &mut Ui, taskbar: isize) {
    ui.popup_for = Some(taskbar);
    let snap = api::snapshot();
    if snap.updated_at.is_none_or(|t| timefmt::now_unix() - t > 60) {
        ui.refreshing = true;
        let _ = ui.refresh.send(());
    }
    draw_popup(ui);
    draw_bars(ui);
    unsafe {
        let _ = ShowWindow(ui.popup, SW_SHOW);
        let _ = SetForegroundWindow(ui.popup);
    }
}

fn hide_popup(ui: &mut Ui) {
    if ui.popup_for.take().is_none() {
        return;
    }
    unsafe {
        let _ = ShowWindow(ui.popup, SW_HIDE);
    }
    draw_bars(ui);
}

/// Shows the right-click menu. Runs outside the UI borrow because the menu pumps messages.
fn context_menu(owner: HWND) {
    let Some((position, autostart)) = with_ui(|ui| (ui.config.position, sys::autostart_enabled())) else {
        return;
    };
    let checked = |on: bool| if on { MF_STRING | MF_CHECKED } else { MF_STRING };
    let cmd = unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING, CMD_REFRESH as usize, w!("Refresh now"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, checked(position == Position::Right), CMD_POS_RIGHT as usize, w!("Show next to the clock"));
        let _ = AppendMenuW(menu, checked(position == Position::Left), CMD_POS_LEFT as usize, w!("Show on the left"));
        let _ = AppendMenuW(menu, checked(autostart), CMD_AUTOSTART as usize, w!("Start with Windows"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, w!("Quit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, None, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0
    };
    with_ui(|ui| match cmd {
        CMD_REFRESH => {
            ui.refreshing = true;
            let _ = ui.refresh.send(());
        }
        CMD_POS_RIGHT | CMD_POS_LEFT => {
            ui.config.position = if cmd == CMD_POS_LEFT { Position::Left } else { Position::Right };
            config::save(&ui.config);
            tick(ui);
        }
        CMD_AUTOSTART => sys::set_autostart(!autostart),
        CMD_QUIT => unsafe { PostQuitMessage(0) },
        _ => {}
    });
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wparam.0 == TIMER_TICK => {
            with_ui(tick);
            return LRESULT(0);
        }
        WM_APP_DATA => {
            with_ui(|ui| {
                ui.refreshing = false;
                draw_bars(ui);
                if ui.popup_for.is_some() {
                    draw_popup(ui);
                }
            });
            return LRESULT(0);
        }
        WM_APP_RAISE => {
            with_ui(tick);
            return LRESULT(0);
        }
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE => {
            with_ui(|ui| {
                ui.light = sys::light_taskbar();
                draw_bars(ui);
                tick(ui);
            });
        }
        WM_POWERBROADCAST if wparam.0 == PBT_APMRESUMEAUTOMATIC as usize => {
            with_ui(|ui| ui.refresh.send(()).ok());
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE | WM_MOUSELEAVE => {
            let hover = msg == WM_MOUSEMOVE;
            with_ui(|ui| {
                let Some(i) = ui.bars.iter().position(|b| b.hwnd == hwnd) else { return };
                if ui.bars[i].hover == hover {
                    return;
                }
                ui.bars[i].hover = hover;
                if hover {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut track);
                    }
                }
                draw_bar(ui, &ui.bars[i]);
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            with_ui(|ui| {
                let Some(taskbar) = ui.bars.iter().find(|b| b.hwnd == hwnd).map(|b| b.placed.taskbar) else {
                    return;
                };
                if ui.popup_for == Some(taskbar) {
                    hide_popup(ui);
                } else {
                    show_popup(ui, taskbar);
                }
            });
            return LRESULT(0);
        }
        WM_RBUTTONUP => {
            if let Some(popup) = with_ui(|ui| {
                hide_popup(ui);
                ui.popup
            }) {
                context_menu(popup);
            }
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Deferred: deactivation often arrives while the UI state is borrowed.
        WM_ACTIVATE if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE => unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_HIDE, WPARAM(0), LPARAM(0));
        },
        WM_APP_HIDE => {
            with_ui(hide_popup);
            return LRESULT(0);
        }
        WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
            with_ui(hide_popup);
            return LRESULT(0);
        }
        WM_SETCURSOR => {
            let on_link = with_ui(|ui| cursor_in(ui.refresh_hit)).unwrap_or(false);
            if on_link {
                unsafe {
                    if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                        SetCursor(Some(hand));
                    }
                }
                return LRESULT(1);
            }
        }
        WM_LBUTTONUP => {
            with_ui(|ui| {
                if cursor_in(ui.refresh_hit) {
                    ui.refreshing = true;
                    let _ = ui.refresh.send(());
                    draw_popup(ui);
                }
            });
            return LRESULT(0);
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn cursor_in(r: RECT) -> bool {
    let mut pt = POINT::default();
    unsafe { GetCursorPos(&mut pt).is_ok() && pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom }
}
