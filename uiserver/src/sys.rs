use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const SYS_READ: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_IOCTL: usize = 16;
const SYS_OPENAT: usize = 257;

const AT_FDCWD: isize = -100;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;

const MAP_SHARED: usize = 0x01;

const DISPLAY_IOCTL_GET_INFO: usize = 0x4453_0001;
const DISPLAY_IOCTL_CREATE_SURFACE: usize = 0x4453_0002;
const DISPLAY_IOCTL_PRESENT: usize = 0x4453_0003;
const DISPLAY_IOCTL_PRESENT_RECT: usize = 0x4453_0004;
const CONSOLE_IOCTL_GET_STATE: usize = 0x434f_0001;
const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: usize = 0x434f_0002;
const CONSOLE_IOCTL_SET_FOCUS: usize = 0x434f_0003;
const CONSOLE_SESSION_CAPACITY: usize = 8;

const RUNTIME_IOCTL_GET_GENERATION: usize = 0x5254_0001;
const RUNTIME_IOCTL_SNAPSHOT_PROGRAMS: usize = 0x5254_0002;
const RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS: usize = 0x5254_0003;
const RUNTIME_IOCTL_REQUEST_LAUNCH: usize = 0x5254_0004;

const LAUNCH_TARGET_FIRST_AVAILABLE: u16 = 2;

