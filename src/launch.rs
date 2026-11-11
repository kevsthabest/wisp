//! Opens apps and URLs and runs commands through the Windows shell.

use std::sync::mpsc::{Receiver, Sender, channel};

use windows::Win32::Foundation::{E_FAIL, ERROR_CANCELLED, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONINFORMATION, MB_YESNO, MessageBoxW, PostMessageW, SW_SHOWNORMAL,
};
use windows::core::{HSTRING, PCWSTR, Result, w};

/// Uses `AppsFolder` to open both desktop and Microsoft Store apps.
pub fn app(id: &str, path: Option<&str>, admin: bool) -> Result<()> {
    if admin && let Some(path) = path {
        return execute(path, None, true);
    }
    unsafe {
        let name = HSTRING::from(format!("shell:AppsFolder\\{id}"));
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        SHParseDisplayName(&name, None, &mut pidl, 0, None)?;
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_FLAG_NO_UI,
            lpIDList: pidl.cast(),
            lpVerb: if admin { w!("runas") } else { PCWSTR::null() },
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let result = ShellExecuteExW(&mut info);
        CoTaskMemFree(Some(pidl.cast()));
        result
    }
}

/// The Windows Settings app's AppUserModelID. An `ms-settings:`
/// link names a page of this app, and the app itself only
/// activates through the activation manager in a session
/// without Explorer (see `activate`) — so the app entry and its
/// links take the same route, the link riding as the activation
/// argument, which is how the app receives a page.
pub const SETTINGS_AUMID: &str =
    "windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";

/// Whether an AppsFolder id is an AppUserModelID: a package
/// family name and an app id joined by `!`, the family carrying
/// its publisher suffix after `_`. Desktop apps in the folder
/// carry path-like ids instead, and the activation manager
/// cannot take those — the distinction chooses the launch route.
pub fn looks_like_aumid(id: &str) -> bool {
    match id.split_once('!') {
        Some((family, app)) => !family.is_empty() && family.contains('_') && !app.is_empty(),
        None => false,
    }
}

/// Whether an open target is an `ms-settings:` link (the scheme
/// is case-insensitive, like every URI scheme). Such a link is
/// not a document to open — it names a page of the Settings app.
pub fn is_ms_settings_link(target: &str) -> bool {
    target
        .get(.."ms-settings:".len())
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("ms-settings:"))
}

/// Activates a packaged app by AppUserModelID through the
/// application activation manager, the route that talks to the
/// AppModel activation service directly. The AppsFolder route
/// in `app` instead asks the shell namespace to invoke the
/// item, and in a session without Explorer that invoke class is
/// not registered: the launch fails in milliseconds with
/// REGDB_E_CLASSNOTREG ("Class not registered") and the app
/// never starts — Windows Settings from this launcher in a
/// standalone session, every time, while Coquille's own
/// surfaces launch packaged apps (Files) this way in the same
/// session. The caller owns COM: the launch worker's thread is
/// apartment-threaded already.
pub fn activate(id: &str, args: &str) -> Result<()> {
    let aumid = HSTRING::from(id);
    let arguments = HSTRING::from(args);
    unsafe {
        let manager: IApplicationActivationManager =
            CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)?;
        manager.ActivateApplication(PCWSTR(aumid.as_ptr()), PCWSTR(arguments.as_ptr()), AO_NONE)?;
    }
    Ok(())
}

/// The launch worker's route for an app launch. The worker only
/// runs in a standalone session, where a packaged app must go
/// through the activation manager (see `activate`); an elevated
/// launch and a desktop app keep the shell routes they already
/// work through. Bridge mode never reaches this — it calls
/// `app` synchronously, exactly as it always has.
fn app_on_worker(id: &str, path: Option<&str>, admin: bool, args: Option<&str>) -> Result<()> {
    if !admin && looks_like_aumid(id) {
        return activate(id, args.unwrap_or(""));
    }
    app(id, path, admin)
}

pub fn open(target: &str) -> Result<()> {
    execute(target, None, false)
}

pub fn shell(command: &str, admin: bool) -> Result<()> {
    execute(&system_tool(r"System32\cmd.exe"), Some(&format!("/k {command}")), admin)
}

