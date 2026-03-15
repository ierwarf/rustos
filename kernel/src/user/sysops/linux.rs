use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use x86_64::VirtAddr;
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::PageTableFlags;

use crate::debug;
use crate::io::device as device_ns;
use crate::multitask;
use crate::paging;
use crate::rtc;
use crate::user::abi::UserAbi;
use crate::user::handles::{BootFileSeekWhence, KernelHandle};
use crate::user::linux as linux_abi;

use super::console;
use super::device;
use super::file;
use super::usermem;

const PAGE_SIZE: u64 = 4096;
const LINUX_SIGSET_SIZE: u64 = 8;
const FILE_MMAP_COPY_CHUNK_LEN: usize = 4096;
const MAX_IOV_COUNT: usize = 256;
const DEFAULT_STACK_RLIMIT_BYTES: u64 = 8 * 1024 * 1024;
const GETRANDOM_FLAG_NONBLOCK: u64 = 0x0001;
const GETRANDOM_FLAG_RANDOM: u64 = 0x0002;
const RSEQ_FLAG_UNREGISTER: u64 = 0x1;

#[derive(Debug, Clone, Copy)]
pub(crate) enum LinuxSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    Busy,
    DisplayUnavailable,
    IllegalSeek,
    InvalidArgument,
    NoMemory,
    NotFound,
    NotTty,
    PermissionDenied,
    ReadOnlyFilesystem,
    Unsupported,
}

impl From<paging::AddressSpaceError> for LinuxSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

impl From<device::DeviceSysopError> for LinuxSysopError {
    fn from(value: device::DeviceSysopError) -> Self {
        match value {
            device::DeviceSysopError::AddressSpace(err) => Self::AddressSpace(err),
            device::DeviceSysopError::BadFileDescriptor => Self::BadFileDescriptor,
            device::DeviceSysopError::Busy => Self::Busy,
            device::DeviceSysopError::InvalidArgument => Self::InvalidArgument,
            device::DeviceSysopError::DisplayUnavailable => Self::DisplayUnavailable,
            device::DeviceSysopError::NotFound => Self::NotFound,
            device::DeviceSysopError::Unsupported => Self::Unsupported,
        }
    }
}

impl From<file::FileSysopError> for LinuxSysopError {
    fn from(value: file::FileSysopError) -> Self {
        match value {
            file::FileSysopError::AddressSpace(err) => Self::AddressSpace(err),
            file::FileSysopError::BadFileDescriptor => Self::BadFileDescriptor,
            file::FileSysopError::InvalidArgument => Self::InvalidArgument,
            file::FileSysopError::NotFound => Self::NotFound,
            file::FileSysopError::PermissionDenied => Self::PermissionDenied,
            file::FileSysopError::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            file::FileSysopError::Unsupported => Self::Unsupported,
        }
    }
}

pub(crate) fn write(fd: u64, user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    if matches!(fd, 1 | 2) {
        return console::write_from_current_process(user_ptr, len).map_err(Into::into);
    }

    if let Some(written) = file::write_current_process_file(fd, user_ptr, user_len)? {
        return Ok(written);
    }

    Err(LinuxSysopError::BadFileDescriptor)
}

pub(crate) fn read(fd: u64, user_ptr: u64, user_len: u64) -> Result<usize, LinuxSysopError> {
    if fd == 0 {
        let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        if len == 0 {
            return Ok(0);
        }

        return console::read_into_current_process(user_ptr, len).map_err(Into::into);
    }

    if let Some(read) = file::read_current_process_file(fd, user_ptr, user_len)? {
        return Ok(read);
    }

    device::read_current_process_handle(fd, user_ptr, user_len).map_err(Into::into)
}

pub(crate) fn writev(fd: u64, iov_ptr: u64, iov_count: u64) -> Result<usize, LinuxSysopError> {
    let iov_count = usize::try_from(iov_count).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if iov_count > MAX_IOV_COUNT {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if iov_count == 0 {
        return Ok(0);
    }

    let mut total_written = 0usize;
    for index in 0..iov_count {
        let iovec_ptr = iov_ptr
            .checked_add((index * size_of::<linux_abi::LinuxIovec>()) as u64)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        let mut iovec = linux_abi::LinuxIovec::default();
        let iovec_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(iovec).cast::<u8>(),
                size_of::<linux_abi::LinuxIovec>(),
            )
        };
        usermem::copy_from_current_user_exact(iovec_ptr, iovec_bytes)?;
        if iovec.iov_len == 0 {
            continue;
        }

        let written = write(fd, iovec.iov_base, iovec.iov_len)?;
        total_written = total_written
            .checked_add(written)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        if written < usize::try_from(iovec.iov_len).map_err(|_| LinuxSysopError::InvalidArgument)? {
            break;
        }
    }

    Ok(total_written)
}

