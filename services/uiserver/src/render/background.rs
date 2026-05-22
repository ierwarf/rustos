//! Aurora desktop background.
//!
//! The background is composed in three passes that approximate a twilight
//! sky: a vertical navy → indigo → violet gradient, soft radial aurora
//! washes in cyan / lavender / mint, and a sparse deterministic starfield.
//! The full composite is expensive enough that it runs on a background
//! thread the first time the display is configured.

use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::canvas::{Rect, SurfaceCanvas};

use super::colors::{
    COLOR_AURORA_CYAN, COLOR_AURORA_MINT, COLOR_AURORA_VIOLET, COLOR_BG_DEEP, COLOR_BG_MID,
    COLOR_BG_TOP, COLOR_STARFIELD,
};

pub(crate) struct DesktopBackground {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) pixels: Vec<u32>,
}

pub(crate) fn start_desktop_background_loader(
    width: usize,
    height: usize,
) -> Receiver<DesktopBackground> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let pixels = build_desktop_background(width, height);
        let _ = sender.send(DesktopBackground {
            width,
            height,
            pixels,
        });
    });
    receiver
}

pub(crate) fn build_desktop_background(width: usize, height: usize) -> Vec<u32> {
    let mut pixels = vec![0; width.saturating_mul(height)];
    if pixels.is_empty() {
        return pixels;
    }

    let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), width as u32, height as u32, width);
    paint_sky(&mut canvas, width, height);
    pixels
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
    let screen = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    paint_sky_gradient(canvas, width, height);

    let w_i = width as i32;
    let h_i = height as i32;
    let base_radius = (w_i.max(h_i)) as u32;

    canvas.fill_radial_glow(
        (w_i * 22) / 100,
        (h_i * 18) / 100,
        (base_radius * 6) / 10,
        COLOR_AURORA_CYAN,
        120,
    );
    canvas.fill_radial_glow(
        (w_i * 82) / 100,
        (h_i * 28) / 100,
        (base_radius * 5) / 10,
        COLOR_AURORA_VIOLET,
        110,
    );
    canvas.fill_radial_glow(
        (w_i * 18) / 100,
        (h_i * 92) / 100,
        (base_radius * 4) / 10,
        COLOR_AURORA_MINT,
        70,
    );
    canvas.fill_radial_glow(
        (w_i * 70) / 100,
        (h_i * 80) / 100,
        (base_radius * 5) / 10,
        COLOR_AURORA_VIOLET,
        55,
    );

    sprinkle_starfield(canvas, screen);
}

fn sprinkle_starfield(canvas: &mut SurfaceCanvas<'_>, area: Rect) {
    if area.width < 8 || area.height < 8 {
        return;
    }
    // Deterministic pseudo-random sparkle: a tiny LCG seeded by the canvas
    // dimensions so the same display always gets the same starfield.
    let mut state: u64 = (area.width as u64)
        .wrapping_mul(2_862_933_555_777_941_757)
        .wrapping_add((area.height as u64).wrapping_mul(3_037_000_493));
    let count = ((area.width.saturating_mul(area.height)) / 7_500).clamp(60, 600);
    for _ in 0..count {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let x_rand = ((state >> 33) as u32) % area.width as u32;
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let y_rand = ((state >> 33) as u32) % area.height as u32;
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let brightness = ((state >> 40) as u8) | 0x40; // 64..255
        let x = area.x + x_rand as usize;
        let y = area.y + y_rand as usize;
        canvas.fill_rect_alpha(
            Rect {
                x,
                y,
                width: 1,
                height: 1,
            },
            COLOR_STARFIELD,
            brightness.saturating_sub(120).max(40),
        );
    }
}
