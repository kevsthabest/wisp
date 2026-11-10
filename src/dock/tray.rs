//! The dock's side of the notification area: which icons show where, and
//! their pixels.

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DrawIconEx, GetIconInfo, HICON, ICONINFO};

use super::logic;
use crate::indexer::IconPixels;
use crate::tray_host::{Icon, Key};

/// An icon the dock shows, with its pixels at the dock's DPI.
pub struct Shown {
    pub icon: Icon,
    pub pixels: Option<Arc<IconPixels>>,
}

/// What a tooltip belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tip {
    Chevron,
    /// An icon in the dock, by its inline position.
    Inline(usize),
    /// An icon in the flyout, by its index in `shown`.
    Flyout(usize),
}

/// Explorer's own icons without a callback, such as its volume icon, belong
/// to the system indicators the dock already shows.
fn wanted(icon: &Icon) -> bool {
    icon.visible() && !(icon.callback == 0 && icon.exes.iter().any(|exe| exe.ends_with(r"\explorer.exe")))
}

/// Picks the shown icons and the promoted ones for the dock, reusing pixels
/// for icons that didn't change.
pub fn update(old: Vec<Shown>, icons: Vec<Icon>, size: i32) -> (Vec<Shown>, Vec<usize>) {
    let mut cache: HashMap<Key, (u64, Arc<IconPixels>)> = old
        .into_iter()
        .filter_map(|shown| Some((shown.icon.key(), (shown.icon.revision, shown.pixels?))))
        .collect();
    let shown: Vec<Shown> = icons
        .into_iter()
        .filter(wanted)
        .map(|icon| {
            let cached = cache
                .remove(&icon.key())
                .filter(|(revision, pixels)| *revision == icon.revision && pixels.width == size as u32);
            let pixels = cached.map(|(_, pixels)| pixels).or_else(|| {
                let image = icon.image.as_ref()?;
                icon_pixels(image.0, size).map(Arc::new)
            });
            Shown { icon, pixels }
        })
        .collect();
    let standalone = crate::backend::is_standalone();
    let inline = shown
        .iter()
        .enumerate()
        .filter(|(_, shown)| shown.pixels.is_some() && inline_eligible(shown.icon.promoted, standalone))
        .map(|(index, _)| index)
        .take(logic::MAX_INLINE)
        .collect();
    (shown, inline)
}

/// Whether a shown icon earns a place in the strip itself
/// rather than the chevron flyout. Bridge mode follows the
/// promotion preference Explorer recorded in
/// NotifyIconSettings: only a promoted icon is inline. The
/// standalone feed has no such record to read — the broker
/// never sends one — so every icon it shows is inline unless
/// something explicitly demoted it; the broker's own hidden
/// flag (already filtered out of `shown`) is the overflow
/// split there, the same rule the shell's native bar applies.
fn inline_eligible(promoted: Option<bool>, standalone: bool) -> bool {
    if standalone {
        promoted != Some(false)
    } else {
        promoted == Some(true)
    }
}

/// The largest size an icon is drawn at before resampling. Tray icons are
/// rarely over 64 px, and an app's oversized icon must not cost the dock
/// hundreds of megabytes.
const MAX_NATIVE: i32 = 256;

/// The physical pixel size of a tray icon at `dpi`.
pub fn icon_size(dpi: u32) -> i32 {
    (logic::TRAY_ICON * dpi as f32 / 96.0).round() as i32
}

/// Renders an icon onto black and onto white: their difference is its
/// coverage, so alpha icons and old masked icons both come out right.
/// The result is premultiplied BGRA.
///
/// The icon is drawn at the size its app gave it and only then resampled,
/// because `DrawIconEx` scales with nearest-neighbor and makes tray icons
/// look pixelated.
pub fn icon_pixels(icon: HICON, size: i32) -> Option<IconPixels> {
    let native = native_size(icon)
        .filter(|&native| native > 0)
        .map_or(size, |native| native.min(MAX_NATIVE.max(size)));
    let black = draw_on(icon, native, 0x00)?;
    let white = draw_on(icon, native, 0xFF)?;
    let mut bgra = vec![0u8; black.len()];
    let (out, _) = bgra.as_chunks_mut::<4>();
    let (black, _) = black.as_chunks::<4>();
    let (white, _) = white.as_chunks::<4>();
    for ((out, b), w) in out.iter_mut().zip(black).zip(white) {
        // On black a pixel is its premultiplied color; white adds 1 - alpha.
        let alpha = 255 - (0..3).map(|c| w[c].saturating_sub(b[c])).max().unwrap_or(0);
        for c in 0..3 {
            out[c] = b[c].min(alpha);
        }
        out[3] = alpha;
    }
    if out.iter().all(|p| p[3] == 0) {
        return None;
    }
    let source = IconPixels {
        width: native as u32,
        height: native as u32,
        bgra,
    };
    Some(if native == size {
        source
    } else {
        resample(&source, size.max(1) as u32)
    })
}

