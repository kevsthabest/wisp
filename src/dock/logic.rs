//! Matching and DIP geometry.

use crate::config::WindowListMode;
use crate::indexer::AppEntry;

pub const DEFAULT_APP_ICON_SIZE: f32 = 32.0;
pub const MIN_APP_ICON_SIZE: f32 = 24.0;
pub const MAX_APP_ICON_SIZE: f32 = 40.0;
pub const TILE_TOP: f32 = 5.0;
pub const TILE_BOTTOM: f32 = 51.0;
pub const TILE_RADIUS: f32 = 8.0;
pub const ICON_CENTER_Y: f32 = 26.0;
pub const INDICATOR_TOP: f32 = 46.0;

pub fn slide_value(from: f32, to: f32, progress: f32) -> f32 {
    let t = progress.clamp(0.0, 1.0);
    let ease = t * t * (3.0 - 2.0 * t);
    from + (to - from) * ease
}

pub fn icon_pixels(dpi: u32, size: f32) -> i32 {
    (size * dpi as f32 / 96.0).round() as i32
}

pub const HEIGHT: f32 = 56.0;
pub const MARGIN: f32 = 9.0;
pub const SLOT: f32 = 48.0;
pub const PAD: f32 = 8.0;
pub const SEPARATOR: f32 = 9.0;

pub fn layout_scale(app_icon_size: f32) -> f32 {
    app_icon_size / DEFAULT_APP_ICON_SIZE
}

pub fn height(scale: f32) -> f32 {
    HEIGHT * scale
}
pub fn margin(scale: f32) -> f32 {
    MARGIN * scale
}
pub fn slot(scale: f32) -> f32 {
    SLOT * scale
}
pub fn pad(scale: f32) -> f32 {
    PAD * scale
}
pub fn separator_width(scale: f32) -> f32 {
    SEPARATOR * scale
}

pub fn width(running: usize) -> f32 {
    2.0 * PAD + (1 + running) as f32 * SLOT
}

pub fn width_scaled(running: usize, scale: f32) -> f32 {
    width(running) * scale
}

pub fn left(index: usize) -> f32 {
    PAD + index as f32 * SLOT
}

pub fn left_scaled(index: usize, scale: f32) -> f32 {
    left(index) * scale
}

pub fn hit(x: f32, y: f32, running: usize) -> Option<usize> {
    if !(0.0..HEIGHT).contains(&y) {
        return None;
    }
    (0..1 + running).find(|&i| (left(i)..left(i) + SLOT).contains(&x))
}

pub fn hit_scaled(x: f32, y: f32, running: usize, scale: f32) -> Option<usize> {
    hit(x / scale, y / scale, running)
}

pub const CHEVRON_W: f32 = 28.0;
pub const TRAY_W: f32 = 32.0;
pub const VOLUME_W: f32 = 46.0;
pub const BATTERY_W: f32 = 50.0;
pub const CLOCK_W: f32 = 88.0;
/// The window list above an app icon, in DIPs: the popup's padding, and the
/// preview card each window gets. A card is a live preview over the window's
/// title; the preview area keeps a 16:9 shape and DWM letterboxes the actual
/// window inside it, whatever shape the window is.
pub const LIST_PAD: (f32, f32) = (7.0, 3.0);
pub const LIST_THUMB_W: f32 = 216.0;
pub const LIST_THUMB_H: f32 = 122.0;
/// Inset between a card's edge and its preview and title.
pub const LIST_CARD_IN: f32 = 6.0;
/// Space between a card's preview and its title.
pub const LIST_CARD_GAP: f32 = 4.0;
/// The title line under a card's preview.
pub const LIST_TITLE_H: f32 = 18.0;
/// One window's whole card: inset, preview, gap, title, inset.
pub const LIST_CARD_H: f32 = LIST_CARD_IN + LIST_THUMB_H + LIST_CARD_GAP + LIST_TITLE_H + LIST_CARD_IN;

/// The popup's width: one preview, its card insets, and the popup padding.
pub fn list_width() -> f32 {
    LIST_THUMB_W + 2.0 * LIST_CARD_IN + 2.0 * LIST_PAD.0
}

/// The top of a window's card, in popup DIPs.
pub fn list_card_top(index: usize) -> f32 {
    LIST_PAD.1 + index as f32 * LIST_CARD_H
}

