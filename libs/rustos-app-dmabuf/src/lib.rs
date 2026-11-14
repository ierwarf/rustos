//! Ring3 client for RustOS app DMA-BUF allocation (`docs/ai/abi/app-dmabuf.md`).
//!
//! The kernel returns an fd that carries a `DeviceBuffer` capability. This
//! crate is the single client-side owner of the allocation syscall, the
//! post-allocation identity check, and the CPU mapping; Wayland clients (and a
//! future guest libgbm shim) pass `as_fd()` to `zwp_linux_dmabuf_v1` with
//! `modifier = LINEAR`. Nothing here is authority: size, rights and revocation
//! are enforced by the kernel record, never by these checks.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr;

use rustos_user_abi::syscall::{
    APP_BUFFER_MAX_BYTES, APP_BUFFER_STAT_RDEV, RustosAppBufferAllocArgs,
    SYS_RUSTOS_APP_BUFFER_ALLOC,
};

/// DRM fourcc `AR24`.
pub const FORMAT_ARGB8888: u32 = 0x3432_5241;
/// DRM fourcc `XR24`.
pub const FORMAT_XRGB8888: u32 = 0x3432_5258;
/// `DRM_FORMAT_MOD_LINEAR`, the only modifier the compositor imports.
pub const MODIFIER_LINEAR: u64 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Argb8888,
    Xrgb8888,
}

impl Format {
    pub const fn fourcc(self) -> u32 {
        match self {
            Format::Argb8888 => FORMAT_ARGB8888,
            Format::Xrgb8888 => FORMAT_XRGB8888,
        }
    }
}

/// Where allocation requests go. The kernel is the only production source;
/// the trait exists so geometry, identity and mapping logic run on the host.
pub trait BufferSource {
    fn alloc(&self, args: &RustosAppBufferAllocArgs) -> io::Result<OwnedFd>;
}

/// `SYS_RUSTOS_APP_BUFFER_ALLOC` through the Linux syscall ABI.
pub struct KernelSource;

