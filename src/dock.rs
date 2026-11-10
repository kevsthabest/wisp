//! Independent, nonactivating app dock on the message-loop thread.

#[path = "dock/desktops.rs"]
mod desktops;
#[path = "dock/logic.rs"]
pub(crate) mod logic;
#[path = "dock/render.rs"]
mod render;
#[path = "dock/space.rs"]
pub mod space;

#[path = "dock/status.rs"]
mod status;
#[path = "dock/thumbs.rs"]
mod thumbs;
#[path = "dock/tray.rs"]
mod tray;

/// Reserved height in DIPs at the default app icon size.
pub const DEFAULT_RESERVED_HEIGHT: f32 = logic::HEIGHT + 2.0 * logic::MARGIN;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{
    CoTaskMemFree,
    StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetDoubleClickTime, GetKeyState, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE,
    VK_LBUTTON, VK_RBUTTON, VK_SHIFT,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Error, GUID, Result, w};

use crate::commands::open_windows::{self, Window};
use crate::config::{Backdrop, WindowListMode};
use crate::indexer::{AppEntry, IconPixels};
use crate::theme::Palette;
use crate::tray_host::{self, Action, TrayHost};

pub const WM_SHOW_LAUNCHER: u32 = WM_APP + 20;
const WM_REFRESH: u32 = WM_APP + 21;
/// The tray host's icons changed.
const WM_TRAY_CHANGED: u32 = WM_APP + 22;
/// Asks the owner to show the Windows taskbar for a moment, for icons the
/// dock lacks.
pub const WM_DOCK_SHOW_TASKBAR: u32 = WM_APP + 23;
const WM_CLOSE_FLYOUT: u32 = WM_APP + 24;
/// Shows a notification icon's tooltip after a short hover.
const TIP_TIMER: usize = 104;
const TIP_MS: u32 = 450;
/// Tooltips wrap past this width, in DIPs.
const TIP_MAX: f32 = 320.0;
/// Lists an app's windows after the same hover, and hides it once the pointer
/// has left both the list and the dock.
const WINDOWS_TIMER: usize = 107;
const WINDOWS_HIDE_TIMER: usize = 108;
const WINDOWS_HIDE_MS: u32 = 220;
const REFRESH_TIMER: usize = 101;
/// Watches the pointer for the bottom edge while the dock is hidden, and
/// for leaving while it shows.
const POLL_TIMER: usize = 102;
const POLL_MS: u32 = 100;
/// Polls the pointer may spend away from the dock before it hides.
const HIDE_AFTER: u8 = 5;
/// Steps the slide-in animation.
const SLIDE_TIMER: usize = 103;
const SLIDE_MS: f32 = 180.0;
const HOVER_TIMER: usize = 105;
const HOVER_MS: f32 = 110.0;
/// Hides the desktop pill after a scroll.
const DESKTOP_TIMER: usize = 106;
const DESKTOP_PILL_MS: u32 = 1100;
/// Wheel input right after a switch is the same flick, not another request.
const DESKTOP_PAUSE: Duration = Duration::from_millis(300);
thread_local! { static EVENT_TARGET: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) }; }

struct Item {
    app: AppEntry,
    windows: Vec<Window>,
}

struct State {
    hwnd: HWND,
    owner: HWND,
    apps: Vec<AppEntry>,
    icons: HashMap<String, Arc<IconPixels>>,
    items: Vec<Item>,
    palette: Palette,
    backdrop: Backdrop,
    window_list_mode: WindowListMode,
    app_icon_size: f32,
    renderer: render::Renderer,
    dpi: u32,
    hover: Option<usize>,
    visible: bool,
    fullscreen: bool,
    scheduled: bool,
    scroll: usize,
    capacity: usize,
    status: status::Status,
    hover_part: Option<logic::Part>,
    /// The pointer touched the bottom edge, so the dock slid in.
    revealed: bool,
    away: u8,
    /// Stays revealed regardless of the pointer, for the demo's screenshots,
    /// optionally at another DPI than the monitor's.
    held: bool,
    /// Whether the dock hides until the pointer touches a bottom edge. When
    /// false it stays on the primary screen.
    autohide: bool,
    suspended: bool,
    frame_paced: bool,
    held_dpi: Option<u32>,
    /// Where the window rests, in physical pixels, and when it began sliding there.
    top: i32,
    slide: Option<Instant>,
    slide_from: f32,
    slide_to: f32,
    hover_levels: Vec<f32>,
    hover_tick: Option<Instant>,
    /// Wheel delta over the volume not yet spent on a whole percent.
    wheel: i32,
    /// Wheel travel toward the next desktop switch, and when the last one happened.
    desktop_travel: i32,
    desktop_switched: Option<Instant>,
    /// The desktops the pill above the dock shows, while it shows.
    desktop_pill: Option<desktops::Desktops>,
    /// The dock's left edge on screen, in physical pixels.
    left: i32,
    width: i32,
    /// The window's height on screen, in physical pixels. Width
    /// and height swap roles on vertical edges, so both are
    /// stored rather than recomputing one from the layout.
    win_height: i32,
    /// Which screen edge the dock is anchored to. Geometry is
    /// computed in dock space (logic::Edge) and mapped at the
    /// window boundary; Bottom is the historical behaviour.
    edge: logic::Edge,
    surface_rect: Option<RECT>,
    /// Hosts the notification area while the taskbar is hidden.
    tray_host: Option<TrayHost>,
    tray: Vec<tray::Shown>,
    /// Indices into `tray` of the icons shown in the dock itself.
    inline: Vec<usize>,
    /// Lists every notification icon above the chevron.
    flyout: HWND,
    flyout_open: bool,
    flyout_hover: Option<usize>,
    flyout_renderer: render::Renderer,
    tip: HWND,
    tip_renderer: render::Renderer,
    /// What the pointer rests on, and the text the tooltip shows for it.
    tip_target: Option<tray::Tip>,
    tip_text: String,
    /// The window list above the hovered app icon: which slot it belongs to,
    /// its rows and their windows, and the row under the pointer.
    windows: HWND,
    windows_renderer: render::Renderer,
    windows_slot: Option<usize>,
    windows_rows: Vec<String>,
    windows_targets: Vec<isize>,
    windows_hover: Option<usize>,
    /// Live previews of the listed windows, drawn by DWM over the list.
    windows_thumbs: thumbs::Thumbs,
    /// The last click on a notification icon, to tell double clicks.
    tray_click: Option<(tray_host::Key, Instant)>,
    /// The foreground class the narration last reported as
    /// blocking a reveal, so the poll loop (10×/s) reports a
    /// blocked reveal once per blocking window, not forever.
    block_class: String,
    /// The demo's flyout and its hovered icon, applied as icons arrive.
    held_flyout: Option<Option<usize>>,
}

/// Own on the launcher's message-loop thread. Never borrows Launcher from its wndproc.
pub struct Dock {
    hwnd: HWND,
    shared: bool,
    state: Box<RefCell<State>>,
    hooks: Vec<HWINEVENTHOOK>,
}

impl Dock {
    pub fn new(owner: HWND) -> Result<Self> {
        Self::create(owner, false)
    }

    pub fn on_surface(owner: HWND) -> Result<Self> {
        Self::create(owner, true)
    }

