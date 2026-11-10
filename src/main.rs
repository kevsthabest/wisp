// Keep a console for debug logs. Release builds run without a console.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod calc;
mod channel;
mod commands;
mod config;
mod coquille;
mod dock;
mod drag;
mod editor;
mod files;
mod history;
mod hotkey;
mod indexer;
// Settings also uses package-manager detection and update instructions.
#[allow(dead_code)]
mod install;
mod launch;
mod morph;
mod render;
mod search;
mod surface;
mod system;
mod taskbar;
mod theme;
mod tray;
mod tray_host;
mod util;

fn main() {
    if taskbar::dispatch_watchdog() {
        return;
    }
    if let Err(e) = app::run() {
        util::error_box(&format!("Wisp could not start:\n{}", e.message()));
    }
}
