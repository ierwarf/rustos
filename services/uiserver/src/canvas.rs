use core::convert::Infallible;

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

    pub(crate) fn stroke_rect(&mut self, rect: Rect, color: u32) {
        if rect.is_empty() {
            return;
        }
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: 1,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y.saturating_add(rect.height.saturating_sub(1)),
                width: rect.width,
                height: 1,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x,
                y: rect.y,
                width: 1,
                height: rect.height,
            },
            color,
        );
        self.fill_rect(
            Rect {
                x: rect.x.saturating_add(rect.width.saturating_sub(1)),
                y: rect.y,
                width: 1,
                height: rect.height,
            },
            color,
        );
    }

    pub(crate) fn fill_pattern_grid(&mut self, rect: Rect, spacing: usize, color: u32, alpha: u8) {
        if spacing == 0 || alpha == 0 {
            return;
        }
        let rect = rect.intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }

        let x_end = rect.x.saturating_add(rect.width);
        let y_end = rect.y.saturating_add(rect.height);

        // Vertical grid lines. The previous implementation called
        // `fill_rect_alpha` per vertical strip, each of which produced a fresh
        // 1-pixel slice per row that always fell below the SIMD threshold and
        // re-entered the scalar blender. That was the dominant cost in the
        // first-frame desktop refresh (~400 ms on TCG). Inline the blend here
        // so each cell is a single pixel of per-pixel arithmetic with no slice
        // bookkeeping or function-call cost in the inner loop.
        let alpha32 = alpha as u32;
        let inv_alpha32 = 255_u32.saturating_sub(alpha32);
        let src_b = color & 0xff;
        let src_g = (color >> 8) & 0xff;
        let src_r = (color >> 16) & 0xff;
        let blend_pixel = |dst: &mut u32| {
            let cur = *dst;
            let cur_b = cur & 0xff;
            let cur_g = (cur >> 8) & 0xff;
            let cur_r = (cur >> 16) & 0xff;
            let out_b = (src_b * alpha32 + cur_b * inv_alpha32) / 255;
            let out_g = (src_g * alpha32 + cur_g * inv_alpha32) / 255;
            let out_r = (src_r * alpha32 + cur_r * inv_alpha32) / 255;
            *dst = (out_r << 16) | (out_g << 8) | out_b;
        };

        for row in rect.y..y_end {
            let Some(row_start) = row
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(row_pixels) =
                self.pixels.get_mut(row_start..row_start.saturating_add(rect.width))
            else {
                return;
            };
            let mut x_offset = 0usize;
            while x_offset < rect.width {
                blend_pixel(&mut row_pixels[x_offset]);
                x_offset = x_offset.saturating_add(spacing);
            }
        }

        let mut y = rect.y;
        while y < y_end {
            let Some(row_start) = y
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(rect.x))
            else {
                return;
            };
            let Some(row_pixels) =
                self.pixels.get_mut(row_start..row_start.saturating_add(rect.width))
            else {
                return;
            };
            for pixel in row_pixels.iter_mut() {
                blend_pixel(pixel);
            }
            y = y.saturating_add(spacing);
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

        for row in 0..dst_rect.height {
            let src_row = match (src_y + row).checked_mul(src_width) {
                Some(offset) => offset.saturating_add(src_x),
                None => return,
            };
            let dst_row = match (dst_rect.y + row).checked_mul(self.stride_pixels) {
                Some(offset) => offset.saturating_add(dst_rect.x),
                None => return,
            };
            for col in 0..dst_rect.width {
                let src_index = src_row.saturating_add(col);
                let dst_index = dst_row.saturating_add(col);
                let Some(alpha_value) = alpha.get(src_index).copied() else {
                    return;
                };
                if alpha_value == 0 {
                    continue;
                }
                let Some(dst_pixel) = self.pixels.get_mut(dst_index) else {
                    return;
                };
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
    let mut next = (estimate + 1) / 2;
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