    fn create(owner: HWND, shared: bool) -> Result<Self> {
        if EVENT_TARGET.with(|target| !target.get().is_invalid()) {
            return Err(Error::new(E_FAIL, "A dock already exists on this thread"));
        }
        let mut state = Box::new(RefCell::new(State {
            hwnd: HWND::default(),
            owner,
            apps: Vec::new(),
            icons: HashMap::new(),
            items: Vec::new(),
            palette: Palette::new(true, true),
            backdrop: Backdrop::Acrylic,
            window_list_mode: WindowListMode::default(),
            app_icon_size: logic::DEFAULT_APP_ICON_SIZE,
            renderer: render::Renderer::new()?,
            dpi: 96,
            hover: None,
            visible: false,
            fullscreen: false,
            scheduled: false,
            scroll: 0,
            capacity: usize::MAX,
            status: status::read(),
            hover_part: None,
            revealed: false,
            away: 0,
            held: false,
            autohide: true,
            suspended: false,
            frame_paced: false,
            held_dpi: None,
            top: 0,
            slide: None,
            slide_from: 0.0,
            slide_to: 0.0,
            hover_levels: Vec::new(),
            hover_tick: None,
            wheel: 0,
            desktop_travel: 0,
            desktop_switched: None,
            desktop_pill: None,
            left: 0,
            width: 0,
            win_height: 0,
            edge: logic::Edge::Bottom,
            surface_rect: None,
            tray_host: None,
            tray: Vec::new(),
            inline: Vec::new(),
            flyout: HWND::default(),
            flyout_open: false,
            flyout_hover: None,
            flyout_renderer: render::Renderer::new()?,
            tip: HWND::default(),
            tip_renderer: render::Renderer::new()?,
            tip_target: None,
            tip_text: String::new(),
            windows: HWND::default(),
            windows_renderer: render::Renderer::new()?,
            windows_slot: None,
            windows_rows: Vec::new(),
            windows_targets: Vec::new(),
            windows_hover: None,
            windows_thumbs: thumbs::Thumbs::new(),
            tray_click: None,
            held_flyout: None,
            block_class: String::new(),
        }));
        let instance = unsafe { GetModuleHandleW(None)? };
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("Wisp.Dock"),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&class) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread());
        }
        let hwnd = if shared {
            unsafe {
                SetWindowLongPtrW(owner, GWLP_USERDATA, (&raw mut *state) as isize);
            }
            owner
        } else {
            unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                    class.lpszClassName,
                    w!("Wisp Dock"),
                    WS_POPUP | WS_CAPTION,
                    0,
                    0,
                    64,
                    44,
                    None,
                    None,
                    Some(instance.into()),
                    Some((&raw mut *state).cast()),
                )?
            }
        };
        state.borrow_mut().hwnd = hwnd;
        let mut dock = Self {
            hwnd,
            shared,
            state,
            hooks: Vec::new(),
        };
        // The flyout and tooltips are owned by the dock, so they stay above it.
        let popup = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(popup_wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("Wisp.DockPopup"),
            hCursor: class.hCursor,
            ..Default::default()
        };
        if unsafe { RegisterClassExW(&popup) } == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(Error::from_thread());
        }
        for (style, title) in [
            // The flyout takes focus, for Esc and closing on a click elsewhere.
            (WS_EX_TOOLWINDOW | WS_EX_TOPMOST, w!("Notification area")),
            (
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
                w!("Wisp Dock tooltip"),
            ),
            // The window list takes clicks, but not the focus.
            (
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                w!("Wisp Dock windows"),
            ),
        ] {
            let window = unsafe {
                CreateWindowExW(
                    style,
                    popup.lpszClassName,
                    title,
                    WS_POPUP | WS_CAPTION,
                    0,
                    0,
                    1,
                    1,
                    Some(hwnd),
                    None,
                    Some(instance.into()),
                    Some((&raw mut *dock.state).cast()),
                )?
            };
            let mut s = dock.state.borrow_mut();
            if s.flyout.is_invalid() {
                s.flyout = window;
            } else if s.tip.is_invalid() {
                s.tip = window;
            } else {
                s.windows = window;
            }
        }
        EVENT_TARGET.with(|target| target.set(hwnd));
        for (first, last) in [
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_NAMECHANGE),
            (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_LOCATIONCHANGE),
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            // Windows on other desktops are cloaked, so switching desktops changes the apps.
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ] {
            let hook = unsafe {
                SetWinEventHook(
                    first,
                    last,
                    None,
                    Some(window_event),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if hook.is_invalid() {
                return Err(Error::from_thread());
            }
            dock.hooks.push(hook);
        }
        dock.apply_theme(Palette::new(true, true), Backdrop::Acrylic);
        dock.refresh();
        Ok(dock)
    }

    pub fn draw_on(&mut self, rt: &windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget) -> Result<()> {
        self.state.borrow_mut().paint(Some(rt))
    }

    pub fn shared(&self) -> bool {
        self.shared
    }

    pub fn dispatch(&self, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { wndproc(self.hwnd, msg, wp, lp) }
    }

    pub fn owns_message(msg: u32, wp: WPARAM) -> bool {
        msg == WM_TIMER && (REFRESH_TIMER..=WINDOWS_HIDE_TIMER).contains(&wp.0)
            || matches!(msg, WM_REFRESH | WM_TRAY_CHANGED)
    }

    pub fn mostly_visible(&self) -> bool {
        let s = self.state.borrow();
        unsafe { crate::morph::can_morph(IsWindowVisible(self.hwnd).as_bool(), s.slide_value()) }
    }

    pub fn pace_frames(&mut self) {
        self.state.borrow_mut().frame_paced = true;
    }

    /// Slides the dock in as the pointer would, and keeps it in, for captures.
    pub fn capture_reveal(&mut self) {
        let mut s = self.state.borrow_mut();
        s.held = true;
        s.reveal("capture hold");
    }

    pub fn capture_slide(&mut self) {
        let mut s = self.state.borrow_mut();
        let to = if s.slide_to > 0.5 { 0.0 } else { 1.0 };
        s.revealed = to < 0.5;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
        s.start_slide(to);
    }

    pub fn animating(&self) -> bool {
        self.state.borrow().slide.is_some()
    }

    pub fn frame(&mut self) {
        let mut s = self.state.borrow_mut();
        if s.slide.is_some() {
            s.step_slide();
        }
    }

    /// Pauses placement without hiding the shared surface.
    pub fn suspend(&mut self) -> RECT {
        let mut s = self.state.borrow_mut();
        s.close_flyout();
        s.hide_tip();
        s.hide_windows();
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
            if !IsWindowVisible(self.hwnd).as_bool() {
                // Park the surface just past the anchor edge, in
                // whichever direction that edge hides toward.
                let layout_scale = logic::layout_scale(s.app_icon_size);
                let margin = (logic::margin(layout_scale) * s.dpi as f32 / 96.0).round() as i32;
                let dir = s.edge.hide_dir();
                let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
                let cross = if s.edge.is_vertical() { w } else { h };
                rect.left = s.left + dir.0 * (cross + margin);
                rect.top = s.top + dir.1 * (cross + margin);
                rect.right = rect.left + w;
                rect.bottom = rect.top + h;
            }
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
        }
        s.stop_slide();
        s.suspended = true;
        s.narrate("suspend: the launcher takes the surface; the edge poll is off");
        rect
    }

    pub fn resume(&mut self) {
        let mut s = self.state.borrow_mut();
        s.suspended = false;
        s.surface_rect = None;
        s.revealed = true;
        s.visible = true;
        s.away = 0;
        s.stop_slide();
        s.refresh();
        unsafe {
            SetTimer(Some(self.hwnd), POLL_TIMER, POLL_MS, None);
        }
        s.narrate("resume: back from the launcher; the edge poll is on");
    }

    pub fn surface_rect(&mut self, rect: RECT) {
        let mut s = self.state.borrow_mut();
        s.surface_rect = Some(rect);
        s.publish_rects();
    }

    pub fn resting_rect(&self) -> RECT {
        let s = self.state.borrow();
        RECT {
            left: s.left,
            top: s.top,
            right: s.left + s.width,
            bottom: s.top + s.win_height,
        }
    }

    /// Call after index or icon batches.
    pub fn set_apps(&mut self, apps: &[AppEntry], icons: &HashMap<String, Arc<IconPixels>>) {
        let mut s = self.state.borrow_mut();
        s.apps = apps.to_vec();
        for (id, pixels) in icons {
            s.icons.entry(id.clone()).or_insert_with(|| pixels.clone());
        }
        s.refresh();
    }

    pub fn set_window_list_mode(&mut self, mode: WindowListMode) {
        let mut s = self.state.borrow_mut();
        s.window_list_mode = mode;
        s.hide_windows();
    }

    pub fn set_app_icon_size(&mut self, size: u32) {
        let size = (size as f32).clamp(logic::MIN_APP_ICON_SIZE, logic::MAX_APP_ICON_SIZE);
        let mut s = self.state.borrow_mut();
        if s.app_icon_size == size {
            return;
        }
        s.app_icon_size = size;
        s.place();
        unsafe {
            let _ = InvalidateRect(Some(s.hwnd), None, false);
        }
    }

    pub fn refresh(&mut self) {
        self.state.borrow_mut().refresh();
    }

    pub fn apply_theme(&mut self, palette: Palette, backdrop: Backdrop) {
        self.state.borrow_mut().apply_material(palette.text.r > 0.5, backdrop);
    }

    /// Hosts the notification area in the dock while `enabled`, which
    /// should follow the hidden taskbar. Failure leaves icons to Explorer.
    pub fn set_tray(&mut self, enabled: bool) {
        let mut s = self.state.borrow_mut();
        if enabled == s.tray_host.is_some() {
            return;
        }
        if enabled {
            // The tray's producer is the only thing the backend
            // changes: Explorer's proxy, or the Coquille broker's
            // pipe feed. Everything downstream reads the same model.
            let started = if crate::backend::is_standalone() {
                TrayHost::start_standalone(self.hwnd, WM_TRAY_CHANGED)
            } else {
                TrayHost::start(self.hwnd, WM_TRAY_CHANGED)
            };
            match started {
                Ok(host) => s.tray_host = Some(host),
                Err(error) => {
                    eprintln!("Dock notification area: {error}");
                    return;
                }
            }
        } else {
            s.close_flyout();
            s.hide_tip();
            s.tray_host = None;
        }
        s.sync_tray();
    }

    /// Keeps notification icons reaching Wisp after Explorer shows its taskbar.
    pub fn outrank_tray(&self) {
        if let Some(host) = &self.state.borrow().tray_host {
            host.outrank();
        }
    }

    /// Lets the dock auto-hide: it appears when the pointer reaches the
    /// bottom edge of a screen.
    pub fn show(&mut self) {
        self.present(false);
    }

    /// Shows the dock and keeps it on screen. A standalone shell
    /// session has no taskbar-peek sequence to reveal the dock and
    /// no Explorer bar to fall back on: the bar is the session's
    /// furniture, present from logon, so the config's auto-hide
    /// default (written for life alongside Explorer) does not
    /// apply until the shell says otherwise.
    pub fn show_revealed(&mut self) {
        self.present(true);
    }

    fn present(&mut self, revealed: bool) {
        self.state.borrow_mut().suspended = false;
        let mut s = self.state.borrow_mut();
        s.visible = true;
        // A dock that stays visible comes back in place, without sliding.
        s.revealed = revealed || !s.autohide;
        s.surface_rect = None;
        s.stop_slide();
        unsafe {
            SetTimer(Some(self.hwnd), POLL_TIMER, POLL_MS, None);
        }
        s.narrate(if revealed {
            "present: shown revealed"
        } else {
            "present: shown, autohide decides"
        });
        s.refresh();
    }
    /// Reveals the dock for good, with a fixed hover, so screenshots can show it
    /// without moving the pointer.
    pub fn hold(&mut self, hover: Option<usize>, hover_part: Option<&str>, dpi: Option<u32>) {
        let mut s = self.state.borrow_mut();
        s.held = true;
        s.held_dpi = dpi;
        s.revealed = true;
        s.hover = hover;
        s.animate_hover();
        s.hover_part = match hover_part {
            Some("volume") => Some(logic::Part::Volume),
            Some("battery") => Some(logic::Part::Battery),
            Some("clock") => Some(logic::Part::Clock),
            Some("chevron") => Some(logic::Part::Chevron),
            Some(part) => part
                .strip_prefix("tray")
                .and_then(|i| i.parse().ok())
                .map(logic::Part::Tray),
            _ => None,
        };
        // "flyout" opens the flyout, "flyout2" also hovers its third icon.
        s.held_flyout = hover_part
            .and_then(|part| part.strip_prefix("flyout"))
            .map(|rest| rest.parse().ok());
        s.reveal("held for capture");
        s.stop_slide();
        s.place();
        if hover_part == Some("desktops") {
            s.show_desktops(desktops::read());
        }
        s.sync_tray();
    }
    /// Keeps the dock visible on the primary screen, or lets it auto-hide.
    pub fn set_autohide(&mut self, autohide: bool) {
        let mut s = self.state.borrow_mut();
        if s.autohide == autohide {
            return;
        }
        s.autohide = autohide;
        if !autohide && s.visible && !s.suspended && !s.revealed {
            s.reveal("autohide turned off");
        } else {
            // Move to the primary screen, or start the hide countdown.
            s.away = 0;
            s.place();
        }
    }

    /// Whether the dock stays on screen instead of auto-hiding.
    pub fn stays_visible(&self) -> bool {
        !self.state.borrow().autohide
    }

    /// Moves the dock to another screen edge. Placement,
    /// popups, and the work-area reservation follow on the next
    /// sync; bridge mode calls this with Bottom only (edges are
    /// a standalone-shell capability in this phase).
    pub fn set_edge(&mut self, edge: logic::Edge) {
        let mut s = self.state.borrow_mut();
        if s.edge == edge {
            return;
        }
        s.edge = edge;
        s.surface_rect = None;
        s.close_flyout();
        s.hide_tip();
        s.hide_windows();
        s.place();
    }

    /// The room a dock that stays visible keeps on its screen
    /// edge, in DIPs: the strip's cross size and the margins on
    /// both sides of it. (Named for its original bottom-edge
    /// meaning; the value is the thickness on any edge.)
    pub fn reserved_height(&self) -> f32 {
        let s = self.state.borrow();
        let scale = logic::layout_scale(s.app_icon_size);
        logic::height(scale) + 2.0 * logic::margin(scale)
    }

    /// Hides the dock until `show`, such as while the launcher is open.
    pub fn hide(&mut self) {
        let mut s = self.state.borrow_mut();
        s.close_flyout();
        s.hide_tip();
        s.hide_windows();
        s.visible = false;
        s.stop_slide();
        s.revealed = false;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        s.narrate("hide: hidden until shown again (the launcher is open, or a sync asked)");
    }
}

