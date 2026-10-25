//! The menu bar item shows `5h 9% · 7d 49%`; clicking it opens a menu with every limit, reset
//! times, the weekly split by product, and the Refresh / Launch at Login / Quit actions.

use std::cell::{Cell, RefCell};
use std::mem::discriminant;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSFontWeightRegular, NSMenu, NSMenuDelegate, NSMenuItem, NSStatusBar, NSStatusItem,
};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer,
};

use super::sys;
use crate::api::{self, Snapshot};
use crate::{config, timefmt};

/// `NSVariableStatusItemLength`.
const VARIABLE_LENGTH: f64 = -1.0;

/// Set by the polling thread; the main-thread timer picks it up.
static DIRTY: AtomicBool = AtomicBool::new(true);

fn notify_data() {
    DIRTY.store(true, Ordering::Release);
}

#[derive(Clone, PartialEq)]
enum Row {
    /// Regular text with no action.
    Info(String),
    /// Greyed-out secondary text.
    Note(String),
    Separator,
    Refresh,
    LaunchAtLogin(bool),
    Quit,
}

struct Ivars {
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    refresh: Sender<()>,
    refreshing: Cell<bool>,
    shown_title: RefCell<String>,
    shown_rows: RefCell<Vec<Row>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and `Controller` does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct Controller;

    impl Controller {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            if DIRTY.swap(false, Ordering::AcqRel) {
                self.ivars().refreshing.set(false);
            }
            self.update();
        }

        #[unsafe(method(refresh:))]
        fn refresh(&self, _sender: Option<&AnyObject>) {
            self.start_refresh();
        }

        #[unsafe(method(toggleLaunchAtLogin:))]
        fn toggle_launch_at_login(&self, _sender: Option<&AnyObject>) {
            sys::set_launch_at_login(!sys::launch_at_login());
            self.update();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSMenuDelegate for Controller {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            let stale = api::snapshot().updated_at.is_none_or(|t| timefmt::now_unix() - t > 60);
            if stale {
                self.start_refresh();
            } else {
                self.update();
            }
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: NSObject's `init` has this signature.
        unsafe { msg_send![super(this), init] }
    }

    fn as_target(&self) -> &AnyObject {
        self.as_ref()
    }

    fn start_refresh(&self) {
        self.ivars().refreshing.set(true);
        let _ = self.ivars().refresh.send(());
        self.update();
    }

    fn update(&self) {
        let snap = api::snapshot();
        let ivars = self.ivars();

        let title = status_title(&snap);
        if *ivars.shown_title.borrow() != title {
            if let Some(button) = ivars.item.button(self.mtm()) {
                button.setTitle(&NSString::from_str(&title));
            }
            *ivars.shown_title.borrow_mut() = title;
        }

        let rows = menu_rows(&snap, ivars.refreshing.get(), sys::launch_at_login());
        if *ivars.shown_rows.borrow() != rows {
            self.apply_rows(&rows);
            *ivars.shown_rows.borrow_mut() = rows;
        }
    }

    /// Updates titles in place when only the text changed (keeps an open menu steady);
    /// rebuilds the menu when rows were added or removed.
    fn apply_rows(&self, rows: &[Row]) {
        let menu = &self.ivars().menu;
        let same_shape = {
            let old = self.ivars().shown_rows.borrow();
            old.len() == rows.len() && old.iter().zip(rows).all(|(a, b)| discriminant(a) == discriminant(b))
        };
        if same_shape {
            for (i, row) in rows.iter().enumerate() {
                if let Some(item) = menu.itemAtIndex(i as isize) {
                    configure(&item, row);
                }
            }
            return;
        }
        menu.removeAllItems();
        for row in rows {
            menu.addItem(&self.make_item(row));
        }
    }

    fn make_item(&self, row: &Row) -> Retained<NSMenuItem> {
        let mtm = self.mtm();
        let (action, key): (Option<Sel>, &str) = match row {
            Row::Separator => return NSMenuItem::separatorItem(mtm),
            Row::Refresh => (Some(sel!(refresh:)), "r"),
            Row::LaunchAtLogin(_) => (Some(sel!(toggleLaunchAtLogin:)), ""),
            Row::Quit => (Some(sel!(quit:)), "q"),
            Row::Info(_) | Row::Note(_) => (None, ""),
        };
        // SAFETY: the selectors exist on `Controller`, which is set as the target.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(""),
                action,
                &NSString::from_str(key),
            )
        };
        if action.is_some() {
            unsafe { item.setTarget(Some(self.as_target())) };
        }
        configure(&item, row);
        item
    }
}