pub(crate) fn close(fd: u64) -> Result<(), LinuxSysopError> {
    if fd <= 2 {
        return Ok(());
    }

    device::close_current_process_handle(fd).map_err(Into::into)
}

pub(crate) fn openat(
    dirfd: u64,
    path_ptr: u64,
    flags: u64,
    mode: u64,
) -> Result<u64, LinuxSysopError> {
    let _ = flags;
    let _ = mode;

    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    let is_absolute = path.as_bytes().first().copied() == Some(b'/');
    if !is_absolute && dirfd != linux_abi::AT_FDCWD as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if path.starts_with("/dev/") {
        return device::open_path_for_current_process(&path).map_err(Into::into);
    }

    file::open_path_for_current_process(&path, flags, mode).map_err(Into::into)
}

pub(crate) fn access(path_ptr: u64, mode: u64) -> Result<(), LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    check_access_path(linux_abi::AT_FDCWD as u64, &path, mode, 0)
}

pub(crate) fn faccessat(
    dirfd: u64,
    path_ptr: u64,
    mode: u64,
    flags: u64,
) -> Result<(), LinuxSysopError> {
    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    check_access_path(dirfd, &path, mode, flags)
}

pub(crate) fn pread64(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    offset: u64,
) -> Result<usize, LinuxSysopError> {
    if fd <= 2 {
        return Err(LinuxSysopError::IllegalSeek);
    }

    match file::pread_current_process_file(fd, user_ptr, user_len, offset)? {
        Some(read) => Ok(read),
        None => Err(LinuxSysopError::IllegalSeek),
    }
}

pub(crate) fn lseek(fd: u64, offset: i64, whence: u64) -> Result<u64, LinuxSysopError> {
    if fd <= 2 {
        return Err(LinuxSysopError::IllegalSeek);
    }

    let whence = match whence {
        linux_abi::SEEK_SET => BootFileSeekWhence::Start,
        linux_abi::SEEK_CUR => BootFileSeekWhence::Current,
        linux_abi::SEEK_END => BootFileSeekWhence::End,
        _ => return Err(LinuxSysopError::InvalidArgument),
    };

    match file::seek_current_process_file(fd, offset, whence)? {
        Some(position) => Ok(position),
        None => Err(LinuxSysopError::IllegalSeek),
    }
}

pub(crate) fn fstat(fd: u64, stat_ptr: u64) -> Result<(), LinuxSysopError> {
    let stat = if fd <= 2 {
        build_device_stat(fd)
    } else {
        match file::metadata_current_process_file(fd)? {
            Some(metadata) => build_regular_file_stat(fd, metadata.len),
            None => stat_for_non_file_handle(fd)?,
        }
    };

    write_linux_stat(stat_ptr, &stat)
}

pub(crate) fn newfstatat(
    dirfd: u64,
    path_ptr: u64,
    stat_ptr: u64,
    flags: u64,
) -> Result<(), LinuxSysopError> {
    let supported_flags = linux_abi::AT_EMPTY_PATH;
    if flags & !supported_flags != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let path = usermem::read_current_user_c_string(path_ptr, 128)?;
    if path.is_empty() {
        if flags & linux_abi::AT_EMPTY_PATH == 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        return fstat(dirfd, stat_ptr);
    }

    let is_absolute = path.as_bytes().first().copied() == Some(b'/');
    if !is_absolute && dirfd != linux_abi::AT_FDCWD as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let stat = if path.starts_with("/dev/") {
        match device_ns::lookup(&path) {
            Ok(descriptor) => build_device_stat(device_inode_seed(descriptor.path.as_bytes())),
            Err(device_ns::DeviceLookupError::NotFound) => return Err(LinuxSysopError::NotFound),
            Err(device_ns::DeviceLookupError::InvalidPath) => {
                return Err(LinuxSysopError::InvalidArgument);
            }
        }
    } else {
        let metadata = file::metadata_for_current_process_path(&path)?;
        build_regular_file_stat(path_inode_seed(path.as_bytes()), metadata.len)
    };

    write_linux_stat(stat_ptr, &stat)
}

