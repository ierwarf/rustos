use core::convert::Infallible;
use std::sync::OnceLock;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Size};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{Pixel, RgbColor};

use crate::app::CursorMotion;
use crate::cursor_sprites::{
    CURSOR_SPRITE_ALPHA, CURSOR_SPRITE_CHANNELS, CURSOR_SPRITE_MAX_DISTANCE, CURSOR_SPRITE_SIZE,
};
use crate::simd;

const COLOR_CURSOR_BLUE: u32 = 0x0036_94ff;
const COLOR_CURSOR_WHITE: u32 = 0x00ec_faff;
const CURSOR_ROTATION_FP: i32 = 1024;
const CURSOR_VISUAL_RADIUS: usize = 72;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl Rect {
    pub(crate) const fn empty() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    pub(crate) fn contains(&self, px: u32, py: u32) -> bool {
        let px = px as usize;
        let py = py as usize;
        px >= self.x
            && py >= self.y
            && px < self.x.saturating_add(self.width)
            && py < self.y.saturating_add(self.height)
    }

    pub(crate) const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub(crate) fn intersect(&self, other: Rect) -> Rect {
        let start_x = self.x.max(other.x);
        let start_y = self.y.max(other.y);
        let end_x = self
            .x
            .saturating_add(self.width)
            .min(other.x.saturating_add(other.width));
        let end_y = self
            .y
            .saturating_add(self.height)
            .min(other.y.saturating_add(other.height));
        if start_x >= end_x || start_y >= end_y {
            return Rect::empty();
        }

        Rect {
            x: start_x,
            y: start_y,
            width: end_x - start_x,
            height: end_y - start_y,
        }
    }

    pub(crate) fn union(&self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return *self;
        }

        let start_x = self.x.min(other.x);
        let start_y = self.y.min(other.y);
        let end_x = self
            .x
            .saturating_add(self.width)
            .max(other.x.saturating_add(other.width));
        let end_y = self
            .y
            .saturating_add(self.height)
            .max(other.y.saturating_add(other.height));

        Rect {
            x: start_x,
            y: start_y,
            width: end_x.saturating_sub(start_x),
            height: end_y.saturating_sub(start_y),
        }
    }
}

pub(crate) struct SurfaceCanvas<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    stride_pixels: usize,
    clip_rect: Rect,
}

impl<'a> SurfaceCanvas<'a> {
    pub(crate) fn new(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride_pixels: usize,
    ) -> Self {
        let (width, height, clip_rect) =
            sanitized_surface_geometry(pixels.len(), width, height, stride_pixels);

        Self {
            pixels,
            width,
            height,
            stride_pixels,
            clip_rect,
        }
    }

    pub(crate) fn with_clip(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride_pixels: usize,
        clip_rect: Rect,
    ) -> Self {
        let (width, height, screen_rect) =
            sanitized_surface_geometry(pixels.len(), width, height, stride_pixels);

        Self {
            pixels,
            width,
            height,
            stride_pixels,
            clip_rect: screen_rect.intersect(clip_rect),
        }
    }

    pub(crate) fn clip_rect(&self) -> Rect {
        self.clip_rect
    }

    pub(crate) fn fill_rect(&mut self, rect: Rect, color: u32) {
        let rect = rect.intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }

