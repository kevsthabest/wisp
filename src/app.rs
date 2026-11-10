//! Launcher lifecycle, input and message loop.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::DROPEFFECT_NONE;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::SHDoDragDrop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCWSTR, Result, w};

use crate::commands::{self, Command, Commands};
use crate::config::{Config, Position, Theme};
use crate::dock::space::DockSpace;
use crate::dock::{self, Dock};
use crate::drag::{self, Payload};
use crate::files::{FileResults, FileSearch};
use crate::history::History;
use crate::indexer::{AppEntry, IconPixels, IndexEvent, Indexer};
use crate::render::{Frame, Renderer, SurfaceFrame, layout};
use crate::search::{self, Action, Drill, FileScope, Kind, ResultItem, Searcher};
use crate::surface::nav::{self, Direction, Rect};
use crate::surface::{self, Input, Library, Widget};
use crate::taskbar::{self, TaskbarHider};
use crate::theme::Palette;
use crate::{hotkey, launch, system, tray, util};

const WM_APP_FRAME: u32 = WM_APP + 8;
const WM_APP_INDEX: u32 = WM_APP + 2;
const WM_APP_FILES: u32 = WM_APP + 5;
const WM_APP_SHOW: u32 = WM_APP + 3;
const WM_APP_QUIT: u32 = WM_APP + 4;
/// wisp-settings posts this message after it changes the config.
const WM_APP_RELOAD: u32 = WM_APP + 6;
/// Drives the scripted capture of the dock and the launcher.
const REEL_TIMER: usize = 96;
const REEL_MS: u32 = 50;
const WM_APP_COMMANDS: u32 = WM_APP + 7;
const CLASS_NAME: PCWSTR = crate::channel::LAUNCHER_CLASS;
const HOTKEY_ID: i32 = 1;
const CARET_TIMER: usize = 1;
/// Reads the state again while a surface is open, so it stays live.
const SURFACE_TIMER: usize = 2;
const SURFACE_REFRESH_MS: u32 = 1000;
/// Checks whether the user is done with a taskbar shown from the dock.
const PEEK_TIMER: usize = 3;
/// Tries again to hide the taskbar after a failure. At sign-in Explorer may
/// not have finished creating it.
const TASKBAR_TIMER: usize = 4;
const TASKBAR_RETRY_MS: u32 = 2000;
const TASKBAR_ATTEMPTS: u32 = 5;
/// File results shown after apps, and for a query that starts with `?`.
const MIXED_FILE_RESULTS: usize = 5;
const ONLY_FILE_RESULTS: usize = 30;
const RESCAN_AFTER: Duration = Duration::from_secs(120);
const PLACEHOLDER: &str = "Search apps and files, or try wifi, vol, time";
/// Space below the launcher at the bottom of the screen, in DIPs. Matches the
/// dock's margin so the query box covers the dock.
const BOTTOM_MARGIN: f32 = 4.0;

static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

// Preserve index updates and hide requests during reentrant message handling.
const DEFER_INDEX: u8 = 1;
const DEFER_FILES: u8 = 4;
const DEFER_HIDE: u8 = 2;
const DEFER_COMMANDS: u8 = 8;

thread_local! {
    static LAUNCHER: RefCell<Option<Launcher>> = const { RefCell::new(None) };
    static DEFERRED: Cell<u8> = const { Cell::new(0) };
    static DRAGGING: Cell<bool> = const { Cell::new(false) };
}

/// Returns `None` if the launcher is already borrowed. Windows can send
/// reentrant messages during `ShowWindow` or a modal `ShellExecuteEx` call.
/// Runs deferred work after the outer handler releases the borrow.
fn with_launcher<R>(f: impl FnOnce(&mut Launcher) -> R) -> Option<R> {
    let result = LAUNCHER.with(|cell| cell.try_borrow_mut().ok().and_then(|mut l| l.as_mut().map(f)));
    if result.is_some() {
        run_deferred();
    }
    result
}

/// Defers `work` if a reentrant call cannot borrow the launcher.
fn with_launcher_or_defer(work: u8, f: impl FnOnce(&mut Launcher)) {
    if with_launcher(f).is_none() {
        DEFERRED.with(|d| d.set(d.get() | work));
    }
}

fn run_deferred() {
    loop {
        let work = DEFERRED.with(|d| d.replace(0));
        if work == 0 {
            return;
        }
        let ran = LAUNCHER.with(|cell| {
            let Ok(mut launcher) = cell.try_borrow_mut() else {
                return false;
            };
            if let Some(l) = launcher.as_mut() {
                if work & DEFER_INDEX != 0 {
                    l.drain_index_events();
                }
                if work & DEFER_FILES != 0 {
                    l.drain_file_results();
                }
                if work & DEFER_COMMANDS != 0 {
                    l.drain_command_events();
                }
                if work & DEFER_HIDE != 0 {
                    l.hide();
                }
            }
            true
        });
        if !ran {
            DEFERRED.with(|d| d.set(d.get() | work));
            return;
        }
    }
}

pub fn run() -> Result<()> {
    let _trace = util::perf::Session::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    // The shell platform is chosen before any window exists
    // (src/backend.rs): --coquille means the Coquille supervisor
    // spawned us into a standalone session.
    crate::backend::init(if flag("--coquille") {
        crate::backend::Backend::StandaloneShell
    } else {
        crate::backend::Backend::ExplorerBridge
    });

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ole = drag::Ole::new()?;

        // The mutex limits Wisp to one running instance.
        let _mutex = CreateMutexW(None, true, crate::channel::INSTANCE_MUTEX)?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Wait up to 3 s for a starting instance to create its window.
            for _ in 0..30 {
                if let Ok(existing) = FindWindowW(CLASS_NAME, PCWSTR::null()) {
                    let mut pid = 0;
                    GetWindowThreadProcessId(existing, Some(&mut pid));
                    let _ = AllowSetForegroundWindow(pid);
                    let msg = if flag("--quit") { WM_APP_QUIT } else { WM_APP_SHOW };
                    let _ = PostMessageW(Some(existing), msg, WPARAM(0), LPARAM(0));
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            return Err(windows::core::Error::new(
                E_FAIL,
                "Wisp is running, but its launcher window was not found within 3 s.",
            ));
        }
        if flag("--quit") {
            return Ok(());
        }

        let renderer = Renderer::new()?;
        let system_dpi = GetDpiForSystem();
        let instance = GetModuleHandleW(None)?;
        // build.rs embeds the icon as resource ID 1.
        let load_icon = |metric| -> Result<HICON> {
            let size = GetSystemMetricsForDpi(metric, system_dpi);
            let icon = LoadImageW(
                Some(instance.into()),
                PCWSTR(1 as _),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            )?;
            Ok(HICON(icon.0))
        };
        let small_icon = load_icon(SM_CXSMICON)?;
        let large_icon = load_icon(SM_CXICON)?;

        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: large_icon,
            hIconSm: small_icon,
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            return Err(windows::core::Error::from_thread());
        }
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS_NAME,
            &HSTRING::from(crate::channel::NAME),
            WS_POPUP | WS_CAPTION,
            0,
            0,
            100,
            100,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        // Apply WM_NCCALCSIZE before the first draw to remove the visible frame.
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )?;

        // Config::load creates the config on first run.
        let first_run = !Config::path().exists();
        let (config, config_error) = Config::load();
        let (events_tx, events_rx) = channel();
        let (files_tx, files_rx) = channel();
        // Icons are drawn at 32 DIPs; extract them at the system DPI's pixel size.
        let icon_size = (32 * system_dpi as i32 + 95) / 96;
        let (commands, command_events) = Commands::spawn(hwnd, WM_APP_COMMANDS, icon_size);
        let launcher = Launcher {
            hwnd,
            config,
            config_mtime: Config::modified(),
            registered_hotkey: None,
            tray_icon: small_icon,
            palette: Palette::new(true, true),
            renderer,
            dpi: system_dpi,
            position: (0, 0),
            rows_that_fit: usize::MAX,
            visible: false,
            editor: crate::editor::Editor::default(),
            searcher: Searcher::new(),
            results: Vec::new(),
            base_results: Vec::new(),
            file_results: Vec::new(),
            file_query: None,
            selected: 0,
            scroll: 0,
            caret_visible: true,
            last_mouse: None,
            drag_press: None,
            drag_consumed: false,
            apps: Vec::new(),
            icons: HashMap::new(),
            history: History::load(),
            indexer: Indexer::spawn(hwnd, WM_APP_INDEX, events_tx, icon_size),
            index_events: events_rx,
            file_search: FileSearch::spawn(hwnd, WM_APP_FILES, files_tx, icon_size),
            file_events: files_rx,
            last_scan: Instant::now(),
            commands,
            command_events,
            command_state: commands::State::default(),
            list: None,
            armed: None,
            scope: None,
            surfaces: Library::load(),
            surface_errors: Vec::new(),
            surface: None,
            commands_sent: 0,
            taskbar: None,
            taskbar_failures: 0,
            dock_space: None,
            dock: None,
            morph: None,
            morphed: false,
            outline: None,
            previous_foreground: HWND::default(),
        };
        LAUNCHER.with(|cell| *cell.borrow_mut() = Some(launcher));
        with_launcher(|l| {
            l.start();
            if crate::util::perf::hook_on("WISP_CAPTURE") {
                if !matches!(
                    crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref(),
                    Some("hidden" | "reel")
                ) && let Some(dock) = &mut l.dock
                {
                    dock.hold(
                        None,
                        crate::util::perf::hook("WISP_CAPTURE_DOCK_PART").as_deref(),
                        crate::util::perf::hook("DOCK_DEMO_DPI").and_then(|s| s.parse().ok()),
                    );
                }
                if let Some(query) = crate::util::perf::hook("WISP_CAPTURE_QUERY") {
                    l.editor.insert(&query);
                }
                SetTimer(
                    Some(l.hwnd),
                    99,
                    crate::util::perf::hook("WISP_CAPTURE_DURATION_MS")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(3500),
                    None,
                );
                #[cfg(feature = "perf-trace")]
                if let Some(scenario) = crate::util::perf::hook("WISP_PERF_SCENARIO") {
                    if scenario.ends_with("hidden")
                        && let Some(dock) = &mut l.dock
                    {
                        dock.hide();
                        dock.show();
                    }
                    if !scenario.starts_with("idle") {
                        SetTimer(Some(l.hwnd), 97, 250, None);
                    }
                }
                if crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref() == Some("reel") {
                    // The app icons load during the first 4 s.
                    SetTimer(Some(l.hwnd), REEL_TIMER, 4000, None);
                } else if crate::util::perf::hook_on("WISP_CAPTURE_SCENARIO") {
                    SetTimer(Some(l.hwnd), 98, 45, None);
                }
            }
            if let Some(error) = config_error {
                tray::notify(l.hwnd, "Wisp config", &error, true);
            }
            if flag("--show") {
                l.show();
            } else if first_run && let Err(error) = launch::settings(true) {
                // Onboarding is optional. Without the settings app, Wisp starts as usual.
                tray::notify(l.hwnd, "Could not open Settings", &error.message(), true);
            }
        });

        util::perf::event("ready");
        let active = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let frame_active = active.clone();
        let frame_pending = pending.clone();
        let frame_alive = alive.clone();
        let window = hwnd.0 as usize;
        let pacing = std::thread::spawn(move || {
            while frame_alive.load(Ordering::Acquire) {
                if !frame_active.load(Ordering::Acquire) {
                    std::thread::park();
                    continue;
                }
                let _ = windows::Win32::Graphics::Dwm::DwmFlush();
                if !frame_pending.swap(true, Ordering::AcqRel) {
                    let _ = PostMessageW(Some(HWND(window as _)), WM_APP_FRAME, WPARAM(0), LPARAM(0));
                }
            }
        });
        let mut msg = MSG::default();
        loop {
            let animating =
                with_launcher(|l| l.morph.is_some() || l.dock.as_ref().is_some_and(Dock::animating)).unwrap_or(false);
            active.store(animating, Ordering::Release);
            if animating {
                pacing.thread().unpark();
            }
            if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                break;
            }
            if msg.message == WM_APP_FRAME {
                pending.store(false, Ordering::Release);
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        alive.store(false, Ordering::Release);
        pacing.thread().unpark();
        let _ = pacing.join();
        LAUNCHER.with(|cell| cell.borrow_mut().take());
    }
    Ok(())
}