impl BufferSource for KernelSource {
    fn alloc(&self, args: &RustosAppBufferAllocArgs) -> io::Result<OwnedFd> {
        let result = unsafe { raw_syscall1(SYS_RUSTOS_APP_BUFFER_ALLOC, args as *const _ as usize) };
        let fd = decode_syscall_result(result)?;
        let fd = i32::try_from(fd).map_err(|_| io::Error::from_raw_os_error(libc::EBADF))?;
        // SAFETY: a successful syscall installed a new descriptor owned by us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn decode_syscall_result(result: isize) -> io::Result<usize> {
    if (-4095..0).contains(&result) {
        return Err(io::Error::from_raw_os_error((-result) as i32));
    }
    usize::try_from(result).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

#[cfg(target_arch = "x86_64")]
unsafe fn raw_syscall1(number: u64, arg0: usize) -> isize {
    let result: isize;
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn raw_syscall1(_number: u64, _arg0: usize) -> isize {
    -(libc::ENOSYS as isize)
}

/// A mapped, CPU-writable app DMA-BUF.
pub struct AppDmabuf {
    fd: OwnedFd,
    ptr: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
}

impl AppDmabuf {
    /// Allocates a linear `width` x `height` buffer (stride = 4 * width) and
    /// maps it read/write. Fails closed on any mismatch between what was asked
    /// and what the kernel returned.
    pub fn allocate(
        source: &dyn BufferSource,
        width: u32,
        height: u32,
        format: Format,
    ) -> io::Result<Self> {
        let einval = || io::Error::from_raw_os_error(libc::EINVAL);
        let stride = width.checked_mul(4).ok_or_else(einval)?;
        let len = u64::from(stride)
            .checked_mul(u64::from(height))
            .filter(|len| *len != 0 && *len <= APP_BUFFER_MAX_BYTES)
            .ok_or_else(einval)?;
        let args = RustosAppBufferAllocArgs {
            width,
            height,
            stride,
            format: format.fourcc(),
            reserved: [0; 2],
        };
        let fd = source.alloc(&args)?;
        Self::map(fd, width, height, stride, format, len as usize)
    }

    fn map(
        fd: OwnedFd,
        width: u32,
        height: u32,
        stride: u32,
        format: Format,
        len: usize,
    ) -> io::Result<Self> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        check_identity(&st, len)?;
        Self::map_unchecked(fd, width, height, stride, format, len)
    }

    fn map_unchecked(
        fd: OwnedFd,
        width: u32,
        height: u32,
        stride: u32,
        format: Format,
        len: usize,
    ) -> io::Result<Self> {
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, ptr: mapped.cast(), len, width, height, stride, format })
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn stride(&self) -> u32 {
        self.stride
    }
    pub fn format(&self) -> Format {
        self.format
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn pixels_mut(&mut self) -> &mut [u32] {
        // SAFETY: the mapping is `len` bytes, page aligned, owned by `self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast::<u32>(), self.len / 4) }
    }

    /// Transfers the descriptor and mapping to the caller, who must `munmap`
    /// the returned pointer with the returned length.
    pub fn into_raw_parts(self) -> (OwnedFd, *mut u32, usize) {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is never dropped, so the descriptor is moved exactly once.
        let fd = unsafe { ptr::read(&this.fd) };
        (fd, this.ptr.cast(), this.len)
    }
}

impl Drop for AppDmabuf {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are the mapping created in `map_unchecked`.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// A genuine app buffer is a character-device handle with the reserved
/// `st_rdev` and a capability at least as large as the requested geometry.
fn check_identity(st: &libc::stat, needed: usize) -> io::Result<()> {
    let proto = || io::Error::from_raw_os_error(libc::EPROTO);
    if st.st_mode & libc::S_IFMT != libc::S_IFCHR
        || st.st_rdev as u64 != APP_BUFFER_STAT_RDEV
        || (st.st_size as u64) < needed as u64
    {
        return Err(proto());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::CString;

    struct Fake {
        seen: RefCell<Option<RustosAppBufferAllocArgs>>,
        result: RefCell<Option<io::Result<OwnedFd>>>,
    }

    impl BufferSource for Fake {
        fn alloc(&self, args: &RustosAppBufferAllocArgs) -> io::Result<OwnedFd> {
            *self.seen.borrow_mut() = Some(*args);
            self.result.borrow_mut().take().expect("single call")
        }
    }

    fn fake(result: io::Result<OwnedFd>) -> Fake {
        Fake { seen: RefCell::new(None), result: RefCell::new(Some(result)) }
    }

    fn memfd(len: usize) -> OwnedFd {
        let name = CString::new("t").unwrap();
        let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
        assert!(fd >= 0);
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(unsafe { libc::ftruncate(fd.as_raw_fd(), len as _) }, 0);
        fd
    }

    #[test]
    fn syscall_results_decode_errno_and_fd() {
        assert_eq!(decode_syscall_result(7).unwrap(), 7);
        assert_eq!(decode_syscall_result(-libc::EBUSY as isize).unwrap_err().raw_os_error(), Some(libc::EBUSY));
        assert_eq!(decode_syscall_result(-4095).unwrap_err().raw_os_error(), Some(4095));
    }

    #[test]
    fn request_carries_linear_stride_and_fourcc() {
        let source = fake(Err(io::Error::from_raw_os_error(libc::ENOMEM)));
        let err = AppDmabuf::allocate(&source, 64, 32, Format::Xrgb8888).err().unwrap();
        assert_eq!(err.raw_os_error(), Some(libc::ENOMEM));
        let args = source.seen.borrow().unwrap();
        assert_eq!((args.width, args.height, args.stride, args.format), (64, 32, 256, FORMAT_XRGB8888));
        assert_eq!(args.reserved, [0; 2]);
    }

    #[test]
    fn invalid_geometry_never_reaches_the_kernel() {
        for (w, h) in [(0, 8), (8, 0), (u32::MAX, 2), (8192, 8193)] {
            let source = fake(Err(io::Error::from_raw_os_error(libc::EIO)));
            let err = AppDmabuf::allocate(&source, w, h, Format::Argb8888).err().unwrap();
            assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{w}x{h}");
            assert!(source.seen.borrow().is_none(), "{w}x{h}");
        }
    }

    #[test]
    fn forged_regular_fd_is_refused_after_allocation() {
        // A source that hands back a plain memfd is not a DeviceBuffer.
        let source = fake(Ok(memfd(4096)));
        let err = AppDmabuf::allocate(&source, 8, 8, Format::Argb8888).err().unwrap();
        assert_eq!(err.raw_os_error(), Some(libc::EPROTO));
    }

    #[test]
    fn identity_requires_char_device_rdev_and_capacity() {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        st.st_mode = libc::S_IFCHR;
        st.st_rdev = APP_BUFFER_STAT_RDEV as _;
        st.st_size = 256;
        assert!(check_identity(&st, 256).is_ok());
        assert!(check_identity(&st, 257).is_err());
        st.st_rdev = 0;
        assert!(check_identity(&st, 1).is_err());
        st.st_rdev = APP_BUFFER_STAT_RDEV as _;
        st.st_mode = libc::S_IFREG;
        assert!(check_identity(&st, 1).is_err());
    }

    #[test]
    fn mapped_pixels_are_writable_and_parts_transfer_ownership() {
        let fd = memfd(8 * 8 * 4);
        let mut buf = AppDmabuf::map_unchecked(fd, 8, 8, 32, Format::Argb8888, 256).unwrap();
        assert_eq!((buf.width(), buf.height(), buf.stride(), buf.len()), (8, 8, 32, 256));
        buf.pixels_mut().fill(0xFF11_2233);
        assert_eq!(buf.pixels_mut()[63], 0xFF11_2233);
        let (fd, ptr, len) = buf.into_raw_parts();
        assert_eq!(unsafe { *ptr }, 0xFF11_2233);
        assert_eq!(unsafe { libc::munmap(ptr.cast(), len) }, 0);
        drop(fd);
    }
}
