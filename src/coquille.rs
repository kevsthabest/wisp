//! The Coquille component-pipe client (standalone backend).
//!
//! Wisp under Coquille is a managed component: the supervisor
//! spawns it with `--coquille`, and everything Explorer used to
//! provide arrives here instead — the tray model (published by
//! the host's broker), the theme tokens, and launcher requests.
//! Outbound, the dock's tray gestures and work-area reservations
//! go back over the same pipe.
//!
//! The wire format is Coquille's component protocol: one JSON
//! value per line, the same shapes `shell-protocol` defines
//! (hand-mirrored here — the fork deliberately carries no
//! Coquille crate dependency; the protocol doc is the contract).
//! The client reconnects on its own; while the pipe is down the
//! dock simply shows the last model it had.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW,
    CreateSolidBrush, DEFAULT_CHARSET, DIB_RGB_COLORS, DeleteDC, DeleteObject, FONT_CLIP_PRECISION,
    FONT_OUTPUT_PRECISION, FONT_QUALITY, FillRect, GdiFlush, GetTextExtentPoint32W, HGDIOBJ, SelectObject, SetBkMode,
    SetTextColor, TRANSPARENT, TextOutW,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO, PostMessageW, WM_APP};
use windows::core::{GUID, PCWSTR};

use crate::theme::TokenColors;
use crate::tray_host::model::Owner;
use crate::tray_host::wire::{self, Notify};
use crate::tray_host::{self, OwnedIcon};

/// The supervisor's pipe path, session-scoped. Coquille's name
/// format is `\\.\pipe\coquille-supervisor-v1-s<session id>`
/// (hand-mirrored like the rest of the protocol here — the fork
/// carries no Coquille crate dependency). The pipe namespace is
/// machine-global while shells are per-session: without the
/// suffix, two signed-in sessions' supervisors would share one
/// name and this client could land on the wrong session's shell.
fn pipe_path() -> String {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    let mut session = 0u32;
    let _ = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    format!(r"\\.\pipe\coquille-supervisor-v1-s{session}")
}

const PROTOCOL_VERSION: u32 = 1;

/// Mirrors the launcher's own constant (src/app.rs) — a
/// show_launcher event from the shell lands as this message.
const WM_APP_SHOW: u32 = WM_APP + 3;
/// Posted to the launcher when fresh theme tokens arrive;
/// handled in app.rs by re-running its theme application.
/// Offset 9 continues the launcher window's own WM_APP block
/// (app.rs uses 2–8); 40 would collide with WM_TASKBAR_SHOW.
pub const WM_APP_THEME: u32 = WM_APP + 9;

static CONNECTED: AtomicBool = AtomicBool::new(false);
static LAUNCHER_HWND: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static OUTBOUND: OnceLock<Mutex<Option<std::sync::mpsc::Sender<String>>>> = OnceLock::new();
static TOKENS: Mutex<Option<TokenColors>> = Mutex::new(None);
static LAST_SNAPSHOT: Mutex<Option<serde_json::Value>> = Mutex::new(None);
/// The last reservation sent, for dedupe: (wanted, edge,
/// thickness in DIPs, target monitor).
type WorkAreaKey = (bool, crate::dock::logic::Edge, i32, Option<String>);
static LAST_WORK_AREA: Mutex<Option<WorkAreaKey>> = Mutex::new(None);
/// The edge the shell's config names (decision 13: the shell
/// owns it; Wisp's own config does not rule in standalone).
static EDGE_NAME: Mutex<String> = Mutex::new(String::new());
/// The monitor the shell names for the bar, when it names one
/// (`None` = primary).
static TARGET_MONITOR: Mutex<Option<String>> = Mutex::new(None);

pub fn connected() -> bool {
    CONNECTED.load(Ordering::SeqCst)
}

