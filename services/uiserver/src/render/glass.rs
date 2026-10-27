//! Frosted-glass painting helpers.
//!
//! Chrome that floats over the wallpaper samples a blurred copy of the pixels
//! beneath it, then adds a dark shade, a white tint, a soft sheen and a
//! one-pixel edge. The wallpaper is static, so the blur only runs when the
//! chrome is rebuilt, never per frame. Everything is integer math: edge
//! coverage uses 1/16-pixel fixed point.

use crate::canvas::{Rect, SurfaceCanvas};

use super::colors::{COLOR_GLASS_DARK, COLOR_PANEL_HIGHLIGHT, COLOR_SHADOW};

/// The unblurred wallpaper that glass panels sample.
#[derive(Clone, Copy)]
pub(super) struct Backdrop<'a> {
    pub(super) pixels: &'a [u32],
    pub(super) stride: usize,
}

impl Backdrop<'_> {
    fn bounds(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.stride,
            height: self.pixels.len() / self.stride.max(1),
        }
    }
}

/// How a glass panel looks. Alphas are 0..=255.
#[derive(Clone, Copy)]
pub(super) struct GlassStyle {
    pub(super) radius: usize,
    /// Dark shade laid over the blur so white text keeps its contrast.
    pub(super) shade_alpha: u8,
    /// White tint that gives the frosted-milk look.
    pub(super) tint_alpha: u8,
    /// Peak alpha of the one-pixel edge; it fades toward the bottom.
    pub(super) edge_alpha: u8,
}

const BLUR_RADIUS: usize = 10;
const BLUR_PASSES: usize = 2;
/// Fixed-point units per pixel for edge coverage.
const FP: i64 = 16;
const SHEEN_ALPHA: u8 = 26;

/// Paints a frosted glass panel over `rect`. `canvas` must be the surface
/// that `backdrop` describes, with the panel area still showing the wallpaper.
pub(super) fn draw_glass_panel(
    canvas: &mut SurfaceCanvas<'_>,
    backdrop: Backdrop<'_>,
    rect: Rect,
    style: GlassStyle,
) {
    let rect = rect.intersect(backdrop.bounds());
    if rect.width < 4 || rect.height < 4 {
        return;
    }
    let radius = style.radius.min(rect.width / 2).min(rect.height / 2);
    let surface = panel_surface(backdrop, rect, radius, style.edge_alpha);
    canvas.draw_surface(
        &surface,
        rect.width,
        rect.height,
        rect.width,
        rect.x,
        rect.y,
    );

    let inner = Rect {
        x: rect.x + 1,
        y: rect.y + 1,
        width: rect.width - 2,
        height: rect.height - 2,
    };
    let inner_radius = radius.saturating_sub(1);
    canvas.fill_rounded_rect_alpha(inner, COLOR_GLASS_DARK, style.shade_alpha, inner_radius);
    canvas.fill_rounded_rect_alpha(inner, COLOR_PANEL_HIGHLIGHT, style.tint_alpha, inner_radius);
    // A sheen that fades out by the middle, so there is no hard seam.
    let sheen_rows = inner.height / 2;
    for row in 0..sheen_rows {
        let inset = corner_inset(inner_radius, row);
        let alpha = SHEEN_ALPHA as usize * (sheen_rows - row) / sheen_rows.max(1);
        canvas.fill_rect_alpha(
            Rect {
                x: inner.x + inset,
                y: inner.y + row,
                width: inner.width.saturating_sub(inset * 2),
                height: 1,
            },
            COLOR_PANEL_HIGHLIGHT,
            alpha as u8,
        );
    }
}

