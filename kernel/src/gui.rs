use core::ptr;

use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::paging;

pub const BOOT_INFO_MAGIC: u64 = 0x5255_5354_4F53_4749; // "RUSTOSGI"
pub const BOOT_INFO_VERSION: u32 = 1;
const HUGE_2MIB: u64 = 2 * 1024 * 1024;

pub static GOP_SCREEN: Mutex<Framebuffer> = Mutex::new(Framebuffer {
    front_base: ptr::null_mut(),
    back_base: ptr::null_mut(),
    size: 0,
    width: 0,
    height: 0,
    stride_bytes: 0,
    bpp: 4,
    format: BootPixelFormat::Unknown,
    use_double_buffer: false,
});

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPixelFormat {
    Rgb = 0,
    Bgr = 1,
    Bitmask = 2,
    Unknown = 0xff,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FramebufferInfo {
    pub addr: u64,
    pub size: u64,
    pub back_buffer_addr: u64,
    pub back_buffer_size: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: BootPixelFormat,
    pub bytes_per_pixel: u8,
    pub _reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub magic: u64,
    pub version: u32,
    pub _reserved0: u32,
    pub framebuffer: FramebufferInfo,
}

pub struct Framebuffer {
    front_base: *mut u8,
    back_base: *mut u8,
    size: usize,
    width: usize,
    height: usize,
    stride_bytes: usize,
    bpp: usize,
    format: BootPixelFormat,
    use_double_buffer: bool,
}

unsafe impl Send for Framebuffer {}

impl Framebuffer {
    fn color_bytes(&self, color: Rgb888) -> (u8, u8, u8) {
        match self.format {
            BootPixelFormat::Rgb => (color.r(), color.g(), color.b()),
            _ => (color.b(), color.g(), color.r()),
        }
    }

    fn clipped_rect(&self, x: i64, y: i64, w: u32, h: u32) -> Option<(usize, usize, usize, usize)> {
        if w == 0 || h == 0 {
            return None;
        }

        let x0 = x.max(0).min(self.width as i64) as usize;
        let y0 = y.max(0).min(self.height as i64) as usize;
        let x1 = x.saturating_add(w as i64).max(0).min(self.width as i64) as usize;
        let y1 = y.saturating_add(h as i64).max(0).min(self.height as i64) as usize;
        if x0 >= x1 || y0 >= y1 {
            return None;
        }

        Some((x0, y0, x1, y1))
    }

    pub fn fill_rect(&self, x: i64, y: i64, w: u32, h: u32, color: Rgb888, alpha: u8) {
        if alpha == 0 {
            return;
        }

        let Some((x0, y0, x1, y1)) = self.clipped_rect(x, y, w, h) else {
            return;
        };
        let cols = x1 - x0;
        let rows = y1 - y0;
        let Some(start) = y0.checked_mul(self.stride_bytes).and_then(|v| {
            x0.checked_mul(self.bpp)
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
                        let mut n = cols;
                        while n >= 4 {
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
                            n -= 4;
                        }
                        while n > 0 {
                            if aligned {
                                ptr::write(p, px32);
                            } else {
                                ptr::write_unaligned(p, px32);
                            }
                            p = p.add(1);
                            n -= 1;
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
    }

    pub fn fill(&self, color: Rgb888) {
        self.fill_rect(0, 0, self.width as u32, self.height as u32, color, 255);
    }

    pub fn draw_pixel(&self, x: usize, y: usize, color: Rgb888, alpha: u8) {
        if alpha == 0 || x >= self.width || y >= self.height {
            return;
        }

        let Some(idx) = y
            .checked_mul(self.stride_bytes)
            .and_then(|v| x.checked_mul(self.bpp).and_then(|xoff| v.checked_add(xoff)))
        else {
            return;
        };
        let Some(last) = idx.checked_add(self.bpp.saturating_sub(1)) else {
            return;
        };
        if last >= self.size {
            return;
        }

        let base = self.active_buffer();
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

    fn active_buffer(&self) -> *mut u8 {
        if self.use_double_buffer {
            self.back_base
        } else {
            self.front_base
        }
    }

    pub fn refresh(&self) {
        if self.use_double_buffer {
            unsafe {
                crate::asmtools::copy_sse2(self.back_base, self.front_base, self.size);
            }
        }
    }
}

pub fn init(boot_info_ptr: *const BootInfo) {
    let boot_info = boot_info_from_ptr(boot_info_ptr);
    let framebuffer = build_framebuffer(boot_info.framebuffer);
    mark_framebuffer_write_combine(boot_info.framebuffer);
    *GOP_SCREEN.lock() = framebuffer;
}

fn build_framebuffer(src: FramebufferInfo) -> Framebuffer {
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

fn boot_info_from_ptr(boot_info_ptr: *const BootInfo) -> &'static BootInfo {
    if boot_info_ptr.is_null() {
        panic!("boot info pointer is null");
    }

    let boot_info = unsafe { &*boot_info_ptr };
    if boot_info.magic != BOOT_INFO_MAGIC {
        panic!("boot info magic mismatch");
    }
    if boot_info.version != BOOT_INFO_VERSION {
        panic!("boot info version mismatch");
    }

    boot_info
}

fn mark_framebuffer_write_combine(info: FramebufferInfo) {
    let end_addr = info
        .addr
        .checked_add(info.size.saturating_sub(1))
        .expect("framebuffer end address overflow");
    let start_block = info.addr / HUGE_2MIB;
    let end_block = end_addr / HUGE_2MIB;

    use crate::paging::KERNEL_PML4;

    interrupts::without_interrupts(|| {
        let mut pml4 = KERNEL_PML4.lock();
        for block_index in start_block..=end_block {
            pml4.add_flags(block_index, paging::WRITE_COMBINE_BIT);
        }
    });
}