struct Launcher {
    hwnd: HWND,
    config: Config,
    config_mtime: Option<SystemTime>,
    /// Registered shortcut; may differ from `config.hotkey` if registration fails.
    registered_hotkey: Option<String>,
    tray_icon: HICON,
    palette: Palette,
    renderer: Renderer,
    dpi: u32,
    /// Screen coordinates of the launcher's left edge and its top edge, or its
    /// bottom edge at the bottom of the screen, in physical pixels.
    position: (i32, i32),
    /// Number of results that fit in the current monitor's work area.
    rows_that_fit: usize,
    visible: bool,

    editor: crate::editor::Editor,
    searcher: Searcher,
    /// What the launcher shows: `base_results` with `file_results` inserted
    /// before the web search result.
    results: Vec<ResultItem>,
    /// Results computed on the UI thread: apps, calculator, URL, command, web search.
    base_results: Vec<ResultItem>,
    /// File search results, which arrive later from the file search worker.
    file_results: Vec<ResultItem>,
    /// The file query sent to the worker, if the current query searches files.
    file_query: Option<String>,
    selected: usize,
    scroll: usize,
    caret_visible: bool,
    /// Last cursor position in screen pixels, independent of the launcher layout.
    last_mouse: Option<(i32, i32)>,
    drag_press: Option<((i32, i32), Payload)>,
    drag_consumed: bool,

    apps: Vec<AppEntry>,
    icons: HashMap<String, Arc<IconPixels>>,
    history: History,
    indexer: Indexer,
    index_events: Receiver<IndexEvent>,
    file_search: FileSearch,
    file_events: Receiver<FileResults>,
    last_scan: Instant,

    commands: Commands,
    command_events: Receiver<commands::Event>,
    command_state: commands::State,
    /// The items of the open list, such as Wi-Fi networks. `None` while loading.
    list: Option<Vec<ResultItem>>,
    /// A command waiting for a second Enter, such as Restart.
    armed: Option<Command>,
    /// The list Tab opened, if any.
    scope: Option<Scope>,

    surfaces: Library,
    /// Problems with the user's surface files that the user has been told about.
    surface_errors: Vec<String>,
    surface: Option<OpenSurface>,
    /// Commands sent to the worker. State read before the worker ran all of
    /// them would undo changes the launcher already shows.
    commands_sent: u64,

    /// Hides the Windows taskbar while `hide_taskbar` is on.
    taskbar: Option<TaskbarHider>,
    /// Failed attempts to hide or restore the taskbar since the config changed.
    taskbar_failures: u32,
    /// Room kept for a dock that stays visible.
    dock_space: Option<DockSpace>,
    dock: Option<Dock>,
    morph: Option<Morph>,
    morphed: bool,
    outline: Option<RECT>,
    previous_foreground: HWND,
}

struct Morph {
    from: RECT,
    to: RECT,
    start: Instant,
    opening: bool,
    from_opacity: f32,
}

/// A surface shown instead of the results. The query filters its widgets.
struct OpenSurface {
    /// Index into `Launcher::surfaces`.
    index: usize,
    widgets: Vec<Widget>,
    rects: Vec<Rect>,
    focus: usize,
    /// The cursor inside the focused widget, such as an output device.
    stop: usize,
    scroll: f32,
    /// The query before the surface opened. Leaving it restores the query.
    query: String,
}

/// A list that Tab opens on a result: its actions, or a command's items such
/// as Wi-Fi networks. The query filters the list.
struct Scope {
    /// Shown before the query.
    title: String,
    drill: Option<Drill>,
    /// The result's actions, when `drill` is `None`.
    actions: Vec<ResultItem>,
    /// The query before Tab. Leaving the list restores it.
    query: String,
}

impl Launcher {
    fn start(&mut self) {
        if crate::backend::is_standalone() {
            // The pipe client's threads outlive every surface;
            // they feed the tray model and theme as the shell
            // publishes them.
            crate::coquille::start(self.hwnd);
        }
        self.sync_shell();
        self.apply_theme();
        self.set_collapsed_style(true);
        if self.unified() {
            self.paint();
        }
        // Windows needs the tray icon before it can show notifications.
        tray::add(self.hwnd, self.tray_icon, &self.tooltip());
        if !crate::util::perf::hook_on("WISP_CAPTURE") {
            self.register_hotkey(true);
        }
        self.indexer.rescan();
        self.last_scan = Instant::now();
    }

    fn tooltip(&self) -> String {
        match &self.registered_hotkey {
            Some(hotkey) => format!("{} ({hotkey})", crate::channel::NAME),
            None => format!("{} (no shortcut)", crate::channel::NAME),
        }
    }