        for row in rect.y..rect.y.saturating_add(rect.height) {
            let Some(row_start) = row
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(row_end) = row_start.checked_add(rect.width) else {
                return;
            };
            let Some(row_pixels) = self.pixels.get_mut(row_start..row_end) else {
                return;
            };
            row_pixels.fill(color);
        }
    }

    pub(crate) fn fill_rect_alpha(&mut self, rect: Rect, color: u32, alpha: u8) {
        if alpha == 0 {
            return;
        }
        if alpha == u8::MAX {
            self.fill_rect(rect, color);
            return;
        }

        let rect = rect.intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }

        for row in rect.y..rect.y.saturating_add(rect.height) {
            let Some(row_start) = row
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(row_end) = row_start.checked_add(rect.width) else {
                return;
            };
            let Some(row_pixels) = self.pixels.get_mut(row_start..row_end) else {
                return;
            };
            simd::blend_solid_bgr(row_pixels, color, alpha);
        }
    }

    pub(crate) fn fill_rounded_rect_alpha(
        &mut self,
        rect: Rect,
        color: u32,
        alpha: u8,
        radius: usize,
    ) {
        if alpha == 0 || rect.is_empty() {
            return;
        }
        let r = radius.min(rect.width / 2).min(rect.height / 2);
        if r == 0 {
            self.fill_rect_alpha(rect, color, alpha);
            return;
        }

        let middle = Rect {
            x: rect.x,
            y: rect.y.saturating_add(r),
            width: rect.width,
            height: rect.height.saturating_sub(r.saturating_mul(2)),
        };
        if !middle.is_empty() {
            self.fill_rect_alpha(middle, color, alpha);
        }

        // For commonly-used radii we precompute the per-row (inset, AA alpha)
        // table once at startup so dragging windows or repainting large
        // rounded shadows doesn't pay a sqrt + float-floor per row per
        // layer per frame.
        let profile = corner_profile(r);

        for (dy, row) in profile.iter().copied().enumerate().take(r) {
            let inset = row.inset as usize;
            if inset >= rect.width {
                continue;
            }
            let row_width = rect.width.saturating_sub(inset.saturating_mul(2));
            let row_x = rect.x.saturating_add(inset);
            let top_y = rect.y.saturating_add(dy);
            let bot_y = rect
                .y
                .saturating_add(rect.height)
                .saturating_sub(1)
                .saturating_sub(dy);
            if row_width > 0 {
                self.fill_rect_alpha(
                    Rect {
                        x: row_x,
                        y: top_y,
                        width: row_width,
                        height: 1,
                    },
                    color,
                    alpha,
                );
                if bot_y != top_y {
                    self.fill_rect_alpha(
                        Rect {
                            x: row_x,
                            y: bot_y,
                            width: row_width,
                            height: 1,
                        },
                        color,
                        alpha,
                    );
                }
            }

            let aa_q8 = row.aa_alpha_q8 as u32;
            if aa_q8 != 0 && inset > 0 {
                let aa_alpha = (((alpha as u32) * aa_q8 + 128) >> 8).min(255) as u8;
                if aa_alpha > 0 {
                    let left_x = row_x.saturating_sub(1);
                    let right_x = rect.x.saturating_add(rect.width).saturating_sub(inset);
                    self.put_pixel_alpha(left_x as i32, top_y as i32, color, aa_alpha);
                    if right_x < rect.x.saturating_add(rect.width) {
                        self.put_pixel_alpha(right_x as i32, top_y as i32, color, aa_alpha);
                    }
                    if bot_y != top_y {
                        self.put_pixel_alpha(left_x as i32, bot_y as i32, color, aa_alpha);
                        if right_x < rect.x.saturating_add(rect.width) {
                            self.put_pixel_alpha(right_x as i32, bot_y as i32, color, aa_alpha);
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn fill_v_gradient(&mut self, rect: Rect, top_color: u32, bottom_color: u32) {
        let rect = rect.intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }
        let top_r = ((top_color >> 16) & 0xff) as i32;
        let top_g = ((top_color >> 8) & 0xff) as i32;
        let top_b = (top_color & 0xff) as i32;
        let bot_r = ((bottom_color >> 16) & 0xff) as i32;
        let bot_g = ((bottom_color >> 8) & 0xff) as i32;
        let bot_b = (bottom_color & 0xff) as i32;
        let span = rect.height.max(1) as i32;
        for dy in 0..rect.height {
            let t = dy as i32;
            let inv = span - t;
            let r = ((top_r * inv + bot_r * t) / span) as u32 & 0xff;
            let g = ((top_g * inv + bot_g * t) / span) as u32 & 0xff;
            let b = ((top_b * inv + bot_b * t) / span) as u32 & 0xff;
            let row_color = (r << 16) | (g << 8) | b;
            let Some(row_start) = (rect.y + dy)
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(row_pixels) = self
                .pixels
                .get_mut(row_start..row_start.saturating_add(rect.width))
            else {
                return;
            };
            row_pixels.fill(row_color);
        }
    }

    pub(crate) fn fill_radial_glow(
        &mut self,
        center_x: i32,
        center_y: i32,
        radius: u32,
        color: u32,
        max_alpha: u8,
    ) {
        if radius == 0 || max_alpha == 0 {
            return;
        }
        let radius_i = radius as i32;
        let bounding = Rect {
            x: (center_x - radius_i).max(0) as usize,
            y: (center_y - radius_i).max(0) as usize,
            width: (radius_i * 2 + 1) as usize,
            height: (radius_i * 2 + 1) as usize,
        }
        .intersect(self.clip_rect);
        if bounding.is_empty() {
            return;
        }

        let r2 = (radius_i as i64).saturating_mul(radius_i as i64);
        const COVERAGE_SCALE: i64 = 1 << 16;
        for y in bounding.y..bounding.y.saturating_add(bounding.height) {
            let dy = y as i64 - center_y as i64;
            let dy2 = dy * dy;
            if dy2 > r2 {
                continue;
            }
            let Some(row_start) = y
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(bounding.x))
            else {
                continue;
            };
            let Some(row_pixels) = self
                .pixels
                .get_mut(row_start..row_start.saturating_add(bounding.width))
            else {
                continue;
            };
            for (offset, pixel) in row_pixels.iter_mut().enumerate() {
                let x = bounding.x + offset;
                let dx = x as i64 - center_x as i64;
                let d2 = dx * dx + dy2;
                if d2 > r2 {
                    continue;
                }
                // Smooth fall-off: coverage = (1 - d²/R²)² in fixed-point Q16.
                let coverage_linear = ((r2 - d2) * COVERAGE_SCALE) / r2;
                let coverage = (coverage_linear * coverage_linear) / COVERAGE_SCALE;
                let alpha = ((max_alpha as i64) * coverage) / COVERAGE_SCALE;
                let alpha = alpha.clamp(0, 255) as u8;
                if alpha == 0 {
                    continue;
                }
                blend_pixel(pixel, color, alpha);
            }
        }
    }

    pub(crate) fn draw_cursor(&mut self, cursor_x: u32, cursor_y: u32, motion: CursorMotion) {
        let sprite_index = motion.sprite_index.min(CURSOR_SPRITE_MAX_DISTANCE) as usize;
        let sprite_area = CURSOR_SPRITE_SIZE
            .saturating_mul(CURSOR_SPRITE_SIZE)
            .saturating_mul(CURSOR_SPRITE_CHANNELS);
        let sprite_offset = sprite_index.saturating_mul(sprite_area);
        let Some(sprite_alpha) =
            CURSOR_SPRITE_ALPHA.get(sprite_offset..sprite_offset.saturating_add(sprite_area))
        else {
            return;
        };

        let (cos_fp, sin_fp) = cursor_motion_rotation(motion);
        let radius = CURSOR_VISUAL_RADIUS as i32;
        let hotspot = (CURSOR_SPRITE_SIZE / 2) as i32;
        let center_x = cursor_x as i32;
        let center_y = cursor_y as i32;
        for local_y in -radius..=radius {
            for local_x in -radius..=radius {
                let source_x = div_round(
                    local_x
                        .saturating_mul(cos_fp)
                        .saturating_add(local_y.saturating_mul(sin_fp)),
                    CURSOR_ROTATION_FP,
                )
                .saturating_add(hotspot);
                let source_y = div_round(
                    local_y
                        .saturating_mul(cos_fp)
                        .saturating_sub(local_x.saturating_mul(sin_fp)),
                    CURSOR_ROTATION_FP,
                )
                .saturating_add(hotspot);
                if source_x < 0
                    || source_y < 0
                    || source_x as usize >= CURSOR_SPRITE_SIZE
                    || source_y as usize >= CURSOR_SPRITE_SIZE
                {
                    continue;
                }
                let alpha_index = (source_y as usize)
                    .saturating_mul(CURSOR_SPRITE_SIZE)
                    .saturating_add(source_x as usize)
                    .saturating_mul(CURSOR_SPRITE_CHANNELS);
                let Some(blue_alpha) = sprite_alpha.get(alpha_index).copied() else {
                    continue;
                };
                let white_alpha = sprite_alpha
                    .get(alpha_index.saturating_add(1))
                    .copied()
                    .unwrap_or_default();
                self.put_pixel_alpha(
                    center_x.saturating_add(local_x),
                    center_y.saturating_add(local_y),
                    COLOR_CURSOR_BLUE,
                    blue_alpha,
                );
                self.put_pixel_alpha(
                    center_x.saturating_add(local_x),
                    center_y.saturating_add(local_y),
                    COLOR_CURSOR_WHITE,
                    white_alpha,
                );
            }
        }
    }

    pub(crate) fn draw_surface(
        &mut self,
        src_pixels: &[u32],
        src_width: usize,
        src_height: usize,
        src_stride_pixels: usize,
        dst_x: usize,
        dst_y: usize,
    ) {
        if src_width == 0 || src_height == 0 {
            return;
        }
        if src_stride_pixels < src_width {
            return;
        }
        let Some(required_len) = src_stride_pixels
            .checked_mul(src_height.saturating_sub(1))
            .and_then(|prefix| prefix.checked_add(src_width))
        else {
            return;
        };
        if src_pixels.len() < required_len {
            return;
        }

        let dst_rect = Rect {
            x: dst_x,
            y: dst_y,
            width: src_width,
            height: src_height,
        }
        .intersect(self.clip_rect);
        if dst_rect.is_empty() {
            return;
        }

        let src_x = dst_rect.x.saturating_sub(dst_x);
        let src_y = dst_rect.y.saturating_sub(dst_y);

        for row in 0..dst_rect.height {
            let Some(src_row) = (src_y + row)
                .checked_mul(src_stride_pixels)
                .and_then(|offset| offset.checked_add(src_x))
            else {
                return;
            };
            let Some(dst_row) = (dst_rect.y + row)
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(dst_rect.x))
            else {
                return;
            };
            let Some(src_end) = src_row.checked_add(dst_rect.width) else {
                return;
            };
            let Some(dst_end) = dst_row.checked_add(dst_rect.width) else {
                return;
            };
            if src_end > src_pixels.len() || dst_end > self.pixels.len() {
                return;
            }
            let src = &src_pixels[src_row..src_end];
            let dst = &mut self.pixels[dst_row..dst_end];
            simd::copy_u32s(src, dst);
        }
    }

    pub(crate) fn blit_alpha_mask(
        &mut self,
        alpha: &[u8],
        src_width: usize,
        src_height: usize,
        dst_x: usize,
        dst_y: usize,
        color: u32,
    ) {
        if src_width == 0 || src_height == 0 {
            return;
        }
        let required_len = match src_width.checked_mul(src_height) {
            Some(len) => len,
            None => return,
        };
        if alpha.len() < required_len {
            return;
        }

        let dst_rect = Rect {
            x: dst_x,
            y: dst_y,
            width: src_width,
            height: src_height,
        }
        .intersect(self.clip_rect);
        if dst_rect.is_empty() {
            return;
        }

        let src_x = dst_rect.x.saturating_sub(dst_x);
        let src_y = dst_rect.y.saturating_sub(dst_y);

        // Row-based access: take one slice per row so the inner loop only
        // sees plain index iteration without per-pixel bounds checks. With
        // ~470 k iterations per shadow blit this used to dominate the drag
        // hot path.
        for row in 0..dst_rect.height {
            let src_row_start = match (src_y + row).checked_mul(src_width) {
                Some(offset) => offset.saturating_add(src_x),
                None => return,
            };
            let dst_row_start = match (dst_rect.y + row).checked_mul(self.stride_pixels) {
                Some(offset) => offset.saturating_add(dst_rect.x),
                None => return,
            };
            let Some(src_row) =
                alpha.get(src_row_start..src_row_start.saturating_add(dst_rect.width))
            else {
                return;
            };
            let Some(dst_row) = self
                .pixels
                .get_mut(dst_row_start..dst_row_start.saturating_add(dst_rect.width))
            else {
                return;
            };
            for (dst_pixel, &alpha_value) in dst_row.iter_mut().zip(src_row.iter()) {
                if alpha_value == 0 {
                    continue;
                }
                blend_pixel(dst_pixel, color, alpha_value);
            }
        }
    }

    fn put_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        if !self.clip_rect.contains(x as u32, y as u32) {
            return;
        }

        let Some(index) = (y as usize)
            .checked_mul(self.stride_pixels)
            .and_then(|offset| offset.checked_add(x as usize))
        else {
            return;
        };
        if let Some(pixel) = self.pixels.get_mut(index) {
            *pixel = color;
        }
    }

    fn put_pixel_alpha(&mut self, x: i32, y: i32, color: u32, alpha: u8) {
        if alpha == 0 || x < 0 || y < 0 || !self.clip_rect.contains(x as u32, y as u32) {
            return;
        }

        let Some(index) = (y as usize)
            .checked_mul(self.stride_pixels)
            .and_then(|offset| offset.checked_add(x as usize))
        else {
            return;
        };
        if let Some(pixel) = self.pixels.get_mut(index) {
            blend_pixel(pixel, color, alpha);
        }
    }
}

fn cursor_motion_rotation(motion: CursorMotion) -> (i32, i32) {
    if motion.sprite_index == 0 || (motion.dx == 0 && motion.dy == 0) {
        return (CURSOR_ROTATION_FP, 0);
    }

    let dx = i64::from(motion.dx);
    let dy = i64::from(motion.dy);
    let length_sq = dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy));
    let length = integer_sqrt(length_sq).max(1) as i64;
    (
        ((dx * i64::from(CURSOR_ROTATION_FP)) / length) as i32,
        ((dy * i64::from(CURSOR_ROTATION_FP)) / length) as i32,
    )
}

