//! Privileged MM broker for service-admitted mapping plans.
//!
//! - **Owner:** `syscalld` owns mapping policy; Compat validates the broker
//!   envelope and `kernel-mm` owns page-table mechanism.
//! - **Boundary:** Requested ranges, flags, protections, backing identities,
//!   and service replies are untrusted until complete-plan validation.
//! - **Lifecycle:** Classify, retain backing, preflight the complete span,
//!   commit atomically, or roll back all staged holds.
//! - **Concurrency:** Exact process generation serializes map mutation; no
//!   service IPC occurs while PTE state is partially changed.
//! - **Failure:** Invalid/overflowing/W+X/overlapping plans, short remote reads,
//!   allocation, exec, and exit races leave the prior address space intact.
//! - **Forbidden:** No policy table in ring0, destructive prevalidation,
//!   guest-selected frame, or partial `mprotect`.
//! - **Evidence:** `memory-map`.
use super::*;

mod pager_admission;

use alloc::vec;
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
    RustosMmMapBrokerResult, VFS_DEVICE_ACCESS_DRM_COMPAT, VFS_IPC_PAYLOAD_CAPACITY,
};

use crate::user::handles::{KernelHandle, RemoteVfsHandleKind};
use crate::user::memfd::{MemfdError, MemfdHandle};

const PAGE_SIZE: u64 = 4096;
const FILE_COPY_CHUNK: usize = VFS_IPC_PAYLOAD_CAPACITY;

