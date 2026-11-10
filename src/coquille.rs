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

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO, PostMessageW, WM_APP};
use windows::core::GUID;

use crate::theme::TokenColors;
use crate::tray_host::model::Owner;
use crate::tray_host::wire::{self, Notify};
use crate::tray_host::{self, OwnedIcon};

const PIPE_PATH: &str = r"\\.\pipe\coquille-supervisor-v1";
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
    let file = std::fs::OpenOptions::new().read(true).write(true).open(PIPE_PATH)?;
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

    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        handle_line(&line);
    }
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
            if let Some(tokens) = parse_tokens(&payload) {
                *TOKENS.lock().unwrap_or_else(|e| e.into_inner()) = Some(tokens);
                post_launcher(WM_APP_THEME);
            } else if placement_changed {
                post_launcher(WM_APP_THEME);
            }
        }
        "launcher" if payload.get("kind").and_then(|k| k.as_str()) == Some("show_launcher") => {
            post_launcher(WM_APP_SHOW);
        }
        _ => {}
    }
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
}

struct KnownIcon {
    version: u32,
    image_hash: u64,
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
        let (image, image_hash) = match entry.get("image") {
            Some(img) if !img.is_null() => {
                let w = img.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let h = img.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let decoded = img
                    .get("bgra_b64")
                    .and_then(|v| v.as_str())
                    .and_then(base64_decode)
                    .unwrap_or_default();
                (icon_from_bgra(w, h, &decoded), hash_bytes(&decoded))
            }
            _ => (None, 0),
        };
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
                let image_changed = prev.image_hash != image_hash;
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
        producer.known.insert(key, KnownIcon { version, image_hash });
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