fn integer_sqrt(value: i64) -> u64 {
    if value <= 0 {
        return 0;
    }

    let mut estimate = value as u64;
    let mut next = estimate.div_ceil(2);
    while next < estimate {
        estimate = next;
        next = (estimate + value as u64 / estimate) / 2;
    }
    estimate
}

fn div_round(value: i32, divisor: i32) -> i32 {
    if value >= 0 {
        value.saturating_add(divisor / 2) / divisor
    } else {
        value.saturating_sub(divisor / 2) / divisor
    }
}

// Maximum radius we keep in the precomputed corner-profile cache. Anything
// larger falls back to the float path; in practice the UI uses radii ≤ 24.
const MAX_CACHED_RADIUS: usize = 32;

#[derive(Clone, Copy)]
struct CornerRow {
    /// How many columns from the corner this row sits inside the body
    /// (i.e. the corner triangle eats `inset` pixels at each side).
    inset: u16,
    /// Sub-pixel coverage of the boundary pixel, scaled to Q8 (0..=256).
    /// Multiplied into the caller's alpha to produce an antialiased edge.
    aa_alpha_q8: u16,
}

static CORNER_CACHE: OnceLock<Vec<Vec<CornerRow>>> = OnceLock::new();

fn corner_profile(radius: usize) -> &'static [CornerRow] {
    let cache =
        CORNER_CACHE.get_or_init(|| (0..=MAX_CACHED_RADIUS).map(build_corner_rows).collect());
    if let Some(profile) = cache.get(radius) {
        profile.as_slice()
    } else {
        // Cold path for unusually large radii — rebuild on every call.
        // No callers in the current renderer hit this branch.
        Box::leak(build_corner_rows(radius).into_boxed_slice())
    }
}