impl Drop for Dock {
    fn drop(&mut self) {
        EVENT_TARGET.with(|target| target.set(HWND::default()));
        unsafe {
            for hook in self.hooks.drain(..) {
                let _ = UnhookWinEvent(hook);
            }
            let _ = KillTimer(Some(self.hwnd), REFRESH_TIMER);
            let _ = KillTimer(Some(self.hwnd), POLL_TIMER);
            let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let _ = KillTimer(Some(self.hwnd), HOVER_TIMER);
            let _ = KillTimer(Some(self.hwnd), DESKTOP_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
            // Remove the pointer before synchronous destruction can reenter the wndproc.
            let s = self.state.borrow();
            for hwnd in [s.flyout, s.tip, s.windows, self.hwnd] {
                if !hwnd.is_invalid() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
            }
            drop(s);
            // Destroys the owned flyout, tooltip and window list too.
            if self.shared {
                let _ = DestroyWindow(self.state.borrow().flyout);
                let _ = DestroyWindow(self.state.borrow().tip);
                let _ = DestroyWindow(self.state.borrow().windows);
            } else {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl State {
    fn paint(&mut self, rt: Option<&windows::Win32::Graphics::Direct2D::ID2D1HwndRenderTarget>) -> Result<()> {
        let hwnd = self.hwnd;
        let parts = self.parts();
        let State {
            renderer,
            items,
            icons,
            palette,
            dpi,
            hover,
            hover_levels,
            app_icon_size,
            capacity,
            scroll,
            status,
            hover_part,
            tray,
            inline,
            flyout_open,
            edge,
            ..
        } = self;
        let frame = &render::Frame {
            dpi: *dpi,
            edge: *edge,
            palette,
            items,
            icons,
            hover: *hover,
            hover_levels,
            app_icon_size: *app_icon_size,
            layout_scale: logic::layout_scale(*app_icon_size),
            capacity: *capacity,
            scroll: *scroll,
            status,
            hover_part: *hover_part,
            parts: &parts,
            tray,
            inline,
            flyout_open: *flyout_open,
        };
        match rt {
            Some(rt) => renderer.draw_on(rt, frame),
            None => renderer.draw(hwnd, frame),
        }
    }

    fn apply_material(&mut self, dark: bool, backdrop: Backdrop) {
        self.backdrop = backdrop;
        self.palette = crate::theme::apply_backdrop(self.hwnd, dark, backdrop, true);
        for hwnd in [self.flyout, self.tip, self.windows] {
            crate::theme::apply_backdrop(hwnd, dark, backdrop, true);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    fn refresh(&mut self) {
        if self.suspended {
            return;
        }
        // Other apps' windows change constantly. A hidden dock skips the window
        // scan and catches up in `reveal`.
        if self.visible && !self.revealed && self.slide.is_none() && !self.held {
            unsafe {
                if IsWindowVisible(self.hwnd).as_bool() {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
            return;
        }
        let windows = open_windows::list();
        let mut items: Vec<Item> = Vec::new();
        for window in windows {
            let aumid = window_aumid(HWND(window.hwnd as _));
            let app = logic::match_app(&self.apps, &window.exe, aumid.as_deref())
                .cloned()
                .unwrap_or_else(|| {
                    let id = aumid.filter(|s| !s.is_empty()).unwrap_or_else(|| window.exe.clone());
                    AppEntry {
                        name: open_windows::program_name(&window.exe).into(),
                        id,
                        path: (!window.exe.is_empty()).then(|| window.exe.clone()),
                    }
                });
            if app.id.is_empty() {
                continue;
            }
            if let Some(item) = items.iter_mut().find(|item| item.app.id.eq_ignore_ascii_case(&app.id)) {
                item.windows.push(window);
            } else {
                items.push(Item {
                    app,
                    windows: vec![window],
                });
            }
        }
        // A slot may now belong to another app, or contain different windows.
        let previous = self
            .windows_slot
            .and_then(|slot| self.item_at(slot))
            .map(|item| (item.app.id.clone(), item.app.name.clone(), item.windows.clone()));
        self.items = logic::stable_order(std::mem::take(&mut self.items), items, |old, new| {
            old.app.id.eq_ignore_ascii_case(&new.app.id)
        });
        if let Some(slot) = self.windows_slot
            && self.item_at(slot).is_none_or(|item| {
                !logic::show_window_list(self.window_list_mode, item.windows.len())
                    || previous.as_ref().is_none_or(|(id, name, windows)| {
                        !id.eq_ignore_ascii_case(&item.app.id)
                            || *name != item.app.name
                            || windows.len() != item.windows.len()
                            || windows
                                .iter()
                                .any(|old| !item.windows.iter().any(|new| old.hwnd == new.hwnd))
                    })
            })
        {
            self.hide_windows();
        }
        self.status = status::read();
        self.place();
        if self.suspended {
            return;
        }
        self.fullscreen = fullscreen_foreground(self.hwnd);
        if !self.fullscreen {
            self.refresh_window_titles();
        }
        if self.fullscreen {
            if self.revealed || self.slide.is_some() {
                self.narrate("hide: refresh latched a fullscreen foreground");
            }
            self.hide_windows();
            self.stop_slide();
            self.revealed = false;
        }
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if self.visible && (self.revealed || self.slide.is_some()) && !self.fullscreen {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_HIDE
                },
            );
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn count(&self) -> usize {
        self.items.len().saturating_sub(self.scroll).min(self.capacity)
    }

    /// Slides the dock in when the pointer reaches a bottom edge, and out once
    /// it has been away for a moment.
    fn poll(&mut self) {
        let _span = crate::util::perf::Span::new("dock_poll");
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return;
        }
        let hidden = !self.revealed && self.slide.is_none();
        if !hidden && fullscreen_foreground(self.hwnd) {
            self.stop_slide();
            self.revealed = false;
            self.close_flyout();
            self.hide_tip();
            self.hide_windows();
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.narrate("hide: a fullscreen app holds the foreground");
            return;
        }
        if !self.revealed {
            let fullscreen = fullscreen_foreground(self.hwnd);
            if !self.autohide {
                // Back after a fullscreen app, or after the launcher closed.
                if !fullscreen {
                    self.reveal("poll: the dock does not autohide");
                }
                return;
            }
            // Dragging along the anchor edge, such as a scroll bar, is not a request for the dock.
            let dragging =
                unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let on_edge = unsafe {
                GetMonitorInfoW(MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST), &mut info).as_bool()
                    && at_anchor_edge(cursor, self.edge, &info)
            };
            if !dragging && on_edge {
                if fullscreen {
                    // The wedge this guards: the pointer is at
                    // the edge asking for the dock, and only the
                    // fullscreen test says no. Say which window
                    // that is — once per window, not per poll.
                    let class = foreground_class();
                    if self.block_class != class {
                        self.block_class = class.clone();
                        self.narrate(&format!(
                            "reveal blocked: fullscreen foreground is {class:?}; the dock stays hidden until it yields"
                        ));
                    }
                } else {
                    self.reveal("poll: pointer at the anchor edge");
                }
            }
            return;
        }
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(self.hwnd, &mut rect) };
        if self.flyout_open && !self.held {
            // Activation closes the flyout on a click elsewhere; this covers
            // clicks while it could not take the foreground.
            let pressed =
                unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 };
            let mut flyout = RECT::default();
            let _ = unsafe { GetWindowRect(self.flyout, &mut flyout) };
            let inside =
                |r: &RECT| cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
            if pressed && !inside(&flyout) && !inside(&rect) {
                self.close_flyout();
            }
        }
        let slack = (12 * self.dpi / 96) as i32;
        // The gap beyond the dock (between it and its screen edge)
        // counts as on it, so the pointer can rest on the edge.
        // The dock stays while its flyout is open.
        let mut windows_rect = RECT::default();
        let windows_open = unsafe { IsWindowVisible(self.windows) }.as_bool()
            && unsafe { GetWindowRect(self.windows, &mut windows_rect) }.is_ok();
        let over_windows = windows_open
            && cursor.x >= windows_rect.left
            && cursor.x < windows_rect.right
            && cursor.y >= windows_rect.top
            && cursor.y < windows_rect.bottom;
        // Slack extends on every side except the anchor side,
        // where the screen edge itself is the bound.
        let (sl, st, sr, sb) = match self.edge {
            logic::Edge::Bottom => (slack, slack, slack, 0),
            logic::Edge::Top => (slack, 0, slack, slack),
            logic::Edge::Left => (0, slack, slack, slack),
            logic::Edge::Right => (slack, slack, 0, slack),
        };
        let near = !self.autohide
            || self.held
            || self.flyout_open
            || over_windows
            || cursor.x >= rect.left - sl
                && cursor.x < rect.right + sr
                && cursor.y >= rect.top - st
                && cursor.y < rect.bottom + sb;
        if near {
            self.away = 0;
        } else {
            self.away += 1;
            if self.away >= HIDE_AFTER {
                self.hide_tip();
                self.hide_windows();
                self.revealed = false;
                self.start_slide(1.0);
                self.narrate("hide: the pointer has been away");
                return;
            }
        }
        // Polling also catches volume changes from keys and other apps.
        self.update_status();
    }

    /// One line of stderr narration for a hide/show transition,
    /// with the state that explains it. Hide/show wedges are
    /// invisible without this: the dock writes no log of its
    /// own, and a stuck state looks identical to a dead process
    /// from outside.
    fn narrate(&self, transition: &str) {
        eprintln!(
            "[dock] {transition} (revealed={} slide_to={} fullscreen={} autohide={} suspended={})",
            self.revealed, self.slide_to, self.fullscreen, self.autohide, self.suspended
        );
    }

    /// Shows the dock, or reverses an outgoing slide at its current position.
    fn reveal(&mut self, why: &str) {
        let hidden = !self.revealed && self.slide.is_none();
        let reversing = self.slide.is_some() && self.slide_to > 0.5;
        self.revealed = true;
        self.away = 0;
        if hidden {
            self.slide_to = 1.0;
        }
        if hidden || reversing {
            self.narrate(&format!("show: {why}"));
        }
        self.block_class.clear();
        self.start_slide(0.0);
        self.refresh();
    }

    fn slide_value(&self) -> f32 {
        let Some(start) = self.slide else { return self.slide_to };
        logic::slide_value(
            self.slide_from,
            self.slide_to,
            start.elapsed().as_secs_f32() * 1000.0 / crate::util::perf::slowdown() / SLIDE_MS,
        )
    }

    fn start_slide(&mut self, to: f32) {
        if to > 0.5 {
            self.hide_windows();
        }
        crate::util::perf::event("slide_begin");
        self.slide_from = self.slide_value();
        self.slide_to = to;
        self.slide = Some(Instant::now());
        if !self.frame_paced {
            unsafe {
                SetTimer(Some(self.hwnd), SLIDE_TIMER, 10, None);
            }
        }
    }

    fn stop_slide(&mut self) {
        self.slide = None;
        self.slide_from = 0.0;
        self.slide_to = 0.0;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
        }
    }

    fn slide_offset(&self, height: i32) -> i32 {
        let layout_scale = logic::layout_scale(self.app_icon_size);
        ((height + (logic::margin(layout_scale) * self.dpi as f32 / 96.0).round() as i32) as f32 * self.slide_value())
            .round() as i32
    }

    fn step_slide(&mut self) {
        let _span = crate::util::perf::Span::new("slide");
        crate::util::perf::event("slide_frame");
        let tick = Instant::now();
        let elapsed = self.slide.map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0);
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
            // The slide runs along the anchor edge's axis: the
            // cross size is the window's extent across the strip.
            let cross = if self.edge.is_vertical() {
                rect.right - rect.left
            } else {
                rect.bottom - rect.top
            };
            let offset = self.slide_offset(cross);
            let dir = self.edge.hide_dir();
            let _ = SetWindowPos(
                self.hwnd,
                None,
                self.left + dir.0 * offset,
                self.top + dir.1 * offset,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            self.publish_rects();
            if self
                .slide
                .is_some_and(|start| start.elapsed().as_secs_f32() * 1000.0 / crate::util::perf::slowdown() >= SLIDE_MS)
            {
                self.slide = None;
                let _ = KillTimer(Some(self.hwnd), SLIDE_TIMER);
                if !self.revealed {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
            }
        }
        if crate::util::perf::hook_on("WISP_FRAME_TIMING") {
            eprintln!(
                "slide frame timestamp_ms={elapsed:.3} work_ms={:.3}",
                tick.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    fn animate_hover(&mut self) {
        if self.hover_tick.is_none() {
            self.hover_tick = Some(Instant::now());
        }
        unsafe {
            SetTimer(Some(self.hwnd), HOVER_TIMER, 10, None);
        }
    }

    fn step_hover(&mut self) {
        let now = Instant::now();
        let step = self.hover_tick.replace(now).map_or(1.0, |last| {
            now.duration_since(last).as_secs_f32() * 1000.0 / crate::util::perf::slowdown() / HOVER_MS
        });
        self.hover_levels.resize(self.items.len() + 1, 0.0);
        let mut moving = false;
        for (index, level) in self.hover_levels.iter_mut().enumerate() {
            let goal = if self.hover == Some(index) { 1.0 } else { 0.0 };
            *level += (goal - *level).clamp(-step, step);
            moving |= *level != goal;
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
            if !moving {
                self.hover_tick = None;
                let _ = KillTimer(Some(self.hwnd), HOVER_TIMER);
            }
        }
    }

    fn place(&mut self) {
        let previous = (self.dpi, self.left, self.top, self.width, self.scroll, self.capacity);
        if self.suspended {
            return;
        }
        unsafe {
            // The dock appears on the screen whose bottom edge the pointer
            // touched, or stays on the primary screen.
            let mut cursor = POINT::default();
            if self.autohide {
                let _ = GetCursorPos(&mut cursor);
            }
            // In a standalone session the shell may name the
            // monitor the bar lives on; that outranks the pointer.
            let named = if crate::backend::is_standalone() {
                crate::coquille::target_monitor().and_then(|name| monitor_by_name(&name))
            } else {
                None
            };
            let monitor = named.unwrap_or_else(|| MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY));
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return;
            }
            // Move first, then query the window's new monitor DPI. Skipped on the
            // same monitor, where the jump would show.
            if MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONULL) != monitor {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    info.rcMonitor.left,
                    info.rcMonitor.top,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
                );
            }
            self.dpi = self.held_dpi.unwrap_or_else(|| GetDpiForWindow(self.hwnd)).max(96);
            let scale = self.dpi as f32 / 96.0;
            // Notification icons are rendered for the DPI they are shown at.
            let size = tray::icon_size(self.dpi);
            if self
                .tray
                .iter()
                .any(|shown| shown.pixels.as_ref().is_some_and(|p| p.width != size as u32))
            {
                let icons = std::mem::take(&mut self.tray)
                    .into_iter()
                    .map(|shown| shown.icon)
                    .collect();
                (self.tray, self.inline) = tray::update(Vec::new(), icons, size);
            }
            let size = logic::icon_pixels(self.dpi, self.app_icon_size);
            for item in &self.items {
                if self.icons.get(&item.app.id).is_none_or(|p| p.width != size as u32) {
                    let path = item
                        .app
                        .path
                        .clone()
                        .unwrap_or_else(|| format!("shell:AppsFolder\\{}", item.app.id));
                    if let Some(pixels) = tray::app_pixels(&path, size) {
                        self.icons.insert(item.app.id.clone(), Arc::new(pixels));
                    }
                }
            }
            let parts = self.parts();
            let layout_scale = logic::layout_scale(self.app_icon_size);
            // Everything up to the window size is computed in
            // dock space: the main axis runs along the strip, the
            // cross axis is its thickness, whichever screen axes
            // those are for the current edge.
            let mon_main = if self.edge.is_vertical() {
                info.rcMonitor.bottom - info.rcMonitor.top
            } else {
                info.rcMonitor.right - info.rcMonitor.left
            };
            let available =
                mon_main as f32 / scale - 8.0 * layout_scale - logic::status_width_scaled(&parts, layout_scale);
            self.capacity = (((available - 2.0 * logic::pad(layout_scale) - logic::separator_width(layout_scale))
                / logic::slot(layout_scale)) as usize)
                .saturating_sub(1);
            self.scroll = self.scroll.min(self.items.len().saturating_sub(self.capacity));
            let running = self.count();
            let main_px = (logic::total_width_scaled(running, &parts, layout_scale) * scale).ceil() as i32;
            let cross_px = (logic::height(layout_scale) * scale).round() as i32;
            let margin_px = (logic::margin(layout_scale) * scale).round() as i32;
            let (width, height) = if self.edge.is_vertical() {
                (cross_px, main_px)
            } else {
                (main_px, cross_px)
            };
            let (x, y_rest) = match self.edge {
                logic::Edge::Bottom => (
                    info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width) / 2,
                    info.rcMonitor.bottom - cross_px - margin_px,
                ),
                logic::Edge::Top => (
                    info.rcMonitor.left + (info.rcMonitor.right - info.rcMonitor.left - width) / 2,
                    info.rcMonitor.top + margin_px,
                ),
                logic::Edge::Left => (
                    info.rcMonitor.left + margin_px,
                    info.rcMonitor.top + (info.rcMonitor.bottom - info.rcMonitor.top - height) / 2,
                ),
                logic::Edge::Right => (
                    info.rcMonitor.right - cross_px - margin_px,
                    info.rcMonitor.top + (info.rcMonitor.bottom - info.rcMonitor.top - height) / 2,
                ),
            };
            self.left = x;
            self.top = y_rest;
            self.width = width;
            self.win_height = height;
            let dir = self.edge.hide_dir();
            let offset = self.slide_offset(cross_px);
            crate::theme::shape(self.hwnd, width, height, 8.0 * layout_scale, self.dpi);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x + dir.0 * offset,
                y_rest + dir.1 * offset,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        if previous != (self.dpi, self.left, self.top, self.width, self.scroll, self.capacity) {
            self.hide_windows();
        }
        // The chevron and icons move whenever the dock's width changes.
        if self.flyout_open {
            self.layout_flyout();
        }
        if unsafe { IsWindowVisible(self.tip) }.as_bool() {
            self.show_tip();
        }
        self.publish_rects();
    }

    /// The status parts, with the notification area while the dock hosts it.
    fn parts(&self) -> Vec<logic::Part> {
        self.status.parts(self.tray_host.as_ref().map(|_| self.inline.len()))
    }

    /// Maps a dock-space strip segment (`u_from..u_to`, the full
    /// cross extent) to a screen RECT where the dock rests, in
    /// physical pixels — the one place part/item rects learn
    /// which edge the dock sits on. On the unified surface the
    /// main axis scales across the surface and the dock hugs the
    /// surface edge it anchors to (the surface is a horizontal
    /// mode; vertical edges never share it).
    fn strip_rect(&self, u_from: f32, u_to: f32) -> RECT {
        let scale = self.dpi as f32 / 96.0;
        let px = |dip: f32| (dip * scale).round() as i32;
        let layout_scale = logic::layout_scale(self.app_icon_size);
        let cross = logic::height(layout_scale);
        if let Some(rect) = self.surface_rect
            && !self.edge.is_vertical()
        {
            let x = |dip: f32| {
                rect.left + (px(dip) as f32 * (rect.right - rect.left) as f32 / self.width.max(1) as f32).round() as i32
            };
            let top = match self.edge {
                logic::Edge::Top => rect.top,
                _ => rect.bottom - px(cross),
            };
            return RECT {
                left: x(u_from),
                top,
                right: x(u_to),
                bottom: top + px(cross),
            };
        }
        let (ax, ay) = self.edge.to_window(u_from, 0.0);
        let (bx, by) = self.edge.to_window(u_to, cross);
        let dir = self.edge.hide_dir();
        let offset = self.slide_offset(px(cross));
        let ox = self.left + dir.0 * offset;
        let oy = self.top + dir.1 * offset;
        RECT {
            left: ox + px(ax),
            top: oy + px(ay),
            right: ox + px(bx),
            bottom: oy + px(by),
        }
    }

    /// A part's rectangle on screen where the dock rests, in physical pixels.
    fn part_rect(&self, part: logic::Part) -> Option<RECT> {
        let running = self.count();
        let layout_scale = logic::layout_scale(self.app_icon_size);
        let left = logic::part_left_scaled(part, running, &self.parts(), layout_scale)?;
        Some(self.strip_rect(left, left + part.width_scaled(layout_scale)))
    }

    /// An app slot's rectangle on screen where the dock rests, in physical pixels.
    fn item_rect(&self, slot: usize) -> RECT {
        let layout_scale = logic::layout_scale(self.app_icon_size);
        let left = logic::left_scaled(slot, layout_scale);
        self.strip_rect(left, left + logic::slot(layout_scale))
    }

    /// A flyout cell's rectangle on screen, in physical pixels.
    fn cell_rect(&self, index: usize) -> RECT {
        let scale = self.dpi as f32 / 96.0;
        let mut flyout = RECT::default();
        let _ = unsafe { GetWindowRect(self.flyout, &mut flyout) };
        let (left, top) = logic::flyout_cell(index);
        let px = |dip: f32| (dip * scale).round() as i32;
        RECT {
            left: flyout.left + px(left),
            top: flyout.top + px(top),
            right: flyout.left + px(left + logic::FLYOUT_CELL),
            bottom: flyout.top + px(top + logic::FLYOUT_CELL),
        }
    }

    /// Tells the tray host where icons are, for apps that place their
    /// windows by Shell_NotifyIconGetRect. Hidden icons point at the chevron.
    fn publish_rects(&self) {
        let Some(host) = &self.tray_host else { return };
        let mut rects = tray_host::Rects {
            fallback: self.part_rect(logic::Part::Chevron),
            ..Default::default()
        };
        for (position, &index) in self.inline.iter().enumerate() {
            if let Some(rect) = self.part_rect(logic::Part::Tray(position)) {
                rects.icons.insert(self.tray[index].icon.key(), rect);
            }
        }
        if self.flyout_open {
            for (index, shown) in self.tray.iter().enumerate() {
                rects.icons.insert(shown.icon.key(), self.cell_rect(index));
            }
        }
        host.set_rects(rects);
    }

    /// Rereads the tray host's icons.
    fn sync_tray(&mut self) {
        let icons = self.tray_host.as_ref().map(TrayHost::icons).unwrap_or_default();
        let size = tray::icon_size(self.dpi);
        (self.tray, self.inline) = tray::update(std::mem::take(&mut self.tray), icons, size);
        if self.flyout_hover.is_some_and(|i| i >= self.tray.len()) {
            self.flyout_hover = None;
        }
        let stale = match self.tip_target {
            Some(tray::Tip::Inline(i)) => i >= self.inline.len(),
            Some(tray::Tip::Flyout(i)) => i >= self.tray.len(),
            Some(tray::Tip::Chevron) => self.tray_host.is_none(),
            // The desktop pill has nothing to do with the tray.
            None => false,
        };
        if stale {
            self.hide_tip();
        }
        if self.tray_host.is_none() {
            self.close_flyout();
        }
        self.place();
        if self.held {
            self.apply_hold();
        }
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Shows the demo's held flyout or tooltip once the icons are known.
    fn apply_hold(&mut self) {
        if self.tray_host.is_none() {
            return;
        }
        if let Some(hover) = self.held_flyout {
            if !self.flyout_open {
                self.open_flyout();
            }
            self.flyout_hover = hover.filter(|&i| i < self.tray.len());
            self.tip_target = self.flyout_hover.map(tray::Tip::Flyout);
        } else {
            self.tip_target = match self.hover_part {
                Some(logic::Part::Chevron) => Some(tray::Tip::Chevron),
                Some(logic::Part::Tray(i)) if i < self.inline.len() => Some(tray::Tip::Inline(i)),
                _ => None,
            };
        }
        self.show_tip();
        unsafe {
            let _ = InvalidateRect(Some(self.flyout), None, false);
        }
    }

    fn toggle_flyout(&mut self) {
        if self.flyout_open {
            self.close_flyout();
        } else {
            self.open_flyout();
        }
    }

    fn open_flyout(&mut self) {
        self.hide_tip();
        self.hide_windows();
        self.flyout_open = true;
        self.flyout_hover = None;
        self.layout_flyout();
        unsafe {
            let _ = ShowWindow(self.flyout, SW_SHOWNA);
            // The click on the chevron lets Wisp take the foreground, which
            // the flyout needs for Esc and to close on a click elsewhere.
            if !self.held {
                let _ = SetForegroundWindow(self.flyout);
            }
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        self.publish_rects();
    }

    /// Sizes the flyout to its icons and centers it above the chevron.
    fn layout_flyout(&mut self) {
        let scale = self.dpi as f32 / 96.0;
        let (width, height) = logic::flyout_size(self.tray.len());
        let (width, height) = ((width * scale).round() as i32, (height * scale).round() as i32);
        let Some(chevron) = self.part_rect(logic::Part::Chevron) else {
            return;
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
        }
        let margin = (logic::FLYOUT_GAP * scale).round() as i32;
        let monitor = info.rcMonitor;
        let bounds = (
            monitor.left + margin,
            monitor.top + margin,
            monitor.right - margin,
            monitor.bottom - margin,
        );
        let (x, y) = logic::popup_pos(
            self.edge,
            (chevron.left, chevron.top, chevron.right, chevron.bottom),
            (width, height),
            bounds,
            margin,
        );
        unsafe {
            let _ = SetWindowPos(self.flyout, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE);
            let _ = InvalidateRect(Some(self.flyout), None, false);
        }
    }

    fn close_flyout(&mut self) {
        if !self.flyout_open {
            return;
        }
        self.flyout_open = false;
        self.flyout_hover = None;
        if matches!(self.tip_target, Some(tray::Tip::Flyout(_))) {
            self.hide_tip();
        }
        unsafe {
            let _ = ShowWindow(self.flyout, SW_HIDE);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
        self.publish_rects();
    }

    /// Sends a gesture on `tray[index]` to its app, as Explorer would.
    /// `center` is the icon's center on screen.
    fn click_tray(&mut self, index: usize, action: Action, center: POINT) {
        let Some(shown) = self.tray.get(index) else { return };
        let key = shown.icon.key();
        let double = Duration::from_millis(unsafe { GetDoubleClickTime() }.into());
        let action = match action {
            Action::Click
                if self
                    .tray_click
                    .is_some_and(|(last, at)| last == key && at.elapsed() < double) =>
            {
                self.tray_click = None;
                Action::DoubleClick
            }
            Action::Click => {
                self.tray_click = Some((key, Instant::now()));
                Action::Click
            }
            other => other,
        };
        self.hide_tip();
        // Bridge mode posts the callback straight to the owner
        // window; standalone, the broker owns the owners, so the
        // gesture relays over the pipe for it to deliver.
        let result = if crate::backend::is_standalone() {
            crate::coquille::invoke_tray(&self.tray[index].icon, action, center.x, center.y)
        } else {
            tray_host::invoke(&self.tray[index].icon, action, center.x, center.y)
        };
        if let Err(error) = result {
            eprintln!("Dock notification icon: {error}");
        }
    }

    /// An inline icon's gesture; its anchor is the icon's center.
    fn click_inline(&mut self, position: usize, action: Action) {
        let (Some(&index), Some(rect)) = (self.inline.get(position), self.part_rect(logic::Part::Tray(position)))
        else {
            return;
        };
        let center = POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        };
        self.click_tray(index, action, center);
    }

    /// The pointer rests on `target`; its tooltip follows after a moment.
    fn hover_tip(&mut self, target: Option<tray::Tip>) {
        // The open flyout already shows what the chevron's tooltip offers.
        let target = target.filter(|&t| !(t == tray::Tip::Chevron && self.flyout_open));
        if self.tip_target == target || self.held {
            return;
        }
        self.hide_tip();
        self.tip_target = target;
        if target.is_some() {
            self.hide_windows();
            unsafe {
                SetTimer(Some(self.hwnd), TIP_TIMER, TIP_MS, None);
            }
        }
    }

    /// Lists the windows of the app under the pointer, or of none.
    fn hover_windows(&mut self, slot: Option<usize>) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
        }
        let slot = slot.filter(|&slot| {
            self.window_list_mode != WindowListMode::Off
                && self
                    .item_at(slot)
                    .is_some_and(|item| logic::show_window_list(self.window_list_mode, item.windows.len()))
        });
        if self.windows_slot == slot {
            return;
        }
        let shown = self.windows_slot.is_some();
        self.hide_windows();
        self.windows_slot = slot;
        if slot.is_some() {
            self.hide_tip();
            unsafe {
                let _ = SetTimer(Some(self.hwnd), WINDOWS_TIMER, if shown { 0 } else { TIP_MS }, None);
            }
        }
    }

    /// The item in a visible slot.
    fn item_at(&self, slot: usize) -> Option<&Item> {
        slot.checked_sub(1).and_then(|i| self.items.get(i + self.scroll))
    }

    /// Hides the window list and forgets its windows.
    fn hide_windows(&mut self) {
        self.windows_slot = None;
        self.windows_hover = None;
        self.windows_rows.clear();
        self.windows_targets.clear();
        self.windows_thumbs.clear();
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
            let _ = KillTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER);
            let _ = ShowWindow(self.windows, SW_HIDE);
        }
    }

    /// Keeps the list a moment while the pointer crosses to it, then hides it.
    fn leave_windows(&mut self) {
        if self.windows_slot.is_none() {
            return;
        }
        let shown = unsafe { IsWindowVisible(self.windows) }.as_bool();
        if !shown {
            // It never appeared, so a pending hover is now stale.
            self.hide_windows();
            return;
        }
        unsafe {
            let _ = SetTimer(Some(self.hwnd), WINDOWS_HIDE_TIMER, WINDOWS_HIDE_MS, None);
        }
    }

    /// Draws the hovered app's windows above its icon.
    fn show_windows(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), WINDOWS_TIMER);
        }
        if !self.visible || self.suspended || self.fullscreen || !self.revealed || self.slide_to > 0.5 {
            self.hide_windows();
            return;
        }
        let Some(slot) = self.windows_slot else { return };
        let Some(item) = self.item_at(slot) else {
            self.hide_windows();
            return;
        };
        if !logic::show_window_list(self.window_list_mode, item.windows.len()) {
            self.hide_windows();
            return;
        }
        let rows: Vec<String> = item
            .windows
            .iter()
            .map(|window| match window.title.trim() {
                "" => item.app.name.clone(),
                title => title.to_string(),
            })
            .collect();
        let targets: Vec<isize> = item.windows.iter().map(|window| window.hwnd).collect();
        if rows.is_empty() {
            self.hide_windows();
            return;
        }
        self.windows_hover = None;
        self.layout_windows(slot, rows, targets);
    }

    /// Updates titles by handle, preserving the displayed order and click targets
    /// even when a foreground change has reordered the app's windows.
    fn refresh_window_titles(&mut self) {
        let Some(slot) = self.windows_slot.filter(|_| !self.windows_rows.is_empty()) else {
            return;
        };
        let Some(item) = self.item_at(slot) else { return };
        let mut rows = self.windows_rows.clone();
        for (row, target) in rows.iter_mut().zip(&self.windows_targets) {
            let Some(window) = item.windows.iter().find(|window| window.hwnd == *target) else {
                self.hide_windows();
                return;
            };
            *row = match window.title.trim() {
                "" => item.app.name.clone(),
                title => title.to_string(),
            };
        }
        if rows != self.windows_rows {
            self.layout_windows(slot, rows, self.windows_targets.clone());
        }
    }

    /// Measures and places the list after its row text changes.
    fn layout_windows(&mut self, slot: usize, mut rows: Vec<String>, mut targets: Vec<isize>) {
        let scale = self.dpi as f32 / 96.0;
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info) }.as_bool() {
            self.hide_windows();
            return;
        }
        let anchor = self.item_rect(slot);
        let gap = (6.0 * scale).round() as i32;
        // The rows stack along the screen's vertical; the room
        // for them is the space inward from the anchor — above a
        // bottom dock, below a top dock, the full work height
        // beside a side dock.
        let available = match self.edge {
            logic::Edge::Bottom => anchor.top - gap - (info.rcWork.top + gap),
            logic::Edge::Top => info.rcWork.bottom - gap - (anchor.bottom + gap),
            logic::Edge::Left | logic::Edge::Right => info.rcWork.bottom - gap - (info.rcWork.top + gap),
        };
        let capacity = logic::window_list_rows(available, scale, logic::LIST_PAD.1, logic::LIST_CARD_H);
        if capacity == 0 {
            self.hide_windows();
            return;
        }
        if rows.len() > capacity {
            // Keep the first (most recently used) windows. The final row only
            // reports the remainder and has no target or hover highlight.
            let visible = capacity - 1;
            let total = self.item_at(slot).map_or(targets.len(), |item| item.windows.len());
            rows.truncate(visible);
            targets.truncate(visible);
            rows.push(format!("+{} more", total - visible));
        }
        let Ok((width, height)) = self.windows_renderer.measure_windows(&rows) else {
            self.hide_windows();
            return;
        };
        let (width, height) = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
        let work = info.rcWork;
        let (x, y) = logic::popup_pos(
            self.edge,
            (anchor.left, anchor.top, anchor.right, anchor.bottom),
            (width, height),
            (work.left, work.top, work.right, work.bottom),
            gap,
        );
        self.hide_tip();
        self.windows_hover = self.windows_hover.filter(|&row| row < targets.len());
        self.windows_rows = rows;
        self.windows_targets = targets;
        unsafe {
            let _ = SetWindowPos(
                self.windows,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.windows), None, false);
        }
        // DWM draws the previews itself, into the rects the renderer lays
        // out; sync them only once the popup sits at its final size.
        let wanted: Vec<(isize, RECT)> = self
            .windows_targets
            .iter()
            .enumerate()
            .map(|(index, &target)| {
                let (left, top, right, bottom) = logic::list_thumb_rect(index);
                (
                    target,
                    RECT {
                        left: (left * scale).round() as i32,
                        top: (top * scale).round() as i32,
                        right: (right * scale).round() as i32,
                        bottom: (bottom * scale).round() as i32,
                    },
                )
            })
            .collect();
        self.windows_thumbs.sync(self.windows, &wanted);
    }

    /// The row of the window list under a popup pointer position.
    fn windows_hit(&self, lp: LPARAM) -> Option<usize> {
        let scale = self.dpi as f32 / 96.0;
        let y = ((lp.0 >> 16) as i16) as f32 / scale;
        logic::list_row_at(y, self.windows_targets.len())
    }

    fn hide_tip(&mut self) {
        self.tip_target = None;
        self.desktop_pill = None;
        unsafe {
            let _ = KillTimer(Some(self.hwnd), DESKTOP_TIMER);
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let _ = ShowWindow(self.tip, SW_HIDE);
        }
    }

    /// Shows the tooltip above what the pointer rests on.
    fn show_tip(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
        }
        let (text, anchor) = match self.tip_target {
            Some(tray::Tip::Chevron) => ("Show hidden icons".to_string(), self.part_rect(logic::Part::Chevron)),
            Some(tray::Tip::Inline(i)) => (
                self.inline
                    .get(i)
                    .map(|&i| self.tray[i].icon.tip.clone())
                    .unwrap_or_default(),
                self.part_rect(logic::Part::Tray(i)),
            ),
            Some(tray::Tip::Flyout(i)) => (
                self.tray.get(i).map(|shown| shown.icon.tip.clone()).unwrap_or_default(),
                Some(self.cell_rect(i)),
            ),
            None => return,
        };
        let text = text.trim().to_string();
        let Some(anchor) = anchor.filter(|_| !text.is_empty()) else {
            return;
        };
        let scale = self.dpi as f32 / 96.0;
        let Ok((width, height)) = self.tip_renderer.measure_tip(&text, TIP_MAX) else {
            return;
        };
        let (width, height) = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
        }
        let gap = (6.0 * scale).round() as i32;
        let mon = info.rcMonitor;
        let (x, y) = logic::popup_pos(
            self.edge,
            (anchor.left, anchor.top, anchor.right, anchor.bottom),
            (width, height),
            (mon.left + gap, mon.top + gap, mon.right - gap, mon.bottom - gap),
            gap,
        );
        self.tip_text = text;
        unsafe {
            let _ = SetWindowPos(
                self.tip,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.tip), None, false);
        }
    }

    /// Offers the taskbar for icons the dock lacks, and Windows' settings
    /// for which icons show in the dock.
    fn chevron_menu(&mut self) {
        self.close_flyout();
        self.hide_tip();
        self.hide_windows();
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let _ = AppendMenuW(menu, MF_STRING, 1, w!("Show taskbar"));
            let _ = AppendMenuW(menu, MF_STRING, 2, w!("Taskbar settings"));
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            )
            .0;
            let _ = DestroyMenu(menu);
            match command {
                1 => {
                    let _ = PostMessageW(Some(self.owner), WM_DOCK_SHOW_TASKBAR, WPARAM(0), LPARAM(0));
                }
                2 => {
                    if let Err(error) = crate::launch::open("ms-settings:taskbar") {
                        eprintln!("Dock taskbar settings: {error}");
                    }
                }
                _ => {}
            }
        }
    }

    /// The pointer swap point: window DIPs become dock space
    /// here, between the DPI division and the layout-scale
    /// division inside the logic functions, so every caller
    /// (click, hover, wheel) works in dock space unchanged.
    fn dock_space(&self, lp: LPARAM) -> (f32, f32) {
        let scale = self.dpi as f32 / 96.0;
        self.edge
            .from_window((lp.0 as i16) as f32 / scale, ((lp.0 >> 16) as i16) as f32 / scale)
    }

    fn hit(&self, lp: LPARAM) -> Option<usize> {
        let running = self.count();
        let (u, v) = self.dock_space(lp);
        logic::hit_scaled(u, v, running, logic::layout_scale(self.app_icon_size))
    }

    fn hit_part(&self, lp: LPARAM) -> Option<logic::Part> {
        let running = self.count();
        let (u, v) = self.dock_space(lp);
        logic::part_hit_scaled(u, v, running, &self.parts(), logic::layout_scale(self.app_icon_size))
    }

    /// The speaker toggles mute; the clock opens the calendar and notifications;
    /// the battery opens its settings. The chevron toggles the flyout, and
    /// notification icons get the click.
    fn click_part(&mut self, part: logic::Part) {
        let target = match part {
            logic::Part::Chevron => {
                self.toggle_flyout();
                return;
            }
            logic::Part::Tray(position) => {
                self.click_inline(position, Action::Click);
                return;
            }
            logic::Part::Volume => {
                if let Some(volume) = self.status.volume
                    && let Err(error) = crate::commands::audio::set_mute(!volume.muted)
                {
                    eprintln!("Dock mute: {error}");
                }
                self.update_status();
                return;
            }
            logic::Part::Clock => "ms-actioncenter:",
            logic::Part::Battery => "ms-settings:batterysaver",
        };
        if let Err(error) = crate::launch::open(target) {
            eprintln!("Dock status: {error}");
        }
    }

    /// Wheel `delta` over the dock switches desktops a notch at a time,
    /// negative toward the right.
    fn wheel_desktops(&mut self, delta: i32) {
        if self.desktop_switched.is_some_and(|at| at.elapsed() < DESKTOP_PAUSE) {
            self.desktop_travel = 0;
            return;
        }
        let (step, travel) = logic::wheel_desktop(self.desktop_travel, delta);
        self.desktop_travel = travel;
        let Some(right) = step else { return };
        // Explorer updates its list after the switch, so a showing pill knows better.
        let mut desktops = self.desktop_pill.take().unwrap_or_else(desktops::read);
        let target = if right {
            desktops.current + 1
        } else {
            desktops.current.wrapping_sub(1)
        };
        // Like Windows, the ends don't wrap around. The pill still shows where you are.
        if target < desktops.names.len() {
            desktops::switch(right);
            desktops.current = target;
            self.desktop_switched = Some(Instant::now());
        }
        self.show_desktops(desktops);
    }

    /// Shows the desktops in a pill above the dock for a moment.
    fn show_desktops(&mut self, desktops: desktops::Desktops) {
        self.hide_windows();
        self.tip_target = None;
        let name = &desktops.names[desktops.current];
        let Ok((width, height)) = self.tip_renderer.measure_desktops(desktops.names.len(), name) else {
            return;
        };
        let scale = self.dpi as f32 / 96.0;
        let (width, height) = ((width * scale).ceil() as i32, (height * scale).ceil() as i32);
        let gap = (6.0 * scale).round() as i32;
        self.desktop_pill = Some(desktops);
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIP_TIMER);
            let mut dock = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut dock);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST), &mut info);
            let mon = info.rcMonitor;
            let (x, y) = logic::popup_pos(
                self.edge,
                (dock.left, dock.top, dock.right, dock.bottom),
                (width, height),
                (mon.left, mon.top, mon.right, mon.bottom),
                gap,
            );
            let _ = SetWindowPos(
                self.tip,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(Some(self.tip), None, false);
            if !self.held {
                SetTimer(Some(self.hwnd), DESKTOP_TIMER, DESKTOP_PILL_MS, None);
            }
        }
    }

    /// Wheel `delta` over the speaker changes the volume by 2% a notch.
    fn wheel_volume(&mut self, delta: i32) {
        let Some(volume) = self.status.volume else { return };
        let (percent, rest) = logic::wheel_volume(volume.percent, self.wheel + delta);
        self.wheel = rest;
        if percent != volume.percent
            && let Err(error) = crate::commands::audio::set_volume(percent)
        {
            eprintln!("Dock volume: {error}");
        }
        self.update_status();
    }

    /// Rereads the status, redrawing if it changed.
    fn update_status(&mut self) {
        let _span = crate::util::perf::Span::new("status");
        let status = status::read();
        if status != self.status {
            self.status = status;
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    fn click(&mut self, index: usize, new_instance: bool) {
        if index == 0 {
            unsafe {
                let _ = PostMessageW(Some(self.owner), WM_SHOW_LAUNCHER, WPARAM(0), LPARAM(0));
            }
        } else if let Some(item) = self.items.get(index - 1 + self.scroll) {
            if !new_instance && let Some(window) = item.windows.first() {
                // Clicking the app already in front minimizes it, as its
                // taskbar button does; any other click brings it forward.
                if item.windows.iter().any(|w| open_windows::is_foreground(w.hwnd)) {
                    for window in &item.windows {
                        if let Err(error) = open_windows::minimize(window.hwnd) {
                            eprintln!("Dock minimize: {error}");
                        }
                    }
                } else if let Err(error) = open_windows::focus(window.hwnd) {
                    eprintln!("Dock activation: {error}");
                }
            } else {
                let result = if self.apps.iter().any(|app| app.id == item.app.id) {
                    crate::launch::app(&item.app.id, item.app.path.as_deref(), false)
                } else if let Some(path) = &item.app.path {
                    crate::launch::open(path)
                } else {
                    crate::launch::app(&item.app.id, None, false)
                };
                if let Err(error) = result {
                    eprintln!("Dock launch: {error}");
                }
            }
        }
    }

    fn menu(&mut self, index: usize) {
        let Some(item) = index.checked_sub(1).and_then(|i| self.items.get(i + self.scroll)) else {
            return;
        };
        let windows: Vec<_> = item.windows.iter().map(|window| window.hwnd).collect();
        unsafe {
            let Ok(menu) = CreatePopupMenu() else {
                return;
            };
            let _ = AppendMenuW(
                menu,
                MF_STRING | if windows.is_empty() { MF_GRAYED } else { MF_ENABLED },
                2,
                w!("Close window(s)"),
            );
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // Use the dock as menu owner. Its wndproc rejects RefCell reentrancy.
            let _ = SetForegroundWindow(self.hwnd);
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                None,
                self.hwnd,
                None,
            )
            .0;
            let _ = DestroyMenu(menu);
            if command == 2 {
                for hwnd in windows {
                    let _ = open_windows::close(hwnd);
                }
            }
        }
        self.refresh();
    }
}

