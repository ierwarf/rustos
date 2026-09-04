//! Bootstrap Linux memory syscalls before the MM broker is reachable.
//!
//! - **Owner:** `kernel-compat` owns the bootstrap `mmap`/`mprotect`/`munmap`/
//!   `brk` envelope; `kernel-mm` owns the page-table mechanism it calls.
//! - **Boundary:** Addresses, lengths, protections, and flags arrive from an
//!   untrusted guest and are canonicalized before any address-space call.
//! - **Lifecycle:** Validate and align the request, resolve or reserve the
//!   range, replace an existing `MAP_FIXED` range by unmapping it first, then
//!   publish the mapping and advance the cursor.
//! - **Concurrency:** Every address-space edit runs inside the current
//!   process-state critical section; no mapping decision is made outside it.
//! - **Failure:** A refused unmap, a reservation conflict, or an allocation
//!   shortfall returns a Linux errno and leaves the prior address space intact.
//! - **Forbidden:** No discarded address-space result, no guest-selected
//!   frame, no W+X protection, and no partial range left published on error.
//! - **Evidence:** `memory-map`.

use core::mem::size_of;

use x86_64::VirtAddr;

use crate::memory::PageTableFlags;

use super::*;

const PAGE_SIZE: u64 = 4096;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SigaltstackRequestExtra {
    has_new: u8,
    _pad: [u8; 7],
    new_ss_sp: u64,
    new_ss_size: u64,
    new_ss_flags: u32,
    _pad2: [u8; 4],
}

pub(super) fn syscall_linux_madvise(start: u64, user_len: u64, advice: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MADVISE);
    request.arg0 = start;
    request.arg1 = user_len;
    request.flags = advice;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_sigaltstack(stack_ptr: u64, old_stack_ptr: u64) -> u64 {
    const SIGALTSTACK_WIRE_SIZE: usize = 24; // ss_sp + ss_flags + (pad) + ss_size
    let mut request = new_procd_request(rustos_user_abi::syscall::PROCD_OP_SIGALTSTACK);
    let mut extra = SigaltstackRequestExtra::default();
    let Some(old_stack) = multitask::current_linux_thread_state().map(|state| state.signal_stack)
    else {
        // sigaltstack is per Linux thread.  Treating a missing thread state as
        // a disabled stack would manufacture a valid-looking ABI response.
        return linux_errno(LINUX_EINVAL);
    };
    if stack_ptr != 0 {
        if old_stack.flags & linux_abi::SS_ONSTACK != 0 {
            return linux_errno(LINUX_EPERM);
        }
        let mut buf = [0_u8; SIGALTSTACK_WIRE_SIZE];
        if let Err(err) = usermem::copy_from_current_user_exact(stack_ptr, &mut buf) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        let ss_sp = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let ss_flags = i32::from_le_bytes(buf[8..12].try_into().unwrap());
        let ss_size = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        extra.has_new = 1;
        extra.new_ss_sp = ss_sp;
        extra.new_ss_flags = ss_flags as u32;
        extra.new_ss_size = ss_size;
    }
    if old_stack_ptr != 0 {
        if let Err(err) =
            usermem::validate_current_user_write_buffer(old_stack_ptr, SIGALTSTACK_WIRE_SIZE)
        {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
        request.flags |= 0x1;
    }
    // SAFETY: `SigaltstackRequestExtra` is `repr(C)` and spells out both of its
    // padding runs as `_pad` fields, so every byte in `size_of` is a named
    // field that `Default` and the assignments above initialized. The slice
    // borrows `extra`, which outlives the copy into `request.payload` below.
    let extra_bytes = unsafe {
        core::slice::from_raw_parts(
            (&extra as *const SigaltstackRequestExtra).cast::<u8>(),
            size_of::<SigaltstackRequestExtra>(),
        )
    };
    if extra_bytes.len() > request.payload.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.payload_len = extra_bytes.len() as u32;
    request.payload[..extra_bytes.len()].copy_from_slice(extra_bytes);

    let response = match call_procd(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if extra.has_new != 0 {
        let _ = multitask::with_current_user_process_and_linux_thread_state_mut(
            |_, _, _, _, linux_thread_state| {
                let Some(state) = linux_thread_state.as_mut() else {
                    return;
                };
                state.signal_stack = if extra.new_ss_flags & linux_abi::SS_DISABLE != 0 {
                    linux_abi::LinuxSignalStack {
                        sp: 0,
                        flags: linux_abi::SS_DISABLE,
                        _pad: 0,
                        size: 0,
                    }
                } else {
                    linux_abi::LinuxSignalStack {
                        sp: extra.new_ss_sp,
                        flags: extra.new_ss_flags,
                        _pad: 0,
                        size: extra.new_ss_size,
                    }
                };
            },
        );
    }
    if old_stack_ptr != 0 {
        let mut out = [0_u8; SIGALTSTACK_WIRE_SIZE];
        let old_flags = if old_stack.size == 0 {
            linux_abi::SS_DISABLE
        } else {
            old_stack.flags
        };
        out[0..8].copy_from_slice(&old_stack.sp.to_le_bytes());
        out[8..12].copy_from_slice(&old_flags.to_le_bytes());
        out[16..24].copy_from_slice(&old_stack.size.to_le_bytes());
        if let Err(err) = usermem::write_current_user_bytes(old_stack_ptr, &out) {
            return linux_errno(address_space_error_to_linux_errno(err));
        }
    } else if response.payload_len != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    0
}

pub(super) fn syscall_linux_brk(addr: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_BRK);
    request.arg0 = addr;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) => {
            return bootstrap_brk(addr);
        }
        Err(_) => return 0,
    };
    if response.payload_len as usize != size_of::<u64>() {
        return 0;
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    u64::from_le_bytes(bytes)
}