pub(crate) fn clock_gettime(clock_id: u64, timespec_ptr: u64) -> Result<(), LinuxSysopError> {
    let timespec = match clock_id {
        linux_abi::CLOCK_REALTIME => realtime_timespec(),
        linux_abi::CLOCK_MONOTONIC => monotonic_timespec(),
        _ => return Err(LinuxSysopError::InvalidArgument),
    };
    write_user_timespec(timespec_ptr, &timespec)
}

pub(crate) fn set_robust_list(head_ptr: u64, len: u64) -> Result<(), LinuxSysopError> {
    if head_ptr != 0 {
        let head_len = usize::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let Some(result) =
            multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
                if abi != UserAbi::Linux {
                    return Err(LinuxSysopError::Unsupported);
                }
                let Some(state) = linux_state.as_mut() else {
                    return Err(LinuxSysopError::Unsupported);
                };
                if head_len != 0 {
                    address_space
                        .validate_user_read_buffer(VirtAddr::new(head_ptr), head_len)
                        .map_err(LinuxSysopError::AddressSpace)?;
                }
                state.robust_list_head = head_ptr;
                state.robust_list_len = len;
                Ok(())
            })
        else {
            return Err(LinuxSysopError::Unsupported);
        };

        return result;
    }

    let Some(result) = multitask::with_current_user_process_mut(|_, abi, _, linux_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }
        let Some(state) = linux_state.as_mut() else {
            return Err(LinuxSysopError::Unsupported);
        };
        state.robust_list_head = 0;
        state.robust_list_len = len;
        Ok(())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn prlimit64(
    pid: u64,
    resource: u64,
    new_limit_ptr: u64,
    old_limit_ptr: u64,
) -> Result<(), LinuxSysopError> {
    if pid != 0 && Some(pid) != multitask::current_user_id() {
        return Err(LinuxSysopError::PermissionDenied);
    }
    if resource != linux_abi::RLIMIT_STACK {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if new_limit_ptr != 0 {
        let mut requested = linux_abi::LinuxRlimit::default();
        let requested_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(requested).cast::<u8>(),
                size_of::<linux_abi::LinuxRlimit>(),
            )
        };
        usermem::copy_from_current_user_exact(new_limit_ptr, requested_bytes)?;
    }
    if old_limit_ptr != 0 {
        let current = linux_abi::LinuxRlimit {
            rlim_cur: DEFAULT_STACK_RLIMIT_BYTES,
            rlim_max: DEFAULT_STACK_RLIMIT_BYTES,
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                core::ptr::addr_of!(current).cast::<u8>(),
                size_of::<linux_abi::LinuxRlimit>(),
            )
        };
        usermem::write_current_user_bytes(old_limit_ptr, bytes)?;
    }
    Ok(())
}

pub(crate) fn getrandom(
    user_ptr: u64,
    user_len: u64,
    flags: u64,
) -> Result<usize, LinuxSysopError> {
    if flags & !(GETRANDOM_FLAG_NONBLOCK | GETRANDOM_FLAG_RANDOM) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    let mut rng = crate::random::Random::new();
    let mut copied = 0usize;
    let mut chunk = [0_u8; 256];
    while copied < len {
        let chunk_len = (len - copied).min(chunk.len());
        rng.fill_bytes(&mut chunk[..chunk_len]);
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        usermem::write_current_user_bytes(chunk_ptr, &chunk[..chunk_len])?;
        copied += chunk_len;
    }
    Ok(len)
}

