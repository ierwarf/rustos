use super::*;

use alloc::vec::Vec;
use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use rustos_user_abi::syscall::{
    IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY, MM_BROKER_ABI_VERSION, MM_BROKER_FD_KIND_DEVICE,
    MM_BROKER_FD_KIND_DISPLAY_SURFACE, MM_BROKER_FD_KIND_FILE, MM_BROKER_FD_KIND_MEMFD,
    MM_BROKER_FD_KIND_NONE, MM_BROKER_FD_RIGHT_MAP, MM_BROKER_FD_RIGHT_READ,
    MM_BROKER_FD_RIGHT_WRITE, MM_BROKER_OP_DESCRIBE_FD, MM_BROKER_OP_MAP_ANON,
    MM_BROKER_OP_MAP_DEVICE_SHARED, MM_BROKER_OP_MAP_FILE_PRIVATE, MM_BROKER_OP_MAP_MEMFD_SHARED,
    MM_BROKER_OP_PROTECT, MM_BROKER_OP_QUERY_LAYOUT, MM_BROKER_OP_UNMAP, MM_BROKER_PATH_CAPACITY,
    RustosMmBrokerArgs, RustosMmFdBrokerResult, RustosMmLayoutBrokerResult,
    RustosMmMapBrokerResult,
};

use crate::user::handles::{KernelHandle, RemoteVfsHandleKind, VfsFileHandle};
use crate::user::memfd::{MemfdError, MemfdHandle};

const PAGE_SIZE: u64 = 4096;
const FILE_COPY_CHUNK: usize = 4096;

#[derive(Clone)]
enum FileMappingSource {
    Vfs(VfsFileHandle),
    Remote { remote_id: u64 },
}

pub(super) fn syscall_linux_rustos_mm_broker(args_ptr: u64) -> u64 {
    if !ipc_ops::current_process_has_service_capability(IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY) {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosMmBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != MM_BROKER_ABI_VERSION {
        return linux_errno(LINUX_EINVAL);
    }

    let result = match args.op {
        MM_BROKER_OP_QUERY_LAYOUT => broker_query_layout(&args),
        MM_BROKER_OP_DESCRIBE_FD => broker_describe_fd(&args),
        MM_BROKER_OP_MAP_ANON => broker_map_anon(&args),
        MM_BROKER_OP_MAP_FILE_PRIVATE => broker_map_file_private(&args),
        MM_BROKER_OP_MAP_MEMFD_SHARED => broker_map_memfd_shared(&args),
        MM_BROKER_OP_MAP_DEVICE_SHARED => broker_map_device_shared(&args),
        MM_BROKER_OP_PROTECT => broker_protect(&args),
        MM_BROKER_OP_UNMAP => broker_unmap(&args),
        _ => Err(LINUX_EINVAL),
    };

    match result {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

fn broker_query_layout(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let linux = process_state
            .linux_process_state()
            .copied()
            .unwrap_or_default();
        RustosMmLayoutBrokerResult {
            brk_start: linux.brk_start,
            brk_current: linux.brk_current,
            brk_mapped_end: linux.brk_mapped_end,
            mmap_next: linux.mmap_next,
            user_range_start: paging::USER_SPACE_BASE,
            user_range_end: paging::USER_SPACE_END_EXCLUSIVE,
        }
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result)
}

fn broker_describe_fd(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let Some(entry) = process_state.handles().get_entry(args.fd) else {
            return Err(LINUX_EBADF);
        };
        let mut result = RustosMmFdBrokerResult::default();
        result.rights = broker_rights(entry.rights());

        match entry.handle() {
            KernelHandle::VfsFile(file) => {
                result.kind = MM_BROKER_FD_KIND_FILE;
                result.len = file.len() as u64;
                copy_path(&mut result, &file.path());
            }
            KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                result.kind = MM_BROKER_FD_KIND_FILE;
                result.len = remote.len();
                copy_path(&mut result, &remote.path());
            }
            KernelHandle::Memfd(memfd) => {
                result.kind = MM_BROKER_FD_KIND_MEMFD;
                result.len = memfd.len() as u64;
                copy_path(&mut result, &memfd.path());
            }
            KernelHandle::DisplaySurface(surface) => {
                result.kind = MM_BROKER_FD_KIND_DISPLAY_SURFACE;
                result.len = surface.mapping_len();
                copy_path(&mut result, "display-surface");
            }
            KernelHandle::Device(_) => {
                result.kind = MM_BROKER_FD_KIND_DEVICE;
                copy_path(&mut result, "device");
            }
            _ => {
                result.kind = MM_BROKER_FD_KIND_NONE;
            }
        }
        Ok(result)
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_map_anon(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_count = page_count(len)?;
    let page_flags = protection_to_page_flags(args.prot)?;

    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        if args.prot == 0 {
            return Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
                addr: start,
                len: args.len,
            });
        }
        process_state
            .address_space_mut()
            .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, page_flags)
            .map_err(address_space_error_to_linux_errno)?;
        process_state.set_mapping_cursor(start.saturating_add(args.len));
        Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
            addr: start,
            len: args.len,
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_map_file_private(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let source = file_mapping_source(args.target_pid, args.fd)?;

    let Some(map_result) =
        multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
            process_state
                .address_space_mut()
                .map_zeroed_user_bytes_at(VirtAddr::new(start), len, page_flags)
                .map_err(address_space_error_to_linux_errno)?;
            process_state.set_mapping_cursor(start.saturating_add(args.len));
            Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
                addr: start,
                len: args.len,
            })
        })
    else {
        return Err(LINUX_ESRCH);
    };
    let map_result = map_result?;

    if let Err(errno) = copy_file_mapping(args.target_pid, start, len, args.offset, &source) {
        let _ = broker_unmap(args);
        return Err(errno);
    }
    write_out(args, &map_result)
}