#[derive(Clone)]
enum FileMappingSource {
    Remote { remote_id: u64, path: String },
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
            .ok_or(LINUX_EINVAL)?;
        Ok::<_, i64>(RustosMmLayoutBrokerResult {
            brk_start: linux.brk_start,
            brk_current: linux.brk_current,
            brk_mapped_end: linux.brk_mapped_end,
            mmap_next: linux.mmap_next,
            user_range_start: paging::USER_SPACE_BASE,
            user_range_end: paging::USER_SPACE_END_EXCLUSIVE,
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_describe_fd(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let Some(entry) = process_state.handles().get_entry(args.fd) else {
            return Err(LINUX_EBADF);
        };
        let mut result = RustosMmFdBrokerResult {
            rights: broker_rights(entry.rights()),
            ..RustosMmFdBrokerResult::default()
        };

        match entry.handle() {
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
    let mapping_end = checked_mapping_end(start, page_count)?;
    let page_flags = protection_to_page_flags(args.prot)?;

    // Anonymous mappings are admitted to pagerd before publication. Fault
    // dispatch is kernel-originated and pagerd positively authenticates the
    // receive-side `(0, 0)` ring0 identity for FAULT_RESOLVE; user-originated
    // BACKING_OBJECT requests retain exact nonzero subject authentication.
    const PAGER_DEMAND_ADMISSION_WIRED: bool = true;
    // The pager may not be a client of the transport it is the only server
    // for. If pagerd's own anonymous memory were demand-backed, its first
    // touch would park pagerd on a fault that only pagerd can resolve, and
    // every later fault in the system would stall behind it - the classic
    // external-pager self-deadlock. Nothing else in the system can break that
    // cycle, so the exclusion is enforced here at admission rather than left
    // to pagerd happening not to allocate. Its mapping falls through to the
    // eager path below, exactly as it did before demand paging existed.
    let target_is_pager = ipc_ops::process_owns_pager_policy(args.target_pid);
    if PAGER_DEMAND_ADMISSION_WIRED && !target_is_pager && args.prot != 0 {
        let mut admission =
            pager_admission::admit_anonymous_region(args.target_pid, start, mapping_end, args.prot);
        // `MAP_FIXED` replacement.
        //
        // An overlap here is usually not stale residue, it is `mmap` being
        // asked to *replace* a range that is already mapped, which Linux
        // performs as an implicit unmap of the target range. `ld.so` does
        // exactly this for every shared library: it reserves the whole library
        // span, then maps the zero-fill BSS `MAP_FIXED` inside it. Falling
        // through to the eager path instead left the previous mapping's pages
        // in place, so `map_zeroed_user_pages_at` refused the range and the
        // loader reported `libc.so.6: cannot map zero-fill pages` - an ENOMEM
        // raised with 1.59 GiB free.
        //
        // Tear the range down and admit once more. The retry is bounded to one
        // attempt: if the range still overlaps after an explicit removal, the
        // publication really is residue and the eager fallback below is the
        // right answer.
        if matches!(
            admission,
            pager_admission::AnonymousAdmission::Eager(
                pager_admission::EagerByContract::StaleRegionOverlap
            )
        ) {
            let _ = multitask::unmap_pager_vma_for_process(args.target_pid, start, mapping_end);
            admission = pager_admission::admit_anonymous_region(
                args.target_pid,
                start,
                mapping_end,
                args.prot,
            );
        }
        match admission {
            pager_admission::AnonymousAdmission::Demand => {
                let Some(result) =
                    multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
                        process_state.set_mapping_cursor(mapping_end);
                        RustosMmMapBrokerResult {
                            addr: start,
                            len: args.len,
                        }
                    })
                else {
                    return Err(LINUX_ESRCH);
                };
                return write_out(args, &result);
            }
            // Wired is this target's contract, not a downgrade: either no
            // pager transport exists yet, or the target is a member of the
            // graph that resolves faults. Fall through to the eager path
            // below, exactly as every mapping did before demand paging.
            pager_admission::AnonymousAdmission::Eager(_) => {}
            // The pager transport is live and refused this range. Mapping it
            // eagerly and reporting success is what let a full pagerd region
            // table silently disable demand paging for the rest of a boot,
            // with the eager mapping making the downgrade invisible. Fail the
            // mapping instead; nothing here may fabricate success.
            pager_admission::AnonymousAdmission::Failed(errno) => return Err(errno),
        }
    }

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
        process_state.set_mapping_cursor(mapping_end);
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
    let mapping_end = checked_mapping_end(start, page_count(len)?)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let source = file_mapping_source(args.target_pid, args.fd)?;

    let Some(map_result) =
        multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
            process_state
                .address_space_mut()
                .map_zeroed_user_bytes_at(VirtAddr::new(start), len, page_flags)
                .map_err(address_space_error_to_linux_errno)?;
            process_state.set_mapping_cursor(mapping_end);
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
    let mapping_end = checked_mapping_end(start, page_count(len)?)?;
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
        process_state.record_shared_memfd_mapping(start, args.len, args.offset, hold);
        process_state.set_mapping_cursor(mapping_end);
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
    checked_mapping_end(start, page_count)?;

    let Some(result) = multitask::with_process_state_by_pid_mut(args.target_pid, |process_state| {
        let mut surface_fd = args.fd;
        let entry = process_state
            .handles()
            .get_entry(args.fd)
            .ok_or(LINUX_EBADF)?;
        match entry.handle() {
            KernelHandle::DisplaySurface(_) => {
                if !entry.rights().allows_shared_map() {
                    return Err(LINUX_EACCES);
                }
            }
            KernelHandle::RemoteVfs(remote)
                if remote.kind() == RemoteVfsHandleKind::Device
                    && remote.device_access() == VFS_DEVICE_ACCESS_DRM_COMPAT =>
            {
                if args.offset == 0 || !entry.rights().allows_read() {
                    return Err(LINUX_EINVAL);
                }
                surface_fd = args.offset;
            }
            KernelHandle::Device(_) if args.offset != 0 => {
                if !entry.rights().allows_shared_map() {
                    return Err(LINUX_EACCES);
                }
                surface_fd = args.offset;
            }
            _ => return Err(LINUX_ENODEV),
        }
        let surface_entry = process_state
            .handles()
            .get_entry(surface_fd)
            .ok_or(LINUX_EBADF)?;
        if !surface_entry.rights().allows_shared_map() {
            return Err(LINUX_EACCES);
        }
        let KernelHandle::DisplaySurface(mut surface) = *surface_entry.handle() else {
            return Err(LINUX_ENODEV);
        };
        if args.len != surface.mapping_len() {
            return Err(LINUX_EINVAL);
        }
        if let Some(region) = surface.mapped_region() {
            return Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
                addr: region.start.as_u64(),
                len: region.end().as_u64().saturating_sub(region.start.as_u64()),
            });
        }
        let external_mapping = surface.external_physical_mapping();
        let mut shared_region_hold = None;
        let frames = if let Some((phys_start, external_len)) = external_mapping {
            if external_len != len || !phys_start.is_multiple_of(PAGE_SIZE) {
                return Err(LINUX_EINVAL);
            }
            let mut frames = Vec::with_capacity(page_count);
            for index in 0..page_count {
                frames.push(
                    phys_start
                        .checked_add((index as u64).saturating_mul(PAGE_SIZE))
                        .ok_or(LINUX_EINVAL)?,
                );
            }
            frames
        } else {
            let shared_region = surface.shared_region().ok_or(LINUX_EINVAL)?;
            shared_region_hold =
                Some(crate::ipc::acquire_shared_region_mapping(shared_region).ok_or(LINUX_EINVAL)?);
            crate::ipc::shared_region_frames(shared_region).ok_or(LINUX_EINVAL)?
        };
        if frames.len() != page_count {
            return Err(LINUX_EINVAL);
        }
        let region = if external_mapping.is_some() {
            process_state
                .address_space_mut()
                .map_existing_user_pages_at_write_combine(VirtAddr::new(start), &frames, page_flags)
        } else {
            process_state
                .address_space_mut()
                .map_existing_user_pages_at(VirtAddr::new(start), &frames, page_flags)
        }
        .map_err(address_space_error_to_linux_errno)?;
        if let Some(hold) = shared_region_hold {
            process_state.record_shared_region_mapping(region.start.as_u64(), args.len, hold);
        }
        surface.set_mapped_region(region);
        let slot = process_state
            .handles_mut()
            .get_mut(surface_fd)
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
    let page_count = page_count(len)?;
    let end = checked_mapping_end(start, page_count)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let pager_prot = pager_admission::pager_protection(args.prot).ok_or(LINUX_EINVAL)?;

    match retry_transient_range_edit("mprotect", || {
        multitask::protect_pager_vma_for_process(args.target_pid, start, end, pager_prot, page_flags)
    }) {
        // Ring0 holds the only map of an anonymous range and has already
        // rewritten it under the exact process-state lock, publishing new VMA
        // generations for every surviving fragment. There is no second replica
        // to notify and therefore nothing that can disagree with it: the whole
        // class of "the pager kept granting the old rights" defects is gone
        // with the second copy, not merely reconciled faster.
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(multitask::PagerVmaError::Denied) => return Err(LINUX_EACCES),
        // The per-process VMA table cannot hold this edit's fragments. This is
        // a declared ring0 bound, not a memory shortage, and `ENOMEM` is the
        // closest errno `mprotect` is allowed to return for it - so name it in
        // the log, because "out of memory" with gigabytes free sends the next
        // investigation to the wrong subsystem every time.
        Err(multitask::PagerVmaError::Pressure) => {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-protect-vma-capacity",
                start,
                end,
            );
            return Err(LINUX_ENOMEM);
        }
        Err(multitask::PagerVmaError::Malformed) => return Err(LINUX_EINVAL),
        Err(_) => return Err(LINUX_EFAULT),
    }

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

/// Retries one pager range edit while it reports transient contention.
///
/// `PagerVmaError::Unstable` means a writer held the publication, or an
/// exception-time installer had not yet dropped its permit - both bounded,
/// both about *this instant*. Reporting it to userspace as a hard error is the
/// mistake this whole class keeps making: `EFAULT` names a bad address, and
/// the address is fine. `mprotect` and `munmap` run in ordinary syscall
/// context, so a retry here is free and is what the condition actually calls
/// for. Exhausting the retries is a real anomaly, so it is counted and named
/// rather than folded into an ordinary errno.
fn retry_transient_range_edit<T>(
    operation: &'static str,
    mut edit: impl FnMut() -> Result<T, multitask::PagerVmaError>,
) -> Result<T, multitask::PagerVmaError> {
    const TRANSIENT_RANGE_EDIT_ATTEMPTS: usize = 8;
    let mut last = multitask::PagerVmaError::Unstable;
    for _ in 0..TRANSIENT_RANGE_EDIT_ATTEMPTS {
        match edit() {
            Err(multitask::PagerVmaError::Unstable) => {
                last = multitask::PagerVmaError::Unstable;
                core::hint::spin_loop();
            }
            other => return other,
        }
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "pager-range-edit-contended-out",
        TRANSIENT_RANGE_EDIT_ATTEMPTS as u64,
        operation.len() as u64,
    );
    Err(last)
}

fn broker_unmap(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let end = start.checked_add(args.len).ok_or(LINUX_EINVAL)?;
    let pager_end = checked_mapping_end(start, page_count(len)?)?;
    match retry_transient_range_edit("munmap", || {
        multitask::unmap_pager_vma_for_process(args.target_pid, start, pager_end)
    }) {
        // Ring0 freed its own VMA slot and that is the entire release. There is
        // no pager region to drop, so `munmap` no longer makes a synchronous
        // round trip to a single serial pager, and no unconfirmed release can
        // be left behind to refuse a later mapping of the same range as an
        // overlap.
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(multitask::PagerVmaError::Pressure) => return Err(LINUX_ENOMEM),
        Err(multitask::PagerVmaError::Malformed) => return Err(LINUX_EINVAL),
        Err(_) => return Err(LINUX_EFAULT),
    }
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
                .shared_region_overlap_segments(start, end)
                .into_iter()
                .map(|(segment_start, segment_len)| {
                    (segment_start, segment_start.saturating_add(segment_len))
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
        process_state.release_shared_region_mappings_in_range(start, end);
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
            KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                Ok(FileMappingSource::Remote {
                    remote_id: remote.remote_id(),
                    path: remote.path(),
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
    let mut chunk = vec![0; FILE_COPY_CHUNK.min(len)];

    while copied < len {
        let count = (len - copied).min(chunk.len());
        let read = match source {
            FileMappingSource::Remote { remote_id, path } => {
                let file_offset = offset.checked_add(copied as u64).ok_or(LINUX_EOVERFLOW)?;
                match kernel_io_manager::api::block::read_bootstrap_file_range(
                    path,
                    file_offset,
                    &mut chunk[..count],
                ) {
                    Ok(Some(read)) => read,
                    Ok(None) => {
                        match offload_ops::call_remote_vfs_read_bytes(
                            *remote_id,
                            file_offset,
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
                    Err(_) => return Err(LINUX_EIO),
                }
            }
        };
        if read == 0 {
            break;
        }

        let dest = start.checked_add(copied as u64).ok_or(LINUX_EOVERFLOW)?;
        let Some(result) = multitask::with_process_state_by_pid_mut(target_pid, |process_state| {
            process_state
                .address_space()
                .initialize_user_bytes(VirtAddr::new(dest), &chunk[..read])
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
    if addr == 0 || !addr.is_multiple_of(PAGE_SIZE) {
        return Err(LINUX_EINVAL);
    }
    VirtAddr::try_new(addr).map_err(|_| LINUX_EINVAL)?;
    Ok(addr)
}

fn checked_mapping_end(start: u64, page_count: usize) -> Result<u64, i64> {
    let span = u64::try_from(page_count)
        .ok()
        .and_then(|pages| pages.checked_mul(PAGE_SIZE))
        .ok_or(LINUX_EOVERFLOW)?;
    let end = start.checked_add(span).ok_or(LINUX_EOVERFLOW)?;
    if end == 0 {
        return Err(LINUX_EOVERFLOW);
    }
    VirtAddr::try_new(end - 1).map_err(|_| LINUX_EINVAL)?;
    Ok(end)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_range_rejects_noncanonical_and_wrapping_addresses() {
        assert_eq!(checked_page_addr(0), Err(LINUX_EINVAL));
        assert_eq!(checked_page_addr(0x0000_8000_0000_0000), Err(LINUX_EINVAL));
        assert_eq!(
            checked_mapping_end(!(PAGE_SIZE - 1), 2),
            Err(LINUX_EOVERFLOW)
        );
    }

    #[test]
    fn mapping_cursor_advances_to_the_rounded_region_end() {
        let start = 0x4000_0000;
        let pages = page_count(PAGE_SIZE as usize + 1).expect("page count");
        assert_eq!(pages, 2);
        assert_eq!(checked_mapping_end(start, pages), Ok(start + 2 * PAGE_SIZE));
    }
}