pub(crate) fn rseq(
    area_ptr: u64,
    len: u64,
    flags: u64,
    signature: u64,
) -> Result<(), LinuxSysopError> {
    if flags & !RSEQ_FLAG_UNREGISTER != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let signature = u32::try_from(signature).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let len_u32 = u32::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;

    let Some(result) =
        multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
            if abi != UserAbi::Linux {
                return Err(LinuxSysopError::Unsupported);
            }
            let Some(state) = linux_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            if flags & RSEQ_FLAG_UNREGISTER != 0 {
                state.rseq_area = 0;
                state.rseq_len = 0;
                state.rseq_signature = 0;
                return Ok(());
            }

            let area_len = usize::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;
            if area_len != 0 {
                address_space
                    .validate_user_write_buffer(VirtAddr::new(area_ptr), area_len)
                    .map_err(LinuxSysopError::AddressSpace)?;
            }
            state.rseq_area = area_ptr;
            state.rseq_len = len_u32;
            state.rseq_signature = signature;
            Ok(())
        })
    else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> Result<u64, LinuxSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;
    if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if !linux_mmap_fd_is_anonymous(fd) {
        if let Some(mapped_addr) =
            mmap_current_process_file(fd, requested_addr, user_len, prot, flags, offset)?
        {
            return Ok(mapped_addr);
        }

        if requested_addr != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        return device::mmap_current_process_handle(fd, user_len, prot, flags, offset)
            .map_err(Into::into);
    }

    if offset != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if flags & linux_abi::MAP_PRIVATE == 0 || flags & linux_abi::MAP_ANONYMOUS == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let page_count = len.div_ceil(PAGE_SIZE as usize);
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let region = {
            let (address_space, linux_state) = process_state.address_space_and_linux_state_mut();
            let Some(state) = linux_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };

            map_linux_user_region(
                address_space,
                state,
                requested_addr,
                fixed_mapping,
                page_count,
                page_flags,
            )?
        };
        process_state.set_mapping_cursor(region.end().as_u64());
        Ok(region.start.as_u64())
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn linux_mmap_fd_is_anonymous(fd: u64) -> bool {
    fd == u64::MAX || fd == u32::MAX as u64
}

pub(crate) fn munmap(start: u64, user_len: u64) -> Result<(), LinuxSysopError> {
    device::munmap_current_process_range(start, user_len).map_err(Into::into)
}

pub(crate) fn mprotect(start: u64, user_len: u64, prot: u64) -> Result<(), LinuxSysopError> {
    let supported_prot = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported_prot != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(());
    }

    let Some(result) = multitask::with_current_user_process_mut(|_, abi, address_space, _| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let validation = if prot & linux_abi::PROT_WRITE != 0 {
            address_space.validate_user_write_buffer(VirtAddr::new(start), len)
        } else {
            address_space.validate_user_read_buffer(VirtAddr::new(start), len)
        };

        validation.map_err(LinuxSysopError::AddressSpace)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn brk(addr: u64) -> u64 {
    let Some(result) =
        multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
            if abi != UserAbi::Linux {
                return 0;
            }

            let Some(state) = linux_state.as_mut() else {
                return 0;
            };
            if addr == 0 {
                return state.brk_current;
            }
            if addr < state.brk_start {
                return state.brk_current;
            }

            let requested_mapped_end = align_up(addr, PAGE_SIZE);
            if !state.can_grow_brk_to(requested_mapped_end) {
                return state.brk_current;
            }

            if requested_mapped_end > state.brk_mapped_end {
                let delta = requested_mapped_end - state.brk_mapped_end;
                let page_count = (delta / PAGE_SIZE) as usize;
                let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
                if address_space
                    .map_zeroed_user_pages_at(
                        VirtAddr::new(state.brk_mapped_end),
                        page_count,
                        flags,
                    )
                    .is_err()
                {
                    return state.brk_current;
                }
                state.brk_mapped_end = requested_mapped_end;
            }

            state.brk_current = addr;
            addr
        })
    else {
        return 0;
    };

    result
}