fn configure(item: &NSMenuItem, row: &Row) {
    let title = match row {
        Row::Info(s) | Row::Note(s) => s.as_str(),
        Row::Refresh => "Refresh Now",
        Row::LaunchAtLogin(_) => "Launch at Login",
        Row::Quit => "Quit Claude Usage",
        Row::Separator => return,
    };
    item.setTitle(&NSString::from_str(title));
    item.setEnabled(!matches!(row, Row::Note(_)));
    if let Row::LaunchAtLogin(on) = row {
        item.setState(if *on { NSControlStateValueOn } else { NSControlStateValueOff });
    }
}

fn status_title(snap: &Snapshot) -> String {
    let usage = snap.usage.as_ref();
    let pct = |v: Option<f64>| v.map_or("–".to_string(), |p| format!("{p:.0}%"));
    let mut title = format!(
        "5h {} · 7d {}",
        pct(usage.and_then(|u| u.session())),
        pct(usage.and_then(|u| u.weekly()))
    );
    // Numbers shown are from an earlier poll.
    if usage.is_some() && snap.error.is_some() {
        title.push_str(" ⚠︎");
    }
    title
}

fn menu_rows(snap: &Snapshot, refreshing: bool, launch_at_login: bool) -> Vec<Row> {
    let now = timefmt::now_unix();
    let header = match snap.plan.as_deref().filter(|p| !p.is_empty()) {
        Some(plan) => format!("Claude usage · {}{}", plan[..1].to_uppercase(), &plan[1..]),
        None => "Claude usage".into(),
    };
    let mut rows = vec![Row::Note(header), Row::Separator];

    let meters = snap.usage.as_ref().map(|u| u.meters()).unwrap_or_default();
    if meters.is_empty() && snap.error.is_none() {
        rows.push(Row::Note("Loading…".into()));
    }
    for m in &meters {
        rows.push(Row::Info(format!("{}  {}  {:.0}%", m.label, text_bar(m.percent), m.percent)));
        if let Some(reset) = m.resets_at {
            let mut line = format!("Resets in {}", timefmt::duration_short(reset - now));
            if let Some(local) = timefmt::local_label(reset, now) {
                line += &format!(" · {local}");
            }
            rows.push(Row::Note(line));
        }
    }

    let breakdown = snap.usage.as_ref().map(|u| u.breakdown()).unwrap_or_default();
    if !breakdown.is_empty() {
        rows.push(Row::Separator);
        rows.push(Row::Note("This week by product".into()));
        for (name, pct) in breakdown {
            rows.push(Row::Info(format!("{name}  {pct:.0}%")));
        }
    }

    rows.push(Row::Separator);
    if let Some(err) = &snap.error {
        rows.push(Row::Note(err.clone()));
    }
    let status = if refreshing {
        "Refreshing…".to_string()
    } else {
        snap.updated_at.map_or("Not updated yet".into(), |t| format!("Updated {}", timefmt::ago(now - t)))
    };
    rows.push(Row::Note(status));
    rows.push(Row::Refresh);
    rows.push(Row::LaunchAtLogin(launch_at_login));
    rows.push(Row::Separator);
    rows.push(Row::Quit);
    rows
}

/// Ten-cell bar, e.g. `▰▰▰▰▱▱▱▱▱▱` for 40%.
fn text_bar(percent: f64) -> String {
    let filled = ((percent / 10.0).round() as usize).min(10);
    "▰".repeat(filled) + &"▱".repeat(10 - filled)
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    // Menu bar only: no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let config = config::load();
    let refresh = api::spawn_worker(Duration::from_secs(config.poll_seconds.max(30)), notify_data);

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(VARIABLE_LENGTH);
    if let Some(button) = item.button(mtm) {
        let size = NSFont::menuBarFontOfSize(0.0).pointSize();
        let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, unsafe { NSFontWeightRegular });
        button.setFont(Some(&font));
    }
    let menu = NSMenu::new(mtm);
    // Info rows have no action; keep them enabled so they don't render greyed out.
    menu.setAutoenablesItems(false);
    item.setMenu(Some(&menu));

    let controller = Controller::new(
        mtm,
        Ivars {
            item,
            menu: menu.clone(),
            refresh,
            refreshing: Cell::new(false),
            shown_title: RefCell::new(String::new()),
            shown_rows: RefCell::new(Vec::new()),
        },
    );
    menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));

    // Common modes: keep ticking while the menu is open (event tracking mode).
    unsafe {
        let timer = NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.0,
            controller.as_target(),
            sel!(tick:),
            None,
            true,
        );
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
    }
    controller.update();
    app.run();
}