pub(crate) const PIXEL_FORMAT_BGRA8888: u32 = 1;
pub(crate) const INPUT_KIND_KEYBOARD: u16 = 1;
pub(crate) const INPUT_KIND_POINTER_MOTION: u16 = 2;
pub(crate) const INPUT_KIND_POINTER_BUTTON: u16 = 3;
pub(crate) const INPUT_ACTION_PRESSED: u16 = 1;
pub(crate) const INPUT_ACTION_RELEASED: u16 = 2;
pub(crate) const POINTER_BUTTON_LEFT: u32 = 1;
pub(crate) const RUNNING_PROGRAM_NAME_CAPACITY: usize = 48;
pub(crate) const PROGRAM_NAME_CAPACITY: usize = 48;
pub(crate) const PROGRAM_PATH_CAPACITY: usize = 64;
pub(crate) const MAX_CONSOLE_SNAPSHOT_BYTES: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DisplayInfo {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride_bytes: u32,
    pub(crate) bytes_per_pixel: u32,
    pub(crate) pixel_format: u32,
    pub(crate) reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DisplaySurfaceCreate {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) pixel_format: u32,
    pub(crate) flags: u32,
    pub(crate) handle: u32,
    pub(crate) bytes_per_pixel: u32,
    pub(crate) stride_bytes: u32,
    pub(crate) reserved: u32,
    pub(crate) mapping_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct DisplayPresentRequest {
    surface_handle: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct DisplayPresentRectRequest {
    surface_handle: u32,
    reserved: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputEvent {
    pub(crate) kind: u16,
    pub(crate) action: u16,
    pub(crate) code: u32,
    pub(crate) value0: i32,
    pub(crate) value1: i32,
    pub(crate) modifiers: u32,
    pub(crate) text: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ConsoleStateInfo {
    pub(crate) active_session_mask: u64,
    pub(crate) focused_session_index: u32,
    pub(crate) reserved: u32,
    pub(crate) output_generations: [u64; CONSOLE_SESSION_CAPACITY],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ConsoleSnapshotSessionOutputRequest {
    session_index: u32,
    reserved: u32,
    bytes_ptr: u64,
    capacity: u64,
    count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ConsoleSetFocusRequest {
    session_index: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeRunningProgram {
    pub(crate) pid: u64,
    pub(crate) program_id: u32,
    pub(crate) session_index: u32,
    pub(crate) display_name: [u8; RUNNING_PROGRAM_NAME_CAPACITY],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeProgram {
    pub(crate) program_id: u32,
    pub(crate) reserved: u32,
    pub(crate) weight_micros: u64,
    pub(crate) display_name: [u8; PROGRAM_NAME_CAPACITY],
    pub(crate) exec_path: [u8; PROGRAM_PATH_CAPACITY],
}

impl Default for RuntimeProgram {
    fn default() -> Self {
        Self {
            program_id: 0,
            reserved: 0,
            weight_micros: 0,
            display_name: [0; PROGRAM_NAME_CAPACITY],
            exec_path: [0; PROGRAM_PATH_CAPACITY],
        }
    }
}

impl Default for RuntimeRunningProgram {
    fn default() -> Self {
        Self {
            pid: 0,
            program_id: 0,
            session_index: 0,
            display_name: [0; RUNNING_PROGRAM_NAME_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeGeneration {
    generation: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeSnapshotRunningProgramsRequest {
    programs_ptr: u64,
    capacity: u64,
    count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeSnapshotProgramsRequest {
    programs_ptr: u64,
    capacity: u64,
    count: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct RuntimeLaunchRequest {
    program_id: u64,
    target_kind: u16,
    reserved: u16,
    reserved2: u32,
    target_value: u64,
}

pub(crate) struct SurfaceMapping {
    base: NonNull<u32>,
    len_bytes: usize,
}

impl SurfaceMapping {
    pub(crate) fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { slice::from_raw_parts_mut(self.base.as_ptr(), self.len_bytes / size_of::<u32>()) }
    }
}

impl Drop for SurfaceMapping {
    fn drop(&mut self) {
        let _ = munmap(self.base.as_ptr().cast::<c_void>(), self.len_bytes);
    }
}

pub(crate) fn open_display() -> Result<OwnedFd, i32> {
    open_device("/dev/display0", O_RDWR)
}

pub(crate) fn open_console() -> Result<OwnedFd, i32> {
    open_device("/dev/console0", O_RDWR)
}

pub(crate) fn open_input() -> Result<OwnedFd, i32> {
    open_device("/dev/input0", O_RDONLY)
}

pub(crate) fn open_runtime() -> Result<OwnedFd, i32> {
    open_device("/dev/runtime0", O_RDWR)
}

pub(crate) fn raw_stderr_line(message: &str) {
    let _ = raw_write(2, message.as_bytes());
    let _ = raw_write(2, b"\n");
}

pub(crate) fn display_get_info(fd: RawFd) -> Result<DisplayInfo, i32> {
    let mut info = DisplayInfo::default();
    ioctl_with_mut(fd, DISPLAY_IOCTL_GET_INFO, &mut info)?;
    Ok(info)
}

pub(crate) fn display_create_surface(
    fd: RawFd,
    width: u32,
    height: u32,
) -> Result<DisplaySurfaceCreate, i32> {
    let mut surface = DisplaySurfaceCreate {
        width,
        height,
        pixel_format: PIXEL_FORMAT_BGRA8888,
        ..DisplaySurfaceCreate::default()
    };
    ioctl_with_mut(fd, DISPLAY_IOCTL_CREATE_SURFACE, &mut surface)?;
    Ok(surface)
}

pub(crate) fn display_present(fd: RawFd, surface_handle: u32) -> Result<(), i32> {
    let mut request = DisplayPresentRequest {
        surface_handle,
        reserved: 0,
    };
    ioctl_with_mut(fd, DISPLAY_IOCTL_PRESENT, &mut request)
}

pub(crate) fn display_present_rect(
    fd: RawFd,
    surface_handle: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<(), i32> {
    let mut request = DisplayPresentRectRequest {
        surface_handle,
        reserved: 0,
        x,
        y,
        width,
        height,
    };
    ioctl_with_mut(fd, DISPLAY_IOCTL_PRESENT_RECT, &mut request)
}

pub(crate) fn map_surface(surface_fd: RawFd, mapping_len: usize) -> Result<SurfaceMapping, i32> {
    let mapped = mmap(
        core::ptr::null_mut(),
        mapping_len,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        surface_fd,
        0,
    )?;
    let base = NonNull::new(mapped.cast::<u32>()).ok_or(22)?;
    Ok(SurfaceMapping {
        base,
        len_bytes: mapping_len,
    })
}

pub(crate) fn read_input(fd: RawFd, events: &mut [InputEvent]) -> Result<usize, i32> {
    let bytes = read(
        fd,
        events.as_mut_ptr().cast::<c_void>(),
        std::mem::size_of_val(events),
    )?;
    if bytes % size_of::<InputEvent>() != 0 {
        return Err(5);
    }
    Ok(bytes / size_of::<InputEvent>())
}

pub(crate) fn console_get_state(fd: RawFd) -> Result<ConsoleStateInfo, i32> {
    let mut state = ConsoleStateInfo::default();
    ioctl_with_mut(fd, CONSOLE_IOCTL_GET_STATE, &mut state)?;
    Ok(state)
}

pub(crate) fn console_snapshot_session_output(
    fd: RawFd,
    session_index: u32,
    bytes: &mut [u8],
) -> Result<usize, i32> {
    let mut request = ConsoleSnapshotSessionOutputRequest {
        session_index,
        reserved: 0,
        bytes_ptr: bytes.as_mut_ptr() as u64,
        capacity: bytes.len() as u64,
        count: 0,
    };
    ioctl_with_mut(fd, CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT, &mut request)?;
    Ok(request.count as usize)
}

pub(crate) fn console_set_focus(fd: RawFd, session_index: u32) -> Result<(), i32> {
    let mut request = ConsoleSetFocusRequest {
        session_index,
        reserved: 0,
    };
    ioctl_with_mut(fd, CONSOLE_IOCTL_SET_FOCUS, &mut request)
}

pub(crate) fn runtime_generation(fd: RawFd) -> Result<u64, i32> {
    let mut request = RuntimeGeneration { generation: 0 };
    ioctl_with_mut(fd, RUNTIME_IOCTL_GET_GENERATION, &mut request)?;
    Ok(request.generation)
}

pub(crate) fn runtime_snapshot_running_programs(
    fd: RawFd,
    programs: &mut [RuntimeRunningProgram],
) -> Result<usize, i32> {
    let mut request = RuntimeSnapshotRunningProgramsRequest {
        programs_ptr: programs.as_mut_ptr() as u64,
        capacity: programs.len() as u64,
        count: 0,
    };
    ioctl_with_mut(fd, RUNTIME_IOCTL_SNAPSHOT_RUNNING_PROGRAMS, &mut request)?;
    Ok(request.count as usize)
}

pub(crate) fn runtime_snapshot_programs(
    fd: RawFd,
    programs: &mut [RuntimeProgram],
) -> Result<usize, i32> {
    let mut request = RuntimeSnapshotProgramsRequest {
        programs_ptr: programs.as_mut_ptr() as u64,
        capacity: programs.len() as u64,
        count: 0,
    };
    ioctl_with_mut(fd, RUNTIME_IOCTL_SNAPSHOT_PROGRAMS, &mut request)?;
    Ok(request.count as usize)
}

pub(crate) fn runtime_request_launch_first_available(
    fd: RawFd,
    program_id: u32,
) -> Result<(), i32> {
    let mut request = RuntimeLaunchRequest {
        program_id: program_id as u64,
        target_kind: LAUNCH_TARGET_FIRST_AVAILABLE,
        reserved: 0,
        reserved2: 0,
        target_value: 0,
    };
    ioctl_with_mut(fd, RUNTIME_IOCTL_REQUEST_LAUNCH, &mut request)
}

fn open_device(path: &str, flags: usize) -> Result<OwnedFd, i32> {
    let path = CString::new(path).map_err(|_| 22)?;
    let raw_fd = openat(AT_FDCWD, &path, flags, 0)?;
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    Ok(fd)
}

fn ioctl_with_mut<T>(fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let result = unsafe { syscall3(SYS_IOCTL, fd as usize, request, arg as *mut T as usize) };
    syscall_unit(result)
}

fn openat(dirfd: isize, path: &CString, flags: usize, mode: usize) -> Result<RawFd, i32> {
    let result = unsafe {
        syscall4(
            SYS_OPENAT,
            dirfd as usize,
            path.as_ptr() as *const c_char as usize,
            flags,
            mode,
        )
    };
    syscall_fd(result)
}

fn read(fd: RawFd, buffer: *mut c_void, len: usize) -> Result<usize, i32> {
    let result = unsafe { syscall3(SYS_READ, fd as usize, buffer as usize, len) };
    syscall_usize(result)
}

fn raw_write(fd: RawFd, buffer: &[u8]) -> Result<usize, i32> {
    let result = unsafe {
        syscall3(
            SYS_WRITE,
            fd as usize,
            buffer.as_ptr() as usize,
            buffer.len(),
        )
    };
    syscall_usize(result)
}

fn mmap(
    requested_addr: *mut c_void,
    len: usize,
    prot: usize,
    flags: usize,
    fd: RawFd,
    offset: u64,
) -> Result<*mut c_void, i32> {
    let result = unsafe {
        syscall6(
            SYS_MMAP,
            requested_addr as usize,
            len,
            prot,
            flags,
            fd as usize,
            offset as usize,
        )
    };
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    Ok(result as *mut c_void)
}

fn munmap(start: *mut c_void, len: usize) -> Result<(), i32> {
    let result = unsafe { syscall2(SYS_MUNMAP, start as usize, len) };
    syscall_unit(result)
}

fn syscall_fd(result: isize) -> Result<RawFd, i32> {
    let value = syscall_usize(result)?;
    i32::try_from(value).map_err(|_| 22)
}

fn syscall_usize(result: isize) -> Result<usize, i32> {
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    usize::try_from(result).map_err(|_| 22)
}

fn syscall_unit(result: isize) -> Result<(), i32> {
    if is_syscall_error(result) {
        return Err(errno_from_result(result));
    }
    Ok(())
}

const fn is_syscall_error(result: isize) -> bool {
    result < 0 && result >= -4095
}

const fn errno_from_result(result: isize) -> i32 {
    (-result) as i32
}

unsafe fn syscall2(number: usize, arg0: usize, arg1: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall3(number: usize, arg0: usize, arg1: usize, arg2: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall4(number: usize, arg0: usize, arg1: usize, arg2: usize, arg3: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            in("r10") arg3 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall6(
    number: usize,
    arg0: usize,
    arg1: usize,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            in("r10") arg3 as isize,
            in("r8") arg4 as isize,
            in("r9") arg5 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}