    /// Tries to restore the previous shortcut if the new shortcut cannot be
    /// registered, so an invalid config does not disable access to the launcher.
    fn register_hotkey(&mut self, report_errors: bool) {
        if self.registered_hotkey.as_deref() == Some(self.config.hotkey.as_str()) {
            return;
        }
        let wanted = match hotkey::parse(&self.config.hotkey) {
            Ok(hotkey) => hotkey,
            Err(error) => {
                if report_errors {
                    tray::notify(self.hwnd, "Wisp config", &error, true);
                }
                return;
            }
        };
        let hwnd = self.hwnd;
        let register =
            |hk: &hotkey::Hotkey| unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, hk.modifiers, hk.vk).is_ok() };

        let previous = self.registered_hotkey.take();
        if previous.is_some() {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            }
        }
        if register(&wanted) {
            self.registered_hotkey = Some(self.config.hotkey.clone());
        } else {
            if let Some(previous) = previous
                && hotkey::parse(&previous).is_ok_and(|hk| register(&hk))
            {
                self.registered_hotkey = Some(previous);
            }
            if report_errors {
                let message = format!(
                    "Windows could not register {}. Another app may use it. Change \"hotkey\" in the config.",
                    self.config.hotkey
                );
                tray::notify(hwnd, "Could not register shortcut", &message, true);
            }
        }
        tray::set_tip(hwnd, &self.tooltip());
    }

    fn reload_config_if_changed(&mut self) {
        let _span = util::perf::Span::new("config");
        let mtime = Config::modified();
        if mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        let (config, error) = Config::load();
        if let Some(error) = error {
            // Keep the last valid config so a read or parse error does not change it.
            tray::notify(self.hwnd, "Wisp config", &error, true);
            return;
        }
        self.config = config;
        self.taskbar_failures = 0;
        self.sync_shell();
        self.apply_theme();
        self.register_hotkey(true);
    }

    /// Hides or restores the Windows taskbar and creates or removes the dock
    /// to match the config.
    fn sync_shell(&mut self) {
        self.sync_taskbar();
        // The shell's edge (standalone only): the unified
        // surface is a bottom-edge construction, so a dock on
        // any other edge is its own window.
        let edge = if crate::backend::is_standalone() {
            crate::coquille::edge()
        } else {
            dock::logic::Edge::Bottom
        };
        let unified = self.bottom() && edge == dock::logic::Edge::Bottom;
        if self.dock.as_ref().is_some_and(|d| d.shared() != unified) {
            self.dock = None;
        }
        if !self.config.dock {
            self.dock = None;
            self.sync_reserve();
            return;
        }
        if self.dock.is_none() {
            match if unified {
                Dock::on_surface(self.hwnd)
            } else {
                Dock::new(self.hwnd)
            } {
                Ok(dock) => self.dock = Some(dock),
                Err(e) => {
                    tray::notify(self.hwnd, "Could not show the dock", &e.message(), true);
                    return;
                }
            }
        }
        if let Some(dock) = &mut self.dock {
            dock.pace_frames();
            dock.set_apps(&self.apps, &self.icons);
            dock.set_window_list_mode(self.config.window_list);
            dock.set_app_icon_size(self.config.dock_icon_size as u32);
            // Notification icons need a home while the taskbar is hidden.
            // Standalone: their home is the broker's feed, gated on
            // the pipe being up — there is no taskbar to hide.
            let tray_on = if crate::backend::is_standalone() {
                crate::coquille::connected()
            } else {
                self.taskbar.as_ref().is_some_and(TaskbarHider::is_active)
            };
            dock.set_tray(tray_on);
            if crate::backend::is_standalone() {
                dock.set_edge(crate::coquille::edge());
            }
            dock.set_autohide(self.config.dock_autohide);
            if self.visible {
                dock.hide()
            } else {
                Self::present_dock(dock)
            }
        }
        self.sync_reserve();
    }

    /// Presents the dock when the launcher closes or the shell
    /// syncs. Bridge mode honours the config's auto-hide; a
    /// standalone shell keeps its bar on screen — there is no
    /// taskbar-peek sequence to reveal it, and the auto-hide
    /// default was written for life alongside Explorer.
    fn present_dock(dock: &mut Dock) {
        if crate::backend::is_standalone() {
            dock.show_revealed();
        } else {
            dock.show();
        }
    }

    /// Keeps maximized windows above a dock that stays visible while the
    /// taskbar is hidden. A shown taskbar keeps its own space.
    fn sync_reserve(&mut self) {
        if crate::backend::is_standalone() {
            // The reservation is the shell's to apply: send the
            // dock's strip over the pipe instead of running an
            // AppBar or reclaiming with SPI_SETWORKAREA (both are
            // Explorer-protocol moves; the broker declines them).
            let wanted = self.dock.as_ref().is_some_and(Dock::stays_visible);
            let height = self
                .dock
                .as_ref()
                .map_or(dock::DEFAULT_RESERVED_HEIGHT, Dock::reserved_height);
            crate::coquille::set_work_area(wanted, height.round() as i32);
            return;
        }
        let hidden = self.taskbar.as_ref().is_some_and(TaskbarHider::is_active);
        let wanted = hidden && self.dock.as_ref().is_some_and(Dock::stays_visible);
        let reserved_height = self
            .dock
            .as_ref()
            .map_or(dock::DEFAULT_RESERVED_HEIGHT, Dock::reserved_height);
        // Before the space exists, so a re-hide while Windows places it
        // cannot take the bottom back.
        if wanted && let Some(hider) = &mut self.taskbar {
            hider.keep_bottom(true);
        }
        if !wanted {
            self.dock_space = None;
        } else if let Some(space) = &self.dock_space {
            // The taskbar hider may have just changed the work area.
            space.set_height(reserved_height);
            space.check();
        } else {
            match DockSpace::new(reserved_height) {
                Ok(space) => self.dock_space = Some(space),
                Err(e) => eprintln!("could not keep space for the dock: {e}"),
            }
        }
        if let Some(hider) = &mut self.taskbar {
            hider.keep_bottom(self.dock_space.is_some());
        }
    }

    /// Shows the Windows taskbar for a moment, from the dock's chevron menu.
    fn peek_taskbar(&mut self) {
        let Some(hider) = &mut self.taskbar else { return };
        if let Err(e) = hider.peek() {
            eprintln!("could not show the taskbar: {e}");
            return;
        }
        // The dock would cover the taskbar.
        if let Some(dock) = &mut self.dock {
            dock.hide();
        }
        unsafe {
            SetTimer(Some(self.hwnd), PEEK_TIMER, 250, None);
        }
    }

    fn peek_tick(&mut self) {
        let done = match &mut self.taskbar {
            Some(hider) => hider.peek_tick().unwrap_or_else(|e| {
                eprintln!("could not hide the taskbar: {e}");
                true
            }),
            None => true,
        };
        if done {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), PEEK_TIMER);
            }
            if let Some(dock) = &mut self.dock
                && !self.visible
            {
                dock.show();
            }
        }
    }

    /// Hides or restores the taskbar to match the config, or hides it again.
    /// Only the taskbar and the dock's notification area change here, so a
    /// retry cannot touch the launcher or the dock's visibility.
    fn sync_taskbar(&mut self) {
        if crate::backend::is_standalone() {
            // No Explorer taskbar exists to hide or restore; the
            // tray's home is the broker's feed, gated on the pipe.
            if let Some(dock) = &mut self.dock {
                dock.set_tray(crate::coquille::connected());
            }
            return;
        }
        let result = match &mut self.taskbar {
            Some(hider) if self.config.hide_taskbar => hider.reapply(),
            _ => TaskbarHider::configure(&mut self.taskbar, self.hwnd, self.config.hide_taskbar),
        };
        match result {
            Ok(()) => self.taskbar_failures = 0,
            Err(e) => self.taskbar_failed(&e),
        }
        let active = self.taskbar.as_ref().is_some_and(TaskbarHider::is_active);
        if let Some(dock) = &mut self.dock {
            // Notification icons need a home while the taskbar is hidden.
            dock.set_tray(active);
        }
    }

    /// Retries quietly every 2 seconds. Only the last failure is reported, by
    /// which time the tray icon exists to show the notification.
    fn taskbar_failed(&mut self, error: &windows::core::Error) {
        eprintln!("could not change the taskbar: {error}");
        self.taskbar_failures += 1;
        if self.taskbar_failures < TASKBAR_ATTEMPTS {
            unsafe {
                SetTimer(Some(self.hwnd), TASKBAR_TIMER, TASKBAR_RETRY_MS, None);
            }
        } else if self.taskbar_failures == TASKBAR_ATTEMPTS {
            tray::notify(self.hwnd, "Could not change the taskbar", &error.message(), true);
        }
    }

    /// Hides the taskbar again after Explorer or Windows shows it.
    fn on_taskbar_message(&mut self, msg: u32) {
        if let Some(hider) = &mut self.taskbar {
            match hider.on_message(msg) {
                // A later failure gets its retries again, but only once the
                // taskbar is really hidden: a failed restore also lands here.
                Ok(true) if self.config.hide_taskbar && hider.is_active() => self.taskbar_failures = 0,
                Ok(_) => {}
                Err(e) => self.taskbar_failed(&e),
            }
        }
        // Explorer restarting or the hider reclaiming space can take the
        // dock's space away.
        self.sync_reserve();
        // Re-hide and reclaim space first, since those operations can change
        // Explorer's z-order. Don't wait for the tray host's periodic check.
        if msg == taskbar::WM_TASKBAR_SHOW
            && let Some(dock) = &self.dock
        {
            dock.outrank_tray();
        }
    }

    fn update_dock_apps(&mut self) {
        if let Some(dock) = &mut self.dock {
            dock.set_apps(&self.apps, &self.icons);
        }
    }

    fn apply_theme(&mut self) {
        let dark = match self.config.theme {
            Theme::Dark => true,
            Theme::Light => false,
            Theme::System => !system::system_uses_light_theme(),
        };
        self.palette = crate::theme::apply_backdrop(self.hwnd, dark, self.config.backdrop, self.unified());
        // Standalone: the shell's pack outranks Wisp's own tables
        // wherever tokens have arrived (before that, the built-in
        // palette stands in — the first frame should not wait on
        // the pipe).
        let token_palette = if crate::backend::is_standalone() {
            crate::coquille::tokens().map(|t| Palette::from_tokens(&t))
        } else {
            None
        };
        if let Some(palette) = &token_palette {
            self.palette = *palette;
        }
        if let Some(dock) = &mut self.dock {
            dock.apply_theme(
                token_palette.unwrap_or_else(|| Palette::new(dark, true)),
                self.config.backdrop,
            );
        }
        // Placement rides the theme event too: a changed edge
        // re-docks and re-reserves without waiting for a shell
        // sync (the unified-surface case is rebuilt by the next
        // sync_shell, which owns dock creation).
        if crate::backend::is_standalone()
            && let Some(dock) = &mut self.dock
            && !dock.shared()
        {
            dock.set_edge(crate::coquille::edge());
            self.sync_reserve();
        }
        self.invalidate();
    }

    fn toggle(&mut self) {
        if self.visible { self.hide() } else { self.show() }
    }

    fn show(&mut self) {
        let _span = util::perf::Span::new("show");
        util::perf::input("show_to_content");
        self.reload_config_if_changed();
        if !crate::util::perf::hook_on("WISP_CAPTURE") {
            self.register_hotkey(false);
        }
        self.commands.refresh();
        self.armed = None;
        self.leave_scope();
        self.leave_surface();
        self.load_surfaces();
        if self.last_scan.elapsed() > RESCAN_AFTER {
            self.indexer.rescan();
            self.last_scan = Instant::now();
        }
        // Selecting the previous query lets the user replace it by typing.
        let should_morph =
            self.unified() && (self.morph.is_some() || self.dock.as_ref().is_some_and(Dock::mostly_visible));
        self.morphed = should_morph;
        let from = if self.unified() {
            let rect = self.outline.unwrap_or_else(|| self.current_rect());
            let suspended = self.dock.as_mut().unwrap().suspend();
            should_morph.then_some(if self.morph.is_some() || self.visible {
                rect
            } else {
                suspended
            })
        } else {
            None
        };
        let opacity = self.morph_opacity();
        if self.morph.is_none() {
            self.outline = None;
        }
        self.morph = from.map(|rect| Morph {
            from: rect,
            to: rect,
            start: Instant::now(),
            opening: true,
            from_opacity: opacity,
        });
        if !self.visible {
            self.previous_foreground = unsafe { GetForegroundWindow() };
        }
        self.editor.select_all();
        self.update_results();
        self.place_on_active_monitor();
        if let Some(from) = from {
            self.begin_morph(from, self.launcher_rect(), true, opacity);
        }
        self.last_mouse = cursor_pos();
        self.visible = true;
        // The launcher takes the dock's place while it is open.
        if let Some(dock) = &mut self.dock
            && !dock.shared()
        {
            dock.hide();
        }
        self.set_collapsed_style(false);
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                if crate::util::perf::hook_on("WISP_CAPTURE") {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_SHOW
                },
            );
            if !crate::util::perf::hook_on("WISP_CAPTURE") {
                let _ = SetForegroundWindow(self.hwnd);
                let _ = SetFocus(Some(self.hwnd));
            }
            // DWM acrylic uses non-client activation as well as foreground state.
            if GetForegroundWindow() == self.hwnd {
                SendMessageW(self.hwnd, WM_NCACTIVATE, Some(WPARAM(1)), Some(LPARAM(-1)));
            }
        }
        self.restart_caret();
        self.paint();
    }

    fn hide(&mut self) {
        self.drag_press = None;
        if !self.visible {
            return;
        }
        let opacity = self.morph_opacity();
        self.visible = false;
        if self.morphed && self.unified() && !self.taskbar.as_ref().is_some_and(TaskbarHider::is_peeking) {
            let to = self.dock.as_ref().unwrap().resting_rect();
            self.begin_morph(self.outline.unwrap_or_else(|| self.current_rect()), to, false, opacity);
            self.set_collapsed_style(true);
            unsafe {
                if GetForegroundWindow() == self.hwnd && !self.previous_foreground.is_invalid() {
                    let _ = SetForegroundWindow(self.previous_foreground);
                }
                let _ = KillTimer(Some(self.hwnd), CARET_TIMER);
                let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
            }
            return;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), CARET_TIMER);
            let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.morph = None;
        self.outline = None;
        self.apply_theme();
        self.set_collapsed_style(true);
        if let Some(dock) = &mut self.dock
            && !self.taskbar.as_ref().is_some_and(TaskbarHider::is_peeking)
        {
            Self::present_dock(dock);
        }
    }

    /// Uses the monitor under the cursor. Reduces the result count and moves
    /// the launcher up if needed to keep it within the work area.
    fn place_on_active_monitor(&mut self) {
        let work = unsafe {
            let mut cursor = POINT::default();
            let _ = GetCursorPos(&mut cursor);
            let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let _ = GetMonitorInfoW(monitor, &mut info);
            // Use the target monitor's DPI before moving the launcher there.
            let (mut dpi_x, mut dpi_y) = (96, 96);
            let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            self.set_dpi(
                crate::util::perf::hook("DOCK_DEMO_DPI")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(dpi_x),
            );
            if self.unified() { info.rcMonitor } else { info.rcWork }
        };

        let margin = self.px(16.0);
        let width = self.px(layout::WIDTH);
        let left = work.left + (work.right - work.left - width) / 2;
        if self.bottom() {
            // The query box covers the dock's spot, and results grow upward.
            let bottom = work.bottom - self.px(if self.unified() { 9.0 } else { BOTTOM_MARGIN });
            let available = (bottom - margin - work.top) as f32 * 96.0 / self.dpi as f32;
            self.rows_that_fit = (((available - layout::height(1)) / layout::ROW_H).max(0.0) as usize + 1).max(1);
            self.position = (left, bottom);
        } else {
            let full_height = self.px(layout::height(self.config.max_results));
            let top = (work.top + (work.bottom - work.top) / 4)
                .min(work.bottom - margin - full_height)
                .max(work.top + margin);
            let available = (work.bottom - margin - top) as f32 * 96.0 / self.dpi as f32;
            self.rows_that_fit = (((available - layout::height(1)) / layout::ROW_H).max(0.0) as usize + 1).max(1);
            self.position = (left, top);
        }
        self.apply_size();
    }

    fn set_dpi(&mut self, dpi: u32) {
        self.dpi = dpi;
        self.renderer.set_dpi(dpi);
    }

    fn on_dpi_changed(&mut self, dpi: u32) {
        if dpi != self.dpi {
            self.set_dpi(dpi);
            self.outline = None;
            self.morph = None;
            self.apply_theme();
            if self.visible {
                self.place_on_active_monitor();
                self.invalidate();
            } else if let Some(dock) = &mut self.dock {
                dock.refresh();
            }
        }
    }

    /// The query box sits at the bottom of the screen and results stack upward.
    fn bottom(&self) -> bool {
        self.config.position == Position::Bottom
    }

    /// The launcher's height in DIPs.
    fn window_h(&self) -> f32 {
        layout::height(self.window_rows())
    }

    fn px(&self, dips: f32) -> i32 {
        (dips * self.dpi as f32 / 96.0).round() as i32
    }

    fn list_rows(&self) -> usize {
        self.config.max_results.min(self.rows_that_fit)
    }

    fn visible_rows(&self) -> usize {
        self.results.len().min(self.list_rows())
    }

    /// Surfaces reserve their widget budget; search uses only populated rows.
    fn window_rows(&self) -> usize {
        if self.surface.is_some() {
            self.list_rows()
        } else if self.unified() {
            self.visible_rows()
        } else if self.results.is_empty() {
            0
        } else {
            self.list_rows()
        }
    }

    fn apply_size(&mut self) {
        let target = self.launcher_rect();
        if let Some(morph) = &mut self.morph {
            if morph.opening {
                morph.to = target;
            }
            return;
        }
        if self.unified() && self.visible && self.outline.is_some_and(|r| r != target) {
            self.begin_morph(self.outline.unwrap(), target, true, 1.0);
        } else {
            self.set_bounds(target);
            self.outline = Some(target);
        }
    }

    fn unified(&self) -> bool {
        self.bottom() && self.dock.as_ref().is_some_and(Dock::shared)
    }

    fn launcher_rect(&self) -> RECT {
        let width = self.px(layout::WIDTH);
        let height = self.px(self.window_h());
        let top = if self.bottom() {
            self.position.1 - height
        } else {
            self.position.1
        };
        RECT {
            left: self.position.0,
            top,
            right: self.position.0 + width,
            bottom: top + height,
        }
    }

    fn current_rect(&self) -> RECT {
        let mut rect = RECT::default();
        unsafe {
            let _ = GetWindowRect(self.hwnd, &mut rect);
        }
        rect
    }

    fn set_bounds(&mut self, rect: RECT) {
        let _span = util::perf::Span::new("bounds");
        let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
        if self.unified() {
            crate::theme::shape(
                self.hwnd,
                width,
                height,
                crate::morph::radius(self.morph_opacity()),
                self.dpi,
            );
        }
        self.renderer.resize(width.max(1) as u32, height.max(1) as u32);
        if self.morph.is_some()
            && let Some(dock) = &mut self.dock
        {
            dock.surface_rect(rect);
        }
    }

    fn set_collapsed_style(&self, collapsed: bool) {
        if !self.unified() && collapsed {
            return;
        }
        unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                self.hwnd,
                GWL_EXSTYLE,
                if collapsed {
                    style | WS_EX_NOACTIVATE.0 as isize
                } else {
                    style & !(WS_EX_NOACTIVATE.0 as isize)
                },
            );
        }
    }

    fn morph_opacity(&self) -> f32 {
        self.morph.as_ref().map_or(if self.visible { 1.0 } else { 0.0 }, |m| {
            let t = self.morph_progress(m);
            m.from_opacity + ((if m.opening { 1.0 } else { 0.0 }) - m.from_opacity) * t
        })
    }

    fn morph_progress(&self, morph: &Morph) -> f32 {
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && let Some(value) = crate::util::perf::hook("WISP_CAPTURE_MORPH")
            && let Ok(value) = value.parse::<f32>()
        {
            return value.clamp(0.0, 1.0);
        }
        (morph.start.elapsed().as_secs_f32() * 1000.0
            / crate::util::perf::slowdown()
            / if morph.opening {
                crate::morph::OPEN_MS
            } else {
                crate::morph::CLOSE_MS
            })
        .min(1.0)
    }

    fn begin_morph(&mut self, from: RECT, to: RECT, opening: bool, from_opacity: f32) {
        util::perf::event("morph_begin");
        self.morph = Some(Morph {
            from,
            to,
            start: Instant::now(),
            opening,
            from_opacity,
        });
        // Composition acrylic re-blurs on every resize. Use its solid tint during
        // motion, then restore the live material once the geometry settles.
        self.palette = crate::theme::apply_backdrop(
            self.hwnd,
            self.palette.text.r > 0.5,
            crate::config::Backdrop::Solid,
            true,
        );
        self.morph.as_mut().unwrap().start = Instant::now();
        self.outline = Some(from);
        self.set_bounds(from);
    }

    fn step_morph(&mut self) {
        let _span = util::perf::Span::new("morph");
        let Some(morph) = &self.morph else {
            return;
        };
        util::perf::event("morph_frame");
        let tick = Instant::now();
        let elapsed = morph.start.elapsed().as_secs_f64() * 1000.0;
        let t = self.morph_progress(morph);
        let opening = morph.opening;
        let from = morph.from;
        let to = morph.to;
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && crate::util::perf::hook("WISP_CAPTURE_SCENARIO").as_deref() == Some("typing")
            && opening
            && self.editor.text() != "typed"
        {
            self.editor.insert("typed");
            self.update_results();
            eprintln!("during animation query={} morph=true", self.editor.text());
        }
        let rect = crate::morph::bounds(from, self.morph.as_ref().map_or(to, |m| m.to), t);
        self.outline = Some(rect);
        self.set_bounds(rect);
        self.paint();
        if t >= 1.0 {
            self.morph = None;
            self.apply_theme();
            if !opening {
                self.outline = None;
                self.set_bounds(self.dock.as_ref().unwrap().resting_rect());
                if let Some(dock) = &mut self.dock {
                    dock.resume();
                }
            }
        }
        if crate::util::perf::hook_on("WISP_FRAME_TIMING") {
            eprintln!(
                "morph frame timestamp_ms={elapsed:.3} work_ms={:.3}",
                tick.elapsed().as_secs_f64() * 1000.0
            );
        }
    }

    fn update_results(&mut self) {
        let _span = util::perf::Span::new("search");
        let rows_before = self.window_rows();
        if self.surface.is_some() {
            self.update_surface();
            if self.window_rows() != rows_before {
                self.apply_size();
            }
            return;
        }
        if let Some(scope) = &self.scope {
            let items = match (scope.drill, &self.list) {
                (Some(_), Some(list)) => list.clone(),
                (Some(drill), None) => vec![commands::loading(drill)],
                (None, _) => scope.actions.clone(),
            };
            let filter = self.editor.text().trim().to_lowercase();
            self.base_results = items
                .into_iter()
                .filter(|item| filter.is_empty() || item.title.to_lowercase().contains(&filter))
                .collect();
            self.file_query = None;
            self.file_results.clear();
            self.merge_results();
            self.selected = 0;
            self.scroll = 0;
            if self.window_rows() != rows_before {
                self.apply_size();
            }
            self.invalidate();
            return;
        }
        self.base_results = commands::search(self.editor.text(), &self.command_state);
        if !commands::is_window_query(self.editor.text()) {
            self.base_results.extend(self.surfaces.search(self.editor.text()));
            self.base_results.extend(
                self.searcher
                    .search(self.editor.text(), &self.apps, &self.history, &self.config),
            );
        }
        if let Some(armed) = &self.armed {
            for item in &mut self.base_results {
                if let Action::Command(command @ Command::Power(power)) = &item.action
                    && command == armed
                {
                    item.title = format!("Press Enter again to {}", power.title().to_lowercase());
                    item.subtitle = "Type anything to cancel".into();
                }
            }
        }

        let scope = if self.config.file_search {
            FileScope::of(self.editor.text(), &self.base_results)
        } else {
            FileScope::None
        };
        let (file_query, limit) = match scope {
            FileScope::None => (None, 0),
            FileScope::Mixed(query) => (Some(query), MIXED_FILE_RESULTS),
            FileScope::Only(query) => (Some(query), ONLY_FILE_RESULTS),
        };
        match file_query {
            None => {
                self.file_query = None;
                self.file_results.clear();
            }
            // Keep the previous file results until the new ones arrive, so the
            // results below the apps don't flicker on each keystroke.
            Some(query) if self.file_query.as_deref() != Some(query) => {
                let exclude = Arc::from(self.config.file_search_exclude.as_slice());
                self.file_search.search(query, limit, exclude);
                self.file_query = Some(query.to_string());
            }
            Some(_) => {}
        }

        self.merge_results();
        self.selected = 0;
        self.scroll = 0;
        if self.window_rows() != rows_before {
            self.apply_size();
        }
        self.invalidate();
    }

    /// Rebuilds `results` from `base_results` and `file_results`.
    fn merge_results(&mut self) {
        let base = std::mem::take(&mut self.base_results);
        let web = base.iter().position(|r| r.kind == Kind::Web).unwrap_or(base.len());
        self.results = Vec::with_capacity(base.len() + self.file_results.len());
        self.results.extend(base[..web].iter().cloned());
        self.results.extend(self.file_results.iter().cloned());
        self.results.extend(base[web..].iter().cloned());
        self.base_results = base;
        if crate::util::perf::hook_on("WISP_CAPTURE")
            && let Some(rows) = crate::util::perf::hook("WISP_CAPTURE_ROWS")
            && let Ok(rows) = rows.parse::<usize>()
        {
            self.results.truncate(rows);
        }
    }

    fn drain_file_results(&mut self) {
        let mut latest = None;
        while let Ok(results) = self.file_events.try_recv() {
            // Icons are cached even for results that are already outdated.
            self.icons.extend(results.icons);
            if let Some(hits) = results.hits
                && self.file_query.as_deref() == Some(results.query.as_str())
            {
                latest = Some(hits);
            }
        }
        let Some(hits) = latest else {
            self.invalidate();
            return;
        };

        let rows_before = self.window_rows();
        // Keep the web search result selected only if the user selected it.
        // When it is the first result, the top result stays selected instead.
        let web_selected = self.selected > 0 && self.results.get(self.selected).is_some_and(|r| r.kind == Kind::Web);
        self.file_results = hits.into_iter().map(search::file_result).collect();
        self.merge_results();
        if web_selected {
            self.selected = self.results.iter().position(|r| r.kind == Kind::Web).unwrap_or(0);
        }
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
        self.move_selection(0, false);
        if self.window_rows() != rows_before {
            self.apply_size();
        }
        self.invalidate();
    }

    fn move_selection(&mut self, delta: isize, wrap: bool) {
        let len = self.results.len() as isize;
        if len == 0 {
            return;
        }
        let target = self.selected as isize + delta;
        self.selected = if wrap {
            target.rem_euclid(len)
        } else {
            target.clamp(0, len - 1)
        } as usize;
        let rows = self.visible_rows();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        self.invalidate();
    }

    fn activate(&mut self, admin: bool) {
        let Some(result) = self.results.get(self.selected).cloned() else {
            return;
        };
        self.perform(&result.action, &result.title, result.kind, admin);
    }

    fn perform(&mut self, action: &Action, title: &str, kind: Kind, admin: bool) {
        let outcome = match action {
            Action::Launch {
                id,
                path,
                admin: as_admin,
            } => {
                let result = launch::app(id, path.as_deref(), admin || *as_admin);
                if result.is_ok() {
                    self.history.record(id);
                    self.update_dock_apps();
                }
                result
            }
            Action::Copy(text) => system::set_clipboard(self.hwnd, text),
            Action::Open(target) => launch::open(target),
            Action::Shell { command, .. } if command.is_empty() => return,
            Action::Shell {
                command,
                admin: as_admin,
            } => launch::shell(command, admin || *as_admin),
            Action::Reveal(path) => launch::reveal(path),
            Action::None => return,
            Action::Command(command) => {
                self.run_command(command.clone());
                return;
            }
            Action::Surface(index) => {
                self.open_surface(*index);
                return;
            }
        };
        match outcome {
            Ok(()) => self.hide(),
            Err(e) if launch::is_cancelled(&e) => {}
            Err(e) => tray::notify(
                self.hwnd,
                &format!("Could not {} {}", kind.verb().to_lowercase(), title),
                &e.message(),
                true,
            ),
        }
    }

    /// Toggles and adjustments stay open and show the new state. Other
    /// commands close the launcher. Commands that lose work need a second Enter.
    fn run_command(&mut self, command: Command) {
        if let Command::Power(power) = command
            && power.needs_confirmation()
            && self.armed.as_ref() != Some(&command)
        {
            self.armed = Some(command);
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
            return;
        }
        self.armed = None;
        match command {
            Command::Quit => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
                return;
            },
            // These need the launcher's foreground rights, so they run here.
            Command::FocusWindow(hwnd) => {
                if let Err(error) = commands::open_windows::focus(hwnd) {
                    tray::notify(self.hwnd, "Wisp", &error, true);
                }
                self.hide();
                return;
            }
            Command::Power(power) => {
                self.hide();
                if let Err(error) = commands::power::run(power) {
                    tray::notify(self.hwnd, "Wisp", &error, true);
                }
                return;
            }
            _ => {}
        }
        // Show the new state now; the worker confirms it.
        surface::apply(&command, &mut self.command_state);
        match &command {
            Command::WifiPower(on) => {
                if let Some(Some(wifi)) = &mut self.command_state.wifi {
                    wifi.on = *on;
                    wifi.network = wifi.network.take().filter(|_| *on);
                }
            }
            Command::BluetoothPower(on) => {
                if let Some(Some(bluetooth)) = &mut self.command_state.bluetooth {
                    bluetooth.on = *on;
                    bluetooth.connected.retain(|_| *on);
                }
            }
            Command::Volume(percent) => {
                if let Some(Some(audio)) = &mut self.command_state.audio {
                    audio.volume = *percent;
                    audio.muted = false;
                }
            }
            Command::Mute(muted) => {
                if let Some(Some(audio)) = &mut self.command_state.audio {
                    audio.muted = *muted;
                }
            }
            Command::CloseWindow(hwnd) => self.command_state.windows.retain(|w| w.hwnd != *hwnd),
            _ => {}
        }
        let stays_open = command.stays_open();
        self.commands.run(command);
        self.commands_sent += 1;
        if !stays_open {
            self.hide();
            return;
        }
        // Reload an open list, since the command may change it.
        if let Some(drill) = self.scope.as_ref().and_then(|s| s.drill) {
            self.list = None;
            self.commands.list(drill);
        }
        let selected = self.selected;
        self.update_results();
        self.move_selection(selected as isize, false);
    }

    /// Opens the selected result's list: a command's items or the result's actions.
    fn enter_scope(&mut self) {
        if self.scope.is_some() {
            return;
        }
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        if let Action::Surface(index) = item.action {
            self.open_surface(index);
            return;
        }
        let (title, drill, actions) = match item.drill {
            Some(drill) => {
                self.list = None;
                self.commands.list(drill);
                (drill.title().to_string(), Some(drill), Vec::new())
            }
            None => {
                let actions = item.actions();
                if actions.is_empty() {
                    return;
                }
                (item.title.clone(), None, actions)
            }
        };
        self.scope = Some(Scope {
            title,
            drill,
            actions,
            query: self.editor.text().to_string(),
        });
        self.editor.select_all();
        self.editor.insert("");
        self.update_results();
    }

    /// Returns to the results the list was opened from. Returns false if no list is open.
    fn leave_scope(&mut self) -> bool {
        let Some(scope) = self.scope.take() else {
            return false;
        };
        self.list = None;
        self.editor.select_all();
        self.editor.insert(&scope.query);
        self.update_results();
        true
    }

    fn drain_command_events(&mut self) {
        let mut changed = false;
        while let Ok(event) = self.command_events.try_recv() {
            match event {
                commands::Event::State(mut state, icons, ran) => {
                    // The worker read this before running every command sent,
                    // so keep what the launcher shows for those.
                    if ran < self.commands_sent {
                        let shown = &self.command_state;
                        state.wifi = shown.wifi.clone();
                        state.bluetooth = shown.bluetooth.clone();
                        state.audio = shown.audio.clone();
                        state.outputs = shown.outputs.clone();
                        for session in &mut state.sessions {
                            if let Some(old) = shown.sessions.iter().find(|s| s.pid == session.pid) {
                                session.volume = old.volume;
                                session.muted = old.muted;
                            }
                        }
                    }
                    changed |= !icons.is_empty() || self.command_state != *state;
                    self.command_state = *state;
                    self.icons.extend(icons);
                }
                commands::Event::List(drill, items) => {
                    if self.scope.as_ref().is_some_and(|s| s.drill == Some(drill)) {
                        self.list = Some(items);
                        changed = true;
                    }
                }
                commands::Event::Failed(error) => tray::notify(self.hwnd, "Wisp", &error, true),
            }
        }
        if changed && self.visible {
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
        }
    }

    fn reveal_selected(&mut self) {
        if let Some(path) = self.results.get(self.selected).and_then(ResultItem::path)
            && launch::reveal(path).is_ok()
        {
            self.hide();
        }
    }

    fn copy_selected_path(&mut self) {
        if let Some(path) = self.results.get(self.selected).and_then(ResultItem::path) {
            let _ = system::set_clipboard(self.hwnd, path);
        }
    }

    fn drain_index_events(&mut self) {
        let mut apps_changed = false;
        let mut icons_changed = false;
        while let Ok(event) = self.index_events.try_recv() {
            match event {
                IndexEvent::Apps(apps) => {
                    self.apps = apps;
                    apps_changed = true;
                }
                IndexEvent::Icons(icons) => {
                    self.icons.extend(icons);
                    icons_changed = true;
                }
            }
        }
        if apps_changed || icons_changed {
            self.update_dock_apps();
        }
        if apps_changed && self.visible {
            let selected = self.selected;
            self.update_results();
            self.move_selection(selected as isize, false);
        } else if icons_changed {
            self.invalidate();
        }
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY) -> bool {
        let down = |key: VIRTUAL_KEY| unsafe { GetKeyState(key.0 as i32) < 0 };
        let (ctrl, shift) = (down(VK_CONTROL), down(VK_SHIFT));
        if self.surface.is_some() && self.on_surface_key(vk, ctrl, shift) {
            self.restart_caret();
            return true;
        }
        let page = self.list_rows() as isize;
        let mut edited = false;
        match vk {
            VK_ESCAPE => {
                if !self.leave_scope() {
                    self.hide();
                }
                return true;
            }
            VK_RETURN => {
                self.activate(ctrl && shift);
                return true;
            }
            VK_UP => self.move_on_screen(-1, true),
            VK_DOWN => self.move_on_screen(1, true),
            VK_TAB if shift => {
                self.leave_scope();
            }
            VK_TAB => self.enter_scope(),
            VK_PRIOR => self.move_on_screen(-page, false),
            VK_NEXT => self.move_on_screen(page, false),
            VK_LEFT => self.editor.left(ctrl, shift),
            VK_RIGHT => self.editor.right(ctrl, shift),
            VK_HOME => self.editor.home(shift),
            VK_END => self.editor.end(shift),
            VK_BACK if self.scope.is_some() && self.editor.text().is_empty() => {
                self.leave_scope();
            }
            VK_BACK => edited = self.editor.backspace(ctrl),
            VK_DELETE => edited = self.editor.delete(ctrl),
            VK_A if ctrl => self.editor.select_all(),
            VK_C if ctrl && shift => self.copy_selected_path(),
            VK_C if ctrl => self.copy(),
            VK_X if ctrl && self.editor.has_selection() => {
                self.copy();
                self.editor.insert("");
                edited = true;
            }
            VK_V if ctrl => {
                if let Some(text) = system::get_clipboard(self.hwnd) {
                    self.editor.insert(&text.replace(['\r', '\n'], " "));
                    edited = true;
                }
            }
            VK_E if ctrl && shift => {
                self.reveal_selected();
                return true;
            }
            _ => return false,
        }
        if edited {
            self.armed = None;
            self.update_results();
        }
        self.restart_caret();
        self.invalidate();
        true
    }

    fn copy(&mut self) {
        let text = if self.editor.has_selection() {
            Some(self.editor.selected_text())
        } else {
            match self.results.get(self.selected) {
                Some(ResultItem {
                    kind: Kind::Calc,
                    title,
                    ..
                }) => Some(title.as_str()),
                _ => None,
            }
        };
        if let Some(text) = text {
            let _ = system::set_clipboard(self.hwnd, text);
        }
    }

    fn on_char(&mut self, unit: u16) {
        // Space presses the focused widget, unless it is part of a filter.
        if unit == u16::from(b' ') && self.surface.is_some() && self.editor.text().is_empty() {
            self.activate_surface();
            return;
        }
        if self.editor.push_utf16(unit) {
            self.armed = None;
            self.update_results();
            self.restart_caret();
        }
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        let y = y as f32 * 96.0 / self.dpi as f32;
        layout::row_at(y, self.visible_rows(), self.window_h(), self.bottom()).map(|row| self.scroll + row)
    }

    /// Moves the selection by `delta` rows on screen. At the bottom of the
    /// screen the list runs upward, so up moves to the next result.
    fn move_on_screen(&mut self, delta: isize, wrap: bool) {
        let delta = if self.bottom() { -delta } else { delta };
        self.move_selection(delta, wrap);
    }

    fn content_point(&self, x: i32, y: i32) -> (i32, i32) {
        if self.morph.is_none() {
            return (x, y);
        }
        let r = self.current_rect();
        let scale = self.dpi as f32 / 96.0;
        let (ox, oy) = crate::morph::content_offset(
            (r.right - r.left) as f32,
            (r.bottom - r.top) as f32,
            layout::WIDTH * scale,
            self.window_h() * scale,
        );
        ((x as f32 - ox).round() as i32, (y as f32 - oy).round() as i32)
    }

    fn on_mouse_move(&mut self, x: i32, y: i32) {
        // Windows also sends mouse moves when the launcher moves under the cursor.
        // Compare screen positions so resizing and morphing keep the selection.
        let Some(position) = cursor_pos() else {
            return;
        };
        if !mouse_moved(&mut self.last_mouse, position) {
            return;
        }
        if self.surface.is_some() {
            if let Some(index) = self.widget_at(x, y) {
                self.focus_widget(index);
            }
            return;
        }
        if let Some(row) = self.row_at(y)
            && row != self.selected
        {
            self.selected = row;
            self.invalidate();
        }
    }

    fn on_click(&mut self, x: i32, y: i32) {
        if self.surface.is_some() {
            self.on_surface_click(x, y);
            return;
        }
        if let Some(row) = self.row_at(y) {
            self.selected = row;
            self.activate(false);
        }
    }

    fn restart_caret(&mut self) {
        self.caret_visible = true;
        unsafe {
            SetTimer(Some(self.hwnd), CARET_TIMER, GetCaretBlinkTime(), None);
        }
        self.invalidate();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn paint(&mut self) {
        let _span = util::perf::Span::new("paint");
        let opacity = self.morph_opacity();
        let content_size = self.morph.as_ref().map(|_| (layout::WIDTH, self.window_h()));
        let text = self.editor.text();
        let frame = Frame {
            palette: &self.palette,
            text,
            caret: util::utf16_offset(text, self.editor.caret()),
            anchor: util::utf16_offset(text, self.editor.anchor()),
            caret_visible: self.caret_visible,
            placeholder: if self.scope.is_some() || self.surface.is_some() {
                "Filter"
            } else {
                PLACEHOLDER
            },
            chip: match (&self.scope, &self.surface) {
                (_, Some(open)) => self.surfaces.specs.get(open.index).map(|spec| spec.title.as_str()),
                (Some(scope), None) => Some(scope.title.as_str()),
                (None, None) => None,
            },
            results: &self.results,
            selected: self.selected,
            scroll: self.scroll,
            rows: self.visible_rows(),
            icons: &self.icons,
            surface: self.surface.as_ref().map(|open| SurfaceFrame {
                widgets: &open.widgets,
                rects: &open.rects,
                focus: open.focus,
                stop: open.stop,
                scroll: open.scroll,
                rows: self.list_rows(),
            }),
            bottom: self.bottom(),
            opacity: crate::morph::fades(opacity).1,
            dock_opacity: crate::morph::fades(opacity).0,
            content_size,
            shared: self.unified(),
            radius: crate::morph::radius(opacity),
        };
        if let Err(e) = self.renderer.draw(self.hwnd, &frame, |rt| {
            if crate::morph::fades(opacity).0 > 0.0
                && let Some(dock) = &mut self.dock
            {
                let size = unsafe { rt.GetSize() };
                let rect = dock.resting_rect();
                let scale = self.dpi as f32 / 96.0;
                let dock_height = (rect.bottom - rect.top) as f32 / scale;
                let (x, y) = crate::morph::content_offset(
                    size.width,
                    size.height,
                    (rect.right - rect.left) as f32 / scale,
                    dock_height,
                );
                unsafe {
                    rt.SetTransform(&windows_numerics::Matrix3x2::translation(x, y));
                }
                let result = dock.draw_on(rt);
                unsafe {
                    rt.SetTransform(&windows_numerics::Matrix3x2::identity());
                }
                result
            } else {
                Ok(())
            }
        }) {
            eprintln!("could not draw launcher: {e}");
        }
    }

    fn on_tray_command(&mut self, cmd: u32) {
        match cmd {
            tray::CMD_OPEN => self.show(),
            tray::CMD_RESCAN => {
                self.indexer.rescan();
                self.last_scan = Instant::now();
            }
            tray::CMD_SETTINGS => {
                if let Err(error) = launch::settings(false) {
                    tray::notify(self.hwnd, "Could not open Settings", &error.message(), true);
                }
            }
            tray::CMD_CONFIG => {
                let path = Config::path();
                let path = path.to_string_lossy();
                // Notepad can edit the config if Windows cannot open .toml files.
                if launch::open(&path).is_err() {
                    let _ = launch::notepad(&path);
                }
            }
            tray::CMD_AUTOSTART => {
                if let Err(e) = system::set_autostart(!system::autostart_enabled()) {
                    tray::notify(self.hwnd, "Could not change Start with Windows", &e.message(), true);
                }
            }
            // Defer destruction until this handler releases the launcher borrow.
            // WM_DESTROY needs to borrow the launcher for shutdown.
            tray::CMD_QUIT => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_APP_QUIT, WPARAM(0), LPARAM(0));
            },
            _ => {}
        }
    }

    /// Reloads the surface library, reporting new problems with the user's files.
    fn load_surfaces(&mut self) {
        let _span = util::perf::Span::new("surfaces");
        self.surfaces = Library::load();
        let new: Vec<&String> = self
            .surfaces
            .errors
            .iter()
            .filter(|e| !self.surface_errors.contains(e))
            .collect();
        if !new.is_empty() {
            let message = new.iter().map(|e| e.as_str()).collect::<Vec<_>>().join("\n");
            tray::notify(self.hwnd, "Wisp surfaces", &message, true);
        }
        self.surface_errors = self.surfaces.errors.clone();
    }

    fn open_surface(&mut self, index: usize) {
        if self.surfaces.specs.get(index).is_none() {
            return;
        }
        self.leave_scope();
        let rows_before = self.window_rows();
        self.surface = Some(OpenSurface {
            index,
            widgets: Vec::new(),
            rects: Vec::new(),
            focus: 0,
            stop: 0,
            scroll: 0.0,
            query: self.editor.text().to_string(),
        });
        self.editor.select_all();
        self.editor.insert("");
        self.results.clear();
        self.update_surface();
        if self.window_rows() != rows_before {
            self.apply_size();
        }
        self.commands.refresh();
        unsafe {
            SetTimer(Some(self.hwnd), SURFACE_TIMER, SURFACE_REFRESH_MS, None);
        }
        self.invalidate();
    }

    /// Returns to the results the surface was opened from. Returns false if no surface is open.
    fn leave_surface(&mut self) -> bool {
        let Some(open) = self.surface.take() else {
            return false;
        };
        unsafe {
            let _ = KillTimer(Some(self.hwnd), SURFACE_TIMER);
        }
        self.editor.select_all();
        self.editor.insert(&open.query);
        self.update_results();
        true
    }

    /// Resolves the open surface against the latest state, keeping the focus
    /// on the same widget.
    fn update_surface(&mut self) {
        let Some(open) = &mut self.surface else {
            return;
        };
        let Some(spec) = self.surfaces.specs.get(open.index) else {
            return;
        };
        let focused = open
            .widgets
            .get(open.focus)
            .map(|w| (w.id.clone(), w.command_at(open.stop).cloned()));
        open.widgets = surface::resolve(spec, &self.command_state, self.editor.text());
        open.rects = nav::layout(&open.widgets, crate::render::surface_width());
        let same = focused
            .as_ref()
            .and_then(|(id, _)| open.widgets.iter().position(|w| &w.id == id));
        let moved = match same {
            Some(index) => {
                open.focus = index;
                // Options can reorder, such as the default output moving first.
                let widget = &open.widgets[index];
                if let Some(stop) = focused.and_then(|(_, command)| widget.stop_of(&command?)) {
                    open.stop = stop;
                }
                open.stop = open.stop.min(widget.stops().saturating_sub(1));
                false
            }
            None => {
                open.focus = surface::first_control(&open.widgets);
                open.stop = open.widgets.get(open.focus).map_or(0, Widget::first_stop);
                true
            }
        };
        self.results.clear();
        // Keep a scroll position the user chose unless the focus moved.
        if moved {
            self.scroll_surface_to_focus();
        } else {
            self.clamp_surface_scroll();
        }
        self.invalidate();
    }

    fn clamp_surface_scroll(&mut self) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            open.scroll = open.scroll.clamp(0.0, max);
        }
    }

    fn view_height(&self) -> f32 {
        self.list_rows() as f32 * layout::ROW_H
    }

    fn scroll_surface_to_focus(&mut self) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            if let Some(rect) = open.rects.get(open.focus) {
                open.scroll = nav::scroll_to(rect, open.scroll, view_h);
            }
            open.scroll = open.scroll.clamp(0.0, max);
        }
    }

    fn focus_widget(&mut self, index: usize) {
        let Some(open) = &mut self.surface else {
            return;
        };
        if index == open.focus || index >= open.widgets.len() {
            return;
        }
        open.focus = index;
        open.stop = open.widgets[index].first_stop();
        self.scroll_surface_to_focus();
        self.invalidate();
    }

    /// Handles keys while a surface is open. Returns false for keys that edit the filter.
    fn on_surface_key(&mut self, vk: VIRTUAL_KEY, ctrl: bool, shift: bool) -> bool {
        let Some(open) = &self.surface else {
            return false;
        };
        let count = open.widgets.len();
        let focus = open.focus;
        match vk {
            VK_ESCAPE => {
                self.leave_surface();
            }
            VK_BACK if self.editor.text().is_empty() => {
                self.leave_surface();
            }
            VK_RETURN => self.activate_surface(),
            VK_TAB if count > 0 => {
                let next = if shift { focus + count - 1 } else { focus + 1 } % count;
                self.focus_widget(next);
            }
            VK_UP | VK_DOWN => {
                let direction = if vk == VK_UP { Direction::Up } else { Direction::Down };
                if let Some(next) = nav::neighbour(&open.rects, focus, direction) {
                    self.focus_widget(next);
                }
            }
            VK_LEFT | VK_RIGHT => {
                let step: i32 = if vk == VK_LEFT { -1 } else { 1 };
                let Some(widget) = open.widgets.get(focus) else {
                    return true;
                };
                match &widget.input {
                    Input::Slider { value, target, .. } => {
                        let by = if ctrl { 1 } else { surface::STEP } as i32;
                        let value = (*value as i32 + step * by).clamp(0, 100) as u32;
                        self.run_command(target.command(value));
                    }
                    Input::Choice { .. } | Input::Buttons { .. }
                        if (0..widget.stops() as i32).contains(&(open.stop as i32 + step)) =>
                    {
                        if let Some(open) = &mut self.surface {
                            open.stop = (open.stop as i32 + step) as usize;
                        }
                        self.invalidate();
                    }
                    _ => {
                        let direction = if step < 0 { Direction::Left } else { Direction::Right };
                        if let Some(next) = nav::neighbour(&open.rects, focus, direction) {
                            self.focus_widget(next);
                        }
                    }
                }
            }
            VK_PRIOR | VK_NEXT => {
                let view_h = self.view_height();
                if let Some(open) = &mut self.surface {
                    let max = (nav::content_height(&open.rects) - view_h).max(0.0);
                    let by = if vk == VK_PRIOR { -view_h } else { view_h };
                    open.scroll = (open.scroll + by).clamp(0.0, max);
                }
                self.invalidate();
            }
            _ => return false,
        }
        true
    }

    /// Runs what the focused widget does, such as switching Wi-Fi off.
    fn activate_surface(&mut self) {
        let Some(open) = &self.surface else {
            return;
        };
        let Some(widget) = open.widgets.get(open.focus) else {
            return;
        };
        if let Some(action) = widget.activate(open.stop) {
            let label = widget.label.clone();
            self.perform(&action, &label, Kind::Command, false);
        }
    }

    /// Converts window pixels to DIPs relative to the surface's top-left corner.
    fn surface_point(&self, x: i32, y: i32) -> Option<(f32, f32)> {
        let open = self.surface.as_ref()?;
        let scale = 96.0 / self.dpi as f32;
        let (ox, oy) = crate::render::surface_origin(self.bottom());
        let (x, y) = (x as f32 * scale, y as f32 * scale);
        if y < oy || y > oy + self.view_height() {
            return None;
        }
        Some((x - ox, y - oy + open.scroll))
    }

    fn widget_at(&self, x: i32, y: i32) -> Option<usize> {
        let (sx, sy) = self.surface_point(x, y)?;
        self.surface.as_ref()?.rects.iter().position(|r| r.contains(sx, sy))
    }

    fn on_surface_click(&mut self, x: i32, y: i32) {
        let Some(index) = self.widget_at(x, y) else {
            return;
        };
        self.focus_widget(index);
        let scale = 96.0 / self.dpi as f32;
        let stop = self
            .renderer
            .stop_at(x as f32 * scale, y as f32 * scale)
            .filter(|(widget, _)| *widget == index)
            .map(|(_, stop)| stop);
        let Some(point) = self.surface_point(x, y) else {
            return;
        };
        let Some(open) = &mut self.surface else {
            return;
        };
        match &open.widgets[index].input {
            Input::Slider { target, .. } => {
                let command = target.command(nav::slider_value(&open.rects[index], point.0));
                self.run_command(command);
            }
            Input::Choice { .. } | Input::Buttons { .. } => {
                if let Some(stop) = stop {
                    open.stop = stop;
                    self.activate_surface();
                }
            }
            _ => self.activate_surface(),
        }
    }

    fn on_surface_wheel(&mut self, up: bool) {
        let view_h = self.view_height();
        if let Some(open) = &mut self.surface {
            let max = (nav::content_height(&open.rects) - view_h).max(0.0);
            open.scroll = (open.scroll + if up { -48.0 } else { 48.0 }).clamp(0.0, max);
        }
        self.invalidate();
    }

    fn surface_tick(&mut self) {
        if self.visible && self.surface.is_some() {
            self.commands.refresh();
            // The clock changes without new state.
            self.update_surface();
        }
    }

    fn shutdown(&mut self) {
        self.dock = None;
        if let Err(e) = TaskbarHider::configure(&mut self.taskbar, self.hwnd, false) {
            eprintln!("could not restore the taskbar: {e}");
        }
        // Removed last, so Windows works the work area out again from the
        // restored taskbar and the remaining app bars. Waits, so the space
        // is back before Wisp exits.
        if let Some(space) = self.dock_space.take() {
            space.close();
        }
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
        }
        tray::remove(self.hwnd);
    }
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

