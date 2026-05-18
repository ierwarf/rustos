// RING3-MIGRATION-REFERENCE START: commercial-max displayd/uiserver should own normal
// framebuffer allocation, format conversion, damage tracking, and presentation policy.
// Ring0 keeps boot framebuffer discovery plus boot/panic emergency output.
use core::convert::Infallible;
use core::ptr;

use boot_protocol::{BootPixelFormat, FramebufferInfo};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::Size;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{OriginDimensions, Pixel, RgbColor};
const DIRTY_TILE_WIDTH: usize = 32;
const DIRTY_TILE_HEIGHT: usize = 32;
pub(crate) const MAX_FRAMEBUFFER_WIDTH: usize = 7680;
pub(crate) const MAX_FRAMEBUFFER_HEIGHT: usize = 4320;
const MAX_DIRTY_COLS: usize = MAX_FRAMEBUFFER_WIDTH.div_ceil(DIRTY_TILE_WIDTH);
const MAX_DIRTY_ROWS: usize = MAX_FRAMEBUFFER_HEIGHT.div_ceil(DIRTY_TILE_HEIGHT);
const ENABLE_FRAMEBUFFER_DOUBLE_BUFFER: bool = false;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FramebufferRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl FramebufferRect {
    #[allow(dead_code)]
    pub(crate) const fn empty() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    pub(crate) fn clip(
        framebuffer: &Framebuffer,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }

        let x0 = x.max(0).min(framebuffer.width as i64) as usize;
        let y0 = y.max(0).min(framebuffer.height as i64) as usize;
        let x1 = x
            .saturating_add(width as i64)
            .max(0)
            .min(framebuffer.width as i64) as usize;
        let y1 = y
            .saturating_add(height as i64)
            .max(0)
            .min(framebuffer.height as i64) as usize;

        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        Some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }

    pub(crate) fn intersection(self, other: Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self
            .x
            .saturating_add(self.width)
            .min(other.x.saturating_add(other.width));
        let y1 = self
            .y
            .saturating_add(self.height)
            .min(other.y.saturating_add(other.height));

        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        Some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

pub(crate) struct Framebuffer {
    front_base: *mut u8,
    back_base: *mut u8,
    size: usize,
    width: usize,
    height: usize,
    stride_bytes: usize,
    bpp: usize,
    format: BootPixelFormat,
    use_double_buffer: bool,
    dirty_cols: usize,
    dirty_rows: usize,
    dirty_any: bool,
    dirty_tiles: [[bool; MAX_DIRTY_COLS]; MAX_DIRTY_ROWS],
}

unsafe impl Send for Framebuffer {}

impl Framebuffer {
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub(crate) fn height(&self) -> usize {
        self.height
    }

    pub(crate) fn stride_bytes(&self) -> usize {
        self.stride_bytes
    }

    pub(crate) fn bytes_per_pixel(&self) -> usize {
        self.bpp
    }

    pub(crate) fn clip_rect(
        &self,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
    ) -> Option<FramebufferRect> {
        FramebufferRect::clip(self, x, y, width, height)
    }

    pub(crate) fn color_bytes(&self, color: Rgb888) -> (u8, u8, u8) {
        match self.format {
            BootPixelFormat::Rgb => (color.r(), color.g(), color.b()),
            _ => (color.b(), color.g(), color.r()),
        }
    }

    pub(crate) fn fill_rect(
        &mut self,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
        color: Rgb888,
        alpha: u8,
    ) {
        if alpha == 0 {
            return;
        }

        let Some(rect) = self.clip_rect(x, y, width, height) else {
            return;
        };
        let cols = rect.width;
        let rows = rect.height;
        let Some(start) = rect.y.checked_mul(self.stride_bytes).and_then(|v| {
            rect.x
                .checked_mul(self.bpp)
                .and_then(|xoff| v.checked_add(xoff))
        }) else {
            return;
        };

        let base = self.active_buffer();
        let (c0, c1, c2) = self.color_bytes(color);

        unsafe {
            let mut row_ptr = base.add(start);
            if alpha == 255 {
                if self.bpp == 4 {
                    let px32 = u32::from_le_bytes([c0, c1, c2, 0]);
                    let aligned = (row_ptr as usize & 0x3) == 0;
                    for _ in 0..rows {
                        let mut p = row_ptr as *mut u32;
                        let mut remaining = cols;
                        while remaining >= 4 {
                            if aligned {
                                ptr::write(p, px32);
                                ptr::write(p.add(1), px32);
                                ptr::write(p.add(2), px32);
                                ptr::write(p.add(3), px32);
                            } else {
                                ptr::write_unaligned(p, px32);
                                ptr::write_unaligned(p.add(1), px32);
                                ptr::write_unaligned(p.add(2), px32);
                                ptr::write_unaligned(p.add(3), px32);
                            }
                            p = p.add(4);
                            remaining -= 4;
                        }
                        while remaining > 0 {
                            if aligned {
                                ptr::write(p, px32);
                            } else {
                                ptr::write_unaligned(p, px32);
                            }
                            p = p.add(1);
                            remaining -= 1;
                        }
                        row_ptr = row_ptr.add(self.stride_bytes);
                    }
                } else {
                    for _ in 0..rows {
                        let mut p = row_ptr;
                        for _ in 0..cols {
                            ptr::write(p, c0);
                            ptr::write(p.add(1), c1);
                            ptr::write(p.add(2), c2);
                            p = p.add(3);
                        }
                        row_ptr = row_ptr.add(self.stride_bytes);
                    }
                }
                self.mark_dirty_rect(rect);
                return;
            }

            let a = alpha as u16;
            let inv = 256u16 - a;
            for _ in 0..rows {
                let mut p = row_ptr;
                for _ in 0..cols {
                    let d0 = ptr::read(p);
                    let d1 = ptr::read(p.add(1));
                    let d2 = ptr::read(p.add(2));
                    ptr::write(p, (((c0 as u16 * a) + (d0 as u16 * inv)) >> 8) as u8);
                    ptr::write(p.add(1), (((c1 as u16 * a) + (d1 as u16 * inv)) >> 8) as u8);
                    ptr::write(p.add(2), (((c2 as u16 * a) + (d2 as u16 * inv)) >> 8) as u8);
                    p = p.add(self.bpp);
                }
                row_ptr = row_ptr.add(self.stride_bytes);
            }
        }

        self.mark_dirty_rect(rect);
    }

    pub(crate) fn fill(&mut self, color: Rgb888) {
        self.fill_rect(0, 0, self.width as u32, self.height as u32, color, 255);
    }

    pub(crate) fn scroll_rect_up(
        &mut self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        rows: usize,
        clear_color: Rgb888,
    ) {
        if rows == 0 || width == 0 || height == 0 || x >= self.width || y >= self.height {
            return;
        }
        let width = width.min(self.width - x);
        let height = height.min(self.height - y);
        let rows = rows.min(height);
        if rows == 0 {
            return;
        }

        let rect = FramebufferRect {
            x,
            y,
            width,
            height,
        };
        let Some((offset, row_bytes)) = self.rect_copy_bounds(rect) else {
            return;
        };
        let copy_height = height.saturating_sub(rows);

        if copy_height != 0 {
            unsafe {
                let base = self.active_buffer();
                let mut src = base.add(offset + rows * self.stride_bytes);
                let mut dst = base.add(offset);
                for _ in 0..copy_height {
                    for byte in 0..row_bytes {
                        let value = ptr::read(src.add(byte));
                        ptr::write(dst.add(byte), value);
                    }
                    src = src.add(self.stride_bytes);
                    dst = dst.add(self.stride_bytes);
                }
            }
            self.mark_dirty_rect(FramebufferRect {
                x,
                y,
                width,
                height: copy_height,
            });
        }

        self.fill_rect(
            x as i64,
            (y + copy_height) as i64,
            width as u32,
            rows as u32,
            clear_color,
            255,
        );
    }

    pub(crate) fn draw_pixel(&mut self, x: usize, y: usize, color: Rgb888, alpha: u8) {
        if alpha == 0 || x >= self.width || y >= self.height {
            return;
        }

        let Some(idx) = self.pixel_index(x, y) else {
            return;
        };

        self.write_pixel(self.active_buffer(), idx, color, alpha);

        self.mark_dirty_tile_for_point(x, y);
    }

    pub(crate) fn draw_bgra8888_frame_from_kernel(
        &mut self,
        src_ptr: *const u8,
        width: usize,
        height: usize,
        stride_bytes: usize,
    ) -> bool {
        if width != self.width || height != self.height {
            return false;
        }

        let Some(min_stride) = width.checked_mul(4) else {
            return false;
        };
        if stride_bytes < min_stride {
            return false;
        }
        if self
            .rect_copy_bounds(FramebufferRect {
                x: 0,
                y: 0,
                width,
                height,
            })
            .is_none()
        {
            return false;
        }

        unsafe {
            let mut src_row = src_ptr;
            let mut dst_row = self.active_buffer();
            for _ in 0..height {
                self.blit_bgra8888_row(dst_row, src_row, width);
                src_row = src_row.add(stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }

        self.mark_all_dirty();
        true
    }

    pub(crate) fn draw_bgra8888_frame_rect_from_kernel(
        &mut self,
        src_ptr: *const u8,
        width: usize,
        height: usize,
        stride_bytes: usize,
        rect: FramebufferRect,
    ) -> bool {
        if width != self.width || height != self.height {
            return false;
        }

        let Some(min_stride) = width.checked_mul(4) else {
            return false;
        };
        if stride_bytes < min_stride {
            return false;
        }

        let Some(rect) = rect.intersection(FramebufferRect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }) else {
            return true;
        };
        if self.rect_copy_bounds(rect).is_none() {
            return false;
        }

        unsafe {
            let mut dst_row = self
                .active_buffer()
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            let mut src_row = src_ptr.add(rect.y * stride_bytes + rect.x * 4);
            for _ in 0..rect.height {
                self.blit_bgra8888_row(dst_row, src_row, rect.width);
                src_row = src_row.add(stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }

        self.mark_dirty_rect(rect);
        true
    }

    pub(crate) fn draw_bgra8888_rect_from_kernel(
        &mut self,
        src_ptr: *const u8,
        stride_bytes: usize,
        rect: FramebufferRect,
    ) -> bool {
        if rect.width == 0 || rect.height == 0 {
            return true;
        }
        if rect.x.saturating_add(rect.width) > self.width
            || rect.y.saturating_add(rect.height) > self.height
        {
            return false;
        }

        let Some(min_stride) = rect.width.checked_mul(4) else {
            return false;
        };
        if stride_bytes < min_stride || self.rect_copy_bounds(rect).is_none() {
            return false;
        }

        unsafe {
            let mut dst_row = self
                .active_buffer()
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            let mut src_row = src_ptr;
            for _ in 0..rect.height {
                self.blit_bgra8888_row(dst_row, src_row, rect.width);
                src_row = src_row.add(stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }

        self.mark_dirty_rect(rect);
        true
    }

    pub(crate) fn present_scene(&mut self) -> bool {
        if !self.use_double_buffer || !self.dirty_any {
            return true;
        }

        for tile_row in 0..self.dirty_rows {
            let mut tile_col = 0;
            while tile_col < self.dirty_cols {
                if !self.dirty_tiles[tile_row][tile_col] {
                    tile_col += 1;
                    continue;
                }

                let start_col = tile_col;
                while tile_col < self.dirty_cols && self.dirty_tiles[tile_row][tile_col] {
                    tile_col += 1;
                }

                if !self
                    .copy_back_to_front_rect(self.dirty_rect_for_run(tile_row, start_col, tile_col))
                {
                    self.mark_all_dirty();
                    return false;
                }
                for col in start_col..tile_col {
                    self.dirty_tiles[tile_row][col] = false;
                }
            }
        }

        self.dirty_any = false;
        true
    }

    pub(crate) fn debug_sample_buffers(&self) -> ([u8; 4], [u8; 4]) {
        (
            self.debug_sample_bytes(self.active_buffer()),
            self.debug_sample_bytes(self.front_base),
        )
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    pub(crate) fn debug_uses_double_buffer(&self) -> bool {
        self.use_double_buffer
    }

    fn active_buffer(&self) -> *mut u8 {
        if self.use_double_buffer {
            self.back_base
        } else {
            self.front_base
        }
    }

    fn debug_sample_bytes(&self, base: *mut u8) -> [u8; 4] {
        let mut sample = [0_u8; 4];
        let byte_len = self.size.min(sample.len());
        unsafe {
            for (index, byte) in sample.iter_mut().enumerate().take(byte_len) {
                *byte = ptr::read_volatile(base.add(index));
            }
        }
        sample
    }

    fn rect_copy_bounds(&self, rect: FramebufferRect) -> Option<(usize, usize)> {
        if rect.width == 0 || rect.height == 0 {
            return None;
        }

        let row_bytes = rect.width.checked_mul(self.bpp)?;
        let start = rect.y.checked_mul(self.stride_bytes).and_then(|offset| {
            rect.x
                .checked_mul(self.bpp)
                .and_then(|xoff| offset.checked_add(xoff))
        })?;
        let last_row_offset = rect
            .height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(self.stride_bytes))?;
        let end = start
            .checked_add(last_row_offset)
            .and_then(|offset| offset.checked_add(row_bytes))?;
        if end > self.size {
            return None;
        }

        Some((start, row_bytes))
    }

    fn mark_all_dirty(&mut self) {
        if !self.use_double_buffer {
            return;
        }

        for row in 0..self.dirty_rows {
            for col in 0..self.dirty_cols {
                self.dirty_tiles[row][col] = true;
            }
        }
        self.dirty_any = true;
    }

    fn mark_dirty_rect(&mut self, rect: FramebufferRect) {
        if !self.use_double_buffer || rect.width == 0 || rect.height == 0 {
            return;
        }

        let start_col = rect.x / DIRTY_TILE_WIDTH;
        let end_col = (rect.x + rect.width - 1) / DIRTY_TILE_WIDTH;
        let start_row = rect.y / DIRTY_TILE_HEIGHT;
        let end_row = (rect.y + rect.height - 1) / DIRTY_TILE_HEIGHT;

        for row in start_row..=end_row.min(self.dirty_rows.saturating_sub(1)) {
            for col in start_col..=end_col.min(self.dirty_cols.saturating_sub(1)) {
                self.dirty_tiles[row][col] = true;
            }
        }
        self.dirty_any = true;
    }

    fn mark_dirty_tile_for_point(&mut self, x: usize, y: usize) {
        if !self.use_double_buffer || self.dirty_cols == 0 || self.dirty_rows == 0 {
            return;
        }

        let tile_col = (x / DIRTY_TILE_WIDTH).min(self.dirty_cols - 1);
        let tile_row = (y / DIRTY_TILE_HEIGHT).min(self.dirty_rows - 1);
        self.dirty_tiles[tile_row][tile_col] = true;
        self.dirty_any = true;
    }

    fn dirty_rect_for_run(
        &self,
        tile_row: usize,
        start_col: usize,
        end_col_exclusive: usize,
    ) -> FramebufferRect {
        let x = start_col * DIRTY_TILE_WIDTH;
        let y = tile_row * DIRTY_TILE_HEIGHT;
        let width = ((end_col_exclusive - start_col) * DIRTY_TILE_WIDTH).min(self.width - x);
        let height = DIRTY_TILE_HEIGHT.min(self.height - y);
        FramebufferRect {
            x,
            y,
            width,
            height,
        }
    }

    fn copy_back_to_front_rect(&self, rect: FramebufferRect) -> bool {
        let Some((offset, copy_len)) = self.rect_copy_bounds(rect) else {
            crate::debug::println!(
                "framebuffer present rejected: dirty rect exceeds bounds x={} y={} width={} height={} stride={} size={}",
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                self.stride_bytes,
                self.size
            );
            return false;
        };
        unsafe {
            let mut src_row = self.back_base.add(offset);
            let mut dst_row = self.front_base.add(offset);
            for _ in 0..rect.height {
                // The front buffer can behave like device memory under QEMU, so
                // keep presents on a plain byte-copy path instead of XMM/AVX stores.
                ptr::copy_nonoverlapping(src_row, dst_row, copy_len);
                src_row = src_row.add(self.stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }
        true
    }

    fn pixel_index(&self, x: usize, y: usize) -> Option<usize> {
        let idx = y
            .checked_mul(self.stride_bytes)
            .and_then(|v| x.checked_mul(self.bpp).and_then(|xoff| v.checked_add(xoff)))?;
        let last = idx.checked_add(self.bpp.saturating_sub(1))?;
        if last >= self.size {
            return None;
        }
        Some(idx)
    }

    fn write_pixel(&self, base: *mut u8, idx: usize, color: Rgb888, alpha: u8) {
        let (c0, c1, c2) = self.color_bytes(color);

        unsafe {
            if alpha == 255 {
                ptr::write(base.add(idx), c0);
                ptr::write(base.add(idx + 1), c1);
                ptr::write(base.add(idx + 2), c2);
                if self.bpp == 4 {
                    ptr::write(base.add(idx + 3), 0);
                }
                return;
            }

            let d0 = ptr::read(base.add(idx));
            let d1 = ptr::read(base.add(idx + 1));
            let d2 = ptr::read(base.add(idx + 2));
            let a = alpha as u16;
            let inv = 256u16 - a;
            ptr::write(
                base.add(idx),
                (((c0 as u16 * a) + (d0 as u16 * inv)) >> 8) as u8,
            );
            ptr::write(
                base.add(idx + 1),
                (((c1 as u16 * a) + (d1 as u16 * inv)) >> 8) as u8,
            );
            ptr::write(
                base.add(idx + 2),
                (((c2 as u16 * a) + (d2 as u16 * inv)) >> 8) as u8,
            );
            if self.bpp == 4 {
                ptr::write(base.add(idx + 3), 0);
            }
        }
    }

    unsafe fn blit_bgra8888_row(&self, dst: *mut u8, src: *const u8, pixels: usize) {
        unsafe {
            crate::arch::simd::blit_bgra8888_row(
                dst,
                src,
                pixels,
                self.bpp,
                matches!(self.format, BootPixelFormat::Rgb),
            );
        }
    }
}

impl DrawTarget for Framebuffer {
    type Color = Rgb888;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(point, color) in pixels {
            if point.x < 0 || point.y < 0 {
                continue;
            }
            self.draw_pixel(point.x as usize, point.y as usize, color, 255);
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.fill(color);
        Ok(())
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(self.width as u32, self.height as u32)
    }
}

pub(crate) fn build_framebuffer(src: FramebufferInfo) -> Framebuffer {
    let width = src.width as usize;
    let height = src.height as usize;
    let stride = src.stride as usize;
    let bpp = src.bytes_per_pixel as usize;
    let size = src.size as usize;

    if src.addr == 0 || size == 0 {
        panic!("framebuffer info is empty");
    }
    if width == 0 || height == 0 || stride == 0 {
        panic!("framebuffer dimensions are invalid");
    }
    if width > MAX_FRAMEBUFFER_WIDTH || height > MAX_FRAMEBUFFER_HEIGHT {
        panic!("framebuffer exceeds dirty tracking grid");
    }
    if stride < width {
        panic!("framebuffer stride is smaller than width");
    }
    if !(3..=4).contains(&bpp) {
        panic!("unsupported bytes_per_pixel");
    }

    let stride_bytes = stride
        .checked_mul(bpp)
        .expect("framebuffer geometry overflow");
    let min_size = stride_bytes
        .checked_mul(height)
        .expect("framebuffer geometry overflow");
    if min_size > size {
        panic!("framebuffer size is smaller than geometry");
    }

    let use_double_buffer = ENABLE_FRAMEBUFFER_DOUBLE_BUFFER
        && can_use_double_buffer(
            src.addr,
            src.back_buffer_addr,
            size,
            src.back_buffer_size as usize,
        );
    let dirty_cols = width.div_ceil(DIRTY_TILE_WIDTH);
    let dirty_rows = height.div_ceil(DIRTY_TILE_HEIGHT);

    Framebuffer {
        front_base: src.addr as *mut u8,
        back_base: src.back_buffer_addr as *mut u8,
        size,
        width,
        height,
        stride_bytes,
        bpp,
        format: src.pixel_format,
        use_double_buffer,
        dirty_cols,
        dirty_rows,
        dirty_any: false,
        dirty_tiles: [[false; MAX_DIRTY_COLS]; MAX_DIRTY_ROWS],
    }
}

fn can_use_double_buffer(front_addr: u64, back_addr: u64, size: usize, back_size: usize) -> bool {
    if front_addr == 0 || back_addr == 0 || back_addr == front_addr || back_size < size {
        return false;
    }

    let front_start = front_addr as usize;
    let back_start = back_addr as usize;
    let Some(front_end) = front_start.checked_add(size) else {
        return false;
    };
    let Some(back_end) = back_start.checked_add(size) else {
        return false;
    };

    back_start >= front_end || front_start >= back_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn test_framebuffer(
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
        guard_bytes: usize,
    ) -> (Framebuffer, Vec<u8>) {
        let stride_bytes = width * bytes_per_pixel;
        let framebuffer_size = stride_bytes * height;
        let mut storage = vec![0u8; framebuffer_size + guard_bytes];
        let framebuffer = Framebuffer {
            front_base: storage.as_mut_ptr(),
            back_base: core::ptr::null_mut(),
            size: framebuffer_size,
            width,
            height,
            stride_bytes,
            bpp: bytes_per_pixel,
            format: BootPixelFormat::Rgb,
            use_double_buffer: false,
            dirty_cols: width.div_ceil(DIRTY_TILE_WIDTH),
            dirty_rows: height.div_ceil(DIRTY_TILE_HEIGHT),
            dirty_any: false,
            dirty_tiles: [[false; MAX_DIRTY_COLS]; MAX_DIRTY_ROWS],
        };
        (framebuffer, storage)
    }

    #[test]
    fn scroll_rect_up_moves_only_the_requested_region() {
        let (mut framebuffer, mut storage) = test_framebuffer(6, 4, 4, 0);

        for y in 0..4usize {
            for x in 0..6usize {
                let idx = y * framebuffer.stride_bytes + x * framebuffer.bpp;
                storage[idx] = (x as u8) + 1;
                storage[idx + 1] = (y as u8) + 1;
                storage[idx + 2] = 0x7f;
                storage[idx + 3] = 0;
            }
        }

        framebuffer.scroll_rect_up(1, 1, 3, 3, 1, Rgb888::BLACK);

        for y in 0..4usize {
            for x in 0..6usize {
                let idx = y * framebuffer.stride_bytes + x * framebuffer.bpp;
                let actual = [
                    storage[idx],
                    storage[idx + 1],
                    storage[idx + 2],
                    storage[idx + 3],
                ];
                let expected = if (1..4).contains(&x) && (1..3).contains(&y) {
                    [(x as u8) + 1, (y as u8) + 2, 0x7f, 0]
                } else if (1..4).contains(&x) && y == 3 {
                    [0, 0, 0, 0]
                } else {
                    [(x as u8) + 1, (y as u8) + 1, 0x7f, 0]
                };
                assert_eq!(actual, expected, "pixel mismatch at ({x}, {y})");
            }
        }
    }

    #[test]
    fn scroll_rect_up_respects_framebuffer_bounds_with_padded_rectangles() {
        let guard_len = 32usize;
        let (mut framebuffer, mut storage) = test_framebuffer(8, 4, 4, guard_len);
        let framebuffer_size = framebuffer.size;
        storage[framebuffer_size..].fill(0xa5);

        framebuffer.scroll_rect_up(1, 0, 7, 4, 1, Rgb888::BLACK);

        assert!(storage[framebuffer_size..].iter().all(|&byte| byte == 0xa5));
    }
}
// RING3-MIGRATION-REFERENCE END: commercial-max displayd/uiserver-owned framebuffer policy.