pub(crate) fn rt_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    sigset_size: u64,
) -> Result<(), LinuxSysopError> {
    let _ = how;
    if sigset_size != LINUX_SIGSET_SIZE {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if set_ptr != 0 {
        let mut incoming = [0_u8; LINUX_SIGSET_SIZE as usize];
        usermem::copy_from_current_user_exact(set_ptr, &mut incoming)?;
    }
    if oldset_ptr != 0 {
        usermem::write_current_user_bytes(oldset_ptr, &0_u64.to_le_bytes())?;
    }
    Ok(())
}

pub(crate) fn ioctl(fd: u64, _request: u64, _arg: u64) -> Result<u64, LinuxSysopError> {
    if matches!(fd, 0 | 1 | 2) {
        return Err(LinuxSysopError::NotTty);
    }

    device::ioctl_current_process_handle(fd, _request, _arg).map_err(Into::into)
}

pub(crate) fn getpid() -> u64 {
    multitask::current_user_id().unwrap_or(0)
}

pub(crate) fn arch_prctl(code: u64, arg: u64) -> Result<u64, LinuxSysopError> {
    match code {
        linux_abi::ARCH_SET_FS => {
            if arg != 0
                && !(paging::USER_SPACE_BASE..paging::USER_SPACE_END_EXCLUSIVE).contains(&arg)
            {
                return Err(LinuxSysopError::InvalidArgument);
            }

            let Some(result) =
                multitask::with_current_user_process_mut(|_, abi, _, linux_state| {
                    if abi != UserAbi::Linux {
                        return Err(LinuxSysopError::Unsupported);
                    }

                    let Some(state) = linux_state.as_mut() else {
                        return Err(LinuxSysopError::Unsupported);
                    };
                    state.fs_base = arg;
                    FsBase::write(VirtAddr::new(arg));
                    Ok(0)
                })
            else {
                return Err(LinuxSysopError::Unsupported);
            };

            result
        }
        linux_abi::ARCH_GET_FS => {
            usermem::write_current_user_bytes(arg, &FsBase::read().as_u64().to_le_bytes())?;
            Ok(0)
        }
        _ => Err(LinuxSysopError::InvalidArgument),
    }
}

pub(crate) fn set_tid_address(user_ptr: u64) -> Result<u64, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_mut(|pid, abi, _, linux_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        let Some(state) = linux_state.as_mut() else {
            return Err(LinuxSysopError::Unsupported);
        };
        state.clear_child_tid = user_ptr;
        Ok(pid)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

pub(crate) fn nanosleep(request_ptr: u64, remaining_ptr: u64) -> Result<(), LinuxSysopError> {
    let request = read_user_timespec(request_ptr)?;
    sleep_for_timespec(&request)?;
    write_zero_timespec(remaining_ptr)
}

pub(crate) fn clock_nanosleep(
    clock_id: u64,
    flags: u64,
    request_ptr: u64,
    remaining_ptr: u64,
) -> Result<(), LinuxSysopError> {
    let request = read_user_timespec(request_ptr)?;
    match clock_id {
        linux_abi::CLOCK_REALTIME | linux_abi::CLOCK_MONOTONIC => {}
        _ => return Err(LinuxSysopError::InvalidArgument),
    }
    if flags & !linux_abi::TIMER_ABSTIME != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if flags & linux_abi::TIMER_ABSTIME != 0 {
        let now = match clock_id {
            linux_abi::CLOCK_REALTIME => realtime_timespec(),
            linux_abi::CLOCK_MONOTONIC => monotonic_timespec(),
            _ => unreachable!(),
        };
        if let Some(remaining) = saturating_timespec_sub(&request, &now) {
            sleep_for_timespec(&remaining)?;
        }
    } else {
        sleep_for_timespec(&request)?;
    }

    write_zero_timespec(remaining_ptr)
}

pub(crate) fn exit_current_process(status: u64) -> ! {
    if status != 0 {
        debug::println!("user process exited with status {}", status);
    }
    let _ = multitask::with_current_user_process_mut(|_, abi, address_space, linux_state| {
        if abi != UserAbi::Linux {
            return;
        }

        let Some(state) = linux_state.as_mut() else {
            return;
        };
        if state.clear_child_tid == 0 {
            return;
        }

        let _ = address_space
            .copy_into_user(VirtAddr::new(state.clear_child_tid), &0_u32.to_le_bytes());
        state.clear_child_tid = 0;
    });
    multitask::exit_current_user_task()
}

fn linux_mmap_page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if prot & linux_abi::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & linux_abi::PROT_EXEC == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

fn align_up(value: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    value.saturating_add(align - 1) & !(align - 1)
}

fn map_linux_user_region(
    address_space: &mut paging::ProcessAddressSpace,
    linux_state: &mut linux_abi::LinuxTaskState,
    requested_addr: u64,
    fixed_mapping: bool,
    page_count: usize,
    page_flags: PageTableFlags,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let span = (page_count as u64)
        .checked_mul(PAGE_SIZE)
        .ok_or(LinuxSysopError::NoMemory)?;
    let default_start = align_up(linux_state.mmap_next, PAGE_SIZE);

    if fixed_mapping {
        return map_linux_user_region_at(
            address_space,
            linux_state,
            requested_addr,
            span,
            page_count,
            page_flags,
            true,
        );
    }

    if requested_addr != 0 {
        let hinted_start = align_up(requested_addr, PAGE_SIZE);
        if let Ok(region) = map_linux_user_region_at(
            address_space,
            linux_state,
            hinted_start,
            span,
            page_count,
            page_flags,
            false,
        ) {
            return Ok(region);
        }
    }

    map_linux_user_region_at(
        address_space,
        linux_state,
        default_start,
        span,
        page_count,
        page_flags,
        false,
    )
}

fn map_linux_user_region_at(
    address_space: &mut paging::ProcessAddressSpace,
    linux_state: &mut linux_abi::LinuxTaskState,
    start: u64,
    span: u64,
    page_count: usize,
    page_flags: PageTableFlags,
    replace_existing: bool,
) -> Result<crate::paging::UserRegion, LinuxSysopError> {
    let end = start.checked_add(span).ok_or(LinuxSysopError::NoMemory)?;
    if end > linux_state.brk_limit() || end <= linux_state.brk_mapped_end {
        return Err(LinuxSysopError::NoMemory);
    }

    if replace_existing {
        match address_space.unmap_user_bytes(
            VirtAddr::new(start),
            usize::try_from(span).map_err(|_| LinuxSysopError::InvalidArgument)?,
        ) {
            Ok(_) | Err(paging::AddressSpaceError::NotMapped) => {}
            Err(err) => return Err(LinuxSysopError::AddressSpace(err)),
        }
    }

    let region = address_space
        .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, page_flags)
        .map_err(LinuxSysopError::AddressSpace)?;
    if region.end().as_u64() > linux_state.mmap_next {
        linux_state.mmap_next = align_up(region.end().as_u64(), PAGE_SIZE);
    }
    Ok(region)
}

fn mmap_current_process_file(
    fd: u64,
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    offset: u64,
) -> Result<Option<u64>, LinuxSysopError> {
    let file_map_len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let page_count = usize::try_from(user_len.div_ceil(PAGE_SIZE))
        .map_err(|_| LinuxSysopError::InvalidArgument)?;
    let fixed_mapping = flags & linux_abi::MAP_FIXED != 0;

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }

        if !matches!(
            process_state.handles().get(fd),
            Some(KernelHandle::BootFile(_))
        ) {
            return Ok(None);
        }
        if offset & (PAGE_SIZE - 1) != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if flags & linux_abi::MAP_ANONYMOUS != 0 || flags & linux_abi::MAP_PRIVATE == 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        if fixed_mapping && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
            return Err(LinuxSysopError::InvalidArgument);
        }

        let page_flags = linux_mmap_page_flags(prot);
        let file_offset = usize::try_from(offset).map_err(|_| LinuxSysopError::InvalidArgument)?;
        let region = {
            let (address_space, linux_state) = process_state.address_space_and_linux_state_mut();
            let Some(state) = linux_state.as_mut() else {
                return Err(LinuxSysopError::Unsupported);
            };
            map_linux_user_region(
                address_space,
                state,
                requested_addr,
                fixed_mapping,
                page_count,
                page_flags,
            )?
        };
        process_state.set_mapping_cursor(region.end().as_u64());

        let file_len = match process_state.handles().get(fd) {
            Some(KernelHandle::BootFile(file)) => file.len(),
            Some(_) => return Ok(None),
            None => return Err(LinuxSysopError::BadFileDescriptor),
        };

        if file_offset < file_len {
            let copy_len = file_map_len.min(file_len - file_offset);
            let mut copied = 0usize;
            let mut chunk = [0_u8; FILE_MMAP_COPY_CHUNK_LEN];
            while copied < copy_len {
                let chunk_len = (copy_len - copied).min(chunk.len());
                let read = {
                    let Some(KernelHandle::BootFile(file)) =
                        process_state.handles_mut().get_mut(fd)
                    else {
                        return Err(LinuxSysopError::BadFileDescriptor);
                    };
                    file.read_at(file_offset + copied, &mut chunk[..chunk_len])
                };
                if read == 0 {
                    break;
                }

                let chunk_ptr = region
                    .start
                    .as_u64()
                    .checked_add(copied as u64)
                    .ok_or(LinuxSysopError::InvalidArgument)?;
                process_state
                    .address_space()
                    .initialize_user_bytes(VirtAddr::new(chunk_ptr), &chunk[..read])
                    .map_err(LinuxSysopError::AddressSpace)?;
                copied += read;
            }
        }

        Ok(Some(region.start.as_u64()))
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn write_linux_stat(stat_ptr: u64, stat: &linux_abi::LinuxStat) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            (stat as *const linux_abi::LinuxStat).cast::<u8>(),
            size_of::<linux_abi::LinuxStat>(),
        )
    };
    usermem::write_current_user_bytes(stat_ptr, bytes)?;
    Ok(())
}

