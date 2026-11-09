//! Live previews of open windows for the dock's window list.
//!
//! DWM draws these itself: Wisp registers a thumbnail between the list
//! popup and each listed window, and the Desktop Window Manager keeps the
//! image current — minimized windows included — with no copying or polling
//! on Wisp's side. A thumbnail is composited over the popup's client area
//! and cannot be clipped by the popup's rounded corners, so the list insets
//! its previews from the card edges instead (see `logic::list_thumb_rect`).

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties, HTHUMBNAIL,
};

/// The thumbnails currently registered to one destination window.
#[derive(Default)]
pub struct Thumbs {
    live: Vec<(isize, HTHUMBNAIL)>,
}

impl Thumbs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the destination show exactly `wanted`: each source window's
    /// handle value paired with its destination rect, in client pixels.
    /// Thumbnails whose window left the list are unregistered; a window
    /// that refuses a thumbnail (it closed, or DWM rejects it) is skipped
    /// rather than taking the rest of the list down with it.
    pub fn sync(&mut self, dest: HWND, wanted: &[(isize, RECT)]) {
        self.live.retain(|(source, thumb)| {
            if wanted.iter().any(|(hwnd, _)| hwnd == source) {
                true
            } else {
                unsafe {
                    let _ = DwmUnregisterThumbnail(*thumb);
                }
                false
            }
        });
        for &(source, rect) in wanted {
            let props = DWM_THUMBNAIL_PROPERTIES {
                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE,
                rcDestination: rect,
                rcSource: RECT::default(),
                opacity: 255,
                fVisible: true.into(),
                fSourceClientAreaOnly: false.into(),
            };
            match self.live.iter().find(|(hwnd, _)| *hwnd == source) {
                Some((_, thumb)) => unsafe {
                    let _ = DwmUpdateThumbnailProperties(*thumb, &props);
                },
                None => {
                    let registered = unsafe {
                        let mut thumb = HTHUMBNAIL::default();
                        DwmRegisterThumbnail(dest, HWND(source as _), &mut thumb).and_then(|()| {
                            DwmUpdateThumbnailProperties(thumb, &props).map(|()| thumb)
                        })
                    };
                    if let Ok(thumb) = registered {
                        self.live.push((source, thumb));
                    }
                }
            }
        }
    }

    /// Unregisters every thumbnail. The destination is about to hide, and
    /// a thumbnail left registered would keep drawing onto it.
    pub fn clear(&mut self) {
        for (_, thumb) in self.live.drain(..) {
            unsafe {
                let _ = DwmUnregisterThumbnail(thumb);
            }
        }
    }
}