/// A window's preview area, in popup DIPs: (left, top, right, bottom).
/// DWM draws the live window into this rect, so the renderer, the hit test
/// and the thumbnail placement all take it from here and cannot drift apart.
pub fn list_thumb_rect(index: usize) -> (f32, f32, f32, f32) {
    let left = LIST_PAD.0 + LIST_CARD_IN;
    let top = list_card_top(index) + LIST_CARD_IN;
    (left, top, left + LIST_THUMB_W, top + LIST_THUMB_H)
}

/// The card under a popup y coordinate, in DIPs, among `count` cards.
pub fn list_row_at(y: f32, count: usize) -> Option<usize> {
    let row = ((y - LIST_PAD.1) / LIST_CARD_H).floor();
    (row >= 0.0 && (row as usize) < count).then_some(row as usize)
}

/// Whole rows of `row_height` that fit in a physical-pixel height,
/// including both DIP paddings.
pub fn window_list_rows(available: i32, scale: f32, padding: f32, row_height: f32) -> usize {
    let available = available.max(0) as f32;
    let rows = ((available / scale - 2.0 * padding).max(0.0) / row_height).floor() as usize;
    // Match the popup's rounding to whole physical pixels at fractional scales.
    if rows > 0 && ((rows as f32 * row_height + 2.0 * padding) * scale).ceil() > available {
        rows - 1
    } else {
        rows
    }
}

pub fn show_window_list(mode: WindowListMode, windows: usize) -> bool {
    windows > 0 && (mode == WindowListMode::Always || mode == WindowListMode::Multiple && windows > 1)
}
/// Notification-area icons shown in the dock; the rest are behind the chevron.
pub const MAX_INLINE: usize = 5;
/// Tray icons share the dock's enlarged visual rhythm.
pub const TRAY_ICON: f32 = 18.0;

/// The status parts after the apps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Opens the notification-area flyout.
    Chevron,
    /// The nth notification-area icon shown in the dock.
    Tray(usize),
    Volume,
    Battery,
    Clock,
}

impl Part {
    pub fn width(self) -> f32 {
        match self {
            Part::Chevron => CHEVRON_W,
            Part::Tray(_) => TRAY_W,
            Part::Volume => VOLUME_W,
            Part::Battery => BATTERY_W,
            Part::Clock => CLOCK_W,
        }
    }

    pub fn width_scaled(self, scale: f32) -> f32 {
        self.width() * scale
    }
}

/// The parts this PC has: `tray` is the number of inline notification icons
/// while the dock hosts the notification area, volume needs an output
/// device, battery a battery.
pub fn parts(tray: Option<usize>, volume: bool, battery: bool) -> Vec<Part> {
    let tray = tray.map(|count| std::iter::once(Part::Chevron).chain((0..count.min(MAX_INLINE)).map(Part::Tray)));
    tray.into_iter()
        .flatten()
        .chain(
            [(Part::Volume, volume), (Part::Battery, battery), (Part::Clock, true)]
                .into_iter()
                .filter_map(|(part, shown)| shown.then_some(part)),
        )
        .collect()
}

/// The notification-area flyout: a grid of cells, as in Windows 11.
pub const FLYOUT_CELL: f32 = 40.0;
pub const FLYOUT_PAD: f32 = 8.0;
pub const FLYOUT_COLUMNS: usize = 5;
/// Space between the flyout and the dock.
pub const FLYOUT_GAP: f32 = 8.0;

/// The flyout's size in DIPs for `count` icons; an empty one keeps one cell.
pub fn flyout_size(count: usize) -> (f32, f32) {
    let columns = count.clamp(1, FLYOUT_COLUMNS);
    let rows = count.max(1).div_ceil(FLYOUT_COLUMNS);
    (
        2.0 * FLYOUT_PAD + columns as f32 * FLYOUT_CELL,
        2.0 * FLYOUT_PAD + rows as f32 * FLYOUT_CELL,
    )
}

/// The cell's top-left corner.
pub fn flyout_cell(index: usize) -> (f32, f32) {
    (
        FLYOUT_PAD + (index % FLYOUT_COLUMNS) as f32 * FLYOUT_CELL,
        FLYOUT_PAD + (index / FLYOUT_COLUMNS) as f32 * FLYOUT_CELL,
    )
}

