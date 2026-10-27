//! Glass desktop wallpaper.
//!
//! A soft mesh gradient: a vertical indigo → violet → teal base lit by large
//! radial glows in cyan, pink, mint and amber. It is deliberately mid-tone so
//! the frosted glass chrome above it reads as translucent. The full composite
//! is expensive enough that it runs on a background thread the first time
//! the display is configured.

use std::sync::mpsc::{self, Receiver};

use crate::canvas::{Rect, SurfaceCanvas};
use crate::sys::{require_background_thread_class, spawn_ui_thread, UiThreadRole};

use super::colors::{
    COLOR_BG_DEEP, COLOR_BG_MID, COLOR_BG_TOP, COLOR_GLOW_AMBER, COLOR_GLOW_CYAN, COLOR_GLOW_MINT,
    COLOR_GLOW_PINK,
};

const BACKGROUND_YIELD_ROWS: usize = 16;

pub(crate) struct DesktopBackground {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u32>,
}

impl DesktopBackground {
    pub(crate) fn fits(&self, width: usize, height: usize) -> bool {
        width != 0
            && height != 0
            && self.width == width
            && self.height == height
            && width.checked_mul(height) == Some(self.pixels.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{build_fast_desktop_background, DesktopBackground};

    #[test]
    fn bootstrap_scene_is_complete_without_waiting_for_optional_worker() {
        let bootstrap = DesktopBackground {
            width: 16,
            height: 10,
            pixels: build_fast_desktop_background(16, 10),
        };
        assert!(bootstrap.fits(16, 10));
        assert!(bootstrap.pixels.iter().any(|pixel| *pixel != 0));
        assert!(!bootstrap.fits(16, 11));
        let truncated = DesktopBackground {
            pixels: bootstrap.pixels[..159].to_vec(),
            ..bootstrap
        };
        assert!(!truncated.fits(16, 10));
    }
}

/// The first GPU frame requires a complete gradient before its strict
/// activation deadline. This one-shot worker retains boot class only until
/// it publishes that scene; the optional full glass pass self-demotes before
/// doing any work. Neither stage grants a CPU compositor fallback.
pub(crate) fn start_desktop_background_loader(
    width: usize,
    height: usize,
) -> Receiver<DesktopBackground> {
    let (sender, receiver) = mpsc::sync_channel(1);
    spawn_ui_thread(
        UiThreadRole::BootstrapDesktop,
        "uiserver-desktop-background",
        move || {
            crate::sys::debug_line("uiserver: first desktop seed worker started");
            let fast_pixels = build_fast_desktop_background(width, height);
            crate::sys::debug_line("uiserver: first desktop seed built");
            if sender
                .send(DesktopBackground {
                    width,
                    height,
                    pixels: fast_pixels,
                })
                .is_err()
            {
                return;
            }
            crate::sys::debug_line("uiserver: first desktop seed queued");
            // Once the first-frame seed has been handed off, the expensive
            // background is unrelated long-lived work, not boot authority.
            require_background_thread_class();
            let pixels = build_desktop_background(width, height);
            let _ = sender.send(DesktopBackground {
                width,
                height,
                pixels,
            });
        },
    )
    .unwrap_or_else(|_| std::process::exit(134));
    receiver
}

fn build_fast_desktop_background(width: usize, height: usize) -> Vec<u32> {
    let mut pixels = vec![0; width.saturating_mul(height)];
    crate::sys::debug_line("uiserver: first desktop seed allocated");
    if pixels.is_empty() {
        return pixels;
    }

    let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), width as u32, height as u32, width);
    paint_sky_gradient(&mut canvas, width, height);
    dither(&mut pixels, width);
    pixels
}

pub(crate) fn build_desktop_background(width: usize, height: usize) -> Vec<u32> {
    let mut pixels = vec![0; width.saturating_mul(height)];
    if pixels.is_empty() {
        return pixels;
    }

    let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), width as u32, height as u32, width);
    paint_sky(&mut canvas, width, height);
    dither(&mut pixels, width);
    pixels
}

/// 4x4 Bayer thresholds, 0..=15.
const BAYER_4X4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Nudge each channel by at most one level in an ordered pattern. A smooth
/// gradient quantized to 8 bits shows as visible bands on a big, soft
/// wallpaper; breaking each band edge up over a few pixels hides the steps
/// without adding visible noise.
fn dither(pixels: &mut [u32], width: usize) {
    if width == 0 {
        return;
    }
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let threshold = BAYER_4X4[(index / width) & 3][(index % width) & 3];
        let nudge: i32 = match threshold {
            0..=4 => -1,
            11..=15 => 1,
            _ => 0,
        };
        if nudge == 0 {
            continue;
        }
        let channel = |shift: u32| {
            let value = ((*pixel >> shift) & 0xff) as i32;
            ((value + nudge).clamp(0, 255) as u32) << shift
        };
        *pixel = (*pixel & 0xff00_0000) | channel(16) | channel(8) | channel(0);
    }
}

/// Cheap fast-path: just the vertical 3-stop gradient. Used during the boot
/// frame so something pretty shows up immediately while the full Aurora
/// pass is still being built on the background thread.
pub(super) fn paint_sky_gradient(canvas: &mut SurfaceCanvas<'_>, width: usize, height: usize) {
    let mid_y = (height as f32 * 0.55) as usize;
    canvas.fill_v_gradient(
        Rect {
            x: 0,
            y: 0,
            width,
            height: mid_y,
        },
        COLOR_BG_TOP,
        COLOR_BG_MID,
    );
    canvas.fill_v_gradient(
        Rect {
            x: 0,
            y: mid_y,
            width,
            height: height.saturating_sub(mid_y),
        },
        COLOR_BG_MID,
        COLOR_BG_DEEP,
    );
}

pub(super) fn paint_sky(canvas: &mut SurfaceCanvas<'_>, width: usize, height: usize) {
    paint_sky_gradient(canvas, width, height);

    let w_i = width as i32;
    let h_i = height as i32;
    let base_radius = (w_i.max(h_i)) as u32;

    canvas.fill_radial_glow_cooperative(
        (w_i * 20) / 100,
        (h_i * 16) / 100,
        (base_radius * 6) / 10,
        COLOR_GLOW_CYAN,
        110,
        BACKGROUND_YIELD_ROWS,
    );
    canvas.fill_radial_glow_cooperative(
        (w_i * 84) / 100,
        (h_i * 24) / 100,
        (base_radius * 5) / 10,
        COLOR_GLOW_PINK,
        100,
        BACKGROUND_YIELD_ROWS,
    );
    canvas.fill_radial_glow_cooperative(
        (w_i * 22) / 100,
        (h_i * 90) / 100,
        (base_radius * 5) / 10,
        COLOR_GLOW_MINT,
        85,
        BACKGROUND_YIELD_ROWS,
    );
    canvas.fill_radial_glow_cooperative(
        (w_i * 78) / 100,
        (h_i * 86) / 100,
        (base_radius * 4) / 10,
        COLOR_GLOW_AMBER,
        70,
        BACKGROUND_YIELD_ROWS,
    );
}