fn stat_for_non_file_handle(fd: u64) -> Result<linux_abi::LinuxStat, LinuxSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        let Some(handle) = process_state.handles().get(fd) else {
            return Err(LinuxSysopError::BadFileDescriptor);
        };

        let stat = match handle {
            KernelHandle::Device(_) => build_device_stat(fd),
            KernelHandle::DisplaySurface(surface) => {
                build_regular_file_stat(fd, surface.frame_len())
            }
            KernelHandle::BootFile(file) => build_regular_file_stat(fd, file.len() as u64),
        };
        Ok(stat)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };

    result
}

fn build_regular_file_stat(inode: u64, len: u64) -> linux_abi::LinuxStat {
    linux_abi::LinuxStat {
        st_ino: inode.max(1),
        st_nlink: 1,
        st_mode: linux_abi::BOOT_FILE_MODE_BITS,
        st_size: len.min(i64::MAX as u64) as i64,
        st_blksize: PAGE_SIZE as i64,
        st_blocks: len.div_ceil(512) as i64,
        ..linux_abi::LinuxStat::default()
    }
}

fn build_device_stat(inode: u64) -> linux_abi::LinuxStat {
    linux_abi::LinuxStat {
        st_ino: inode.max(1),
        st_nlink: 1,
        st_mode: linux_abi::DEVICE_FILE_MODE_BITS,
        st_blksize: PAGE_SIZE as i64,
        ..linux_abi::LinuxStat::default()
    }
}