pub fn notepad(path: &str) -> Result<()> {
    execute(
        &system_tool(r"System32\notepad.exe"),
        Some(&format!("\"{path}\"")),
        false,
    )
}

pub fn reveal(path: &str) -> Result<()> {
    execute(
        &system_tool("explorer.exe"),
        Some(&format!("/select,\"{path}\"")),
        false,
    )
}

/// Starts `wisp-settings.exe` from the folder that contains `wisp.exe`.
/// Offers the release download if the settings app is not there.
pub fn settings(welcome: bool) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
    let settings = exe.with_file_name("wisp-settings.exe");
    if !settings.is_file() {
        // One dialog at a time, however often Settings is chosen.
        static ASKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if ASKING.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return Ok(());
        }
        // Keep onboarding optional and avoid a modal loop while the launcher is borrowed.
        std::thread::spawn(|| {
            let com_initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok() };
            let answer = unsafe {
                MessageBoxW(
                    None,
                    w!(
                        "Settings and updates need wisp-settings.exe beside wisp.exe. Both come in the release zip.\n\nOpen the download page?"
                    ),
                    w!("Wisp Settings"),
                    MB_YESNO | MB_ICONINFORMATION,
                )
            };
            if answer == IDYES
                && let Err(error) = open("https://github.com/SumirSeth/wisp/releases/latest")
            {
                crate::util::error_box(&format!("Could not open the download page: {}", error.message()));
            }
            if com_initialized {
                unsafe { CoUninitialize() };
            }
            ASKING.store(false, std::sync::atomic::Ordering::Release);
        });
        return Ok(());
    }
    execute(&settings.to_string_lossy(), welcome.then_some("--welcome"), false)
}

/// A shell activation the launcher can ask for. These calls can
/// block for as long as they like: a `ShellExecuteExW` for an
/// immersive target waits on shell infrastructure (the app
/// resolver, the activation broker) that Explorer normally keeps
/// warm. The variants carry everything the call needs so the
/// request can cross to another thread unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchRequest {
    App {
        id: String,
        path: Option<String>,
        admin: bool,
        /// Activation arguments for a packaged app — an
        /// `ms-settings:` link asking the Settings app for one
        /// of its pages. Only the activation-manager route
        /// takes them; the shell routes have nowhere to put
        /// them.
        args: Option<String>,
    },
    Open(String),
    Shell {
        command: String,
        admin: bool,
    },
    Reveal(String),
}

impl LaunchRequest {
    pub fn run(&self) -> Result<()> {
        match self {
            LaunchRequest::App {
                id,
                path,
                admin,
                args,
            } => app_on_worker(id, path.as_deref(), *admin, args.as_deref()),
            LaunchRequest::Open(target) => open(target),
            LaunchRequest::Shell { command, admin } => shell(command, *admin),
            LaunchRequest::Reveal(path) => reveal(path),
        }
    }

    /// One line naming the activation, so if a call in the worker
    /// never returns, the log's last `[launch] begin` line names the
    /// exact call that stalled.
    pub fn describe(&self) -> String {
        match self {
            LaunchRequest::App { id, args, .. } => match args {
                Some(args) => format!("app {id} {args}"),
                None => format!("app {id}"),
            },
            LaunchRequest::Open(target) => format!("open {target}"),
            LaunchRequest::Shell { command, .. } => format!("shell {}", truncated(command, 80)),
            LaunchRequest::Reveal(path) => format!("reveal {path}"),
        }
    }
}

/// How an activation ended, in the terms the launcher acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum LaunchOutcome {
    Ok,
    /// The user dismissed a prompt (a UAC consent, say): the
    /// launcher stays silent and stays open, as it does today.
    Cancelled,
    Failed(String),
}

impl From<Result<()>> for LaunchOutcome {
    fn from(result: Result<()>) -> Self {
        match result {
            Ok(()) => LaunchOutcome::Ok,
            Err(e) if is_cancelled(&e) => LaunchOutcome::Cancelled,
            // The HRESULT rides with the message: "Class not
            // registered" alone does not say which call class
            // failed, and the code is what a diagnosis greps
            // for — a failure that reaches the log (and the
            // user's notice) without it is only half reported.
            Err(e) => LaunchOutcome::Failed(format!(
                "{} (HRESULT {:#010X})",
                e.message(),
                e.code().0 as u32
            )),
        }
    }
}