pub(super) fn syscall_linux_mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MMAP);
    request.arg0 = requested_addr;
    request.arg1 = user_len;
    request.flags = flags;
    request.mask = prot as u32;
    request.dirfd = fd;
    // Pack offset into path bytes 0..8.
    request.path[0..8].copy_from_slice(&offset.to_le_bytes());
    request.path_len = 8;
    let response = match call_syscalld(request) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) => {
            return match bootstrap_mmap(requested_addr, user_len, prot, flags, fd, offset) {
                Ok(addr) => addr,
                Err(errno) => linux_errno(errno),
            };
        }
        Err(errno) => return linux_errno(errno),
    };
    if response.status != 0 {
        return linux_errno(response.status.unsigned_abs() as i64);
    }
    if response.payload_len as usize != size_of::<u64>() {
        return linux_errno(LINUX_EINVAL);
    }
    let mut bytes = [0_u8; size_of::<u64>()];
    bytes.copy_from_slice(&response.payload[..size_of::<u64>()]);
    u64::from_le_bytes(bytes)
}

pub(super) fn syscall_linux_mprotect(start: u64, user_len: u64, prot: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MPROTECT);
    request.arg0 = start;
    request.arg1 = user_len;
    request.mask = prot as u32;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) => {
            match bootstrap_mprotect(start, user_len, prot) {
                Ok(()) => 0,
                Err(errno) => linux_errno(errno),
            }
        }
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_munmap(start: u64, user_len: u64) -> u64 {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_MUNMAP);
    request.arg0 = start;
    request.arg1 = user_len;
    match call_syscalld(request).and_then(|response| ensure_empty_syscalld_response(&response)) {
        Ok(()) => 0,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) => {
            match bootstrap_munmap(start, user_len) {
                Ok(()) => 0,
                Err(errno) => linux_errno(errno),
            }
        }
        Err(errno) => linux_errno(errno),
    }
}

// RING3-MIGRATION-REFERENCE START: syscalld/pagerd should own bootstrap Linux
// mmap/brk policy. Ring0 keeps pre-syscalld page-table mutation and initial
// user address-space substrate only.
fn bootstrap_brk(addr: u64) -> u64 {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != crate::user::abi::UserAbi::Linux {
            return 0;
        }
        let (brk_start, brk_mapped_end) = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                return 0;
            };
            if addr == 0 || addr < state.brk_start {
                return state.brk_current;
            }
            let Some(requested_mapped_end) = checked_align_up(addr, PAGE_SIZE) else {
                return state.brk_current;
            };
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
            (state.brk_start, state.brk_mapped_end)
        };
        if let Some(memory_map) = process_state.linux_memory_map_mut() {
            memory_map.set_heap_range(brk_start, brk_mapped_end);
        }
        addr
    }) else {
        return 0;
    };
    result
}