fn window_aumid(hwnd: HWND) -> Option<String> {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd).ok()?;
        // PKEY_AppUserModel_ID, defined here to avoid an unrelated storage feature.
        let key = PROPERTYKEY {
            fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
            pid: 5,
        };
        let mut value = store.GetValue(&key).ok()?;
        let result = PropVariantToStringAlloc(&value);
        let _ = PropVariantClear(&mut value);
        let ptr = result.ok()?;
        let text = ptr.to_string().ok();
        CoTaskMemFree(Some(ptr.0.cast()));
        text.filter(|text| !text.is_empty())
    }
}

/// Finds a monitor by its device name (`\\.\DISPLAY2` — the
/// stable identity the shell's layouts persist against). Used
/// when the shell names the monitor the bar lives on.
pub(crate) fn monitor_by_name(name: &str) -> Option<HMONITOR> {
    struct Search {
        name: Vec<u16>,
        found: Option<HMONITOR>,
    }
    unsafe extern "system" fn callback(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> windows::core::BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            let device: Vec<u16> = info.szDevice.iter().take_while(|&&c| c != 0).copied().collect();
            if device == search.name {
                search.found = Some(monitor);
                return windows::core::BOOL(0);
            }
        }
        windows::core::BOOL(1)
    }
    let mut search = Search {
        name: name.encode_utf16().collect(),
        found: None,
    };
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(callback), LPARAM(&mut search as *mut Search as isize));
    }
    search.found
}

