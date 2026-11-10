//! Notification-area icons for the dock while the taskbar is hidden.
//!
//! An invisible `Shell_TrayWnd` of our own wins `FindWindow`, so apps'
//! Shell_NotifyIcon calls reach it first. It copies each icon's metadata and
//! image, then forwards the original packet to Explorer's tray and returns
//! Explorer's answer, so Explorer keeps every icon. If Wisp exits or crashes
//! the proxy window goes away and apps talk to Explorer directly again.
//! See research/02-tray-host.md.

#[path = "tray_host/model.rs"]
pub mod model;
#[path = "tray_host/proxy.rs"]
mod proxy;
#[path = "tray_host/wire.rs"]
pub mod wire;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, Result};

pub use model::{Action, Key};
pub use proxy::{PROXY_PROP, explorer_tray};

/// A copy of an app's icon, destroyed with its last reference.
#[derive(Debug)]
pub struct OwnedIcon(pub HICON);
// HICONs are USER handles usable from any thread of the process.
unsafe impl Send for OwnedIcon {}
unsafe impl Sync for OwnedIcon {}
impl Drop for OwnedIcon {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(self.0);
        }
    }
}

pub type Icon = model::Icon<Arc<OwnedIcon>>;

/// Where the dock shows icons, in physical screen pixels, for
/// Shell_NotifyIconGetRect. Icons the dock doesn't show use `fallback`,
/// the chevron; without one, Explorer answers.
#[derive(Debug, Clone, Default)]
pub struct Rects {
    pub icons: HashMap<Key, RECT>,
    pub fallback: Option<RECT>,
}

struct Shared {
    model: Mutex<model::Model<Arc<OwnedIcon>>>,
    rects: Mutex<Rects>,
    /// The window told about changes, and with which message.
    notify: AtomicUsize,
    notify_msg: u32,
    /// A change notification is posted and not yet read.
    pending: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn changed(&self) {
        if !self.pending.swap(true, Ordering::SeqCst) {
            let hwnd = HWND(self.notify.load(Ordering::SeqCst) as _);
            unsafe {
                let _ = PostMessageW(Some(hwnd), self.notify_msg, WPARAM(0), LPARAM(0));
            }
        }
    }
}

/// Runs the proxy on its own thread; dropping it removes the proxy.
pub struct TrayHost {
    shared: Arc<Shared>,
    control: HWND,
    thread: Option<JoinHandle<()>>,
}

impl TrayHost {
    /// Posts `msg` to `notify` whenever the icons change. Fails without
    /// side effects if Explorer's tray is missing, another Wisp hosts the
    /// tray, or our window does not win `FindWindow`.
    pub fn start(notify: HWND, msg: u32) -> Result<Self> {
        let shared = Arc::new(Shared {
            model: Mutex::default(),
            rects: Mutex::default(),
            notify: AtomicUsize::new(notify.0 as usize),
            notify_msg: msg,
            pending: AtomicBool::new(false),
        });
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("tray-host".into())
            .spawn(move || proxy::run(thread_shared, ready_tx))
            .map_err(|e| Error::new(E_FAIL, e.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(control)) => Ok(Self {
                shared,
                control: HWND(control as _),
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(Error::new(E_FAIL, message))
            }
            // The thread is stuck; leave it, it exits with the process.
            Err(_) => Err(Error::new(E_FAIL, "The tray host did not start within 5 seconds")),
        }
    }

    /// Standalone mode (Coquille): no proxy thread and no
    /// Explorer to forward to — the broker across the pipe owns
    /// the receiver and feeds this `Shared` through
    /// `remote_apply`. The shared is registered globally because
    /// the pipe client (crate::coquille) lives outside the dock
    /// that owns this host; the registration is a Weak so a closed
    /// dock takes the feed down with it.
    pub fn start_standalone(notify: HWND, msg: u32) -> Result<Self> {
        let shared = Arc::new(Shared {
            model: Mutex::default(),
            rects: Mutex::default(),
            notify: AtomicUsize::new(notify.0 as usize),
            notify_msg: msg,
            pending: AtomicBool::new(false),
        });
        *REMOTE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&shared));
        Ok(Self {
            shared,
            control: HWND::default(),
            thread: None,
        })
    }

    /// The current icons, in Windows' order where it has one.
    pub fn icons(&self) -> Vec<Icon> {
        self.shared.pending.store(false, Ordering::SeqCst);
        let mut icons = lock(&self.shared.model).icons.clone();
        icons.sort_by_key(|icon| icon.order.unwrap_or(usize::MAX));
        icons
    }

    pub fn set_rects(&self, rects: Rects) {
        *lock(&self.shared.rects) = rects;
    }

    /// Restores FindWindow ordering on the host thread after Explorer shows its tray.
    pub fn outrank(&self) {
        unsafe {
            let _ = PostMessageW(Some(self.control), proxy::WM_OUTRANK, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for TrayHost {
    fn drop(&mut self) {
        if !self.control.is_invalid() {
            unsafe {
                let _ = PostMessageW(Some(self.control), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        // Forwarding is bounded by its timeout, so this ends soon.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The standalone feed's target: the `Shared` of the currently
/// hosted (standalone) tray, registered by `start_standalone`.
static REMOTE: Mutex<Option<std::sync::Weak<Shared>>> = Mutex::new(None);

fn remote_shared() -> Option<Arc<Shared>> {
    REMOTE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
}

/// Whether a standalone tray host is currently registered.
pub fn remote_active() -> bool {
    remote_shared().is_some()
}

/// Identity of the registered standalone shared, so the pipe
/// client can tell a *replacement* host (dock closed and
/// reopened) from the one it already fed.
pub fn remote_id() -> usize {
    remote_shared().map(|s| Arc::as_ptr(&s) as usize).unwrap_or(0)
}

/// Applies one broker-fed packet to the standalone model. The
/// broker's apply is the acceptance Explorer's verdict used to
/// provide, so `accepted` is unconditionally true here.
pub fn remote_apply(n: &wire::Notify, image: Option<Option<Arc<OwnedIcon>>>, owner: model::Owner) -> bool {
    let Some(shared) = remote_shared() else {
        return false;
    };
    let changed = lock(&shared.model).apply(n, true, image, owner);
    if changed {
        shared.changed();
    }
    changed
}

/// Sends an app its callback for a gesture on its icon, as Explorer would.
/// `x` and `y` are the icon's center in physical screen pixels.
pub fn invoke(icon: &Icon, action: Action, x: i32, y: i32) -> std::result::Result<(), String> {
    let hwnd = HWND(wire::handle(icon.hwnd) as _);
    unsafe {
        if icon.callback == 0 {
            return Err("The icon has no callback message".into());
        }
        let mut pid = 0;
        if !IsWindow(Some(hwnd)).as_bool() || GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 {
            return Err("The icon's window is gone".into());
        }
        if icon.pid != 0 && pid != icon.pid {
            return Err("The icon's window belongs to another process now".into());
        }
        // The app needs the foreground for its menu or window. The click gave
        // Wisp the right to pass it on.
        if let Err(error) = AllowSetForegroundWindow(pid) {
            eprintln!("Tray foreground for {pid}: {error}");
        }
        // Posted, so a menu's modal loop in the app never blocks the dock.
        for event in model::events(action, icon.version) {
            let (wp, lp) = model::callback_words(icon.version, icon.uid, event, x, y);
            PostMessageW(Some(hwnd), icon.callback, WPARAM(wp), LPARAM(lp)).map_err(|e| e.message())?;
        }
    }
    Ok(())
}