fn path_inode_seed(path: &[u8]) -> u64 {
    fnv1a64(path)
}

fn device_inode_seed(path: &[u8]) -> u64 {
    fnv1a64(path)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn check_access_path(dirfd: u64, path: &str, mode: u64, flags: u64) -> Result<(), LinuxSysopError> {
    if flags & !linux_abi::AT_EACCESS != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let is_absolute = path.as_bytes().first().copied() == Some(b'/');
    if !is_absolute && dirfd != linux_abi::AT_FDCWD as u64 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    if path.starts_with("/dev/") {
        if mode & !(linux_abi::R_OK | linux_abi::W_OK | linux_abi::X_OK | linux_abi::F_OK) != 0 {
            return Err(LinuxSysopError::InvalidArgument);
        }
        match device_ns::lookup(path) {
            Ok(_) => {
                if mode & (linux_abi::W_OK | linux_abi::X_OK) != 0 {
                    Err(LinuxSysopError::PermissionDenied)
                } else {
                    Ok(())
                }
            }
            Err(device_ns::DeviceLookupError::NotFound) => Err(LinuxSysopError::NotFound),
            Err(device_ns::DeviceLookupError::InvalidPath) => Err(LinuxSysopError::InvalidArgument),
        }
    } else {
        file::check_access_for_current_process(path, mode).map_err(Into::into)
    }
}

fn write_user_timespec(
    timespec_ptr: u64,
    timespec: &linux_abi::LinuxTimespec,
) -> Result<(), LinuxSysopError> {
    let bytes = unsafe {
        slice::from_raw_parts(
            (timespec as *const linux_abi::LinuxTimespec).cast::<u8>(),
            size_of::<linux_abi::LinuxTimespec>(),
        )
    };
    usermem::write_current_user_bytes(timespec_ptr, bytes)?;
    Ok(())
}

fn realtime_timespec() -> linux_abi::LinuxTimespec {
    let now = rtc::now();
    linux_abi::LinuxTimespec {
        tv_sec: unix_seconds_from_rtc(now),
        tv_nsec: 0,
    }
}

fn monotonic_timespec() -> linux_abi::LinuxTimespec {
    let ticks = rtc::ticks();
    let ticks_per_second = rtc::ticks_per_second().max(1);
    let seconds = ticks / ticks_per_second;
    let tick_remainder = ticks % ticks_per_second;
    let nanoseconds =
        ((tick_remainder as u128) * 1_000_000_000_u128 / (ticks_per_second as u128)) as i64;
    linux_abi::LinuxTimespec {
        tv_sec: seconds.min(i64::MAX as u64) as i64,
        tv_nsec: nanoseconds,
    }
}

fn read_user_timespec(user_ptr: u64) -> Result<linux_abi::LinuxTimespec, LinuxSysopError> {
    let mut request = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let request_bytes = unsafe {
        core::slice::from_raw_parts_mut(
            core::ptr::addr_of_mut!(request).cast::<u8>(),
            core::mem::size_of::<linux_abi::LinuxTimespec>(),
        )
    };
    usermem::copy_from_current_user_exact(user_ptr, request_bytes)?;
    validate_timespec(&request)?;
    Ok(request)
}

fn validate_timespec(timespec: &linux_abi::LinuxTimespec) -> Result<(), LinuxSysopError> {
    if timespec.tv_sec < 0 || !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return Err(LinuxSysopError::InvalidArgument);
    }
    Ok(())
}