/// The pixel size of the icon's own bitmap, before any drawing scales it.
fn native_size(icon: HICON) -> Option<i32> {
    unsafe {
        let mut info = ICONINFO::default();
        if GetIconInfo(icon, &mut info).is_err() {
            return None;
        }
        // A monochrome icon has only a mask, twice as tall as the icon.
        let monochrome = info.hbmColor.is_invalid();
        let bitmap = if monochrome { info.hbmMask } else { info.hbmColor };
        let mut bm = BITMAP::default();
        let measured = GetObjectW(
            bitmap.into(),
            size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        ) != 0;
        let _ = DeleteObject(info.hbmColor.into());
        let _ = DeleteObject(info.hbmMask.into());
        let height = if monochrome { bm.bmHeight / 2 } else { bm.bmHeight };
        (measured && bm.bmWidth > 0 && height > 0).then(|| bm.bmWidth.max(height))
    }
}

/// Resizes premultiplied BGRA pixels with a separable triangle filter. The
/// filter widens when shrinking, so every source pixel contributes instead
/// of the result sampling a few and turning jagged.
fn resample(source: &IconPixels, size: u32) -> IconPixels {
    let (sw, sh) = (source.width as usize, source.height as usize);
    let side = size.max(1) as usize;
    let mut wide = vec![0f32; side * sh * 4];
    for y in 0..sh {
        for x in 0..side {
            let (start, weights) = taps(sw, side, x);
            let out = &mut wide[(y * side + x) * 4..][..4];
            for (at, weight) in start.zip(&weights) {
                let pixel = &source.bgra[(y * sw + at) * 4..][..4];
                for c in 0..4 {
                    out[c] += pixel[c] as f32 * weight;
                }
            }
        }
    }
    let mut bgra = vec![0u8; side * side * 4];
    let (out, _) = bgra.as_chunks_mut::<4>();
    for y in 0..side {
        for x in 0..side {
            let (start, weights) = taps(sh, side, y);
            let mut pixel = [0f32; 4];
            for (at, weight) in start.zip(&weights) {
                let row = &wide[(at * side + x) * 4..][..4];
                for c in 0..4 {
                    pixel[c] += row[c] * weight;
                }
            }
            for (channel, value) in out[y * side + x].iter_mut().zip(pixel) {
                *channel = value.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    IconPixels {
        width: side as u32,
        height: side as u32,
        bgra,
    }
}

/// The source indices and normalized weights that produce destination
/// pixel `at` in one dimension.
fn taps(source: usize, destination: usize, at: usize) -> (std::ops::RangeInclusive<usize>, Vec<f32>) {
    let scale = source as f32 / destination as f32;
    let radius = scale.max(1.0);
    let center = (at as f32 + 0.5) * scale - 0.5;
    let first = (center - radius).ceil().max(0.0) as usize;
    let last = ((center + radius).floor() as isize).clamp(0, source as isize - 1) as usize;
    let mut weights: Vec<f32> = (first..=last)
        .map(|index| (1.0 - (index as f32 - center).abs() / radius).max(0.0))
        .collect();
    let total: f32 = weights.iter().sum();
    if total > 0.0 {
        weights.iter_mut().for_each(|weight| *weight /= total);
    } else {
        weights.fill(1.0);
    }
    (first..=last, weights)
}

/// The icon drawn at `size` pixels over a gray `level` background, as BGRA rows.
fn draw_on(icon: HICON, size: i32, level: u8) -> Option<Vec<u8>> {
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size, // Top-down rows.
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let mut bits = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(dc);
            return None;
        };
        let previous = SelectObject(dc, bitmap.into());
        let len = (size * size * 4) as usize;
        std::slice::from_raw_parts_mut(bits as *mut u8, len).fill(level);
        let drawn = DrawIconEx(dc, 0, 0, icon, size, size, 0, None, DI_NORMAL).is_ok();
        let _ = GdiFlush();
        let pixels = drawn.then(|| std::slice::from_raw_parts(bits as *const u8, len).to_vec());
        SelectObject(dc, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        pixels
    }
}

/// Loads shell artwork for the exact physical size used by the dock.
pub fn app_pixels(path: &str, size: i32) -> Option<IconPixels> {
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};
    use windows::core::PCWSTR;
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).ok()?;
        let source = crate::indexer::load_icon(&item, size.max(64))?;
        let mut bgra = vec![0; (size * size * 4) as usize];
        for y in 0..size as usize {
            for x in 0..size as usize {
                let sx = ((x as f32 + 0.5) * source.width as f32 / size as f32 - 0.5).max(0.0);
                let sy = ((y as f32 + 0.5) * source.height as f32 / size as f32 - 0.5).max(0.0);
                let (ix, iy) = (sx as u32, sy as u32);
                for c in 0..4 {
                    let at = |x: u32, y: u32| {
                        source.bgra
                            [((y.min(source.height - 1) * source.width + x.min(source.width - 1)) * 4) as usize + c]
                            as f32
                    };
                    let top = at(ix, iy) * (1.0 - sx.fract()) + at(ix + 1, iy) * sx.fract();
                    let bottom = at(ix, iy + 1) * (1.0 - sx.fract()) + at(ix + 1, iy + 1) * sx.fract();
                    bgra[(y * size as usize + x) * 4 + c] =
                        (top * (1.0 - sy.fract()) + bottom * sy.fract()).round() as u8;
                }
            }
        }
        Some(IconPixels {
            width: size as u32,
            height: size as u32,
            bgra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(width: u32, height: u32, bgra: Vec<u8>) -> IconPixels {
        IconPixels { width, height, bgra }
    }

    #[test]
    fn inline_follows_promotion_in_bridge_and_presence_in_standalone() {
        // Bridge: only an explicit promotion is inline; an
        // unrecorded preference stays in the flyout, as
        // Explorer leaves it in the overflow.
        assert!(inline_eligible(Some(true), false));
        assert!(!inline_eligible(Some(false), false));
        assert!(!inline_eligible(None, false));
        // Standalone: no preference exists to consult, so a
        // shown icon is inline; only an explicit demotion —
        // which nothing in the feed currently sends — is not.
        assert!(inline_eligible(Some(true), true));
        assert!(inline_eligible(None, true));
        assert!(!inline_eligible(Some(false), true));
    }

    #[test]
    fn resampling_keeps_a_flat_color() {
        let source = pixels(2, 2, [10u8, 20, 30, 40].repeat(4));
        let resized = resample(&source, 5);
        assert_eq!((resized.width, resized.height), (5, 5));
        assert!(resized.bgra.chunks(4).all(|p| p == [10, 20, 30, 40]));
    }

    #[test]
    fn shrinking_averages_neighbouring_pixels() {
        // Two opaque columns, one black and one white, become one gray column.
        let source = pixels(2, 1, vec![0, 0, 0, 255, 255, 255, 255, 255]);
        let resized = resample(&source, 1);
        assert_eq!(resized.bgra, [128, 128, 128, 255]);
    }

    #[test]
    fn upscaling_keeps_premultiplied_alpha_consistent() {
        // A half-transparent white pixel stays premultiplied instead of
        // brightening into a fringe.
        let source = pixels(1, 1, vec![128, 128, 128, 128]);
        let resized = resample(&source, 2);
        assert!(resized.bgra.chunks(4).all(|p| p == [128, 128, 128, 128]));
    }

    #[test]
    fn edges_blend_without_dark_fringes() {
        // Opaque white beside fully transparent: the blend is half-covered
        // white, so color and alpha stay equal in premultiplied form.
        let source = pixels(2, 1, vec![255, 255, 255, 255, 0, 0, 0, 0]);
        let resized = resample(&source, 1);
        assert_eq!(resized.bgra, [128, 128, 128, 128]);
    }
}