fn build_corner_rows(radius: usize) -> Vec<CornerRow> {
    if radius == 0 {
        return Vec::new();
    }
    let r = radius;
    let r2 = (r as f32) * (r as f32);
    (0..r)
        .map(|dy| {
            let dy_from_center = (r - dy) as f32;
            let max_extent_sq = r2 - dy_from_center * dy_from_center;
            if max_extent_sq <= 0.0 {
                return CornerRow {
                    inset: r as u16,
                    aa_alpha_q8: 0,
                };
            }
            let max_extent_f = max_extent_sq.sqrt();
            let max_extent = (max_extent_f.floor() as usize).min(r);
            let inset = (r - max_extent) as u16;
            let frac = max_extent_f - max_extent as f32;
            let aa_alpha_q8 = (frac * 256.0).round().clamp(0.0, 255.0) as u16;
            CornerRow { inset, aa_alpha_q8 }
        })
        .collect()
}

fn blend_pixel(dst: &mut u32, src: u32, alpha: u8) {
    if alpha == 0 {
        return;
    }
    if alpha == u8::MAX {
        *dst = src;
        return;
    }

    let inv = 255_u32.saturating_sub(alpha as u32);
    let src_r = (src >> 16) & 0xff;
    let src_g = (src >> 8) & 0xff;
    let src_b = src & 0xff;
    let dst_r = (*dst >> 16) & 0xff;
    let dst_g = (*dst >> 8) & 0xff;
    let dst_b = *dst & 0xff;

    let out_r = (src_r * alpha as u32 + dst_r * inv + 127) / 255;
    let out_g = (src_g * alpha as u32 + dst_g * inv + 127) / 255;
    let out_b = (src_b * alpha as u32 + dst_b * inv + 127) / 255;
    *dst = (out_r << 16) | (out_g << 8) | out_b;
}