fn broker_map_memfd_shared(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let memfd = memfd_source(args.target_pid, args.fd)?;
    let writable = args.prot & linux_abi::PROT_WRITE != 0;
    let (frames, hold) = memfd
        .acquire_mapping(
            usize::try_from(args.offset).map_err(|_| LINUX_EINVAL)?,
            len,
            writable,
        )
        .map_err(memfd_error_to_errno)?;
    let page_flags = protection_to_page_flags(args.prot)?;

    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        process_state
            .address_space_mut()
            .map_existing_user_pages_at(VirtAddr::new(start), &frames, page_flags)
            .map_err(address_space_error_to_linux_errno)?;
        process_state.record_shared_memfd_mapping(start, args.len, hold);
        process_state.set_mapping_cursor(start.saturating_add(args.len));
        Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
            addr: start,
            len: args.len,
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_map_device_shared(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_flags = surface_page_flags(args.prot)?;
    let page_count = page_count(len)?;

    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let entry = process_state
            .handles()
            .get_entry(args.fd)
            .ok_or(LINUX_EBADF)?;
        if !entry.rights().allows_shared_map() {
            return Err(LINUX_EACCES);
        }
        let KernelHandle::DisplaySurface(mut surface) = *entry.handle() else {
            return Err(LINUX_ENODEV);
        };
        if args.offset != 0 || args.len != surface.mapping_len() {
            return Err(LINUX_EINVAL);
        }
        if let Some(region) = surface.mapped_region() {
            return Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
                addr: region.start.as_u64(),
                len: region.end().as_u64().saturating_sub(region.start.as_u64()),
            });
        }
        let shared_region = surface.shared_region().ok_or(LINUX_EINVAL)?;
        let frames = crate::ipc::shared_region_frames(shared_region).ok_or(LINUX_EINVAL)?;
        if frames.len() != page_count {
            return Err(LINUX_EINVAL);
        }
        let region = process_state
            .address_space_mut()
            .map_existing_user_pages_at(VirtAddr::new(start), &frames, page_flags)
            .map_err(address_space_error_to_linux_errno)?;
        surface.set_mapped_region(region);
        let slot = process_state
            .handles_mut()
            .get_mut(args.fd)
            .ok_or(LINUX_EBADF)?;
        *slot = KernelHandle::DisplaySurface(surface);
        process_state.set_mapping_cursor(region.end().as_u64());
        Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
            addr: region.start.as_u64(),
            len: args.len,
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_protect(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        process_state
            .address_space_mut()
            .protect_user_bytes(VirtAddr::new(start), len, page_flags)
            .map_err(address_space_error_to_linux_errno)
    }) else {
        return Err(LINUX_ESRCH);
    };
    result
}

