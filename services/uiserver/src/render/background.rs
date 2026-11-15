//! Desktop wallpaper loader and Aspect-Fill scaler.
//!
//! Decodes the default bundled wallpaper image (`I_always_love_you_cosmosy_paradise.qoi`)
//! once and renders an Aspect-Fill (`cover`, center-cropped, aspect-preserving)
//! background surface for the active display resolution.

use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;

use crate::canvas::{Rect, SurfaceCanvas};
use crate::sys::{require_background_thread_class, spawn_ui_thread, UiThreadRole};

use super::colors::{COLOR_BG_DEEP, COLOR_BG_MID, COLOR_BG_TOP};

const WALLPAPER_QOI: &[u8] = include_bytes!("../../assets/I_always_love_you_cosmosy_paradise.qoi");
const FP_BITS: u32 = 24;
const FP_ONE: u128 = 1_u128 << FP_BITS;
const MAX_QOI_PIXELS: usize = 64 * 1024 * 1024;

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

struct DecodedImage {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

#[derive(Clone, Copy)]
struct AspectFillGeometry {
    step_fp: u128,
    origin_x_fp: u128,
    origin_y_fp: u128,
    max_x_fp: u128,
    max_y_fp: u128,
}

fn bundled_wallpaper() -> Option<&'static DecodedImage> {
    static WALLPAPER: OnceLock<Option<DecodedImage>> = OnceLock::new();
    WALLPAPER.get_or_init(|| decode_qoi(WALLPAPER_QOI)).as_ref()
}

fn qoi_hash(r: u8, g: u8, b: u8, a: u8) -> usize {
    (usize::from(r) * 3 + usize::from(g) * 5 + usize::from(b) * 7 + usize::from(a) * 11) & 63
}

fn pack_bgra(r: u8, g: u8, b: u8, a: u8) -> u32 {
    (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
}

fn decode_qoi(bytes: &[u8]) -> Option<DecodedImage> {
    if bytes.len() < 22 || &bytes[0..4] != b"qoif" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[4..8].try_into().ok()?) as usize;
    let height = u32::from_be_bytes(bytes[8..12].try_into().ok()?) as usize;
    let channels = bytes[12];
    if width == 0 || height == 0 || !matches!(channels, 3 | 4) {
        return None;
    }
    let total_pixels = width.checked_mul(height)?;
    if total_pixels > MAX_QOI_PIXELS {
        return None;
    }
    let stream_end = bytes.len().checked_sub(8)?;
    let stream = &bytes[14..stream_end];
    let mut pixels = vec![0_u32; total_pixels];
    let mut index = [0_u32; 64];
    let mut r = 0_u8;
    let mut g = 0_u8;
    let mut b = 0_u8;
    let mut a = 255_u8;
    let mut pos = 0_usize;
    let mut px = 0_usize;

    while px < total_pixels {
        let tag = *stream.get(pos)?;
        pos += 1;
        if tag == 0xfe {
            r = *stream.get(pos)?;
            g = *stream.get(pos + 1)?;
            b = *stream.get(pos + 2)?;
            pos += 3;
            let packed = pack_bgra(r, g, b, a);
            index[qoi_hash(r, g, b, a)] = packed;
            pixels[px] = packed & 0x00ff_ffff;
            px += 1;
        } else if tag == 0xff {
            r = *stream.get(pos)?;
            g = *stream.get(pos + 1)?;
            b = *stream.get(pos + 2)?;
            a = *stream.get(pos + 3)?;
            pos += 4;
            let packed = pack_bgra(r, g, b, a);
            index[qoi_hash(r, g, b, a)] = packed;
            pixels[px] = packed & 0x00ff_ffff;
            px += 1;
        } else {
            match tag & 0xc0 {
                0x00 => {
                    let packed = index[usize::from(tag & 0x3f)];
                    a = (packed >> 24) as u8;
                    r = (packed >> 16) as u8;
                    g = (packed >> 8) as u8;
                    b = packed as u8;
                    pixels[px] = packed & 0x00ff_ffff;
                    px += 1;
                }
                0x40 => {
                    let dr = ((tag >> 4) & 0x03).wrapping_sub(2);
                    let dg = ((tag >> 2) & 0x03).wrapping_sub(2);
                    let db = (tag & 0x03).wrapping_sub(2);
                    r = r.wrapping_add(dr);
                    g = g.wrapping_add(dg);
                    b = b.wrapping_add(db);
                    let packed = pack_bgra(r, g, b, a);
                    index[qoi_hash(r, g, b, a)] = packed;
                    pixels[px] = packed & 0x00ff_ffff;
                    px += 1;
                }
                0x80 => {
                    let b2 = *stream.get(pos)?;
                    pos += 1;
                    let vg = (tag & 0x3f).wrapping_sub(32);
                    let dr = vg.wrapping_add(((b2 >> 4) & 0x0f).wrapping_sub(8));
                    let db = vg.wrapping_add((b2 & 0x0f).wrapping_sub(8));
                    r = r.wrapping_add(dr);
                    g = g.wrapping_add(vg);
                    b = b.wrapping_add(db);
                    let packed = pack_bgra(r, g, b, a);
                    index[qoi_hash(r, g, b, a)] = packed;
                    pixels[px] = packed & 0x00ff_ffff;
                    px += 1;
                }
                _ => {
                    let run = usize::from(tag & 0x3f) + 1;
                    let rgb = pack_bgra(r, g, b, 0);
                    let end = px.saturating_add(run).min(total_pixels);
                    pixels[px..end].fill(rgb);
                    px = end;
                }
            }
        }
    }

    Some(DecodedImage {
        width,
        height,
        pixels,
    })
}