/// A small frosted chip that sits on top of an already-glass surface, so it
/// needs no backdrop of its own: a translucent tint with a bright top edge.
pub(super) fn draw_glass_chip(
    canvas: &mut SurfaceCanvas<'_>,
    rect: Rect,
    radius: usize,
    tint_alpha: u8,
) {
    if rect.is_empty() {
        return;
    }
    canvas.fill_rounded_rect_alpha(rect, COLOR_PANEL_HIGHLIGHT, tint_alpha, radius);
    let edge = Rect {
        x: rect.x + radius.min(rect.width / 2),
        y: rect.y,
        width: rect.width.saturating_sub(radius.min(rect.width / 2) * 2),
        height: 1,
    };
    canvas.fill_rect_alpha(edge, COLOR_PANEL_HIGHLIGHT, tint_alpha.saturating_mul(2));
}

/// A glossy glass tile: `color` as the base, a white gloss over the top half,
/// a dark shade toward the bottom, and a one-pixel bright edge that fades from
/// the top. `alpha` scales every layer so a dimmed tile stays consistent.
pub(super) fn fill_glossy_tile(
    canvas: &mut SurfaceCanvas<'_>,
    rect: Rect,
    radius: usize,
    color: u32,
    alpha: u8,
) {
    if rect.width < 4 || rect.height < 4 || alpha == 0 {
        return;
    }
    let radius = radius.min(rect.width / 2).min(rect.height / 2);
    let scale = |value: u32| (value * alpha as u32 / 255) as u8;
    canvas.fill_rounded_rect_alpha(rect, color, alpha, radius);

    for row in 0..rect.height {
        let inset = corner_inset(radius, row).max(corner_inset(radius, rect.height - 1 - row));
        let percent = (row * 100 / rect.height) as u32;
        let (overlay, strength) = if percent < 50 {
            (COLOR_PANEL_HIGHLIGHT, 64 * (50 - percent) / 50)
        } else {
            (COLOR_SHADOW, 48 * (percent - 50) / 50)
        };
        let overlay_alpha = scale(strength);
        if overlay_alpha == 0 {
            continue;
        }
        canvas.fill_rect_alpha(
            Rect {
                x: rect.x + inset,
                y: rect.y + row,
                width: rect.width.saturating_sub(inset * 2),
                height: 1,
            },
            overlay,
            overlay_alpha,
        );
    }

    for py in 0..rect.height {
        for px in 0..rect.width {
            let near_edge = px <= radius
                || px + radius + 1 >= rect.width
                || py <= radius
                || py + radius + 1 >= rect.height;
            if !near_edge {
                continue;
            }
            let (outer, inner) = edge_coverage(px, py, rect.width, rect.height, radius);
            let ring = outer.saturating_sub(inner);
            if ring == 0 {
                continue;
            }
            let span = rect.height as u32 * 2;
            let edge = 130 * span.saturating_sub(py as u32) / span * ring / 255;
            canvas.fill_rect_alpha(
                Rect {
                    x: rect.x + px,
                    y: rect.y + py,
                    width: 1,
                    height: 1,
                },
                COLOR_PANEL_HIGHLIGHT,
                scale(edge),
            );
        }
    }
}

/// How opaque each part of a window texture is. The body and client area are
/// fully opaque; the title band is translucent glass so the desktop shows
/// through it, and the rounded corners fall away to transparent.
#[derive(Clone, Copy, Debug)]
pub(super) struct WindowAlpha {
    pub(super) title_height: usize,
    pub(super) radius: usize,
    /// Opacity at the top and bottom of the title band.
    pub(super) title_top: u8,
    pub(super) title_bottom: u8,
    /// Opacity of the title band's bright edge rows (the top two and the
    /// hairline under it), so the glass keeps a crisp outline.
    pub(super) title_edge: u8,
    /// Rectangles (window-local) that stay at least this opaque, so caption
    /// buttons read as solid glass rather than see-through shapes.
    pub(super) solid: [Rect; 3],
    pub(super) solid_alpha: u8,
}