impl DrawTarget for SurfaceCanvas<'_> {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            self.put_pixel(
                point.x,
                point.y,
                ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | color.b() as u32,
            );
        }
        Ok(())
    }
}

impl OriginDimensions for SurfaceCanvas<'_> {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

pub(crate) fn cursor_dirty_rect(cursor_x: u32, cursor_y: u32, width: u32, height: u32) -> Rect {
    let radius = CURSOR_VISUAL_RADIUS;
    Rect {
        x: (cursor_x as usize).saturating_sub(radius),
        y: (cursor_y as usize).saturating_sub(radius),
        width: radius.saturating_mul(2).saturating_add(1),
        height: radius.saturating_mul(2).saturating_add(1),
    }
    .intersect(Rect {
        x: 0,
        y: 0,
        width: width as usize,
        height: height as usize,
    })
}

fn sanitized_surface_geometry(
    pixels_len: usize,
    width: u32,
    height: u32,
    stride_pixels: usize,
) -> (u32, u32, Rect) {
    if stride_pixels == 0 || pixels_len == 0 {
        return (0, 0, Rect::empty());
    }

    let requested_width = width as usize;
    let requested_height = height as usize;
    let safe_width = requested_width.min(stride_pixels);
    let safe_height = requested_height.min(pixels_len / stride_pixels);
    let safe_width_u32 = safe_width.min(u32::MAX as usize) as u32;
    let safe_height_u32 = safe_height.min(u32::MAX as usize) as u32;

    (
        safe_width_u32,
        safe_height_u32,
        Rect {
            x: 0,
            y: 0,
            width: safe_width,
            height: safe_height,
        },
    )
}