/// Whether the pointer touches the screen edge the dock is
/// anchored to, within the dock monitor's span on the other
/// axis — the autohide reveal probe, per edge.
fn at_anchor_edge(cursor: POINT, edge: logic::Edge, info: &MONITORINFO) -> bool {
    let m = &info.rcMonitor;
    match edge {
        logic::Edge::Bottom => cursor.y >= m.bottom - 2 && cursor.x >= m.left && cursor.x < m.right,
        logic::Edge::Top => cursor.y <= m.top + 1 && cursor.x >= m.left && cursor.x < m.right,
        logic::Edge::Left => cursor.x <= m.left + 1 && cursor.y >= m.top && cursor.y < m.bottom,
        logic::Edge::Right => cursor.x >= m.right - 2 && cursor.y >= m.top && cursor.y < m.bottom,
    }
}

/// The foreground window's class, for narration.
fn foreground_class() -> String {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return String::new();
        }
        let mut class = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class);
        String::from_utf16_lossy(&class[..len.max(0) as usize])
    }
}

/// What the fullscreen test looks at, gathered from Win32 so
/// the decision itself is a pure function with tests.
struct ForegroundFacts {
    class: String,
    /// The window's rect covers its monitor on all four sides.
    covers: bool,
    zoomed: bool,
    /// The rect is exactly the monitor (a maximized window's
    /// borders spill past it; a true fullscreen's do not).
    exact_monitor: bool,
    /// The work area fills the monitor (nothing reserved).
    work_is_monitor: bool,
}