pub fn flyout_hit(x: f32, y: f32, count: usize) -> Option<usize> {
    if x < FLYOUT_PAD || y < FLYOUT_PAD {
        return None;
    }
    let column = ((x - FLYOUT_PAD) / FLYOUT_CELL) as usize;
    let index = ((y - FLYOUT_PAD) / FLYOUT_CELL) as usize * FLYOUT_COLUMNS + column;
    (column < FLYOUT_COLUMNS && index < count).then_some(index)
}

/// The status parts follow the apps, after a separator.
pub fn status_width(parts: &[Part]) -> f32 {
    SEPARATOR + parts.iter().map(|part| part.width()).sum::<f32>()
}

pub fn status_width_scaled(parts: &[Part], scale: f32) -> f32 {
    status_width(parts) * scale
}

pub fn total_width(running: usize, parts: &[Part]) -> f32 {
    width(running) + status_width(parts)
}

pub fn total_width_scaled(running: usize, parts: &[Part], scale: f32) -> f32 {
    total_width(running, parts) * scale
}

/// Left edge of `part`, or `None` for a part the PC lacks.
pub fn part_left(part: Part, running: usize, parts: &[Part]) -> Option<f32> {
    let index = parts.iter().position(|&p| p == part)?;
    let start = width(running) - PAD + SEPARATOR;
    Some(start + parts[..index].iter().map(|part| part.width()).sum::<f32>())
}

pub fn part_left_scaled(part: Part, running: usize, parts: &[Part], scale: f32) -> Option<f32> {
    part_left(part, running, parts).map(|left| left * scale)
}

pub fn part_hit(x: f32, y: f32, running: usize, parts: &[Part]) -> Option<Part> {
    if !(0.0..HEIGHT).contains(&y) {
        return None;
    }
    parts
        .iter()
        .copied()
        .find(|&part| part_left(part, running, parts).is_some_and(|left| (left..left + part.width()).contains(&x)))
}

pub fn part_hit_scaled(x: f32, y: f32, running: usize, parts: &[Part], scale: f32) -> Option<Part> {
    part_hit(x / scale, y / scale, running, parts)
}

/// Wheel delta per volume percent, for 2% per 120-delta notch.
pub const WHEEL_PER_PERCENT: i32 = 60;

/// Spends whole percents of the accumulated wheel `delta` on `percent`, and
/// returns the new volume with the leftover delta, for precision touchpads.
pub fn wheel_volume(percent: u32, delta: i32) -> (u32, i32) {
    let steps = delta / WHEEL_PER_PERCENT;
    ((percent as i32 + steps).clamp(0, 100) as u32, delta % WHEEL_PER_PERCENT)
}

/// Wheel travel that switches one desktop: a mouse wheel's notch.
pub const DESKTOP_NOTCH: i32 = 120;

/// Adds wheel `delta` to the `travel` so far, negative toward the right.
/// Returns whether to switch right or left once a notch is complete, and the
/// travel left over. Turning the wheel back starts over.
pub fn wheel_desktop(travel: i32, delta: i32) -> (Option<bool>, i32) {
    let travel = if travel != 0 && (travel < 0) != (delta < 0) {
        delta
    } else {
        travel + delta
    };
    if travel.abs() >= DESKTOP_NOTCH {
        (Some(travel < 0), 0)
    } else {
        (None, travel)
    }
}

fn normalize(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .replace('/', "\\")
        .trim_start_matches(r"\\?\")
        .to_lowercase()
}

/// Paths must match completely: unrelated programs often share a filename.
pub fn match_app<'a>(apps: &'a [AppEntry], exe: &str, aumid: Option<&str>) -> Option<&'a AppEntry> {
    if let Some(id) = aumid.filter(|s| !s.is_empty())
        && let Some(app) = apps.iter().find(|app| app.id.eq_ignore_ascii_case(id))
    {
        return Some(app);
    }
    if exe.is_empty()
        || exe
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case("ApplicationFrameHost.exe"))
    {
        return None;
    }
    let exe = normalize(exe);
    apps.iter()
        .find(|app| app.path.as_deref().is_some_and(|path| normalize(path) == exe) || normalize(&app.id) == exe)
}

