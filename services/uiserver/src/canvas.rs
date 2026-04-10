use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Size};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{Pixel, RgbColor};

use crate::simd;

const COLOR_CURSOR_FILL: u32 = 0x00ff_ffff;
const COLOR_CURSOR_OUTLINE: u32 = 0x0000_0000;
const COLOR_CURSOR_SHADOW: u32 = 0x0026_313f;
const CURSOR_BITMAP_WIDTH: usize = 32;
const CURSOR_BITMAP_HEIGHT: usize = 32;
const CURSOR_SHADOW_OFFSET: usize = 1;
const CURSOR_BITMAP: [&[u8]; 32] = [
    b"X...............................",
    b"XX..............................",
    b"XXX.............................",
    b"XXXX............................",
    b"XXOXX...........................",
    b"XXOOXX..........................",
    b"XXOOOXX.........................",
    b"XXOOOOXX........................",
    b"XXOOOOOXX.......................",
    b"XXOOOOOOXX......................",
    b"XXOOOOOOOXX.....................",
    b"XXOOOOOOOOXX....................",
    b"XXOOOOOOOOOXX...................",
    b"XXOOOOOOOOOOXX..................",
    b"XXOOOOOOOOOOOXX.................",
    b"XXOOOOOOOOOOOOXX................",
    b"XXOOOOOOOOOOOOOXX...............",
    b"XXOOOOOOOOOOOOOOXX..............",
    b"XXOOOOOOOOOOOOOOOXX.............",
    b"XXOOOOOOOOOOOOOOOOXX............",
    b"XXOOOOOOXXXXXXXXXXXXX...........",
    b"XXOOOOOXXXXXXXXXXXXXXX..........",
    b"XXOOOOXX........................",
    b"XXOOOXX.........................",
    b"XXOOXX..........................",
    b"XXOXX...........................",
    b"XXXX............................",
    b"XXX.............................",
    b"XX..............................",
    b"X...............................",
    b"................................",
    b"................................",
];

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
        let mut x = rect.x;
        while x < x_end {
            self.fill_rect_alpha(
                Rect {
                    x,
                    y: rect.y,
                    width: 1,
                    height: rect.height,
                },
                color,
                alpha,
            );
            x = x.saturating_add(spacing);
        }

        let mut y = rect.y;
        while y < y_end {
            self.fill_rect_alpha(
                Rect {
                    x: rect.x,
                    y,
                    width: rect.width,
                    height: 1,
                },
                color,
                alpha,
            );
            y = y.saturating_add(spacing);
        }
    }

    pub(crate) fn draw_cursor(&mut self, cursor_x: u32, cursor_y: u32) {
        let base_x = cursor_x as i32;
        let base_y = cursor_y as i32;

        self.draw_cursor_layer(base_x + 1, base_y + 1, None, COLOR_CURSOR_SHADOW);
        self.draw_cursor_layer(base_x, base_y, Some(b'X'), COLOR_CURSOR_OUTLINE);
        self.draw_cursor_layer(base_x, base_y, Some(b'O'), COLOR_CURSOR_FILL);
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

    fn draw_cursor_layer(&mut self, base_x: i32, base_y: i32, only: Option<u8>, color: u32) {
        for (row, line) in CURSOR_BITMAP.iter().enumerate() {
            for (col, &pixel) in line.iter().enumerate() {
                if pixel == b'.' {
                    continue;
                }
                if let Some(expected) = only {
                    if pixel != expected {
                        continue;
                    }
                }
                self.put_pixel(base_x + col as i32, base_y + row as i32, color);
            }
        }
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
    Rect {
        x: cursor_x as usize,
        y: cursor_y as usize,
        width: CURSOR_BITMAP_WIDTH + CURSOR_SHADOW_OFFSET,
        height: CURSOR_BITMAP_HEIGHT + CURSOR_SHADOW_OFFSET,
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