/// Whether a foreground window is a fullscreen app the dock
/// must yield to — as opposed to a shell surface, which only
/// *looks* like one: it covers its monitor by design and
/// holds the foreground whenever the user clicks the desktop
/// or the last app window closes. Explorer's desktop classes
/// were always exempt for exactly that reason. A standalone
/// session's desktop is the shell host's full-monitor window
/// (`CoquilleDesktop`), and without the same exemption this
/// test latched the dock hidden for as long as the desktop
/// held the foreground: refresh hid it, and every poll's edge
/// probe was refused by this test, until some ordinary window
/// — the emergency launcher, say — took the foreground and
/// unstuck it. In bridge mode the class carries no meaning
/// and the verdict is what it always was.
fn fullscreen_cover(facts: &ForegroundFacts, standalone: bool) -> bool {
    if matches!(
        facts.class.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    ) {
        return false;
    }
    if standalone && facts.class == "CoquilleDesktop" {
        return false;
    }
    // Maximized windows have resize borders outside the monitor; don't
    // treat those as fullscreen. Some apps go fullscreen from maximized,
    // though: that shows when the window fills the monitor exactly while
    // the work area, and so a plain maximize, stops short of it.
    facts.covers && (!facts.zoomed || (facts.exact_monitor && !facts.work_is_monitor))
}