pub fn tokens() -> Option<TokenColors> {
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The dock edge the shell last published; Bottom until it says
/// otherwise (and in bridge mode, where edges are not applied).
pub fn edge() -> crate::dock::logic::Edge {
    use crate::dock::logic::Edge;
    match EDGE_NAME.lock().unwrap_or_else(|e| e.into_inner()).as_str() {
        "top" => Edge::Top,
        "left" => Edge::Left,
        "right" => Edge::Right,
        _ => Edge::Bottom,
    }
}

/// The monitor the shell named for the bar, by device name;
/// `None` when it said "primary" or has not said yet.
pub fn target_monitor() -> Option<String> {
    TARGET_MONITOR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn outbound() -> &'static Mutex<Option<std::sync::mpsc::Sender<String>>> {
    OUTBOUND.get_or_init(|| Mutex::new(None))
}

fn send_request(method: &str, params: serde_json::Value) {
    let slot = outbound().lock().unwrap_or_else(|e| e.into_inner());
    let Some(tx) = slot.as_ref() else {
        return;
    };
    let line = serde_json::json!({
        "request": {
            "id": NEXT_ID.fetch_add(1, Ordering::SeqCst),
            "request": { "method": method, "params": params },
        }
    })
    .to_string();
    let _ = tx.send(line);
}

/// Starts the client threads. Called once, from the launcher's
/// start, in standalone mode only.
pub fn start(launcher: HWND) {
    LAUNCHER_HWND.store(launcher.0 as usize, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("coquille-pipe".into())
        .spawn(move || {
            loop {
                match session() {
                    Ok(()) => eprintln!("coquille pipe: disconnected"),
                    Err(e) => eprintln!("coquille pipe: {e}"),
                }
                CONNECTED.store(false, Ordering::SeqCst);
                *outbound().lock().unwrap_or_else(|e| e.into_inner()) = None;
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        })
        .expect("spawn coquille pipe thread");
    // The tray host registers its Shared when the dock starts
    // hosting — which may be after the first snapshot arrives.
    // This housekeeper re-feeds the cached snapshot whenever a
    // *new* host registers, so ordering never loses icons.
    std::thread::Builder::new()
        .name("coquille-feed".into())
        .spawn(move || {
            let mut fed_to = 0usize;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let id = tray_host::remote_id();
                if id != 0 && id != fed_to {
                    if let Some(snapshot) = LAST_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                        apply_snapshot(&snapshot);
                    }
                    fed_to = id;
                }
                if id == 0 {
                    fed_to = 0;
                }
            }
        })
        .expect("spawn coquille feed thread");
}

fn session() -> std::io::Result<()> {
    let path = pipe_path();
    let file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
    let mut writer = file.try_clone()?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("coquille-pipe-writer".into())
        .spawn(move || {
            while let Ok(line) = rx.recv() {
                if writeln!(writer, "{line}").is_err() {
                    return;
                }
            }
        })
        .map_err(std::io::Error::other)?;
    *outbound().lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    send_request(
        "hello",
        serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "component": "wisp",
            "version": env!("CARGO_PKG_VERSION"),
        }),
    );
    send_request(
        "subscribe",
        serde_json::json!({ "topics": ["tray", "theme", "launcher"] }),
    );
    CONNECTED.store(true, Ordering::SeqCst);
    eprintln!("[coquille] pipe session up on {path} (hello + subscribe sent)");

    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        handle_line(&line);
    }
    eprintln!("[coquille] pipe session ended");
    Ok(())
}

fn handle_line(line: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let Some(event) = value.get("event") else {
        return;
    };
    let topic = event.get("topic").and_then(|t| t.as_str()).unwrap_or_default();
    let payload = event.get("payload").cloned().unwrap_or(serde_json::Value::Null);
    match topic {
        "tray" => {
            let icons = payload
                .get("icons")
                .and_then(|i| i.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            eprintln!("[coquille] tray event: {icons} icon(s)");
            *LAST_SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(payload.clone());
            apply_snapshot(&payload);
        }
        "theme" => {
            // Placement rides the theme event (it is shell
            // policy, republished with the pack): store it even
            // when the token parse is what usually drives this.
            let edge_name = payload
                .get("edge")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let monitor = payload
                .get("taskbar_monitor")
                .and_then(|v| v.as_str())
                .filter(|name| !name.is_empty() && *name != "primary")
                .map(str::to_string);
            let placement_changed = {
                let mut edge_slot = EDGE_NAME.lock().unwrap_or_else(|e| e.into_inner());
                let mut monitor_slot = TARGET_MONITOR.lock().unwrap_or_else(|e| e.into_inner());
                let changed = *edge_slot != edge_name || *monitor_slot != monitor;
                *edge_slot = edge_name;
                *monitor_slot = monitor;
                changed
            };
            eprintln!(
                "[coquille] theme event: edge={:?} monitor={:?} tokens={}",
                edge(),
                target_monitor(),
                payload.get("accent").is_some()
            );
            if let Some(tokens) = parse_tokens(&payload) {
                *TOKENS.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokens);
                post_launcher(WM_APP_THEME);
            } else if placement_changed {
                post_launcher(WM_APP_THEME);
            }
        }
        "launcher" if is_show_launcher(topic, &payload) => {
            // The host's chrome prompt was clicked: open the
            // launcher, by the same message the launcher's own
            // hotkey path ends at. Narrated because this is the
            // only evidence the request crossed the pipe.
            eprintln!("[coquille] launcher event: show_launcher — opening the launcher");
            // Label the open bus-raised, so the launcher's focus
            // narration can tell it from a hotkey open: this route
            // carries no input event, and its focus is the kind
            // that can silently fail.
            crate::app::BUS_SHOW.store(true, Ordering::SeqCst);
            post_launcher(WM_APP_SHOW);
        }
        _ => {}
    }
}