fn mouse_pos(lparam: LPARAM) -> (i32, i32) {
    (
        (lparam.0 & 0xFFFF) as i16 as i32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

fn cursor_pos() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok().map(|()| (point.x, point.y))
}

fn mouse_moved(last: &mut Option<(i32, i32)>, position: (i32, i32)) -> bool {
    last.replace(position).is_some_and(|previous| previous != position)
}

// Run OLE without borrowing the launcher: its modal loop dispatches window messages.
fn run_drag(hwnd: HWND, payload: Payload) {
    unsafe {
        let _ = ReleaseCapture();
    }
    let Ok(data) = payload.data_object() else {
        return;
    };
    let _ = payload.set_image(&data);
    DRAGGING.with(|d| d.set(true));
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    let effect = unsafe { SHDoDragDrop(Some(hwnd), &data, None, payload.effect()) };
    // Restore the same query on cancellation. A completed drop uses the normal
    // hide path, including the return morph into the dock.
    with_launcher(|l| {
        if effect.is_ok_and(|effect| effect != DROPEFFECT_NONE) {
            l.hide();
        }
        if l.visible || l.morph.is_some() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                if l.visible {
                    let _ = SetForegroundWindow(hwnd);
                    let _ = SetFocus(Some(hwnd));
                }
            }
            l.invalidate();
        }
    });
    DRAGGING.with(|d| d.set(false));
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if DRAGGING.with(Cell::get)
        && matches!(
            msg,
            WM_ACTIVATE
                | WM_HOTKEY
                | WM_APP_SHOW
                | WM_KEYDOWN
                | WM_SYSKEYDOWN
                | WM_CHAR
                | WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
        )
    {
        return LRESULT(0);
    }
    let dock_message = with_launcher(|l| {
        l.unified()
            && (Dock::owns_message(msg, wparam)
                || !l.visible
                    && l.morph.is_none()
                    && matches!(
                        msg,
                        WM_PAINT
                            | WM_MOUSEMOVE
                            | WM_MOUSELEAVE
                            | WM_MOUSEACTIVATE
                            | WM_LBUTTONDOWN
                            | WM_LBUTTONUP
                            | WM_MBUTTONUP
                            | WM_RBUTTONUP
                            | WM_MOUSEWHEEL
                            | WM_MOUSEHWHEEL
                            | WM_NCACTIVATE
                    ))
    })
    .unwrap_or(false);
    if dock_message {
        return with_launcher(|l| l.dock.as_ref().unwrap().dispatch(msg, wparam, lparam)).unwrap_or(LRESULT(0));
    }
    match msg {
        // DWM needs the frame styles for acrylic. Make the full window client area.
        WM_NCCALCSIZE if wparam.0 != 0 => return LRESULT(0),
        // Keep the activation state but never paint the caption. It lands on
        // the client area, as a "Wisp" title strip across the top of the dock.
        // This also covers the dock's menus: while one is open the launcher is
        // borrowed, so the dock's own handler above never sees the message.
        WM_NCACTIVATE => return unsafe { DefWindowProcW(hwnd, msg, wparam, LPARAM(-1)) },
        WM_NCHITTEST => return LRESULT(HTCLIENT as isize),
        WM_HOTKEY => {
            util::perf::input("hotkey_to_content");
            with_launcher(Launcher::toggle);
        }
        WM_APP_SHOW => {
            with_launcher(Launcher::show);
        }
        crate::coquille::WM_APP_THEME => {
            // Fresh tokens from the shell: re-run the same theme
            // application a config change would.
            with_launcher(Launcher::apply_theme);
        }
        #[cfg(feature = "perf-trace")]
        WM_TIMER if wparam.0 == 97 && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                thread_local! { static STEP: Cell<usize> = const { Cell::new(0) }; }
                let step = STEP.with(|s| {
                    let n = s.get();
                    s.set(n + 1);
                    n
                });
                let scenario = crate::util::perf::hook("WISP_PERF_SCENARIO").unwrap_or_default();
                if scenario == "typing" {
                    if !l.visible {
                        l.show();
                    }
                    util::perf::input("key_to_content");
                    l.on_char(if step.is_multiple_of(6) { 8 } else { 101 });
                    l.paint();
                } else if scenario == "slide" {
                    if let Some(dock) = &mut l.dock {
                        dock.capture_slide();
                    }
                } else if !scenario.starts_with("idle") {
                    if l.visible {
                        l.hide();
                    } else {
                        if scenario == "hidden"
                            && let Some(dock) = &mut l.dock
                        {
                            dock.hide();
                        }
                        util::perf::input("hotkey_to_content");
                        l.show();
                    }
                }
            });
        }
        WM_TIMER if wparam.0 == 98 && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                unsafe {
                    let _ = KillTimer(Some(l.hwnd), 98);
                }
                let scenario = crate::util::perf::hook("WISP_CAPTURE_SCENARIO").unwrap_or_default();
                eprintln!(
                    "scenario={scenario} visible={} morph={} return_to_dock={}",
                    l.visible,
                    l.morph.is_some(),
                    l.morphed
                );
                if scenario == "slide" {
                    if let Some(dock) = &mut l.dock {
                        dock.capture_slide();
                    }
                } else if scenario == "typing" {
                    eprintln!("retained query={}", l.editor.text());
                } else {
                    l.hide();
                    eprintln!(
                        "dismiss visible={} morph={} hwnd_visible={}",
                        l.visible,
                        l.morph.is_some(),
                        unsafe { IsWindowVisible(l.hwnd).as_bool() }
                    );
                }
            });
        }
        WM_TIMER if wparam.0 == REEL_TIMER && crate::util::perf::hook_on("WISP_CAPTURE") => {
            with_launcher(|l| {
                thread_local! { static STEP: Cell<u32> = const { Cell::new(0) }; }
                let step = STEP.with(|s| {
                    s.set(s.get() + 1);
                    s.get()
                });
                if step == 1 {
                    let interval = (REEL_MS as f32 * crate::util::perf::slowdown()) as u32;
                    unsafe {
                        SetTimer(Some(l.hwnd), REEL_TIMER, interval, None);
                    }
                }
                // Steps of REEL_MS: the hidden dock slides in, grows into the
                // launcher, takes a query, then shrinks back into the dock.
                match step {
                    1 => {
                        if let Some(dock) = &mut l.dock {
                            dock.capture_reveal();
                        }
                    }
                    19 => l.show(),
                    // The whole query at once: partial queries can list
                    // apps by their paths on this PC.
                    29 => {
                        for unit in b"note" {
                            l.on_char((*unit).into());
                        }
                        l.paint();
                    }
                    56 => l.hide(),
                    81 => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                    _ => {}
                }
            });
        }
        WM_TIMER if wparam.0 == 99 && crate::util::perf::hook_on("WISP_CAPTURE") => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        WM_APP_QUIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        WM_APP_RELOAD => {
            with_launcher(Launcher::reload_config_if_changed);
        }
        WM_APP_FRAME => {
            with_launcher(|l| {
                l.step_morph();
                if let Some(dock) = &mut l.dock {
                    dock.frame();
                }
            });
        }
        WM_APP_INDEX => with_launcher_or_defer(DEFER_INDEX, Launcher::drain_index_events),
        WM_APP_FILES => with_launcher_or_defer(DEFER_FILES, Launcher::drain_file_results),
        WM_APP_COMMANDS => with_launcher_or_defer(DEFER_COMMANDS, Launcher::drain_command_events),
        WM_ACTIVATE if loword(wparam.0) == WA_INACTIVE => with_launcher_or_defer(DEFER_HIDE, Launcher::hide),
        WM_DPICHANGED => {
            with_launcher(|l| l.on_dpi_changed(loword(wparam.0)));
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            if with_launcher(|l| l.on_key(VIRTUAL_KEY(wparam.0 as u16))) == Some(true) {
                return LRESULT(0);
            }
        }
        WM_CHAR => {
            util::perf::input("key_to_content");
            with_launcher(|l| l.on_char(wparam.0 as u16));
            return LRESULT(0);
        }
        // Suppress the Windows beep for Alt shortcuts handled by the launcher.
        WM_SYSCHAR => return LRESULT(0),
        WM_LBUTTONDOWN => {
            let point = mouse_pos(lparam);
            with_launcher(|l| {
                l.drag_consumed = false;
                l.drag_press = if l.visible && l.surface.is_none() && l.morph.is_none() {
                    l.row_at(point.1)
                        .and_then(|row| l.results.get(row))
                        .and_then(Payload::from_result)
                        .map(|payload| (point, payload))
                } else {
                    None
                };
                if l.drag_press.is_some() {
                    unsafe {
                        SetCapture(hwnd);
                    }
                }
            });
        }
        WM_CAPTURECHANGED => {
            with_launcher(|l| l.drag_press = None);
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_pos(lparam);
            let payload = with_launcher(|l| {
                if wparam.0 & 1 == 0 {
                    l.drag_press = None;
                } else if let Some((start, _)) = l.drag_press.as_ref() {
                    let size = unsafe {
                        (
                            GetSystemMetricsForDpi(SM_CXDRAG, l.dpi),
                            GetSystemMetricsForDpi(SM_CYDRAG, l.dpi),
                        )
                    };
                    if drag::crossed_threshold(*start, (x, y), size) {
                        l.drag_consumed = true;
                        return l.drag_press.take().map(|(_, payload)| payload);
                    }
                }
                let (x, y) = l.content_point(x, y);
                l.on_mouse_move(x, y);
                None
            })
            .flatten();
            if let Some(payload) = payload {
                run_drag(hwnd, payload);
                return LRESULT(0);
            }
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_pos(lparam);
            with_launcher(|l| {
                let consumed = std::mem::take(&mut l.drag_consumed);
                let captured = l.drag_press.take().is_some();
                if captured {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                let mut rect = RECT::default();
                let inside = !captured
                    || unsafe { GetClientRect(hwnd, &mut rect).is_ok() && PtInRect(&rect, POINT { x, y }).as_bool() };
                if !consumed && inside {
                    let (x, y) = l.content_point(x, y);
                    l.on_click(x, y);
                }
            });
        }
        WM_MOUSEWHEEL => {
            let delta = (wparam.0 >> 16) as u16 as i16;
            with_launcher(|l| {
                if l.surface.is_some() {
                    l.on_surface_wheel(delta > 0);
                } else {
                    l.move_on_screen(if delta > 0 { -1 } else { 1 }, false);
                }
            });
        }
        WM_TIMER if wparam.0 == SURFACE_TIMER => {
            with_launcher(Launcher::surface_tick);
        }
        WM_TIMER if wparam.0 == CARET_TIMER => {
            with_launcher(|l| {
                l.caret_visible = !l.caret_visible;
                l.invalidate();
            });
        }
        WM_PAINT => unsafe {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(hwnd, &mut ps);
            with_launcher(Launcher::paint);
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        },
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETTINGCHANGE => {
            // Explorer waits for every window while it broadcasts this, and
            // the taskbar hider asks Explorer for its state: let it go first.
            unsafe {
                let _ = ReplyMessage(LRESULT(0));
            }
            with_launcher(|l| {
                l.on_taskbar_message(msg);
                l.apply_theme();
            });
        }
        WM_DISPLAYCHANGE | taskbar::WM_TASKBAR_SHOW | taskbar::WM_RECOVERY_DONE => {
            unsafe {
                let _ = ReplyMessage(LRESULT(0));
            }
            with_launcher(|l| {
                l.on_taskbar_message(msg);
                if msg == WM_DISPLAYCHANGE
                    && !l.visible
                    && let Some(dock) = &mut l.dock
                {
                    dock.refresh();
                }
            });
        }
        dock::WM_SHOW_LAUNCHER => {
            with_launcher(Launcher::show);
        }
        dock::WM_DOCK_SHOW_TASKBAR => {
            with_launcher(Launcher::peek_taskbar);
        }
        WM_TIMER if wparam.0 == PEEK_TIMER => {
            with_launcher(Launcher::peek_tick);
        }
        WM_TIMER if wparam.0 == TASKBAR_TIMER => {
            unsafe {
                let _ = KillTimer(Some(hwnd), TASKBAR_TIMER);
            }
            with_launcher(Launcher::sync_taskbar);
        }
        WM_ENDSESSION if wparam.0 != 0 => taskbar::emergency_restore(),
        tray::WM_TRAY => match loword(lparam.0 as usize) {
            WM_LBUTTONUP => {
                with_launcher(Launcher::show);
            }
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                // Release the launcher borrow before the menu's modal loop handles messages.
                let Some(hotkey) = with_launcher(|l| l.registered_hotkey.clone().unwrap_or_default()) else {
                    return LRESULT(0);
                };
                let cmd = tray::show_menu(hwnd, &hotkey, system::autostart_enabled());
                with_launcher(|l| l.on_tray_command(cmd));
            }
            _ => {}
        },
        // Keep Wisp running after Alt+F4 so the shortcut can reopen the launcher.
        WM_CLOSE => {
            with_launcher(Launcher::hide);
            return LRESULT(0);
        }
        WM_DESTROY => unsafe {
            with_launcher(Launcher::shutdown);
            PostQuitMessage(0);
        },
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            // Explorer removes tray icons when it restarts.
            with_launcher(|l| {
                tray::add(l.hwnd, l.tray_icon, &l.tooltip());
                // The new Explorer shows its taskbar again.
                l.on_taskbar_message(m);
            });
        }
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::mouse_moved;

    #[test]
    fn stationary_cursor_keeps_selection_across_layout_changes() {
        let mut last = Some((800, 600));
        // Growing or moving the launcher changes content coordinates, not these.
        for _ in 0..3 {
            assert!(!mouse_moved(&mut last, (800, 600)));
        }
    }

    #[test]
    fn real_cursor_movement_allows_hover() {
        let mut last = Some((-800, 600));
        assert!(mouse_moved(&mut last, (-799, 600)));
        assert!(!mouse_moved(&mut last, (-799, 600)));
        assert!(mouse_moved(&mut last, (-799, 601)));
        assert!(mouse_moved(&mut last, (-800, 600)));
    }

    #[test]
    fn unknown_cursor_position_needs_a_baseline() {
        let mut last = None;
        assert!(!mouse_moved(&mut last, (800, 600)));
        assert_eq!(last, Some((800, 600)));
        assert!(mouse_moved(&mut last, (801, 600)));
    }
}