fn broker_unmap(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let end = start.checked_add(args.len).ok_or(LINUX_EINVAL)?;
    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let mut external_segments = Vec::new();
        external_segments.extend(
            process_state
                .shared_memfd_overlap_segments(start, end)
                .into_iter()
                .map(|(segment_start, segment_len)| {
                    (
                        segment_start,
                        segment_start.saturating_add(segment_len as u64),
                    )
                }),
        );
        external_segments.extend(
            process_state
                .handles()
                .surface_overlap_segments(start, end)
                .into_iter()
                .map(|(segment_start, segment_len)| {
                    (segment_start, segment_start.saturating_add(segment_len))
                }),
        );
        external_segments.sort_by_key(|(segment_start, _)| *segment_start);

        if external_segments.is_empty() {
            process_state
                .address_space_mut()
                .unmap_user_bytes(VirtAddr::new(start), len)
                .map_err(address_space_error_to_linux_errno)?;
        } else {
            let mut cursor = start;
            for (segment_start, segment_end) in external_segments {
                let segment_start = segment_start.max(cursor);
                let segment_end = segment_end.min(end);
                if cursor < segment_start {
                    let owned_len =
                        usize::try_from(segment_start - cursor).map_err(|_| LINUX_EINVAL)?;
                    process_state
                        .address_space_mut()
                        .unmap_user_bytes(VirtAddr::new(cursor), owned_len)
                        .map_err(address_space_error_to_linux_errno)?;
                }
                if segment_start < segment_end {
                    let pages = page_count(
                        usize::try_from(segment_end - segment_start).map_err(|_| LINUX_EINVAL)?,
                    )?;
                    process_state
                        .address_space_mut()
                        .unmap_user_pages_without_free_at(VirtAddr::new(segment_start), pages)
                        .map_err(address_space_error_to_linux_errno)?;
                    cursor = segment_end;
                }
            }
            if cursor < end {
                let owned_len = usize::try_from(end - cursor).map_err(|_| LINUX_EINVAL)?;
                process_state
                    .address_space_mut()
                    .unmap_user_bytes(VirtAddr::new(cursor), owned_len)
                    .map_err(address_space_error_to_linux_errno)?;
            }
        }
        process_state.release_shared_memfd_mappings_in_range(start, end);
        process_state
            .handles_mut()
            .clear_surface_mappings_in_range(start, args.len);
        Ok(())
    }) else {
        return Err(LINUX_ESRCH);
    };
    result
}

fn file_mapping_source(target_pid: u64, fd: u64) -> Result<FileMappingSource, i64> {
    let Some(source) = multitask::with_process_state_by_pid_mut(target_pid, |process_state| {
        let entry = process_state.handles().get_entry(fd).ok_or(LINUX_EBADF)?;
        if !entry.rights().allows_read() {
            return Err(LINUX_EACCES);
        }
        match entry.handle() {
            KernelHandle::VfsFile(file) => Ok(FileMappingSource::Vfs(file.clone())),
            KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                Ok(FileMappingSource::Remote {
                    remote_id: remote.remote_id(),
                })
            }
            _ => Err(LINUX_EINVAL),
        }
    }) else {
        return Err(LINUX_ESRCH);
    };
    source
}

fn memfd_source(target_pid: u64, fd: u64) -> Result<MemfdHandle, i64> {
    let Some(source) = multitask::with_process_state_by_pid_mut(target_pid, |process_state| {
        let entry = process_state.handles().get_entry(fd).ok_or(LINUX_EBADF)?;
        match entry.handle() {
            KernelHandle::Memfd(memfd) => Ok(memfd.clone()),
            _ => Err(LINUX_EINVAL),
        }
    }) else {
        return Err(LINUX_ESRCH);
    };
    source
}

