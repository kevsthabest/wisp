#![allow(dead_code)]
#[path = "../src/app.rs"]
mod app;
#[path = "../src/backend.rs"]
mod backend;
#[path = "../src/calc.rs"]
mod calc;
#[path = "../src/commands/mod.rs"]
mod commands;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/coquille.rs"]
mod coquille;
#[path = "../src/dock.rs"]
mod dock;
#[path = "../src/drag.rs"]
mod drag;
#[path = "../src/editor.rs"]
mod editor;
#[path = "../src/files.rs"]
mod files;
#[path = "../src/history.rs"]
mod history;
#[path = "../src/hotkey.rs"]
mod hotkey;
#[path = "../src/indexer.rs"]
mod indexer;
#[path = "../src/install.rs"]
mod install;
#[path = "../src/launch.rs"]
mod launch;
#[path = "../src/morph.rs"]
mod morph;
#[path = "../src/render.rs"]
mod render;
#[path = "../src/search.rs"]
mod search;
#[path = "../src/surface/mod.rs"]
mod surface;
#[path = "../src/system.rs"]
mod system;
#[path = "../src/taskbar.rs"]
mod taskbar;
#[path = "../src/theme.rs"]
mod theme;
#[path = "../src/tray.rs"]
mod tray;
#[path = "../src/tray_host.rs"]
mod tray_host;
#[path = "../src/util.rs"]
mod util;
mod channel {
    use windows::core::{PCWSTR, w};
    pub const NIGHTLY: bool = true;
    pub const NAME: &str = "Wisp capture";
    pub const DATA_DIR: &str = "wisp-demo";
    pub const LAUNCHER_CLASS: PCWSTR = w!("Wisp.Capture.Launcher");
    pub const INSTANCE_MUTEX: PCWSTR = w!("Local\\Wisp.Capture.SingleInstance");
    pub const RUN_VALUE: PCWSTR = w!("Wisp capture");
}
fn main() {
    if let Err(e) = app::run() {
        eprintln!("{e}");
    }
}
