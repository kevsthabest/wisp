//! Which shell platform this process serves (Coquille SPEC §14.1).
//!
//! `ExplorerBridge` is Wisp's original behaviour: Explorer owns
//! the tray, the taskbar and the work area, and Wisp proxies,
//! hides and reserves around it. `StandaloneShell` is the
//! Coquille session: no Explorer exists, so the tray model, the
//! theme and the work-area reservation arrive over the component
//! pipe instead (see `crate::coquille`), and nothing may wait on
//! an Explorer window that will never appear.
//!
//! The backend is chosen once, in `app::run()`, before any window
//! exists; every Explorer coupling in the tree branches on it at
//! exactly the seam points (tray source, start gating, theme
//! source, work-area owner) and nowhere else.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    ExplorerBridge,
    StandaloneShell,
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

pub fn init(backend: Backend) {
    let _ = BACKEND.set(backend);
}

pub fn current() -> Backend {
    BACKEND.get().copied().unwrap_or(Backend::ExplorerBridge)
}

pub fn is_standalone() -> bool {
    current() == Backend::StandaloneShell
}
