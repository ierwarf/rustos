use alloc::vec::Vec;
use core::convert::Infallible;
use core::ptr;

use boot_protocol::{BootPixelFormat, FramebufferInfo};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::Size;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::{OriginDimensions, Pixel, RgbColor};

const DIRTY_TILE_WIDTH: usize = 32;
const DIRTY_TILE_HEIGHT: usize = 32;
const MAX_FRAMEBUFFER_WIDTH: usize = 7680;
const MAX_FRAMEBUFFER_HEIGHT: usize = 4320;
const MAX_DIRTY_COLS: usize = MAX_FRAMEBUFFER_WIDTH.div_ceil(DIRTY_TILE_WIDTH);
const MAX_DIRTY_ROWS: usize = MAX_FRAMEBUFFER_HEIGHT.div_ceil(DIRTY_TILE_HEIGHT);
pub(crate) const MAX_FRAMEBUFFER_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FramebufferRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

impl FramebufferRect {
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

pub(crate) struct FramebufferImage {
    width: usize,
    height: usize,
    stride_bytes: usize,
    bpp: usize,
    pixels: Vec<u8>,
}

pub(crate) struct FramebufferFrontSnapshot<const CAPACITY: usize> {
    rect: Option<FramebufferRect>,
    len: usize,
    bytes: [u8; CAPACITY],
}

impl<const CAPACITY: usize> FramebufferFrontSnapshot<CAPACITY> {
    pub(crate) const fn empty() -> Self {
        Self {
            rect: None,
            len: 0,
            bytes: [0; CAPACITY],
        }
    }