fn bootstrap_mmap(
    requested_addr: u64,
    user_len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> Result<u64, i64> {
    let flags = linux_mmap_effective_flags(flags);
    if offset != 0 || !linux_mmap_fd_is_anonymous(fd) {
        return Err(LINUX_EINVAL);
    }
    if flags & linux_abi::MAP_ANONYMOUS == 0
        || linux_mmap_mapping_type(flags) != linux_abi::MAP_PRIVATE
    {
        return Err(LINUX_EINVAL);
    }
    if prot & !(linux_abi::PROT_READ | linux_abi::PROT_WRITE | linux_abi::PROT_EXEC) != 0 {
        return Err(LINUX_EINVAL);
    }
    if prot & linux_abi::PROT_WRITE != 0 && prot & linux_abi::PROT_EXEC != 0 {
        return Err(LINUX_EACCES);
    }
    let fixed = flags & linux_abi::MAP_FIXED != 0;
    if fixed && (requested_addr == 0 || requested_addr & (PAGE_SIZE - 1) != 0) {
        return Err(LINUX_EINVAL);
    }
    let len = checked_align_up(user_len, PAGE_SIZE).ok_or(LINUX_ENOMEM)?;
    if len == 0 {
        return Err(LINUX_EINVAL);
    }
    let page_count = usize::try_from(len / PAGE_SIZE).map_err(|_| LINUX_EINVAL)?;
    let page_flags = linux_mmap_page_flags(prot);

    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != crate::user::abi::UserAbi::Linux {
            return Err(LINUX_ENOSYS);
        }
        let mapped = {
            let (address_space, linux_process_state) =
                process_state.address_space_and_linux_process_state_mut();
            let Some(state) = linux_process_state.as_mut() else {
                return Err(LINUX_ENOSYS);
            };
            let start = if fixed {
                requested_addr
            } else {
                find_bootstrap_mmap_start(address_space, state, requested_addr, len)?
            };
            let end = start.checked_add(len).ok_or(LINUX_ENOMEM)?;
            if end > state.brk_limit() || end <= state.brk_mapped_end {
                return Err(LINUX_ENOMEM);
            }
            if fixed {
                // `MAP_FIXED` is an implicit unmap of the target range, so a
                // refusal here means the range is still mapped and the mapping
                // below would either fail as an overlap or replace a leaf whose
                // frame nothing released. Discarding the result turned that
                // into a silent leak.
                match address_space.unmap_user_bytes(VirtAddr::new(start), len as usize) {
                    Ok(_) | Err(paging::AddressSpaceError::NotMapped) => {}
                    Err(err) => return Err(address_space_error_to_linux_errno(err)),
                }
                let _ = state.release_reserved_range(start, end);
            } else if state.has_reserved_overlap(start, end)
                || address_space
                    .regions()
                    .iter()
                    .any(|region| region.start.as_u64() < end && start < region.end().as_u64())
            {
                return Err(LINUX_ENOMEM);
            }
            let region = address_space
                .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, page_flags)
                .map_err(address_space_error_to_linux_errno)?;
            state.mmap_next = state.mmap_next.max(region.end().as_u64());
            region.start.as_u64()
        };
        process_state.set_mapping_cursor(mapped.checked_add(len).ok_or(LINUX_ENOMEM)?);
        Ok(mapped)
    }) else {
        return Err(LINUX_ENOSYS);
    };
    result
}

fn bootstrap_mprotect(start: u64, user_len: u64, prot: u64) -> Result<(), i64> {
    if start & (PAGE_SIZE - 1) != 0 {
        return Err(LINUX_EINVAL);
    }
    let len = checked_align_up(user_len, PAGE_SIZE).ok_or(LINUX_EINVAL)?;
    if len == 0 {
        return Err(LINUX_EINVAL);
    }
    let flags = linux_mmap_page_flags(prot);
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != crate::user::abi::UserAbi::Linux {
            return Err(LINUX_ENOSYS);
        }
        process_state
            .address_space_mut()
            .protect_user_bytes(VirtAddr::new(start), len as usize, flags)
            .map_err(address_space_error_to_linux_errno)
    }) else {
        return Err(LINUX_ENOSYS);
    };
    result
}

fn bootstrap_munmap(start: u64, user_len: u64) -> Result<(), i64> {
    if start & (PAGE_SIZE - 1) != 0 {
        return Err(LINUX_EINVAL);
    }
    let len = checked_align_up(user_len, PAGE_SIZE).ok_or(LINUX_EINVAL)?;
    if len == 0 {
        return Err(LINUX_EINVAL);
    }
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != crate::user::abi::UserAbi::Linux {
            return Err(LINUX_ENOSYS);
        }
        match process_state
            .address_space_mut()
            .unmap_user_bytes(VirtAddr::new(start), len as usize)
        {
            Ok(_) | Err(paging::AddressSpaceError::NotMapped) => Ok(()),
            Err(err) => Err(address_space_error_to_linux_errno(err)),
        }
    }) else {
        return Err(LINUX_ENOSYS);
    };
    result
}

fn find_bootstrap_mmap_start(
    address_space: &paging::ProcessAddressSpace,
    state: &linux_abi::LinuxProcessState,
    requested_addr: u64,
    len: u64,
) -> Result<u64, i64> {
    let mut start = if requested_addr == 0 {
        checked_align_up(state.mmap_next, PAGE_SIZE).ok_or(LINUX_ENOMEM)?
    } else {
        checked_align_up(requested_addr, PAGE_SIZE).ok_or(LINUX_ENOMEM)?
    };
    loop {
        let end = start.checked_add(len).ok_or(LINUX_ENOMEM)?;
        if end > state.brk_limit() {
            return Err(LINUX_ENOMEM);
        }
        let overlaps_mapped = address_space
            .regions()
            .iter()
            .any(|region| region.start.as_u64() < end && start < region.end().as_u64());
        if !overlaps_mapped && !state.has_reserved_overlap(start, end) {
            return Ok(start);
        }
        start = checked_align_up(end, PAGE_SIZE).ok_or(LINUX_ENOMEM)?;
    }
}

fn linux_mmap_fd_is_anonymous(fd: u64) -> bool {
    fd == u64::MAX || fd == u32::MAX as u64
}

fn linux_mmap_effective_flags(flags: u64) -> u64 {
    flags & !(linux_abi::MAP_DENYWRITE | linux_abi::MAP_EXECUTABLE)
}

fn linux_mmap_mapping_type(flags: u64) -> u64 {
    flags & linux_abi::MAP_TYPE
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

fn checked_align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two());
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}
// RING3-MIGRATION-REFERENCE END: syscalld/pagerd-owned bootstrap memory policy.