/// Convert an opaque window texture into premultiplied-alpha form in place.
///
/// `pixels` covers a `width` x `height` region with `stride` pixels per row,
/// so this works directly on an atlas region. Existing alpha bits are ignored;
/// the color channels are read as opaque and premultiplied by the computed
/// alpha. Apply it exactly once after the whole region has been written, never
/// to already-premultiplied pixels.
pub(super) fn apply_window_alpha(
    pixels: &mut [u32],
    stride: usize,
    width: usize,
    height: usize,
    spec: WindowAlpha,
) {
    if width == 0 || height == 0 || stride < width {
        return;
    }
    let radius = spec.radius.min(width / 2).min(height / 2);
    let title_height = spec.title_height.min(height);
    for y in 0..height {
        let Some(row) = pixels.get_mut(y * stride..y * stride + width) else {
            return;
        };
        let row_alpha = title_row_alpha(&spec, y, title_height);
        // Straight edges are fully covered; only the corner squares need the
        // rounded-rectangle distance.
        let corner_row = y <= radius || y + radius + 1 >= height;
        if row_alpha == 255 && !corner_row {
            // The opaque body: premultiplying by 255 only sets the alpha byte.
            for pixel in row.iter_mut() {
                *pixel = 0xff00_0000 | (*pixel & 0x00ff_ffff);
            }
            continue;
        }
        for (x, pixel) in row.iter_mut().enumerate() {
            let coverage = if corner_row && (x <= radius || x + radius + 1 >= width) {
                edge_coverage(x, y, width, height, radius).0
            } else {
                255
            };
            let mut alpha = row_alpha;
            if y < title_height && spec.solid.iter().any(|rect| contains(rect, x, y)) {
                alpha = alpha.max(u32::from(spec.solid_alpha));
            }
            *pixel = premultiply(*pixel & 0x00ff_ffff, alpha * coverage / 255);
        }
    }
}

fn title_row_alpha(spec: &WindowAlpha, y: usize, title_height: usize) -> u32 {
    if y >= title_height {
        return 255;
    }
    let (top, bottom) = (i64::from(spec.title_top), i64::from(spec.title_bottom));
    let blended = top + (bottom - top) * y as i64 / title_height.max(1) as i64;
    let on_edge = y < 2 || y + 1 == title_height;
    let alpha = if on_edge {
        blended.max(i64::from(spec.title_edge))
    } else {
        blended
    };
    alpha.clamp(0, 255) as u32
}

fn contains(rect: &Rect, x: usize, y: usize) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn premultiply(rgb: u32, alpha: u32) -> u32 {
    let channel = |shift: u32| (((rgb >> shift) & 0xff) * alpha / 255) << shift;
    (alpha << 24) | channel(16) | channel(8) | channel(0)
}

/// Horizontal inset of a rounded corner of `radius` at `row` rows below the
/// top edge, so a flat band can follow the curve instead of leaking past it.
pub(super) fn corner_inset(radius: usize, row: usize) -> usize {
    if radius == 0 || row >= radius {
        return 0;
    }
    // Work in half pixels so the sample sits at the center of the row.
    let r2 = radius as u64 * 2;
    let dy = r2 - (row as u64 * 2 + 1);
    let reach = isqrt(r2 * r2 - dy * dy) / 2;
    (radius as u64 - reach.min(radius as u64)) as usize
}

/// The blurred wallpaper under `rect`, with the rounded corners cut back to
/// the unblurred wallpaper and a one-pixel white edge along the outline.
fn panel_surface(backdrop: Backdrop<'_>, rect: Rect, radius: usize, edge_alpha: u8) -> Vec<u32> {
    let mut surface = blurred_rect(backdrop, rect);
    let (width, height) = (rect.width, rect.height);
    for py in 0..height {
        for px in 0..width {
            let near_edge = px <= radius
                || px + radius + 1 >= width
                || py <= radius
                || py + radius + 1 >= height;
            if !near_edge {
                continue;
            }
            let (outer, inner) = edge_coverage(px, py, width, height, radius);
            let pixel = &mut surface[py * width + px];
            if outer < 255 {
                let source = (rect.y + py) * backdrop.stride + rect.x + px;
                if let Some(background) = backdrop.pixels.get(source) {
                    *pixel = mix(*background, *pixel, outer);
                }
            }
            let ring = outer.saturating_sub(inner);
            if ring > 0 {
                let falloff = (height as u32 * 2).saturating_sub(py as u32);
                let alpha = edge_alpha as u32 * falloff / (height as u32 * 2) * ring / 255;
                *pixel = mix(*pixel, 0x00ff_ffff, alpha);
            }
        }
    }
    surface
}