/// Runs `LaunchRequest`s one at a time on its own thread and reports
/// each outcome back, mirroring the commands worker: a channel in,
/// an events channel out, and a posted message to say events are
/// waiting. The thread is apartment-threaded COM — the same
/// environment these calls run in on the UI thread today.
///
/// This exists because the launcher and the dock share one UI
/// thread. In a standalone session (Wisp as the shell experience) a
/// synchronous `ShellExecuteExW` for an immersive target can wait
/// forever on activation infrastructure that is not there, and the
/// whole shell freezes with it. On the worker, the worst a stall
/// can do is delay its own outcome.
pub struct LaunchWorker {
    jobs: Sender<LaunchRequest>,
}

impl LaunchWorker {
    pub fn spawn(hwnd: HWND, notify_msg: u32) -> (LaunchWorker, Receiver<LaunchOutcome>) {
        let (jobs, jobs_rx) = channel();
        let (events, events_rx) = channel();
        let hwnd = hwnd.0 as isize;
        std::thread::Builder::new()
            .name("launch".into())
            .spawn(move || {
                worker_loop(
                    jobs_rx,
                    events,
                    move || unsafe {
                        let _ = PostMessageW(Some(HWND(hwnd as _)), notify_msg, WPARAM(0), LPARAM(0));
                    },
                    |request| request.run(),
                );
            })
            .expect("could not start launch thread");
        (LaunchWorker { jobs }, events_rx)
    }

    /// Hands a request to the worker. Never blocks, however long the
    /// activation itself takes — that is the point of the worker.
    pub fn dispatch(&self, request: LaunchRequest) {
        let _ = self.jobs.send(request);
    }
}

/// The worker body, split from `spawn` so tests can drive it with a
/// stubbed runner: begin/end narration brackets the risky call, so a
/// stall leaves the culprit named in the log.
fn worker_loop(
    jobs: Receiver<LaunchRequest>,
    events: Sender<LaunchOutcome>,
    notify: impl Fn(),
    run: impl Fn(&LaunchRequest) -> Result<()>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    while let Ok(request) = jobs.recv() {
        let what = request.describe();
        eprintln!("[launch] begin: {what}");
        let started = std::time::Instant::now();
        let outcome = LaunchOutcome::from(run(&request));
        let ms = started.elapsed().as_millis();
        match &outcome {
            LaunchOutcome::Ok => eprintln!("[launch] end: {what} — ok in {ms} ms"),
            LaunchOutcome::Cancelled => {
                eprintln!("[launch] end: {what} — cancelled in {ms} ms")
            }
            LaunchOutcome::Failed(message) => {
                eprintln!("[launch] end: {what} — failed in {ms} ms: {message}")
            }
        }
        if events.send(outcome).is_ok() {
            notify();
        }
    }
}

fn truncated(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Uses an absolute path to avoid running a same-named executable in the
/// current directory.
fn system_tool(relative: &str) -> String {
    let mut buf = [0u16; 260];
    let len = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    let windows = if len > 0 && len < buf.len() {
        String::from_utf16_lossy(&buf[..len])
    } else {
        r"C:\Windows".into()
    };
    format!(r"{windows}\{relative}")
}

fn execute(file: &str, params: Option<&str>, admin: bool) -> Result<()> {
    let file = HSTRING::from(file);
    let params = params.map(HSTRING::from);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_FLAG_NO_UI,
        lpVerb: if admin { w!("runas") } else { w!("open") },
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: params.as_ref().map_or(PCWSTR::null(), |p| PCWSTR(p.as_ptr())),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }
}

