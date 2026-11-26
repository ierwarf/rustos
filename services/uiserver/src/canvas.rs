use core::convert::Infallible;
use std::borrow::Cow;
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

const COLOR_CURSOR_BLUE: u32 = 0x000e_2678;
const COLOR_CURSOR_WHITE: u32 = 0x00ec_faff;
const CURSOR_ROTATION_FP: i32 = 1024;
const CURSOR_SCALE_NUM: i32 = 3;
const CURSOR_SCALE_DEN: i32 = 2;
const CURSOR_SUBPIXEL_FP: i32 = 256;
pub(crate) const CURSOR_VISUAL_RADIUS: usize = 108;
pub(crate) const CURSOR_TEXTURE_SIZE: usize = CURSOR_VISUAL_RADIUS * 2 + 1;

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

    pub(crate) fn draw_cursor(&mut self, cursor_x: u32, cursor_y: u32, motion: CursorMotion) {
        let center_x = cursor_x as i32;
        let center_y = cursor_y as i32;
        for_each_cursor_pixel(motion, |local_x, local_y, blue_alpha, white_alpha| {
            let px = center_x.saturating_add(local_x);
            let py = center_y.saturating_add(local_y);
            self.put_pixel_alpha(px, py, COLOR_CURSOR_BLUE, blue_alpha);
            self.put_pixel_alpha(px, py, COLOR_CURSOR_WHITE, white_alpha);
        });
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

    /// Composite premultiplied Wayland ARGB pixels onto the opaque canvas.
    pub(crate) fn draw_surface_premultiplied(
        &mut self,
        src: &[u32],
        width: usize,
        height: usize,
        stride: usize,
        x: usize,
        y: usize,
    ) {
        if width == 0 || height == 0 || stride < width {
            return;
        }
        let Some(required) = stride
            .checked_mul(height - 1)
            .and_then(|v| v.checked_add(width))
        else {
            return;
        };
        if src.len() < required {
            return;
        }
        let rect = Rect {
            x,
            y,
            width,
            height,
        }
        .intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }
        let src_x = rect.x - x;
        let src_y = rect.y - y;
        for row in 0..rect.height {
            let Some(source) = (src_y + row)
                .checked_mul(stride)
                .and_then(|offset| offset.checked_add(src_x))
            else {
                return;
            };
            let Some(source_end) = source.checked_add(rect.width) else {
                return;
            };
            let Some(src_row) = src.get(source..source_end) else {
                return;
            };
            let Some(dest) = (rect.y + row)
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(dest_end) = dest.checked_add(rect.width) else {
                return;
            };
            let Some(dst) = self.pixels.get_mut(dest..dest_end) else {
                return;
            };
            simd::blend_premultiplied_bgr(src_row, dst);
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
            simd::blend_alpha_mask_bgr(src_row, dst_row, color);
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

/// The most recent cursor raster. The texture is a pure function of the sprite
/// index and the quantized rotation, and the cursor is stationary or moving in
/// a steady direction for most frames, so repeating the 47k-sample bilinear
/// pass every scene build is wasted work.
type CursorRasterKey = (usize, i32, i32);
static CURSOR_RASTER_CACHE: std::sync::Mutex<Option<(CursorRasterKey, Vec<u32>)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn render_cursor_bgra(pixels: &mut [u32], motion: CursorMotion) -> bool {
    if pixels.len() != CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE {
        return false;
    }
    render_cursor_bgra_strided(pixels, CURSOR_TEXTURE_SIZE, motion)
}

/// Paint into an atlas region without a temporary packed image. Only the
/// CURSOR_TEXTURE_SIZE visible pixels of each row are touched; padding and
/// adjacent atlas regions remain intact. The final row need not have padding.
pub(crate) fn render_cursor_bgra_strided(
    pixels: &mut [u32],
    stride_pixels: usize,
    motion: CursorMotion,
) -> bool {
    if stride_pixels < CURSOR_TEXTURE_SIZE {
        return false;
    }
    let Some(required) = stride_pixels
        .checked_mul(CURSOR_TEXTURE_SIZE - 1)
        .and_then(|prefix| prefix.checked_add(CURSOR_TEXTURE_SIZE))
    else {
        return false;
    };
    if pixels.len() < required {
        return false;
    }
    let sprite_index = (motion.sprite_index as usize).min(CURSOR_SPRITE_MAX_DISTANCE as usize);
    let (cos_fp, sin_fp) = cursor_motion_rotation(motion);
    let key = (sprite_index, cos_fp, sin_fp);
    if let Ok(mut cache) = CURSOR_RASTER_CACHE.lock() {
        let (cached_key, cached) = cache.get_or_insert_with(|| {
            (
                (usize::MAX, 0, 0),
                vec![0; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE],
            )
        });
        if *cached_key != key {
            *cached_key = (usize::MAX, 0, 0);
            if !rasterize_cursor_bgra(cached, CURSOR_TEXTURE_SIZE, motion) {
                return false;
            }
            *cached_key = key;
        }
        if stride_pixels == CURSOR_TEXTURE_SIZE {
            simd::copy_u32s(cached, &mut pixels[..required]);
        } else {
            for row in 0..CURSOR_TEXTURE_SIZE {
                let src = row * CURSOR_TEXTURE_SIZE;
                let dst = row * stride_pixels;
                simd::copy_u32s(
                    &cached[src..src + CURSOR_TEXTURE_SIZE],
                    &mut pixels[dst..dst + CURSOR_TEXTURE_SIZE],
                );
            }
        }
        return true;
    }
    // Preserve the uncached rendering path if another caller poisoned the cache.
    rasterize_cursor_bgra(pixels, stride_pixels, motion)
}

fn rasterize_cursor_bgra(
    pixels: &mut [u32],
    stride_pixels: usize,
    motion: CursorMotion,
) -> bool {
    for row in 0..CURSOR_TEXTURE_SIZE {
        let start = row * stride_pixels;
        pixels[start..start + CURSOR_TEXTURE_SIZE].fill(0);
    }
    let radius = CURSOR_VISUAL_RADIUS as i32;
    for_each_cursor_pixel(motion, |local_x, local_y, blue_alpha, white_alpha| {
        let index =
            (local_y + radius) as usize * stride_pixels + (local_x + radius) as usize;
        premultiplied_source_over(&mut pixels[index], COLOR_CURSOR_BLUE, blue_alpha);
        premultiplied_source_over(&mut pixels[index], COLOR_CURSOR_WHITE, white_alpha);
    })
}

fn for_each_cursor_pixel(
    motion: CursorMotion,
    mut emit: impl FnMut(i32, i32, u8, u8),
) -> bool {
    let sprite_index = motion.sprite_index.min(CURSOR_SPRITE_MAX_DISTANCE) as usize;
    let sprite_area = CURSOR_SPRITE_SIZE
        .saturating_mul(CURSOR_SPRITE_SIZE)
        .saturating_mul(CURSOR_SPRITE_CHANNELS);
    let sprite_offset = sprite_index.saturating_mul(sprite_area);
    let Some(sprite_alpha) =
        CURSOR_SPRITE_ALPHA.get(sprite_offset..sprite_offset.saturating_add(sprite_area))
    else {
        return false;
    };
    let (cos_fp, sin_fp) = cursor_motion_rotation(motion);
    let radius = CURSOR_VISUAL_RADIUS as i32;
    let hotspot_fp = ((CURSOR_SPRITE_SIZE / 2) as i32).saturating_mul(CURSOR_SUBPIXEL_FP);
    let divisor = CURSOR_ROTATION_FP.saturating_mul(CURSOR_SCALE_NUM);
    let factor = CURSOR_SCALE_DEN.saturating_mul(CURSOR_SUBPIXEL_FP);
    for local_y in -radius..=radius {
        let mut rot_x = (-radius)
            .saturating_mul(cos_fp)
            .saturating_add(local_y.saturating_mul(sin_fp));
        let mut rot_y = local_y
            .saturating_mul(cos_fp)
            .saturating_sub((-radius).saturating_mul(sin_fp));
        for local_x in -radius..=radius {
            let sx_fp = div_round(rot_x.saturating_mul(factor), divisor).saturating_add(hotspot_fp);
            let sy_fp = div_round(rot_y.saturating_mul(factor), divisor).saturating_add(hotspot_fp);
            let (blue_alpha, white_alpha) =
                sample_cursor_alpha_bilinear(sprite_alpha, sx_fp, sy_fp);
            if blue_alpha != 0 || white_alpha != 0 {
                emit(local_x, local_y, blue_alpha, white_alpha);
            }
            rot_x = rot_x.saturating_add(cos_fp);
            rot_y = rot_y.saturating_sub(sin_fp);
        }
    }
    true
}

fn sample_cursor_alpha_bilinear(sprite_alpha: &[u8], sx_fp: i32, sy_fp: i32) -> (u8, u8) {
    let x0 = sx_fp.div_euclid(CURSOR_SUBPIXEL_FP);
    let y0 = sy_fp.div_euclid(CURSOR_SUBPIXEL_FP);
    if x0 < -1
        || y0 < -1
        || x0 >= CURSOR_SPRITE_SIZE as i32
        || y0 >= CURSOR_SPRITE_SIZE as i32
    {
        return (0, 0);
    }
    let fx = sx_fp.rem_euclid(CURSOR_SUBPIXEL_FP);
    let fy = sy_fp.rem_euclid(CURSOR_SUBPIXEL_FP);
    let wx0 = CURSOR_SUBPIXEL_FP - fx;
    let wy0 = CURSOR_SUBPIXEL_FP - fy;
    let weights = [
        wx0.saturating_mul(wy0),
        fx.saturating_mul(wy0),
        wx0.saturating_mul(fy),
        fx.saturating_mul(fy),
    ];
    let coords = [(x0, y0), (x0 + 1, y0), (x0, y0 + 1), (x0 + 1, y0 + 1)];
    let mut blue_acc = 0_i32;
    let mut white_acc = 0_i32;
    for ((sx, sy), weight) in coords.into_iter().zip(weights) {
        if sx < 0
            || sy < 0
            || sx as usize >= CURSOR_SPRITE_SIZE
            || sy as usize >= CURSOR_SPRITE_SIZE
        {
            continue;
        }
        let alpha_index = (sy as usize)
            .saturating_mul(CURSOR_SPRITE_SIZE)
            .saturating_add(sx as usize)
            .saturating_mul(CURSOR_SPRITE_CHANNELS);
        blue_acc += i32::from(sprite_alpha[alpha_index]) * weight;
        white_acc += i32::from(sprite_alpha[alpha_index + 1]) * weight;
    }
    let blue = ((blue_acc + (CURSOR_SUBPIXEL_FP * CURSOR_SUBPIXEL_FP / 2)) >> 16).min(255) as u8;
    let white = ((white_acc + (CURSOR_SUBPIXEL_FP * CURSOR_SUBPIXEL_FP / 2)) >> 16).min(255) as u8;
    (blue, white)
}

fn premultiplied_source_over(destination: &mut u32, color: u32, alpha: u8) {
    if alpha == 0 {
        return;
    }
    let alpha = u32::from(alpha);
    let inverse = 255 - alpha;
    let dst = *destination;
    let destination_alpha = dst >> 24;
    let source_r = ((color >> 16) & 0xff) * alpha / 255;
    let source_g = ((color >> 8) & 0xff) * alpha / 255;
    let source_b = (color & 0xff) * alpha / 255;
    let out_r = source_r + div255_round(((dst >> 16) & 0xff) * inverse);
    let out_g = source_g + div255_round(((dst >> 8) & 0xff) * inverse);
    let out_b = source_b + div255_round((dst & 0xff) * inverse);
    let out_alpha = alpha + div255_round(destination_alpha * inverse);
    *destination = (out_alpha.min(255) << 24)
        | (out_r.min(255) << 16)
        | (out_g.min(255) << 8)
        | out_b.min(255);
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

fn corner_profile(radius: usize) -> Cow<'static, [CornerRow]> {
    let cache =
        CORNER_CACHE.get_or_init(|| (0..=MAX_CACHED_RADIUS).map(build_corner_rows).collect());
    if let Some(profile) = cache.get(radius) {
        Cow::Borrowed(profile.as_slice())
    } else {
        Cow::Owned(build_corner_rows(radius))
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

/// Exact `(value + 127) / 255` for `value in 0..=65025` without hardware division.
#[inline]
pub(crate) fn div255_round(value: u32) -> u32 {
    let t = value + 128;
    (t + (t >> 8)) >> 8
}

fn blend_pixel(dst: &mut u32, src: u32, alpha: u8) {
    if alpha == 0 {
        return;
    }
    if alpha == u8::MAX {
        *dst = src;
        return;
    }

    let alpha = u32::from(alpha);
    let inv = 255 - alpha;
    let src_r = (src >> 16) & 0xff;
    let src_g = (src >> 8) & 0xff;
    let src_b = src & 0xff;
    let dst_v = *dst;
    let dst_r = (dst_v >> 16) & 0xff;
    let dst_g = (dst_v >> 8) & 0xff;
    let dst_b = dst_v & 0xff;

    let out_r = div255_round(src_r * alpha + dst_r * inv);
    let out_g = div255_round(src_g * alpha + dst_g * inv);
    let out_b = div255_round(src_b * alpha + dst_b * inv);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strided_cursor_matches_packed_pixels_without_touching_padding() {
        let mut packed = vec![0; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
        let motion = CursorMotion::stationary();
        assert!(render_cursor_bgra(&mut packed, motion));
        let stride = CURSOR_TEXTURE_SIZE + 19;
        let required = stride * (CURSOR_TEXTURE_SIZE - 1) + CURSOR_TEXTURE_SIZE;
        let mut strided = vec![0xdead_beef; required + 11];
        assert!(render_cursor_bgra_strided(&mut strided, stride, motion));
        for row in 0..CURSOR_TEXTURE_SIZE {
            let start = row * stride;
            assert_eq!(
                &strided[start..start + CURSOR_TEXTURE_SIZE],
                &packed[row * CURSOR_TEXTURE_SIZE..(row + 1) * CURSOR_TEXTURE_SIZE],
            );
            let padding_end = ((row + 1) * stride).min(strided.len());
            assert!(strided[start + CURSOR_TEXTURE_SIZE..padding_end]
                .iter()
                .all(|pixel| *pixel == 0xdead_beef));
        }
        let before = strided.clone();
        assert!(!render_cursor_bgra_strided(&mut strided[..required - 1], stride, motion));
        assert_eq!(strided, before);
        assert!(!render_cursor_bgra_strided(&mut strided, usize::MAX, motion));
        assert_eq!(strided, before);
    }

    #[test]
    fn cursor_texture_scales_sprite_one_point_five_times_with_navy_and_white_center() {
        assert_eq!(CURSOR_VISUAL_RADIUS, 108);
        assert_eq!(CURSOR_TEXTURE_SIZE, 217);
        let mut pixels = vec![0_u32; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
        assert!(render_cursor_bgra(&mut pixels, CursorMotion::stationary()));

        let center = pixels[CURSOR_VISUAL_RADIUS * CURSOR_TEXTURE_SIZE + CURSOR_VISUAL_RADIUS];
        let center_rgb = center & 0x00ff_ffff;
        assert_eq!(center >> 24, 0xff);
        assert_eq!(center_rgb, COLOR_CURSOR_WHITE);

        let mut min_x = usize::MAX;
        let mut max_x = 0_usize;
        let mut min_y = usize::MAX;
        let mut max_y = 0_usize;
        for y in 0..CURSOR_TEXTURE_SIZE {
            for x in 0..CURSOR_TEXTURE_SIZE {
                if pixels[y * CURSOR_TEXTURE_SIZE + x] != 0 {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        let mut src_min_x = usize::MAX;
        let mut src_max_x = 0_usize;
        let mut src_min_y = usize::MAX;
        let mut src_max_y = 0_usize;
        for y in 0..CURSOR_SPRITE_SIZE {
            for x in 0..CURSOR_SPRITE_SIZE {
                let idx = (y * CURSOR_SPRITE_SIZE + x) * CURSOR_SPRITE_CHANNELS;
                if CURSOR_SPRITE_ALPHA[idx] != 0 || CURSOR_SPRITE_ALPHA[idx + 1] != 0 {
                    src_min_x = src_min_x.min(x);
                    src_max_x = src_max_x.max(x);
                    src_min_y = src_min_y.min(y);
                    src_max_y = src_max_y.max(y);
                }
            }
        }
        let unscaled_w = src_max_x - src_min_x + 1;
        let unscaled_h = src_max_y - src_min_y + 1;
        assert!(max_x - min_x + 1 >= unscaled_w * 3 / 2);
        assert!(max_y - min_y + 1 >= unscaled_h * 3 / 2);
    }

    #[test]
    fn cached_cursor_raster_equals_a_fresh_raster() {
        let moving = CursorMotion { dx: 7, dy: -3, sprite_index: 2 };
        let mut first = vec![0_u32; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
        let mut second = first.clone();
        assert!(render_cursor_bgra(&mut first, moving));
        assert!(render_cursor_bgra(&mut second, moving));
        assert_eq!(first, second);
        let mut other = vec![0_u32; first.len()];
        assert!(render_cursor_bgra(&mut other, CursorMotion::stationary()));
        assert!(render_cursor_bgra(&mut second, moving));
        assert_eq!(first, second);
    }

    #[test]
    fn div255_round_matches_exact_rounding_over_full_u8_product_range() {
        for value in 0..=255_u32 * 255 {
            assert_eq!(div255_round(value), (value + 127) / 255, "value={value}");
        }
    }

    #[test]
    fn draw_surface_premultiplied_handles_transparent_opaque_and_clipped_pixels() {
        let mut dst = vec![0x0020_4060_u32; 4 * 4];
        let src = [
            0x0000_0000_u32,
            0xff12_3456_u32,
            0x8040_2010_u32,
            0xffaa_bbcc_u32,
        ];
        {
            let mut canvas = SurfaceCanvas::with_clip(
                &mut dst,
                4,
                4,
                4,
                Rect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                },
            );
            canvas.draw_surface_premultiplied(&src, 2, 2, 2, 1, 1);
            // Empty clip / zero-size rect is a safe no-op.
            canvas.draw_surface_premultiplied(&src, 2, 2, 2, 10, 10);
        }
        assert_eq!(dst[5], 0x0020_4060);
        assert_eq!(dst[6], 0x0012_3456);
        assert_eq!(dst[2 * 4 + 2], 0x00aa_bbcc);
        let blended = dst[9];
        let inv = 255 - 0x80_u32;
        let expected_r = (0x40 + (0x20 * inv + 127) / 255).min(255);
        let expected_g = (0x20 + (0x40 * inv + 127) / 255).min(255);
        let expected_b = (0x10 + (0x60 * inv + 127) / 255).min(255);
        assert_eq!(blended, (expected_r << 16) | (expected_g << 8) | expected_b);
    }

    #[test]
    fn large_radius_rounded_rect_uses_unleaked_profile() {
        let mut pixels = vec![0_u32; 96 * 96];
        let mut canvas = SurfaceCanvas::new(&mut pixels, 96, 96, 96);
        canvas.fill_rounded_rect_alpha(
            Rect {
                x: 4,
                y: 4,
                width: 88,
                height: 88,
            },
            0x00ff_ffff,
            200,
            40,
        );
        assert_eq!(pixels[4 * 96 + 4], 0);
        assert_ne!(pixels[48 * 96 + 48], 0);
    }
}