/// Keeps items in the order they first appeared. The window list arrives in
/// z-order, which changes whenever an app is focused; following it directly
/// made a clicked icon jump to another place in the dock.
pub fn stable_order<T>(previous: Vec<T>, current: Vec<T>, same: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut current = current;
    let mut ordered = Vec::with_capacity(current.len());
    for old in previous {
        if let Some(at) = current.iter().position(|new| same(&old, new)) {
            ordered.push(current.remove(at));
        }
    }
    ordered.append(&mut current);
    ordered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_list_fits_whole_rows_with_padding_at_each_scale() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for padding in [0.0, LIST_PAD.1, 8.0] {
                assert_eq!(window_list_rows(-10, scale, padding, LIST_CARD_H), 0);
                assert_eq!(window_list_rows(0, scale, padding, LIST_CARD_H), 0);
                let padding_height = (2.0 * padding * scale).ceil() as i32;
                assert_eq!(window_list_rows(padding_height, scale, padding, LIST_CARD_H), 0);
                for rows in [1, 2, 10, 100] {
                    let height = ((rows as f32 * LIST_CARD_H + 2.0 * padding) * scale).ceil() as i32;
                    assert_eq!(window_list_rows(height, scale, padding, LIST_CARD_H), rows);
                    assert_eq!(window_list_rows(height - 1, scale, padding, LIST_CARD_H), rows - 1);
                    assert_eq!(window_list_rows(height + 1, scale, padding, LIST_CARD_H), rows);
                }
            }
        }
    }

    #[test]
    fn window_list_cards_stack_from_the_top_padding() {
        assert_eq!(list_card_top(0), LIST_PAD.1);
        assert_eq!(list_card_top(2), LIST_PAD.1 + 2.0 * LIST_CARD_H);
        let (left, top, right, bottom) = list_thumb_rect(1);
        assert_eq!(left, LIST_PAD.0 + LIST_CARD_IN);
        assert_eq!(top, list_card_top(1) + LIST_CARD_IN);
        assert_eq!(right - left, LIST_THUMB_W);
        assert_eq!(bottom - top, LIST_THUMB_H);
        // The title fits between a card's preview and the next card.
        assert!(bottom + LIST_CARD_GAP + LIST_TITLE_H <= list_card_top(2));
        // The popup is exactly as wide as a card plus its padding.
        assert_eq!(list_width(), right + LIST_CARD_IN + LIST_PAD.0);
    }

    #[test]
    fn window_list_hit_follows_the_cards() {
        assert_eq!(list_row_at(LIST_PAD.1 - 0.5, 3), None);
        assert_eq!(list_row_at(LIST_PAD.1, 3), Some(0));
        assert_eq!(list_row_at(LIST_PAD.1 + LIST_CARD_H - 0.5, 3), Some(0));
        assert_eq!(list_row_at(LIST_PAD.1 + LIST_CARD_H, 3), Some(1));
        assert_eq!(list_row_at(LIST_PAD.1 + 3.0 * LIST_CARD_H, 3), None);
        assert_eq!(list_row_at(LIST_PAD.1, 0), None);
    }

    #[test]
    fn window_list_modes_follow_the_selected_policy() {
        assert!(show_window_list(WindowListMode::Always, 1));
        assert!(show_window_list(WindowListMode::Always, 3));
        assert!(!show_window_list(WindowListMode::Multiple, 1));
        assert!(show_window_list(WindowListMode::Multiple, 2));
        assert!(!show_window_list(WindowListMode::Off, 1));
        assert!(!show_window_list(WindowListMode::Off, 2));
        assert!(!show_window_list(WindowListMode::Always, 0));
    }

    #[test]
    fn app_icon_pixel_size_scales_with_dpi() {
        assert_eq!(icon_pixels(96, 24.0), 24);
        assert_eq!(icon_pixels(144, DEFAULT_APP_ICON_SIZE), 48);
        assert_eq!(icon_pixels(192, MAX_APP_ICON_SIZE), 80);
    }
    #[test]
    fn dock_geometry_scales_with_app_icon_size() {
        let small = layout_scale(MIN_APP_ICON_SIZE);
        let normal = layout_scale(DEFAULT_APP_ICON_SIZE);
        let large = layout_scale(MAX_APP_ICON_SIZE);
        assert_eq!((small, normal, large), (0.75, 1.0, 1.25));
        assert_eq!(height(small), 42.0);
        assert_eq!(slot(small), 36.0);
        assert_eq!(height(large), 70.0);
        assert_eq!(slot(large), 60.0);

        for scale in [small, normal, large] {
            let parts = parts(None, true, true);
            assert_eq!(total_width_scaled(2, &parts, scale), total_width(2, &parts) * scale);
            let x = left_scaled(1, scale) + slot(scale) / 2.0;
            assert_eq!(hit_scaled(x, height(scale) / 2.0, 2, scale), Some(1));
        }
    }

    #[test]
    fn scaled_slot_edges_match_hit_targets() {
        for size in [24.0, 28.0, 32.0, 36.0, 40.0] {
            let scale = layout_scale(size);
            for index in 0..=8 {
                let left = left_scaled(index, scale);
                let right = left + slot(scale);
                let bottom = height(scale);
                assert_eq!(left, (8.0 + index as f32 * 48.0) * size / 32.0);
                assert_eq!(right, left_scaled(index + 1, scale));
                assert_eq!(bottom, 56.0 * size / 32.0);
                assert_eq!(hit_scaled(left, 0.0, 8, scale), Some(index));
                assert_eq!(hit_scaled(right - 0.5, bottom - 0.5, 8, scale), Some(index));
                assert_eq!(hit_scaled(left, bottom, 8, scale), None);
                assert_ne!(hit_scaled(right, 0.0, 8, scale), Some(index));
            }
        }
    }

    #[test]
    fn slide_endpoints_and_reversal_are_continuous() {
        assert_eq!(slide_value(1.0, 0.0, 0.0), 1.0);
        assert_eq!(slide_value(1.0, 0.0, 1.0), 0.0);
        let midway = slide_value(0.0, 1.0, 0.4);
        assert_eq!(slide_value(midway, 0.0, 0.0), midway);
        assert!(slide_value(midway, 0.0, 0.1) < midway);
        assert_eq!(slide_value(0.0, 1.0, 2.0), 1.0);
    }
    #[test]
    fn stable_order_keeps_first_seen_positions() {
        let previous = vec!["a", "b", "c"];
        // Focusing c makes it first in z-order; it must stay third.
        let current = vec!["c", "a", "d", "b"];
        assert_eq!(stable_order(previous, current, |a, b| a == b), ["a", "b", "c", "d"]);
        // Closed apps drop out and leaving ones keep their relative order.
        assert_eq!(stable_order(vec!["a", "b"], vec!["b", "c"], |a, b| a == b), ["b", "c"]);
    }
    #[test]
    fn matches_full_paths_and_store_ids() {
        let apps = vec![
            AppEntry {
                id: "{folder}\\a.exe".into(),
                path: Some(r"C:\Apps\a.exe".into()),
                name: "A".into(),
            },
            AppEntry {
                id: "Package!App".into(),
                path: None,
                name: "Store".into(),
            },
        ];
        assert_eq!(match_app(&apps, r"\\?\C:\APPS\A.EXE", None).unwrap().name, "A");
        assert!(match_app(&apps, r"D:\Other\a.exe", None).is_none());
        assert_eq!(
            match_app(&apps, r"C:\Windows\ApplicationFrameHost.exe", Some("package!app"))
                .unwrap()
                .name,
            "Store"
        );
        assert!(match_app(&apps, "", None).is_none());
    }
    #[test]
    fn wheel_steps_volume_by_two_percent_within_range() {
        assert_eq!(wheel_volume(50, 120), (52, 0));
        assert_eq!(wheel_volume(50, -360), (44, 0));
        assert_eq!(wheel_volume(99, 120), (100, 0));
        assert_eq!(wheel_volume(1, -120), (0, 0));
        // Precision touchpads send fractions of a notch, which add up.
        assert_eq!(wheel_volume(50, 40), (50, 40));
        assert_eq!(wheel_volume(50, 40 + 40), (51, 20));
        assert_eq!(wheel_volume(50, -70), (49, -10));
    }
    #[test]
    fn wheel_switches_one_desktop_a_notch() {
        // Down and touchpad swipes right go to the desktop on the right.
        assert_eq!(wheel_desktop(0, -120), (Some(true), 0));
        assert_eq!(wheel_desktop(0, 120), (Some(false), 0));
        // A fast flick still switches only once.
        assert_eq!(wheel_desktop(0, -480), (Some(true), 0));
        // Precision touchpads send fractions of a notch, which add up.
        assert_eq!(wheel_desktop(0, -50), (None, -50));
        assert_eq!(wheel_desktop(-100, -50), (Some(true), 0));
        // Turning back starts over.
        assert_eq!(wheel_desktop(-100, 30), (None, 30));
    }
    #[test]
    fn floating_layout_and_hit_targets() {
        assert_eq!((HEIGHT, MARGIN, SLOT), (56.0, 9.0, 48.0));
        assert_eq!(width(2), 160.0);
        for count in [0, 1, 8, 30] {
            for index in 0..=count {
                assert_eq!(hit(left(index) + SLOT / 2.0, HEIGHT / 2.0, count), Some(index));
            }
            assert_eq!(hit(-1.0, 20.0, count), None);
            assert_eq!(hit(PAD, HEIGHT, count), None);
            let all = parts(None, true, true);
            let clock = part_left(Part::Clock, count, &all).unwrap();
            assert_eq!(clock + CLOCK_W + PAD, total_width(count, &all));
            let volume = part_left(Part::Volume, count, &all).unwrap();
            assert_eq!(part_hit(volume - 1.0, 20.0, count, &all), None);
            assert_eq!(part_hit(clock + 1.0, 20.0, count, &all), Some(Part::Clock));
            assert_eq!(part_hit(clock - 1.0, 20.0, count, &all), Some(Part::Battery));
            assert_eq!(part_hit(clock, HEIGHT, count, &all), None);
            assert_eq!(part_left(Part::Volume, count, &parts(None, false, false)), None);
        }
        for dpi in [96, 120, 144] {
            assert_eq!(
                icon_pixels(dpi, DEFAULT_APP_ICON_SIZE) as f32,
                DEFAULT_APP_ICON_SIZE * dpi as f32 / 96.0
            );
        }
        assert_eq!(((HEIGHT + MARGIN) * 120.0 / 96.0).round() as i32, 81);
    }
    #[test]
    fn tray_sits_between_apps_and_status() {
        let tray = parts(Some(2), true, true);
        assert_eq!(
            tray,
            [
                Part::Chevron,
                Part::Tray(0),
                Part::Tray(1),
                Part::Volume,
                Part::Battery,
                Part::Clock
            ]
        );
        // The chevron starts right after the apps' separator.
        let chevron = part_left(Part::Chevron, 2, &tray).unwrap();
        assert_eq!(chevron, width(2) - PAD + SEPARATOR);
        assert_eq!(part_left(Part::Tray(0), 2, &tray), Some(chevron + CHEVRON_W));
        let volume = part_left(Part::Volume, 2, &tray).unwrap();
        assert_eq!(volume, chevron + CHEVRON_W + 2.0 * TRAY_W);
        assert_eq!(part_hit(chevron + 1.0, 20.0, 2, &tray), Some(Part::Chevron));
        assert_eq!(part_hit(volume - 1.0, 20.0, 2, &tray), Some(Part::Tray(1)));
        assert_eq!(
            total_width(2, &tray),
            total_width(2, &parts(None, true, true)) + CHEVRON_W + 2.0 * TRAY_W
        );
        // Inline icons are capped; with none, only the chevron shows.
        assert_eq!(parts(Some(9), false, false).len(), 1 + MAX_INLINE + 1);
        assert_eq!(parts(Some(0), false, false), [Part::Chevron, Part::Clock]);
        // Tray icons fit the dock's height budget.
        const { assert!(TRAY_ICON < HEIGHT && TRAY_ICON < TRAY_W) };
    }
    #[test]
    fn flyout_grid_wraps_at_five() {
        assert_eq!(flyout_size(0), flyout_size(1));
        assert_eq!(
            flyout_size(3),
            (2.0 * FLYOUT_PAD + 3.0 * FLYOUT_CELL, 2.0 * FLYOUT_PAD + FLYOUT_CELL)
        );
        assert_eq!(
            flyout_size(6),
            (
                2.0 * FLYOUT_PAD + 5.0 * FLYOUT_CELL,
                2.0 * FLYOUT_PAD + 2.0 * FLYOUT_CELL
            )
        );
        assert_eq!(flyout_cell(6), (FLYOUT_PAD + FLYOUT_CELL, FLYOUT_PAD + FLYOUT_CELL));
        let (x, y) = flyout_cell(6);
        assert_eq!(flyout_hit(x + 1.0, y + 1.0, 7), Some(6));
        assert_eq!(flyout_hit(x + 1.0, y + 1.0, 6), None);
        assert_eq!(flyout_hit(FLYOUT_PAD - 1.0, 10.0, 3), None);
        assert_eq!(flyout_hit(FLYOUT_PAD + 5.0 * FLYOUT_CELL + 1.0, 10.0, 9), None);
    }
}