/// The show-launcher contract on the bus: topic `launcher`,
/// payload kind `show_launcher` (the shape the shell's own
/// native bar consumes from the same topic). Kept as a pure
/// predicate so the contract is testable without a pipe.
fn is_show_launcher(topic: &str, payload: &serde_json::Value) -> bool {
    topic == "launcher" && payload.get("kind").and_then(|k| k.as_str()) == Some("show_launcher")
}

fn post_launcher(msg: u32) {
    let hwnd = HWND(LAUNCHER_HWND.load(Ordering::SeqCst) as _);
    if !hwnd.is_invalid() {
        unsafe {
            let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Parses the hyphenated GUID form the broker publishes
/// (`{g:?}` on its side). windows-core's GUID has no FromStr,
/// so the four fields are split and parsed directly.
fn parse_guid(text: &str) -> Option<GUID> {
    let text = text.trim_matches(|c| c == '{' || c == '}');
    let mut parts = text.split('-');
    let data1 = u32::from_str_radix(parts.next()?, 16).ok()?;
    let data2 = u16::from_str_radix(parts.next()?, 16).ok()?;
    let data3 = u16::from_str_radix(parts.next()?, 16).ok()?;
    let rest: String = parts.collect::<Vec<_>>().concat();
    if rest.len() != 16 {
        return None;
    }
    let mut data4 = [0u8; 8];
    for (i, slot) in data4.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&rest[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(GUID {
        data1,
        data2,
        data3,
        data4,
    })
}

fn parse_tokens(payload: &serde_json::Value) -> Option<TokenColors> {
    let tokens = payload.get("tokens")?;
    let get = |key: &str| tokens.get(key).and_then(|v| v.as_str()).map(str::to_string);
    Some(TokenColors {
        surface: get("surface")?,
        raised: get("raised")?,
        foreground: get("foreground")?,
        muted: get("muted")?,
        accent: get("accent")?,
    })
}

// ---------- tray snapshot → model ----------

#[derive(Default)]
struct Producer {
    known: HashMap<String, KnownIcon>,
    /// Shell icons borrowed from owner executables, per exe
    /// path: snapshots repeat on every change, and asking the
    /// shell for the same file's icon each time would churn.
    exe_icons: HashMap<String, Option<Arc<OwnedIcon>>>,
    /// Letter placeholders, per label character.
    placeholders: HashMap<char, Option<Arc<OwnedIcon>>>,
    /// Icon keys already warned about, so a fallback logs once
    /// per icon, not once per snapshot.
    warned: std::collections::HashSet<String>,
}

struct KnownIcon {
    version: u32,
    /// What the model currently shows for this icon, in a key
    /// space shared by all three picture sources (broker
    /// pixels, owner exe, placeholder — the high bits say
    /// which): a change is a NIF_ICON modify.
    image_key: u64,
}

/// Where a snapshot icon's picture came from. The broker's own
/// pixels are the design; the other two keep the icon visible
/// when the broker has none, because the model's visibility
/// rule (an icon with no image has nothing to show) would
/// otherwise drop it silently — the shell's native bar applies
/// the same fallback rule to the same snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconSource {
    Pixels,
    Exe,
    Placeholder,
    None,
}

static PRODUCER: Mutex<Option<Producer>> = Mutex::new(None);

fn hash_bytes(data: &[u8]) -> u64 {
    // FNV-1a: change detection only, never security.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut table = [255u8; 256];
    for (i, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[c as usize] = i as u8;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in text.as_bytes() {
        if c == b'=' {
            break;
        }
        let v = table[c as usize];
        if v == 255 {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Builds a real HICON from the broker's premultiplied BGRA, so
/// the dock's existing rendering (which draws HICONs) consumes
/// remote icons exactly like local ones.
fn icon_from_bgra(width: u32, height: u32, bgra: &[u8]) -> Option<Arc<OwnedIcon>> {
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return None;
    }
    unsafe {
        let color = CreateBitmap(
            width as i32,
            height as i32,
            1,
            32,
            Some(bgra.as_ptr() as *const std::ffi::c_void),
        );
        if color.is_invalid() {
            return None;
        }
        let mask = CreateBitmap(width as i32, height as i32, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon.map(|h| Arc::new(OwnedIcon(h)))
    }
}

impl Producer {
    /// Picks the picture a snapshot entry will show with, best
    /// first: the broker's pixels; the owner executable's own
    /// shell icon; a letter placeholder. An icon the shell
    /// knows about must never reach the model imageless — the
    /// model's visibility rule drops those without a trace.
    /// The returned key changes exactly when the picture does.
    fn resolve_icon(
        &mut self,
        entry: &serde_json::Value,
        exes: &[String],
        tip: &str,
    ) -> (Option<Arc<OwnedIcon>>, u64, IconSource) {
        if let Some(img) = entry.get("image").filter(|img| !img.is_null()) {
            let w = img.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let h = img.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let decoded = img
                .get("bgra_b64")
                .and_then(|v| v.as_str())
                .and_then(base64_decode)
                .unwrap_or_default();
            if let Some(icon) = icon_from_bgra(w, h, &decoded) {
                // Pixel keys keep the two high bits clear, so the
                // fallback key spaces below can never collide
                // with a real image hash.
                return (Some(icon), hash_bytes(&decoded) & !(3 << 62), IconSource::Pixels);
            }
        }
        for exe in exes {
            let icon = self
                .exe_icons
                .entry(exe.clone())
                .or_insert_with(|| exe_icon(exe))
                .clone();
            if let Some(icon) = icon {
                return (
                    Some(icon),
                    (hash_bytes(exe.as_bytes()) & !(3 << 62)) | (1 << 63),
                    IconSource::Exe,
                );
            }
        }
        let label = placeholder_label(tip, exes);
        let icon = self
            .placeholders
            .entry(label)
            .or_insert_with(|| placeholder_icon(label))
            .clone();
        match icon {
            Some(icon) => (
                Some(icon),
                (hash_bytes(label.encode_utf8(&mut [0u8; 4]).as_bytes()) & !(3 << 62)) | (3 << 62),
                IconSource::Placeholder,
            ),
            None => (None, 0, IconSource::None),
        }
    }
}

/// The icon Windows itself shows for a file: the fallback for
/// a tray icon whose snapshot carries no pixels. `None` for a
/// path the shell has no icon for (a stale exe path, say).
fn exe_icon(path: &str) -> Option<Arc<OwnedIcon>> {
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
    let path = path.trim().trim_matches('"');
    if path.is_empty() {
        return None;
    }
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut info = SHFILEINFOW::default();
        let result = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(0),
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        (result != 0 && !info.hIcon.is_invalid()).then(|| Arc::new(OwnedIcon(info.hIcon)))
    }
}

/// The label a placeholder tile carries: the tip's first
/// letter or digit, else the owner file's, else a question
/// mark — always *some* identifier the user can match to the
/// app, never a blank square.
fn placeholder_label(tip: &str, exes: &[String]) -> char {
    let from_tip = tip.chars().find(|c| c.is_alphanumeric());
    let from_exe = exes.first().and_then(|exe| {
        exe.rsplit(['\\', '/'])
            .next()
            .and_then(|stem| stem.chars().find(|c| c.is_alphanumeric()))
    });
    from_tip
        .or(from_exe)
        .map(|c| c.to_uppercase().next().unwrap_or(c))
        .unwrap_or('?')
}

/// A letter tile for a tray icon with neither broker pixels
/// nor an owner file to borrow an icon from, drawn once and
/// cached by the producer. The pixels are handed to
/// `icon_from_bgra` in the same top-down BGRA the broker's
/// snapshots use, so it lands like any other icon.
fn placeholder_icon(label: char) -> Option<Arc<OwnedIcon>> {
    const SIZE: i32 = 48;
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE, // Top-down rows, like the broker's BGRA.
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let previous = SelectObject(dc, bitmap.into());
        // A slate tile (COLORREF is 0x00BBGGRR) with the label
        // in near-white: neutral against any palette.
        let brush = CreateSolidBrush(COLORREF(0x0057_413A));
        let rect = RECT {
            left: 0,
            top: 0,
            right: SIZE,
            bottom: SIZE,
        };
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
        let font = CreateFontW(
            30,
            0,
            0,
            0,
            700,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(0),
            0,
            PCWSTR::null(),
        );
        let previous_font = SelectObject(dc, font.into());
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0x00F2_F2F2));
        let glyph: Vec<u16> = label.to_string().encode_utf16().collect();
        let mut extent = SIZE::default();
        if GetTextExtentPoint32W(dc, &glyph, &mut extent).as_bool() {
            let _ = TextOutW(dc, (SIZE - extent.cx) / 2, (SIZE - extent.cy) / 2, &glyph);
        }
        let _ = GdiFlush();
        let len = (SIZE * SIZE * 4) as usize;
        let mut bgra = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
        cut_tile_corners(&mut bgra, SIZE as usize);
        SelectObject(dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(dc, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        icon_from_bgra(SIZE as u32, SIZE as u32, &bgra)
    }
}

/// Clears a small radius at each corner of a square BGRA tile,
/// so the placeholder reads as an icon, not a sticker. The cut
/// is symmetric, so it is indifferent to row order.
fn cut_tile_corners(bgra: &mut [u8], size: usize) {
    let r = 10i32;
    let s = size as i32;
    for y in 0..s {
        for x in 0..s {
            let (cx, cy) = match (x < r, x >= s - r, y < r, y >= s - r) {
                (true, _, true, _) => (r - 1, r - 1),
                (_, true, true, _) => (s - r, r - 1),
                (true, _, _, true) => (r - 1, s - r),
                (_, true, _, true) => (s - r, s - r),
                _ => continue,
            };
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy > r * r {
                let at = ((y * s + x) * 4) as usize;
                bgra[at..at + 4].fill(0);
            }
        }
    }
}

/// Diffs one published snapshot against the producer's memory
/// and feeds the changes into the standalone tray model. The
/// broker's snapshot is authoritative: an icon missing from it
/// is deleted here, however it got in.
fn apply_snapshot(snapshot: &serde_json::Value) {
    if !tray_host::remote_active() {
        return;
    }
    let Some(icons) = snapshot.get("icons").and_then(|v| v.as_array()) else {
        return;
    };
    let mut slot = PRODUCER.lock().unwrap_or_else(|e| e.into_inner());
    let producer = slot.get_or_insert_with(Producer::default);
    let mut present: Vec<String> = Vec::new();
    for entry in icons {
        let key = entry
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if key.is_empty() {
            continue;
        }
        present.push(key.clone());
        let hwnd = entry.get("hwnd").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let uid = entry.get("uid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let guid = entry.get("guid").and_then(|v| v.as_str()).and_then(parse_guid);
        let callback = entry.get("callback").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let tip = entry
            .get("tip")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let hidden = entry.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false);
        let version = entry.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let pid = entry.get("pid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let exes: Vec<String> = entry
            .get("exes")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let (image, image_key, source) = producer.resolve_icon(entry, &exes, &tip);
        if source != IconSource::Pixels && producer.warned.insert(key.clone()) {
            eprintln!(
                "[coquille] tray icon {key}: no usable broker image; showing {}",
                match source {
                    IconSource::Exe => "the owner app's own icon",
                    IconSource::Placeholder => "a letter placeholder",
                    _ => "nothing — no fallback resolved",
                }
            );
        }
        let owner = Owner {
            pid,
            exes: exes.clone(),
        };
        let base = Notify {
            op: wire::NIM_MODIFY,
            hwnd,
            uid,
            flags: wire::NIF_MESSAGE | wire::NIF_TIP | wire::NIF_STATE,
            callback,
            hicon: 0,
            tip: Some(tip.clone()),
            state: if hidden { wire::NIS_HIDDEN } else { 0 },
            state_mask: wire::NIS_HIDDEN,
            version: 0,
            guid,
            exe: exes.first().cloned().unwrap_or_default(),
        };
        let known = producer.known.get(&key);
        match known {
            None => {
                let mut add = base.clone();
                add.op = wire::NIM_ADD;
                if image.is_some() {
                    add.flags |= wire::NIF_ICON;
                }
                if guid.is_some() {
                    add.flags |= wire::NIF_GUID;
                }
                tray_host::remote_apply(&add, image.clone().map(Some), owner.clone());
                if version > 0 {
                    let mut setv = base.clone();
                    setv.op = wire::NIM_SETVERSION;
                    setv.version = version;
                    tray_host::remote_apply(&setv, None, owner);
                }
            }
            Some(prev) => {
                let image_changed = prev.image_key != image_key;
                let mut modify = base.clone();
                if image_changed {
                    modify.flags |= wire::NIF_ICON;
                }
                if guid.is_some() {
                    modify.flags |= wire::NIF_GUID;
                }
                let image_param = if image_changed { image.clone().map(Some) } else { None };
                tray_host::remote_apply(&modify, image_param, owner.clone());
                if prev.version != version && version > 0 {
                    let mut setv = base;
                    setv.op = wire::NIM_SETVERSION;
                    setv.version = version;
                    tray_host::remote_apply(&setv, None, owner);
                }
            }
        }
        producer.known.insert(key, KnownIcon { version, image_key });
    }
    // Deletions: known keys the snapshot no longer carries.
    let gone: Vec<String> = producer
        .known
        .keys()
        .filter(|k| !present.contains(k))
        .cloned()
        .collect();
    for key in gone {
        producer.known.remove(&key);
        // The model keys deletions by (hwnd, uid) or GUID; the
        // snapshot key string encodes both forms, so reconstruct
        // from a delete against every known shape: try GUID first,
        // then the hwnd:uid split.
        if let Some(guid) = parse_guid(&key) {
            let delete = Notify {
                op: wire::NIM_DELETE,
                hwnd: 0,
                uid: 0,
                flags: wire::NIF_GUID,
                callback: 0,
                hicon: 0,
                tip: None,
                state: 0,
                state_mask: 0,
                version: 0,
                guid: Some(guid),
                exe: String::new(),
            };
            tray_host::remote_apply(&delete, None, Owner::default());
        } else if let Some((hwnd, uid)) = key.split_once(':') {
            let delete = Notify {
                op: wire::NIM_DELETE,
                hwnd: u32::from_str_radix(hwnd, 16).unwrap_or(0),
                uid: uid.parse().unwrap_or(0),
                flags: 0,
                callback: 0,
                hicon: 0,
                tip: None,
                state: 0,
                state_mask: 0,
                version: 0,
                guid: None,
                exe: String::new(),
            };
            tray_host::remote_apply(&delete, None, Owner::default());
        }
    }
}

// ---------- outbound requests ----------

/// A dock tray gesture in standalone mode: the broker owns the
/// owner windows, so the gesture goes back over the pipe for it
/// to deliver (its `invoke_tray` relay).
pub fn invoke_tray(
    icon: &tray_host::Icon,
    action: tray_host::Action,
    x: i32,
    y: i32,
) -> std::result::Result<(), String> {
    let action_name = match action {
        tray_host::Action::Click => "select",
        tray_host::Action::Menu => "context",
        tray_host::Action::DoubleClick => "double",
    };
    send_request(
        "invoke_tray",
        serde_json::json!({
            "key": icon.key().to_string(),
            "action": action_name,
            "x": x,
            "y": y,
        }),
    );
    Ok(())
}

/// Reserves (or releases) the dock's strip on the monitor the
/// shell named for the bar (the primary by default). In
/// standalone mode the reservation is the shell's to apply —
/// Wisp sends corners, the host performs the call. The strip is
/// the edge's: bottom/top shave the height, left/right the
/// width. `thickness_dip` is in DIPs and converted here, where
/// the target monitor's DPI is known — callers further up only
/// have the dock's.
pub fn set_work_area(wanted: bool, thickness_dip: i32) {
    let edge = edge();
    let monitor_name = target_monitor();
    {
        let mut last = LAST_WORK_AREA.lock().unwrap_or_else(|e| e.into_inner());
        let key = (wanted, edge, thickness_dip, monitor_name.clone());
        if *last == Some(key.clone()) {
            return;
        }
        *last = Some(key);
    }
    let (left, top, right, bottom, dpi) = monitor_rect_and_dpi(monitor_name.as_deref());
    let thickness = (thickness_dip.max(0) as f32 * dpi as f32 / 96.0).round() as i32;
    let (left, top, right, bottom) = if !wanted {
        (left, top, right, bottom)
    } else {
        use crate::dock::logic::Edge;
        match edge {
            Edge::Bottom => (left, top, right, bottom - thickness),
            Edge::Top => (left, top + thickness, right, bottom),
            Edge::Left => (left + thickness, top, right, bottom),
            Edge::Right => (left, top, right - thickness, bottom),
        }
    };
    send_request(
        "set_work_area",
        serde_json::json!({
            "monitor": monitor_name.as_deref().unwrap_or("primary"),
            "left": left,
            "top": top,
            "right": right,
            "bottom": bottom,
            "reserved": wanted,
        }),
    );
}

/// The named monitor's rect and DPI, or the primary's when the
/// name is absent or not found.
fn monitor_rect_and_dpi(name: Option<&str>) -> (i32, i32, i32, i32, u32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let named = name.and_then(crate::dock::monitor_by_name);
        let monitor = named.unwrap_or_else(|| MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY));
        let mut dpi = 96u32;
        let mut dpi_y = 0u32;
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcMonitor;
            return (r.left, r.top, r.right, r.bottom, dpi.max(96));
        }
        (0, 0, 1920, 1080, 96)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_launcher_matches_the_bus_contract() {
        let payload = serde_json::json!({ "kind": "show_launcher" });
        assert!(is_show_launcher("launcher", &payload));
        // Another kind on the launcher topic, and the right
        // kind on another topic, are not the request.
        assert!(!is_show_launcher(
            "launcher",
            &serde_json::json!({ "kind": "hide_launcher" })
        ));
        assert!(!is_show_launcher("launcher", &serde_json::json!({})));
        assert!(!is_show_launcher("tray", &payload));
    }

    #[test]
    fn placeholder_labels_come_from_tip_then_exe_then_mark() {
        assert_eq!(placeholder_label("Tailscale", &[]), 'T');
        assert_eq!(placeholder_label("  onedrive", &[]), 'O');
        assert_eq!(placeholder_label("", &[r"C:\Apps\slack.exe".into()]), 'S');
        assert_eq!(placeholder_label("—", &["/opt/1password".into()]), '1');
        assert_eq!(placeholder_label("", &[]), '?');
        assert_eq!(placeholder_label("", &["".into()]), '?');
    }

    #[test]
    fn tile_corners_cut_only_the_corners() {
        let size = 48usize;
        let mut bgra = vec![255u8; size * size * 4];
        cut_tile_corners(&mut bgra, size);
        let alpha = |x: usize, y: usize| bgra[(y * size + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert_eq!(alpha(size - 1, 0), 0);
        assert_eq!(alpha(0, size - 1), 0);
        assert_eq!(alpha(size - 1, size - 1), 0);
        // The middle of each edge and the centre stay opaque.
        assert_eq!(alpha(size / 2, 0), 255);
        assert_eq!(alpha(0, size / 2), 255);
        assert_eq!(alpha(size / 2, size / 2), 255);
    }

    #[test]
    fn fallback_key_spaces_never_collide_with_pixel_hashes() {
        // Pixel keys mask the two high bits; exe keys set one,
        // placeholder keys set both. A broker image arriving for
        // an icon that showed a fallback must always read as a
        // change, whichever hash its bytes produce.
        for bytes in [&b"tailscale"[..], b"onedrive", b"", b"\0\0\0\0"] {
            let pixel_key = hash_bytes(bytes) & !(3 << 62);
            assert_eq!(pixel_key >> 62, 0);
        }
        assert_eq!(((hash_bytes(b"exe") & !(3 << 62)) | (1 << 63)) >> 62, 2);
        assert_eq!(((hash_bytes(b"L") & !(3 << 62)) | (3 << 62)) >> 62, 3);
    }
}