fn compute_aspect_fill_geometry(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> AspectFillGeometry {
    let src_w_128 = src_w as u128;
    let src_h_128 = src_h as u128;
    let dst_w_128 = dst_w as u128;
    let dst_h_128 = dst_h as u128;
    // Aspect Fill chooses the larger scale max(dst_w/src_w, dst_h/src_h), so
    // the inverse step in source pixels is min(src_w/dst_w, src_h/dst_h).
    let step_fp = if dst_w_128 * src_h_128 >= dst_h_128 * src_w_128 {
        (src_w_128 << FP_BITS) / dst_w_128
    } else {
        (src_h_128 << FP_BITS) / dst_h_128
    };
    let span_w_fp = step_fp * dst_w_128;
    let span_h_fp = step_fp * dst_h_128;
    let origin_x_fp = ((src_w_128 << FP_BITS).saturating_sub(span_w_fp)) / 2;
    let origin_y_fp = ((src_h_128 << FP_BITS).saturating_sub(span_h_fp)) / 2;
    AspectFillGeometry {
        step_fp,
        origin_x_fp,
        origin_y_fp,
        max_x_fp: ((src_w_128 - 1) << FP_BITS),
        max_y_fp: ((src_h_128 - 1) << FP_BITS),
    }
}

fn sample_axis_fp(index: usize, origin_fp: u128, step_fp: u128, max_fp: u128) -> u128 {
    let center = origin_fp + ((index as u128 * 2 + 1) * step_fp) / 2;
    center.saturating_sub(FP_ONE / 2).min(max_fp)
}

fn render_aspect_fill(
    src: &DecodedImage,
    dst_w: usize,
    dst_h: usize,
    bilinear: bool,
) -> Vec<u32> {
    let Some(total) = dst_w.checked_mul(dst_h) else {
        return Vec::new();
    };
    if total == 0 || src.width == 0 || src.height == 0 {
        return vec![0; total];
    }
    let geom = compute_aspect_fill_geometry(src.width, src.height, dst_w, dst_h);
    let mut x_coords = Vec::with_capacity(dst_w);
    for x in 0..dst_w {
        let sx_fp = sample_axis_fp(x, geom.origin_x_fp, geom.step_fp, geom.max_x_fp);
        let x0 = (sx_fp >> FP_BITS) as usize;
        let x1 = (x0 + 1).min(src.width - 1);
        let fx = ((sx_fp >> (FP_BITS - 8)) & 0xff) as u32;
        x_coords.push((x0, x1, fx));
    }

    let mut out = vec![0_u32; total];
    for y in 0..dst_h {
        let sy_fp = sample_axis_fp(y, geom.origin_y_fp, geom.step_fp, geom.max_y_fp);
        let y0 = (sy_fp >> FP_BITS) as usize;
        let row0 = &src.pixels[y0 * src.width..(y0 + 1) * src.width];
        let dst_row = &mut out[y * dst_w..(y + 1) * dst_w];
        if !bilinear {
            let yn = ((sy_fp + (FP_ONE / 2)) >> FP_BITS).min((src.height - 1) as u128) as usize;
            let row_n = &src.pixels[yn * src.width..(yn + 1) * src.width];
            for (dst, &(x0, x1, fx)) in dst_row.iter_mut().zip(x_coords.iter()) {
                *dst = row_n[if fx >= 128 { x1 } else { x0 }];
            }
            continue;
        }
        let y1 = (y0 + 1).min(src.height - 1);
        let fy = ((sy_fp >> (FP_BITS - 8)) & 0xff) as u32;
        let wy0 = 256 - fy;
        let row1 = &src.pixels[y1 * src.width..(y1 + 1) * src.width];
        for (dst, &(x0, x1, fx)) in dst_row.iter_mut().zip(x_coords.iter()) {
            let wx0 = 256 - fx;
            let c00 = row0[x0];
            let c10 = row0[x1];
            let c01 = row1[x0];
            let c11 = row1[x1];
            let w00 = wx0 * wy0;
            let w10 = fx * wy0;
            let w01 = wx0 * fy;
            let w11 = fx * fy;
            let blend = |shift: u32| -> u32 {
                let acc = ((c00 >> shift) & 0xff) * w00
                    + ((c10 >> shift) & 0xff) * w10
                    + ((c01 >> shift) & 0xff) * w01
                    + ((c11 >> shift) & 0xff) * w11;
                ((acc + 32768) >> 16).min(255) << shift
            };
            *dst = blend(16) | blend(8) | blend(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        build_desktop_background, build_fast_desktop_background, bundled_wallpaper,
        compute_aspect_fill_geometry, sample_axis_fp, DecodedImage, DesktopBackground, FP_BITS,
        render_aspect_fill,
    };

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

    #[test]
    fn bundled_wallpaper_decodes_and_builds_smooth_background() {
        let decoded = bundled_wallpaper()
            .expect("bundled I_always_love_you_cosmosy_paradise.qoi must decode");
        assert_eq!((decoded.width, decoded.height), (2000, 945));
        assert_eq!(decoded.pixels.len(), 2000 * 945);

        let scaled = build_desktop_background(64, 40);
        assert_eq!(scaled.len(), 64 * 40);
        assert!(scaled.iter().any(|&p| p != 0));
    }

    #[test]
    fn aspect_fill_crops_centered_along_overflow_axis() {
        // 2000x901 into 1280x800: height covers [0..901), width crops symmetrically.
        // Pixel 799 center is 799.5 * (901/800) = 900.436875, so top-left texel y0=899
        // blends with y1=900 at fy=239/256.
        let geom = compute_aspect_fill_geometry(2000, 901, 1280, 800);
        let sy0 = sample_axis_fp(0, geom.origin_y_fp, geom.step_fp, geom.max_y_fp);
        let sy_last = sample_axis_fp(799, geom.origin_y_fp, geom.step_fp, geom.max_y_fp);
        let first_y = sy0 >> FP_BITS;
        let last_y = sy_last >> FP_BITS;
        let last_fy = (sy_last >> (FP_BITS - 8)) & 0xff;
        assert_eq!(first_y, 0);
        assert_eq!(last_y, 899);
        assert!(last_fy >= 235);

        let first_x = sample_axis_fp(0, geom.origin_x_fp, geom.step_fp, geom.max_x_fp) >> FP_BITS;
        let last_x =
            sample_axis_fp(1279, geom.origin_x_fp, geom.step_fp, geom.max_x_fp) >> FP_BITS;
        assert!(first_x > 260 && first_x < 300);
        assert!(last_x > 1700 && last_x < 1740);

        // Synthetic 4x2 image scaled Aspect Fill into 2x2 keeps middle 2 columns cleanly.
        let src = DecodedImage {
            width: 4,
            height: 2,
            pixels: vec![
                0x11_0000, 0x22_0000, 0x33_0000, 0x44_0000,
                0x11_0000, 0x22_0000, 0x33_0000, 0x44_0000,
            ],
        };
        let out = render_aspect_fill(&src, 2, 2, true);
        assert_eq!(out, vec![0x22_0000, 0x33_0000, 0x22_0000, 0x33_0000]);
    }
}

/// The first GPU frame requires a complete wallpaper before its strict
/// activation deadline. This one-shot worker retains boot class only until
/// it publishes that fast nearest-neighbor seed; the bilinear pass self-demotes
/// before running.
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
            // Once the first-frame seed has been handed off, the higher-quality
            // bilinear pass is background work, not boot authority.
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
    crate::sys::debug_line("uiserver: first desktop seed allocated");
    if let Some(wallpaper) = bundled_wallpaper() {
        return render_aspect_fill(wallpaper, width, height, false);
    }
    let mut pixels = vec![0; width.saturating_mul(height)];
    if pixels.is_empty() {
        return pixels;
    }
    let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), width as u32, height as u32, width);
    paint_sky_gradient(&mut canvas, width, height);
    pixels
}

pub(crate) fn build_desktop_background(width: usize, height: usize) -> Vec<u32> {
    if let Some(wallpaper) = bundled_wallpaper() {
        return render_aspect_fill(wallpaper, width, height, true);
    }
    let mut pixels = vec![0; width.saturating_mul(height)];
    if pixels.is_empty() {
        return pixels;
    }
    let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), width as u32, height as u32, width);
    paint_sky_gradient(&mut canvas, width, height);
    pixels
}

/// Fast boot fill for the initial splash frame before the wallpaper thread
/// publishes the decoded Aspect-Fill background.
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