fn copy_file_mapping(
    target_pid: u64,
    start: u64,
    len: usize,
    offset: u64,
    source: &FileMappingSource,
) -> Result<(), i64> {
    let mut copied = 0usize;
    let mut chunk = Vec::new();
    chunk.resize(FILE_COPY_CHUNK.min(len), 0);

    while copied < len {
        let count = (len - copied).min(chunk.len());
        let read = match source {
            FileMappingSource::Vfs(file) => file.read_at(
                usize::try_from(offset)
                    .map_err(|_| LINUX_EINVAL)?
                    .saturating_add(copied),
                &mut chunk[..count],
            ),
            FileMappingSource::Remote { remote_id } => {
                match offload_ops::call_remote_vfs_read_bytes(
                    *remote_id,
                    offset.saturating_add(copied as u64),
                    count,
                ) {
                    Ok(bytes) => {
                        let read = bytes.len().min(count);
                        chunk[..read].copy_from_slice(&bytes[..read]);
                        read
                    }
                    Err(err) => return Err(err),
                }
            }
        };
        if read == 0 {
            break;
        }

        let Some(result) = multitask::with_process_state_by_pid_mut(target_pid, |process_state| {
            process_state
                .address_space()
                .initialize_user_bytes(VirtAddr::new(start + copied as u64), &chunk[..read])
                .map_err(address_space_error_to_linux_errno)
        }) else {
            return Err(LINUX_ESRCH);
        };
        result?;
        copied += read;
    }
    Ok(())
}

fn write_out<T: Copy>(args: &RustosMmBrokerArgs, value: &T) -> Result<(), i64> {
    if args.out_ptr == 0 {
        return Ok(());
    }
    if args.out_len < core::mem::size_of::<T>() as u64 {
        return Err(LINUX_EINVAL);
    }
    usermem::write_current_user_struct(args.out_ptr, value)
        .map_err(address_space_error_to_linux_errno)
}

fn broker_rights(rights: kernel_object::api::handle::HandleRights) -> u64 {
    let mut value = 0;
    if rights.allows_read() {
        value |= MM_BROKER_FD_RIGHT_READ;
    }
    if rights.allows_write() {
        value |= MM_BROKER_FD_RIGHT_WRITE;
    }
    if rights.allows_shared_map() {
        value |= MM_BROKER_FD_RIGHT_MAP;
    }
    value
}

fn copy_path(result: &mut RustosMmFdBrokerResult, path: &str) {
    let bytes = path.as_bytes();
    let len = bytes.len().min(MM_BROKER_PATH_CAPACITY);
    result.path[..len].copy_from_slice(&bytes[..len]);
    result.path_len = len as u32;
}

fn checked_len(len: u64) -> Result<usize, i64> {
    if len == 0 {
        return Err(LINUX_EINVAL);
    }
    usize::try_from(len).map_err(|_| LINUX_EINVAL)
}

fn checked_page_addr(addr: u64) -> Result<u64, i64> {
    if addr == 0 || addr % PAGE_SIZE != 0 {
        return Err(LINUX_EINVAL);
    }
    Ok(addr)
}

fn page_count(len: usize) -> Result<usize, i64> {
    Ok(len
        .checked_add(PAGE_SIZE as usize - 1)
        .ok_or(LINUX_EINVAL)?
        / PAGE_SIZE as usize)
}

fn protection_to_page_flags(prot: u64) -> Result<PageTableFlags, i64> {
    let supported = linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC;
    if prot & !supported != 0
        || prot & linux_abi::PROT_WRITE != 0 && prot & linux_abi::PROT_EXEC != 0
    {
        return Err(LINUX_EINVAL);
    }
    let mut flags = PageTableFlags::NO_EXECUTE;
    if prot & linux_abi::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & linux_abi::PROT_EXEC != 0 {
        flags.remove(PageTableFlags::NO_EXECUTE);
    }
    Ok(flags)
}

fn surface_page_flags(prot: u64) -> Result<PageTableFlags, i64> {
    let supported = linux_abi::PROT_READ | linux_abi::PROT_WRITE;
    if prot & !supported != 0 || prot & linux_abi::PROT_EXEC != 0 {
        return Err(LINUX_EINVAL);
    }
    let mut flags = PageTableFlags::NO_EXECUTE;
    if prot & linux_abi::PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    Ok(flags)
}

fn memfd_error_to_errno(err: MemfdError) -> i64 {
    match err {
        MemfdError::Busy => LINUX_EBUSY,
        MemfdError::InvalidArgument => LINUX_EINVAL,
        MemfdError::NoMemory => LINUX_ENOMEM,
        MemfdError::PermissionDenied => LINUX_EACCES,
    }
}