/// Identifies user cancellation, including a dismissed UAC prompt.
pub fn is_cancelled(error: &windows::core::Error) -> bool {
    error.code() == ERROR_CANCELLED.to_hresult()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn describes_each_request_for_the_log() {
        assert_eq!(
            LaunchRequest::Open("ms-settings:personalization-background".into()).describe(),
            "open ms-settings:personalization-background"
        );
        assert_eq!(
            LaunchRequest::App {
                id: "Microsoft.WindowsStore_8wekyb3d8bbwe!App".into(),
                path: None,
                admin: false,
                args: None,
            }
            .describe(),
            "app Microsoft.WindowsStore_8wekyb3d8bbwe!App"
        );
        assert_eq!(
            LaunchRequest::App {
                id: SETTINGS_AUMID.into(),
                path: None,
                admin: false,
                args: Some("ms-settings:personalization-background".into()),
            }
            .describe(),
            format!("app {SETTINGS_AUMID} ms-settings:personalization-background")
        );
        assert_eq!(
            LaunchRequest::Reveal(r"C:\Users\me\file.txt".into()).describe(),
            r"reveal C:\Users\me\file.txt"
        );
        let long = "x".repeat(200);
        let described = LaunchRequest::Shell {
            command: long,
            admin: false,
        }
        .describe();
        assert!(
            described.chars().count() < 100,
            "shell lines are truncated: {described}"
        );
    }

    #[test]
    fn outcome_maps_ok_cancelled_and_failed() {
        assert_eq!(LaunchOutcome::from(Ok(())), LaunchOutcome::Ok);
        let cancelled = windows::core::Error::new(ERROR_CANCELLED.to_hresult(), "no".to_string());
        assert_eq!(LaunchOutcome::from(Err(cancelled)), LaunchOutcome::Cancelled);
        // The failure carries its HRESULT beside the message —
        // the code is what a diagnosis greps for.
        let failed = windows::core::Error::new(E_FAIL, "boom".to_string());
        assert_eq!(
            LaunchOutcome::from(Err(failed)),
            LaunchOutcome::Failed("boom (HRESULT 0x80004005)".into())
        );
    }

    #[test]
    fn aumid_shape_decides_the_activation_route() {
        // Packaged apps: family (with publisher suffix) ! app id.
        assert!(looks_like_aumid(SETTINGS_AUMID));
        assert!(looks_like_aumid("Microsoft.WindowsStore_8wekyb3d8bbwe!App"));
        // Desktop apps in the AppsFolder carry path-like ids the
        // activation manager cannot take.
        assert!(!looks_like_aumid(r"C:\Program Files\Vendor\app.exe"));
        assert!(!looks_like_aumid("Microsoft.AutoGenerated.{3B6E7A1F-9C2D}"));
        assert!(!looks_like_aumid("FamilyWithoutSuffix!App"));
        assert!(!looks_like_aumid("!App"));
        assert!(!looks_like_aumid("family_hash!"));
    }

    #[test]
    fn ms_settings_links_are_recognised_case_insensitively() {
        assert!(is_ms_settings_link("ms-settings:personalization-background"));
        assert!(is_ms_settings_link("MS-Settings:display"));
        assert!(is_ms_settings_link("ms-settings:"));
        assert!(!is_ms_settings_link("ms-settings"));
        assert!(!is_ms_settings_link("https://example.com"));
        assert!(!is_ms_settings_link("ms-set"));
    }

    /// The hang fix's seam: dispatch is a channel send, so a runner
    /// that sleeps (the stub for an activation that stalls) cannot
    /// block the caller; the outcome still arrives, in order.
    #[test]
    fn dispatch_never_blocks_on_a_slow_activation() {
        let (jobs, jobs_rx) = channel();
        let (events, events_rx) = channel();
        let (notify_tx, notify_rx) = channel::<()>();
        let worker = std::thread::spawn(move || {
            worker_loop(
                jobs_rx,
                events,
                move || {
                    let _ = notify_tx.send(());
                },
                |_| {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                },
            );
        });
        let worker_handle = LaunchWorker { jobs };
        let started = Instant::now();
        worker_handle.dispatch(LaunchRequest::Open("ms-settings:personalization-background".into()));
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "dispatch blocked on the activation"
        );
        let outcome = events_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the outcome arrives");
        assert_eq!(outcome, LaunchOutcome::Ok);
        notify_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the notify fires");
        drop(worker_handle);
        worker.join().expect("the worker exits with its channel");
    }
}
