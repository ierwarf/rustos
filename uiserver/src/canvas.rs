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
        let clip_rect = Rect {
            x: 0,
            y: 0,
            width: width as usize,
            height: height as usize,
        };

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
        let screen_rect = Rect {
            x: 0,
            y: 0,
            width: width as usize,
            height: height as usize,
        };

        Self {
            pixels,
            width,
            height,
            stride_pixels,
            clip_rect: screen_rect.intersect(clip_rect),
        }
    }

    pub(crate) fn fill_rect(&mut self, rect: Rect, color: u32) {
        let rect = rect.intersect(self.clip_rect);
        if rect.is_empty() {
            return;
        }

        for row in rect.y..rect.y.saturating_add(rect.height) {
            let row_start = row * self.stride_pixels + rect.x;
            let row_end = row_start + rect.width;
            self.pixels[row_start..row_end].fill(color);
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
            let row_start = row * self.stride_pixels + rect.x;
            let row_end = row_start + rect.width;
            simd::blend_solid_bgr(&mut self.pixels[row_start..row_end], color, alpha);
        }
    }

    pub(crate) fn draw_vertical_gradient(&mut self, top: u32, bottom: u32) {
        let top_b = (top & 0xff) as i64;
        let top_g = ((top >> 8) & 0xff) as i64;
        let top_r = ((top >> 16) & 0xff) as i64;
        let bottom_b = (bottom & 0xff) as i64;
        let bottom_g = ((bottom >> 8) & 0xff) as i64;
        let bottom_r = ((bottom >> 16) & 0xff) as i64;
        let denom = self.height.saturating_sub(1).max(1) as i64;
        if self.clip_rect.is_empty() {
            return;
        }

        for y in self.clip_rect.y..self.clip_rect.y.saturating_add(self.clip_rect.height) {
            let y_i64 = y as i64;
            let b = top_b + ((bottom_b - top_b) * y_i64) / denom;
            let g = top_g + ((bottom_g - top_g) * y_i64) / denom;
            let r = top_r + ((bottom_r - top_r) * y_i64) / denom;
            let color = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let row_start = y * self.stride_pixels + self.clip_rect.x;
            let row_end = row_start + self.clip_rect.width;
            self.pixels[row_start..row_end].fill(color);
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
            let src_row = (src_y + row) * src_stride_pixels + src_x;
            let dst_row = (dst_rect.y + row) * self.stride_pixels + dst_rect.x;
            let src = &src_pixels[src_row..src_row + dst_rect.width];
            let dst = &mut self.pixels[dst_row..dst_row + dst_rect.width];
            simd::copy_u32s(src, dst);
        }
    }

    fn put_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return;
        }
        if !self.clip_rect.contains(x as u32, y as u32) {
            return;
        }

        let index = y as usize * self.stride_pixels + x as usize;
        self.pixels[index] = color;
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