/// Coverage (0..=255) of the rounded rectangle and of the same rectangle
/// shrunk by one pixel, at the center of pixel (`px`, `py`). Their difference
/// is the one-pixel outline.
fn edge_coverage(px: usize, py: usize, width: usize, height: usize, radius: usize) -> (u32, u32) {
    let half_w = width as i64 * FP / 2;
    let half_h = height as i64 * FP / 2;
    let rr = radius as i64 * FP;
    let cx = px as i64 * FP + FP / 2 - half_w;
    let cy = py as i64 * FP + FP / 2 - half_h;
    let qx = cx.abs() - (half_w - rr);
    let qy = cy.abs() - (half_h - rr);
    let (ox, oy) = (qx.max(0), qy.max(0));
    let distance = isqrt((ox * ox + oy * oy) as u64) as i64 + qx.max(qy).min(0) - rr;
    (coverage(distance), coverage(distance + FP))
}

/// 255 well inside the shape, 0 well outside, antialiased across one pixel.
fn coverage(distance: i64) -> u32 {
    ((FP / 2 - distance).clamp(0, FP) * 255 / FP) as u32
}

/// Per-channel blend: `amount` 0 keeps `base`, 255 gives `over`.
pub(super) fn mix(base: u32, over: u32, amount: u32) -> u32 {
    let amount = amount.min(255);
    let channel = |shift: u32| {
        let a = (base >> shift) & 0xff;
        let b = (over >> shift) & 0xff;
        ((a * (255 - amount) + b * amount) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

pub(super) fn isqrt(value: u64) -> u64 {
    if value < 2 {
        return value;
    }
    // Start from a power of two at or above the root so Newton's iteration
    // descends to the floor in a handful of steps instead of ~log2(value).
    let mut x = 1_u64 << (64 - value.leading_zeros()).div_ceil(2);
    let mut y = (x + value / x) / 2;
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    x
}

/// A box-blurred copy of the wallpaper under `rect`. The source region is
/// padded so the edges of `rect` see real neighbours instead of smearing.
fn blurred_rect(backdrop: Backdrop<'_>, rect: Rect) -> Vec<u32> {
    let bounds = backdrop.bounds();
    let pad = BLUR_RADIUS * BLUR_PASSES;
    let x0 = rect.x.saturating_sub(pad);
    let y0 = rect.y.saturating_sub(pad);
    let x1 = (rect.x + rect.width + pad).min(bounds.width);
    let y1 = (rect.y + rect.height + pad).min(bounds.height);
    let (width, height) = (x1 - x0, y1 - y0);

    let mut buffer = Vec::with_capacity(width * height);
    for y in y0..y1 {
        let start = y * backdrop.stride + x0;
        buffer.extend_from_slice(&backdrop.pixels[start..start + width]);
    }
    let mut scratch = vec![0_u32; buffer.len()];
    for _ in 0..BLUR_PASSES {
        box_blur(&mut buffer, &mut scratch, width, height, BLUR_RADIUS);
    }

    let mut out = Vec::with_capacity(rect.width * rect.height);
    for y in rect.y..rect.y + rect.height {
        let start = (y - y0) * width + (rect.x - x0);
        out.extend_from_slice(&buffer[start..start + rect.width]);
    }
    out
}

fn box_blur(buffer: &mut [u32], scratch: &mut [u32], width: usize, height: usize, radius: usize) {
    for y in 0..height {
        let row = y * width;
        blur_line(
            width,
            radius,
            |x| buffer[row + x],
            |x, v| scratch[row + x] = v,
        );
    }
    for x in 0..width {
        blur_line(
            height,
            radius,
            |y| scratch[y * width + x],
            |y, v| buffer[y * width + x] = v,
        );
    }
}

/// One-dimensional box blur with edge replication, using a sliding window.
fn blur_line(
    len: usize,
    radius: usize,
    load: impl Fn(usize) -> u32,
    mut store: impl FnMut(usize, u32),
) {
    if len == 0 {
        return;
    }
    let last = len as isize - 1;
    let sample = |index: isize| load(index.clamp(0, last) as usize);
    let r = radius as isize;
    let window = (2 * r + 1) as u32;
    let (mut red, mut green, mut blue) = (0_u32, 0_u32, 0_u32);
    for index in -r..=r {
        let pixel = sample(index);
        red += (pixel >> 16) & 0xff;
        green += (pixel >> 8) & 0xff;
        blue += pixel & 0xff;
    }
    for x in 0..len as isize {
        store(
            x as usize,
            ((red / window) << 16) | ((green / window) << 8) | (blue / window),
        );
        let incoming = sample(x + r + 1);
        let outgoing = sample(x - r);
        red = red + ((incoming >> 16) & 0xff) - ((outgoing >> 16) & 0xff);
        green = green + ((incoming >> 8) & 0xff) - ((outgoing >> 8) & 0xff);
        blue = blue + (incoming & 0xff) - (outgoing & 0xff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WALLPAPER: u32 = 0x0040_5080;

    fn luma(pixel: u32) -> u32 {
        ((pixel >> 16) & 0xff) + ((pixel >> 8) & 0xff) + (pixel & 0xff)
    }

    #[test]
    fn integer_square_root_is_exact_on_squares_and_floors_otherwise() {
        for value in [
            0_u64,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            255,
            256,
            1 << 20,
            (1 << 20) + 7,
        ] {
            let root = isqrt(value);
            assert!(
                root * root <= value && (root + 1) * (root + 1) > value,
                "{value}"
            );
        }
    }

    fn spec() -> WindowAlpha {
        WindowAlpha {
            title_height: 12,
            radius: 6,
            title_top: 160,
            title_bottom: 100,
            title_edge: 220,
            solid: [
                Rect {
                    x: 40,
                    y: 2,
                    width: 8,
                    height: 8,
                },
                Rect::empty(),
                Rect::empty(),
            ],
            solid_alpha: 230,
        }
    }

    #[test]
    fn window_alpha_leaves_the_body_opaque_and_the_title_band_translucent() {
        let (width, height) = (64_usize, 40_usize);
        let mut pixels = vec![0x0040_5080_u32; width * height];
        apply_window_alpha(&mut pixels, width, width, height, spec());

        let body = pixels[30 * width + 32];
        assert_eq!(
            body, 0xff40_5080,
            "body pixels become opaque, color unchanged"
        );

        let glass = pixels[6 * width + 20];
        let alpha = glass >> 24;
        assert!(
            (100..=160).contains(&alpha),
            "title glass is partly transparent: {alpha}"
        );
        assert_eq!(
            (glass >> 16) & 0xff,
            0x40 * alpha / 255,
            "color is premultiplied by the alpha"
        );
    }

    #[test]
    fn window_alpha_makes_the_corners_transparent_and_the_edge_crisp() {
        let (width, height) = (64_usize, 40_usize);
        let mut pixels = vec![0x00ff_ffff_u32; width * height];
        apply_window_alpha(&mut pixels, width, width, height, spec());

        assert_eq!(pixels[0], 0, "the outer corner pixel is fully transparent");
        assert_eq!(pixels[(height - 1) * width], 0, "so is the bottom corner");
        let edge = pixels[20] >> 24;
        let band = pixels[6 * width + 20] >> 24;
        assert!(
            edge > band,
            "the title's top edge row is more opaque than the band"
        );
    }

    #[test]
    fn window_alpha_keeps_caption_buttons_more_opaque_than_the_glass() {
        let (width, height) = (64_usize, 40_usize);
        let mut pixels = vec![0x00ff_ffff_u32; width * height];
        apply_window_alpha(&mut pixels, width, width, height, spec());
        let button = pixels[6 * width + 44] >> 24;
        let glass = pixels[6 * width + 20] >> 24;
        assert!(button >= 230 && button > glass);
    }

    #[test]
    fn window_alpha_respects_stride_and_leaves_neighbouring_pixels_alone() {
        let (stride, width, height) = (80_usize, 64_usize, 40_usize);
        let mut pixels = vec![0x1234_5678_u32; stride * height];
        apply_window_alpha(&mut pixels, stride, width, height, spec());
        for y in 0..height {
            for x in width..stride {
                assert_eq!(pixels[y * stride + x], 0x1234_5678, "padding at ({x},{y})");
            }
        }
        // Degenerate inputs never panic.
        apply_window_alpha(&mut pixels, stride, 0, 0, spec());
        apply_window_alpha(&mut pixels, 10, 64, 40, spec());
    }

    #[test]
    fn corner_inset_follows_the_arc_and_vanishes_below_it() {
        assert_eq!(corner_inset(0, 0), 0);
        assert_eq!(corner_inset(12, 12), 0);
        let insets: Vec<_> = (0..12).map(|row| corner_inset(12, row)).collect();
        assert!(
            insets.windows(2).all(|pair| pair[0] >= pair[1]),
            "{insets:?}"
        );
        assert!(insets[0] >= 6 && insets[11] <= 1, "{insets:?}");
    }

    #[test]
    fn blurring_a_flat_wallpaper_changes_nothing() {
        let pixels = vec![WALLPAPER; 64 * 64];
        let backdrop = Backdrop {
            pixels: &pixels,
            stride: 64,
        };
        let blurred = blurred_rect(
            backdrop,
            Rect {
                x: 8,
                y: 8,
                width: 32,
                height: 16,
            },
        );
        assert_eq!(blurred.len(), 32 * 16);
        assert!(blurred.iter().all(|pixel| *pixel == WALLPAPER));
    }

    #[test]
    fn blurring_spreads_a_bright_block_to_its_neighbours() {
        let (width, height) = (96_usize, 64_usize);
        let mut pixels = vec![0_u32; width * height];
        for y in 24..40 {
            for x in 40..56 {
                pixels[y * width + x] = 0x00ff_ffff;
            }
        }
        let backdrop = Backdrop {
            pixels: &pixels,
            stride: width,
        };
        let rect = Rect {
            x: 8,
            y: 8,
            width: 80,
            height: 48,
        };
        let blurred = blurred_rect(backdrop, rect);
        let at = |x: usize, y: usize| blurred[(y - rect.y) * rect.width + (x - rect.x)];
        assert!(
            luma(at(48, 32)) < 0xff * 3,
            "the block's peak is attenuated"
        );
        assert!(luma(at(48, 32)) > 0, "the block's center keeps its light");
        assert!(luma(at(36, 32)) > 0, "light spreads past the block edge");
        assert_eq!(at(8, 8), 0, "a corner beyond the blur reach stays dark");
    }

    #[test]
    fn panel_keeps_wallpaper_in_the_cut_corners_and_brightens_the_edge() {
        let pixels = vec![WALLPAPER; 128 * 96];
        let mut canvas_pixels = pixels.clone();
        let rect = Rect {
            x: 20,
            y: 20,
            width: 80,
            height: 48,
        };
        {
            let mut canvas = SurfaceCanvas::new(canvas_pixels.as_mut_slice(), 128, 96, 128);
            draw_glass_panel(
                &mut canvas,
                Backdrop {
                    pixels: &pixels,
                    stride: 128,
                },
                rect,
                GlassStyle {
                    radius: 14,
                    shade_alpha: 40,
                    tint_alpha: 40,
                    edge_alpha: 160,
                },
            );
        }
        let at = |x: usize, y: usize| canvas_pixels[y * 128 + x];
        assert_eq!(
            at(rect.x, rect.y),
            WALLPAPER,
            "outer corner pixel is untouched"
        );
        assert_eq!(at(0, 0), WALLPAPER, "outside the panel is untouched");
        let edge = at(rect.x + rect.width / 2, rect.y);
        let interior = at(rect.x + rect.width / 2, rect.y + 6);
        assert!(
            luma(edge) > luma(interior),
            "top edge is brighter than the body"
        );
        assert_ne!(interior, WALLPAPER, "body is tinted");
    }

    #[test]
    fn panels_too_small_to_hold_an_edge_are_skipped() {
        let pixels = vec![WALLPAPER; 32 * 32];
        let mut canvas_pixels = pixels.clone();
        let mut canvas = SurfaceCanvas::new(canvas_pixels.as_mut_slice(), 32, 32, 32);
        draw_glass_panel(
            &mut canvas,
            Backdrop {
                pixels: &pixels,
                stride: 32,
            },
            Rect {
                x: 4,
                y: 4,
                width: 3,
                height: 3,
            },
            GlassStyle {
                radius: 8,
                shade_alpha: 60,
                tint_alpha: 60,
                edge_alpha: 200,
            },
        );
        assert_eq!(canvas_pixels, pixels);
    }

    #[test]
    fn integer_square_root_is_exact_over_a_wide_range() {
        for value in (0..200_000_u64).chain([u64::MAX, u64::MAX - 1, 1 << 62, (1 << 62) - 1]) {
            let root = isqrt(value);
            assert!(
                u128::from(root) * u128::from(root) <= u128::from(value)
                    && u128::from(root + 1) * u128::from(root + 1) > u128::from(value),
                "{value}"
            );
        }
    }

    /// The straightforward per-pixel form the fast paths must reproduce.
    fn reference_window_alpha(
        pixels: &mut [u32],
        stride: usize,
        width: usize,
        height: usize,
        spec: &WindowAlpha,
    ) {
        let radius = spec.radius.min(width / 2).min(height / 2);
        let title_height = spec.title_height.min(height);
        for y in 0..height {
            let row_alpha = title_row_alpha(spec, y, title_height);
            for x in 0..width {
                let near_edge = x <= radius
                    || x + radius + 1 >= width
                    || y <= radius
                    || y + radius + 1 >= height;
                let coverage = if near_edge {
                    edge_coverage(x, y, width, height, radius).0
                } else {
                    255
                };
                let mut alpha = row_alpha;
                if y < title_height && spec.solid.iter().any(|rect| contains(rect, x, y)) {
                    alpha = alpha.max(u32::from(spec.solid_alpha));
                }
                let pixel = &mut pixels[y * stride + x];
                *pixel = premultiply(*pixel & 0x00ff_ffff, alpha * coverage / 255);
            }
        }
    }

    #[test]
    fn the_window_alpha_fast_paths_match_the_per_pixel_form() {
        for (width, height, radius) in [(97, 61, 12), (64, 40, 0), (30, 30, 20), (200, 120, 14)] {
            let stride = width + 3;
            let source: Vec<u32> = (0..stride * height)
                .map(|index| 0xff00_0000 ^ (index as u32).wrapping_mul(2_654_435_761))
                .collect();
            let spec = WindowAlpha {
                title_height: 24,
                radius,
                title_top: 196,
                title_bottom: 150,
                title_edge: 238,
                solid: [
                    Rect { x: 4, y: 2, width: 10, height: 8 },
                    Rect { x: 20, y: 2, width: 10, height: 8 },
                    Rect { x: 40, y: 2, width: 10, height: 8 },
                ],
                solid_alpha: 226,
            };
            let (mut fast, mut slow) = (source.clone(), source);
            apply_window_alpha(&mut fast, stride, width, height, spec);
            reference_window_alpha(&mut slow, stride, width, height, &spec);
            assert_eq!(fast, slow, "{width}x{height} radius {radius}");
        }
    }
}