fn fullscreen_foreground(dock: HWND) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() || hwnd == dock || IsIconic(hwnd).as_bool() {
            return false;
        }
        let mut class = [0u16; 128];
        let len = GetClassNameW(hwnd, &mut class);
        let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
        let mut rect = RECT::default();
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetWindowRect(hwnd, &mut rect).is_err()
            || !GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut info).as_bool()
        {
            return false;
        }
        let facts = ForegroundFacts {
            class,
            covers: rect.left <= info.rcMonitor.left
                && rect.top <= info.rcMonitor.top
                && rect.right >= info.rcMonitor.right
                && rect.bottom >= info.rcMonitor.bottom,
            zoomed: IsZoomed(hwnd).as_bool(),
            exact_monitor: rect == info.rcMonitor,
            work_is_monitor: info.rcWork == info.rcMonitor,
        };
        fullscreen_cover(&facts, crate::backend::is_standalone())
    }
}

unsafe extern "system" fn window_event(
    _: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    _: u32,
    _: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        if !matches!(
            event,
            EVENT_OBJECT_CREATE
                | EVENT_OBJECT_DESTROY
                | EVENT_OBJECT_SHOW
                | EVENT_OBJECT_HIDE
                | EVENT_OBJECT_NAMECHANGE
                | EVENT_OBJECT_LOCATIONCHANGE
                | EVENT_OBJECT_CLOAKED
                | EVENT_OBJECT_UNCLOAKED
        ) || object != OBJID_WINDOW.0
            || child != 0
            || hwnd.is_invalid()
        {
            return;
        }
        if event != EVENT_OBJECT_DESTROY && unsafe { GetAncestor(hwnd, GA_ROOT) } != hwnd {
            return;
        }
    }
    EVENT_TARGET.with(|target| unsafe {
        let hwnd = target.get();
        if !hwnd.is_invalid() {
            let _ = PostMessageW(Some(hwnd), WM_REFRESH, WPARAM(0), LPARAM(0));
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if msg == WM_NCACTIVATE {
            return DefWindowProcW(hwnd, msg, WPARAM(1), LPARAM(-1));
        }
        if msg == WM_MOUSEACTIVATE {
            return LRESULT(MA_NOACTIVATE as isize);
        }
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT as isize);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        if !pointer.is_null()
            && let Ok(mut s) = (*pointer).try_borrow_mut()
        {
            match msg {
                WM_SETTINGCHANGE | WM_POWERBROADCAST | WM_THEMECHANGED => {
                    let dark = s.palette.text.r > 0.5;
                    let backdrop = s.backdrop;
                    s.apply_material(dark, backdrop);
                }
                WM_REFRESH => {
                    if !s.scheduled {
                        s.scheduled = SetTimer(Some(hwnd), REFRESH_TIMER, 100, None) != 0;
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == POLL_TIMER => {
                    s.poll();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == SLIDE_TIMER => {
                    s.step_slide();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == HOVER_TIMER => {
                    s.step_hover();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == TIP_TIMER => {
                    s.show_tip();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == DESKTOP_TIMER => {
                    let _ = KillTimer(Some(hwnd), DESKTOP_TIMER);
                    if s.desktop_pill.take().is_some() {
                        let _ = ShowWindow(s.tip, SW_HIDE);
                    }
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == WINDOWS_TIMER => {
                    s.show_windows();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == WINDOWS_HIDE_TIMER => {
                    let _ = KillTimer(Some(hwnd), WINDOWS_HIDE_TIMER);
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&mut cursor);
                    let over = |window: HWND| {
                        let mut rect = RECT::default();
                        GetWindowRect(window, &mut rect).is_ok()
                            && (rect.left..rect.right).contains(&cursor.x)
                            && (rect.top..rect.bottom).contains(&cursor.y)
                    };
                    if !over(s.hwnd) && !over(s.windows) {
                        s.hide_windows();
                    }
                    return LRESULT(0);
                }
                WM_TRAY_CHANGED => {
                    s.sync_tray();
                    return LRESULT(0);
                }
                WM_TIMER if wp.0 == REFRESH_TIMER => {
                    let _ = KillTimer(Some(hwnd), REFRESH_TIMER);
                    s.scheduled = false;
                    s.refresh();
                    return LRESULT(0);
                }
                WM_DPICHANGED | WM_DISPLAYCHANGE => {
                    let _ = PostMessageW(Some(hwnd), WM_REFRESH, WPARAM(0), LPARAM(0));
                    return LRESULT(0);
                }
                WM_MOUSEMOVE => {
                    let hover = s.hit(lp);
                    let hover_part = s.hit_part(lp);
                    if s.hover != hover || s.hover_part != hover_part {
                        s.hover = hover;
                        s.animate_hover();
                        s.hover_part = hover_part;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    s.hover_tip(match hover_part {
                        Some(logic::Part::Chevron) => Some(tray::Tip::Chevron),
                        Some(logic::Part::Tray(i)) => Some(tray::Tip::Inline(i)),
                        _ => None,
                    });
                    s.hover_windows(if hover_part.is_none() { hover } else { None });
                    let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    });
                    return LRESULT(0);
                }
                WM_MOUSELEAVE => {
                    s.hover = None;
                    s.animate_hover();
                    s.hover_part = None;
                    s.hover_tip(None);
                    s.leave_windows();
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                WM_LBUTTONUP | WM_MBUTTONUP => {
                    // The dock never takes the focus, so the flyout can't see this click.
                    if s.hit_part(lp) != Some(logic::Part::Chevron) {
                        s.close_flyout();
                    }
                    s.hide_windows();
                    if let Some(index) = s.hit(lp) {
                        s.click(index, msg == WM_MBUTTONUP);
                    } else if let Some(part) = s.hit_part(lp)
                        && msg == WM_LBUTTONUP
                    {
                        s.click_part(part);
                    }
                    return LRESULT(0);
                }
                WM_RBUTTONUP => {
                    s.close_flyout();
                    s.hide_windows();
                    if let Some(index) = s.hit(lp) {
                        s.menu(index);
                    } else {
                        match s.hit_part(lp) {
                            Some(logic::Part::Volume) => {
                                if let Err(error) = crate::launch::open("ms-settings:sound") {
                                    eprintln!("Dock sound settings: {error}");
                                }
                            }
                            Some(logic::Part::Chevron) => s.chevron_menu(),
                            Some(logic::Part::Tray(position)) => s.click_inline(position, Action::Menu),
                            _ => {}
                        }
                    }
                    return LRESULT(0);
                }
                WM_MOUSEWHEEL => {
                    s.hide_windows();
                    let delta = (wp.0 >> 16) as i16;
                    // Wheel messages carry screen coordinates.
                    let mut point = POINT {
                        x: (lp.0 as i16).into(),
                        y: ((lp.0 >> 16) as i16).into(),
                    };
                    let _ = ScreenToClient(hwnd, &mut point);
                    let client = LPARAM(((point.y as u16 as isize) << 16) | point.x as u16 as isize);
                    if s.hit_part(client) == Some(logic::Part::Volume) {
                        s.wheel_volume(delta.into());
                        return LRESULT(0);
                    }
                    // Which gesture scrolls the apps follows the
                    // strip's axis: on a horizontal dock it is
                    // shift+wheel (plain wheel switches desktops);
                    // on a vertical dock the wheel runs along the
                    // strip itself, so the two swap.
                    let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                    if shift == s.edge.is_vertical() {
                        s.wheel_desktops(delta.into());
                        return LRESULT(0);
                    }
                    s.scroll = if delta < 0 {
                        (s.scroll + 1).min(s.items.len().saturating_sub(s.capacity))
                    } else {
                        s.scroll.saturating_sub(1)
                    };
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                WM_MOUSEHWHEEL => {
                    s.hide_windows();
                    // Positive is a swipe toward the right.
                    s.wheel_desktops(-i32::from((wp.0 >> 16) as i16));
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let mut paint = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut paint);
                    if let Err(error) = s.paint(None) {
                        eprintln!("Dock paint: {error}");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                WM_CLOSE => {
                    s.hide_windows();
                    s.stop_slide();
                    s.visible = false;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    return LRESULT(0);
                }
                WM_NCDESTROY => {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// The flyout and the tooltip, which share the dock's state.
unsafe extern "system" fn popup_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        if msg == WM_NCCALCSIZE && wp.0 != 0 {
            return LRESULT(0);
        }
        if msg == WM_ERASEBKGND {
            return LRESULT(1);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<State>;
        let Some(cell) = pointer.as_ref() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let tip = cell.try_borrow().is_ok_and(|s| s.tip == hwnd);
        if tip {
            // The tooltip never takes the pointer or the focus.
            match msg {
                WM_NCHITTEST => return LRESULT(HTTRANSPARENT as isize),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                _ => {}
            }
        } else if msg == WM_NCHITTEST {
            return LRESULT(HTCLIENT as isize);
        }
        let windows = cell.try_borrow().is_ok_and(|s| s.windows == hwnd);
        if windows {
            match msg {
                // Clicks land here, but the list never takes the focus.
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_MOUSEMOVE => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    s.away = 0;
                    let _ = KillTimer(Some(s.hwnd), WINDOWS_HIDE_TIMER);
                    let hover = s.windows_hit(lp);
                    if s.windows_hover != hover {
                        s.windows_hover = hover;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    });
                    return LRESULT(0);
                }
                WM_MOUSELEAVE => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    s.leave_windows();
                    return LRESULT(0);
                }
                WM_LBUTTONUP | WM_MBUTTONUP => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    if let Some(row) = s.windows_hit(lp)
                        && let Some(&target) = s.windows_targets.get(row)
                    {
                        s.hide_windows();
                        let result = if msg == WM_MBUTTONUP {
                            open_windows::close(target)
                        } else {
                            open_windows::focus(target)
                        };
                        if let Err(error) = result {
                            eprintln!("Dock window: {error}");
                        }
                    }
                    return LRESULT(0);
                }
                WM_PAINT => {
                    let Ok(mut s) = cell.try_borrow_mut() else {
                        return DefWindowProcW(hwnd, msg, wp, lp);
                    };
                    let mut paint = PAINTSTRUCT::default();
                    let _ = BeginPaint(hwnd, &mut paint);
                    let State {
                        windows_renderer,
                        palette,
                        dpi,
                        windows_rows,
                        windows_targets,
                        windows_hover,
                        ..
                    } = &mut *s;
                    if let Err(error) = windows_renderer.draw_windows(
                        hwnd,
                        *dpi,
                        palette,
                        windows_rows,
                        *windows_hover,
                        windows_targets.len(),
                    ) {
                        eprintln!("Dock window list: {error}");
                    }
                    let _ = EndPaint(hwnd, &paint);
                    return LRESULT(0);
                }
                _ => return DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
        let Ok(mut s) = cell.try_borrow_mut() else {
            return DefWindowProcW(hwnd, msg, wp, lp);
        };
        let scale = s.dpi as f32 / 96.0;
        let cell_at = |s: &State, lp: LPARAM| {
            logic::flyout_hit(
                (lp.0 as i16) as f32 / scale,
                ((lp.0 >> 16) as i16) as f32 / scale,
                s.tray.len(),
            )
        };
        match msg {
            WM_PAINT => {
                let mut paint = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut paint);
                let State {
                    tray,
                    palette,
                    dpi,
                    flyout_hover,
                    flyout_renderer,
                    tip_renderer,
                    tip_text,
                    desktop_pill,
                    ..
                } = &mut *s;
                let result = if let Some(pill) = desktop_pill.as_ref().filter(|_| tip) {
                    tip_renderer.draw_desktops(
                        hwnd,
                        *dpi,
                        palette,
                        (pill.current, pill.names.len()),
                        &pill.names[pill.current],
                    )
                } else if tip {
                    tip_renderer.draw_tip(hwnd, *dpi, palette, tip_text)
                } else {
                    flyout_renderer.draw_flyout(
                        hwnd,
                        &render::FlyoutFrame {
                            dpi: *dpi,
                            palette,
                            tray,
                            hover: *flyout_hover,
                        },
                    )
                };
                if let Err(error) = result {
                    eprintln!("Dock popup paint: {error}");
                }
                let _ = EndPaint(hwnd, &paint);
                return LRESULT(0);
            }
            _ if tip => {}
            WM_MOUSEMOVE => {
                let hover = cell_at(&s, lp);
                if s.flyout_hover != hover {
                    s.flyout_hover = hover;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                s.hover_tip(hover.map(tray::Tip::Flyout));
                let _ = TrackMouseEvent(&mut TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                });
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                s.flyout_hover = None;
                s.hover_tip(None);
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // Like the taskbar's overflow, the flyout closes once an icon is used.
            WM_LBUTTONUP | WM_RBUTTONUP => {
                if let Some(index) = cell_at(&s, lp) {
                    let rect = s.cell_rect(index);
                    let center = POINT {
                        x: (rect.left + rect.right) / 2,
                        y: (rect.top + rect.bottom) / 2,
                    };
                    s.close_flyout();
                    let action = if msg == WM_LBUTTONUP {
                        Action::Click
                    } else {
                        Action::Menu
                    };
                    s.click_tray(index, action, center);
                }
                return LRESULT(0);
            }
            WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
                s.close_flyout();
                return LRESULT(0);
            }
            // Closing within activation changes can reenter; close afterwards.
            WM_ACTIVATE if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE_FLYOUT, WPARAM(0), LPARAM(0));
            }
            WM_CLOSE_FLYOUT => {
                // A held demo flyout stays for its screenshot.
                if !s.held && GetForegroundWindow() != hwnd {
                    s.close_flyout();
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                s.close_flyout();
                return LRESULT(0);
            }
            _ => {}
        }
        drop(s);
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(class: &str, covers: bool, zoomed: bool, exact_monitor: bool, work_is_monitor: bool) -> ForegroundFacts {
        ForegroundFacts {
            class: class.into(),
            covers,
            zoomed,
            exact_monitor,
            work_is_monitor,
        }
    }

    #[test]
    fn shell_surfaces_are_never_fullscreen_apps() {
        // Explorer's desktop and tray classes, either backend.
        for class in ["Progman", "WorkerW", "Shell_TrayWnd", "Shell_SecondaryTrayWnd"] {
            for standalone in [false, true] {
                assert!(
                    !fullscreen_cover(&facts(class, true, false, true, false), standalone),
                    "{class}"
                );
            }
        }
        // The standalone shell's desktop surface: exempt only
        // in a standalone session, where it is the desktop. In
        // bridge mode a window of that class would be judged
        // like any other — the exemption follows the backend,
        // not the name alone.
        assert!(!fullscreen_cover(
            &facts("CoquilleDesktop", true, false, true, false),
            true
        ));
        assert!(fullscreen_cover(
            &facts("CoquilleDesktop", true, false, true, false),
            false
        ));
    }

    #[test]
    fn fullscreen_verdicts_follow_the_cover() {
        // A plain app window that does not cover the monitor.
        assert!(!fullscreen_cover(
            &facts("CabinetWClass", false, false, false, false),
            true
        ));
        // Fullscreen from a windowed state: covers, not zoomed.
        assert!(fullscreen_cover(&facts("mpv", true, false, true, false), true));
        // A maximized window: covers via its spill-over borders
        // but is zoomed and not exactly the monitor.
        assert!(!fullscreen_cover(
            &facts("CabinetWClass", true, true, false, false),
            true
        ));
        // Fullscreen reached from maximized: zoomed, exactly
        // the monitor, and the work area falls short of it (a
        // reservation is in force) — still fullscreen.
        assert!(fullscreen_cover(
            &facts("ApplicationFrameWindow", true, true, true, false),
            true
        ));
        // Zoomed, exact, and the work area is the whole monitor:
        // indistinguishable from a plain maximize on a monitor
        // with nothing reserved; not fullscreen, as before.
        assert!(!fullscreen_cover(
            &facts("ApplicationFrameWindow", true, true, true, true),
            true
        ));
    }
}