    pub(crate) fn clear(&mut self) {
        self.rect = None;
        self.len = 0;
    }
}

impl FramebufferImage {
    pub(crate) const fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            stride_bytes: 0,
            bpp: 0,
            pixels: Vec::new(),
        }
    }

    pub(crate) fn allocate_for_framebuffer(
        &mut self,
        framebuffer: &Framebuffer,
    ) -> Result<(), &'static str> {
        let buffer_len = framebuffer
            .stride_bytes
            .checked_mul(framebuffer.height)
            .ok_or("background buffer size overflow")?;
        self.pixels.resize(buffer_len, 0);
        self.width = framebuffer.width;
        self.height = framebuffer.height;
        self.stride_bytes = framebuffer.stride_bytes;
        self.bpp = framebuffer.bpp;
        Ok(())
    }

    pub(crate) fn matches_framebuffer(&self, framebuffer: &Framebuffer) -> bool {
        self.width == framebuffer.width
            && self.height == framebuffer.height
            && self.stride_bytes == framebuffer.stride_bytes
            && self.bpp == framebuffer.bpp
            && self.pixels.len() == framebuffer.stride_bytes * framebuffer.height
    }

    pub(crate) fn clear(&mut self) {
        self.width = 0;
        self.height = 0;
        self.stride_bytes = 0;
        self.bpp = 0;
        self.pixels.clear();
    }

    pub(crate) fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.pixels
    }

    pub(crate) fn stride_bytes(&self) -> usize {
        self.stride_bytes
    }

    pub(crate) fn bpp(&self) -> usize {
        self.bpp
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
    pub(crate) const fn empty() -> Self {
        Self {
            front_base: ptr::null_mut(),
            back_base: ptr::null_mut(),
            size: 0,
            width: 0,
            height: 0,
            stride_bytes: 0,
            bpp: 4,
            format: BootPixelFormat::Unknown,
            use_double_buffer: false,
            dirty_cols: 0,
            dirty_rows: 0,
            dirty_any: false,
            dirty_tiles: [[false; MAX_DIRTY_COLS]; MAX_DIRTY_ROWS],
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub(crate) fn height(&self) -> usize {
        self.height
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

    pub(crate) fn capture_scene_snapshot<const CAPACITY: usize>(
        &self,
        rect: FramebufferRect,
        snapshot: &mut FramebufferFrontSnapshot<CAPACITY>,
    ) -> bool {
        if rect.width == 0 || rect.height == 0 {
            snapshot.clear();
            return false;
        }

        let Some(row_bytes) = rect.width.checked_mul(self.bpp) else {
            snapshot.clear();
            return false;
        };
        let Some(total_bytes) = row_bytes.checked_mul(rect.height) else {
            snapshot.clear();
            return false;
        };
        if snapshot.bytes.len() < total_bytes {
            snapshot.clear();
            return false;
        }

        unsafe {
            let mut src_row = self
                .scene_buffer()
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            let mut dst_row = snapshot.bytes.as_mut_ptr();
            for _ in 0..rect.height {
                ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
                src_row = src_row.add(self.stride_bytes);
                dst_row = dst_row.add(row_bytes);
            }
        }

        snapshot.rect = Some(rect);
        snapshot.len = total_bytes;
        true
    }

    pub(crate) fn restore_front_snapshot<const CAPACITY: usize>(
        &mut self,
        snapshot: &mut FramebufferFrontSnapshot<CAPACITY>,
    ) -> bool {
        let Some(rect) = snapshot.rect.take() else {
            snapshot.len = 0;
            return false;
        };
        if rect.width == 0 || rect.height == 0 {
            snapshot.len = 0;
            return false;
        }

        let Some(row_bytes) = rect.width.checked_mul(self.bpp) else {
            snapshot.len = 0;
            return false;
        };
        let Some(total_bytes) = row_bytes.checked_mul(rect.height) else {
            snapshot.len = 0;
            return false;
        };
        if snapshot.len < total_bytes || snapshot.bytes.len() < total_bytes {
            snapshot.len = 0;
            return false;
        }

        unsafe {
            let mut src_row = snapshot.bytes.as_ptr();
            let mut dst_row = self
                .front_base
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            for _ in 0..rect.height {
                ptr::copy_nonoverlapping(src_row, dst_row, row_bytes);
                src_row = src_row.add(row_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }

        snapshot.len = 0;
        true
    }

    pub(crate) fn draw_image(&mut self, image: &FramebufferImage) -> bool {
        if !image.matches_framebuffer(self) {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                image.pixels.as_ptr(),
                self.active_buffer(),
                image.pixels.len(),
            );
        }
        self.mark_all_dirty();
        true
    }

    pub(crate) fn draw_image_rect(
        &mut self,
        image: &FramebufferImage,
        x: i64,
        y: i64,
        width: u32,
        height: u32,
    ) -> bool {
        let Some(rect) = self.clip_rect(x, y, width, height) else {
            return false;
        };
        if !image.matches_framebuffer(self) {
            return false;
        }

        let copy_len = rect.width * image.bpp;
        unsafe {
            let mut src_row = image
                .pixels
                .as_ptr()
                .add(rect.y * image.stride_bytes + rect.x * image.bpp);
            let mut dst_row = self
                .active_buffer()
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            for _ in 0..rect.height {
                ptr::copy_nonoverlapping(src_row, dst_row, copy_len);
                src_row = src_row.add(image.stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }

        self.mark_dirty_rect(rect);
        true
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

    pub(crate) fn draw_overlay_pixel(&mut self, x: usize, y: usize, color: Rgb888, alpha: u8) {
        if alpha == 0 || x >= self.width || y >= self.height {
            return;
        }

        let Some(idx) = self.pixel_index(x, y) else {
            return;
        };
        self.write_pixel(self.front_base, idx, color, alpha);
    }

    pub(crate) fn present_scene(&mut self) {
        if !self.use_double_buffer || !self.dirty_any {
            return;
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
                    self.dirty_tiles[tile_row][tile_col] = false;
                    tile_col += 1;
                }

                self.copy_back_to_front_rect(
                    self.dirty_rect_for_run(tile_row, start_col, tile_col),
                );
            }
        }

        self.dirty_any = false;
    }

    fn active_buffer(&self) -> *mut u8 {
        if self.use_double_buffer {
            self.back_base
        } else {
            self.front_base
        }
    }

    fn scene_buffer(&self) -> *mut u8 {
        self.active_buffer()
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

    fn copy_back_to_front_rect(&self, rect: FramebufferRect) {
        let copy_len = rect.width * self.bpp;
        unsafe {
            let mut src_row = self
                .back_base
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            let mut dst_row = self
                .front_base
                .add(rect.y * self.stride_bytes + rect.x * self.bpp);
            for _ in 0..rect.height {
                ptr::copy_nonoverlapping(src_row, dst_row, copy_len);
                src_row = src_row.add(self.stride_bytes);
                dst_row = dst_row.add(self.stride_bytes);
            }
        }
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

    let use_double_buffer = can_use_double_buffer(
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