fn sleep_for_timespec(timespec: &linux_abi::LinuxTimespec) -> Result<(), LinuxSysopError> {
    validate_timespec(timespec)?;
    let seconds = u64::try_from(timespec.tv_sec).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let nanoseconds =
        u64::try_from(timespec.tv_nsec).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let milliseconds = seconds
        .checked_mul(1_000)
        .and_then(|value| value.checked_add(nanoseconds.div_ceil(1_000_000)))
        .unwrap_or(u64::MAX);
    rtc::sleep(milliseconds);
    Ok(())
}

fn write_zero_timespec(user_ptr: u64) -> Result<(), LinuxSysopError> {
    if user_ptr == 0 {
        return Ok(());
    }

    let zero = linux_abi::LinuxTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    write_user_timespec(user_ptr, &zero)
}

fn saturating_timespec_sub(
    target: &linux_abi::LinuxTimespec,
    current: &linux_abi::LinuxTimespec,
) -> Option<linux_abi::LinuxTimespec> {
    let target_ns = timespec_to_nanos(target)?;
    let current_ns = timespec_to_nanos(current)?;
    if target_ns <= current_ns {
        return None;
    }

    let delta_ns = target_ns - current_ns;
    Some(linux_abi::LinuxTimespec {
        tv_sec: i64::try_from(delta_ns / 1_000_000_000).ok()?,
        tv_nsec: i64::try_from(delta_ns % 1_000_000_000).ok()?,
    })
}

fn timespec_to_nanos(timespec: &linux_abi::LinuxTimespec) -> Option<u128> {
    let seconds = u128::try_from(timespec.tv_sec).ok()?;
    let nanoseconds = u128::try_from(timespec.tv_nsec).ok()?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
}

fn unix_seconds_from_rtc(datetime: rtc::RtcDateTime) -> i64 {
    let days = days_from_civil(
        i32::from(datetime.year),
        u32::from(datetime.month),
        u32::from(datetime.day),
    );
    let seconds_in_day = i64::from(datetime.hour) * 3600
        + i64::from(datetime.minute) * 60
        + i64::from(datetime.second);
    days.saturating_mul(86_400).saturating_add(seconds_in_day)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

#[cfg(test)]
mod tests {
    use super::{days_from_civil, unix_seconds_from_rtc};
    use crate::rtc::RtcDateTime;

    #[test]
    fn unix_epoch_day_is_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn rtc_datetime_converts_to_unix_seconds() {
        assert_eq!(
            unix_seconds_from_rtc(RtcDateTime {
                year: 1970,
                month: 1,
                day: 1,
                weekday: 4,
                hour: 0,
                minute: 0,
                second: 0,
            }),
            0
        );
        assert_eq!(
            unix_seconds_from_rtc(RtcDateTime {
                year: 1970,
                month: 1,
                day: 2,
                weekday: 5,
                hour: 1,
                minute: 1,
                second: 1,
            }),
            90_061
        );
    }
}
