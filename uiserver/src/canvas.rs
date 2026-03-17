use core::convert::Infallible;

use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{OriginDimensions, Size};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{Pixel, RgbColor};

const COLOR_CURSOR_FILL: u32 = 0x00ff_ffff;
const COLOR_CURSOR_OUTLINE: u32 = 0x0000_0000;
const COLOR_CURSOR_SHADOW: u32 = 0x0026_313f;
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
}

pub(crate) struct SurfaceCanvas<'a> {
    pixels: &'a mut [u32],
    width: u32,
    height: u32,
    stride_pixels: usize,
}

impl<'a> SurfaceCanvas<'a> {
    pub(crate) fn new(
        pixels: &'a mut [u32],
        width: u32,
        height: u32,
        stride_pixels: usize,
    ) -> Self {
        Self {
            pixels,
            width,
            height,
            stride_pixels,
        }
    }

    pub(crate) fn fill_rect(&mut self, rect: Rect, color: u32) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }

        let start_x = rect.x.min(self.width as usize);
        let start_y = rect.y.min(self.height as usize);
        let end_x = rect.x.saturating_add(rect.width).min(self.width as usize);
        let end_y = rect.y.saturating_add(rect.height).min(self.height as usize);
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        for row in start_y..end_y {
            let row_start = row * self.stride_pixels + start_x;
            let row_end = row * self.stride_pixels + end_x;
            self.pixels[row_start..row_end].fill(color);
        }
    }

    pub(crate) fn fill_rect_alpha(&mut self, rect: Rect, color: u32, alpha: u8) {
        if rect.width == 0 || rect.height == 0 || alpha == 0 {
            return;
        }
        if alpha == u8::MAX {
            self.fill_rect(rect, color);
            return;
        }

        let start_x = rect.x.min(self.width as usize);
        let start_y = rect.y.min(self.height as usize);
        let end_x = rect.x.saturating_add(rect.width).min(self.width as usize);
        let end_y = rect.y.saturating_add(rect.height).min(self.height as usize);
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        for row in start_y..end_y {
            let row_start = row * self.stride_pixels + start_x;
            let row_end = row * self.stride_pixels + end_x;
            for pixel in &mut self.pixels[row_start..row_end] {
                *pixel = blend_bgr(*pixel, color, alpha);
            }
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

        for y in 0..self.height {
            let y_i64 = y as i64;
            let b = top_b + ((bottom_b - top_b) * y_i64) / denom;
            let g = top_g + ((bottom_g - top_g) * y_i64) / denom;
            let r = top_r + ((bottom_r - top_r) * y_i64) / denom;
            let color = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let row_start = y as usize * self.stride_pixels;
            let row_end = row_start + self.width as usize;
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

    fn put_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
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
            if point.x < 0
                || point.y < 0
                || point.x >= self.width as i32
                || point.y >= self.height as i32
            {
                continue;
            }

            let index = point.y as usize * self.stride_pixels + point.x as usize;
            self.pixels[index] =
                ((color.r() as u32) << 16) | ((color.g() as u32) << 8) | color.b() as u32;
        }
        Ok(())
    }
}

impl OriginDimensions for SurfaceCanvas<'_> {
    fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }
}

fn blend_bgr(dst: u32, src: u32, alpha: u8) -> u32 {
    let alpha = alpha as u32;
    let inv_alpha = 255_u32.saturating_sub(alpha);

    let dst_b = dst & 0xff;
    let dst_g = (dst >> 8) & 0xff;
    let dst_r = (dst >> 16) & 0xff;
    let src_b = src & 0xff;
    let src_g = (src >> 8) & 0xff;
    let src_r = (src >> 16) & 0xff;

    let out_b = (src_b * alpha + dst_b * inv_alpha) / 255;
    let out_g = (src_g * alpha + dst_g * inv_alpha) / 255;
    let out_r = (src_r * alpha + dst_r * inv_alpha) / 255;

    (out_r << 16) | (out_g << 8) | out_b
}
