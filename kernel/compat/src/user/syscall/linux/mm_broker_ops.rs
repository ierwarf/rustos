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
use x86_64::structures::paging::PageTableFlags;
use x86_64::VirtAddr;

use rustos_user_abi::syscall::{ServiceCapability,
    RustosMmBrokerArgs, RustosMmConflictBrokerResult, RustosMmFdBrokerResult, RustosMmLayoutBrokerResult,
    RustosMmMapBrokerResult, MM_BROKER_ABI_VERSION,
    MM_BROKER_FD_KIND_DEVICE, MM_BROKER_FD_KIND_DEVICE_BUFFER, MM_BROKER_FD_KIND_DISPLAY_SURFACE, MM_BROKER_FD_KIND_FILE,
    MM_BROKER_FD_KIND_MEMFD, MM_BROKER_FD_KIND_NONE, MM_BROKER_FD_RIGHT_MAP,
    MM_BROKER_FD_RIGHT_READ, MM_BROKER_FD_RIGHT_WRITE, MM_BROKER_OP_BIND_LIBRARY, MM_BROKER_OP_DESCRIBE_FD,
    MM_BROKER_OP_MAP_ANON, MM_BROKER_OP_MAP_DEVICE_SHARED, MM_BROKER_OP_MAP_FILE_PRIVATE,
    MM_BROKER_OP_MAP_MEMFD_SHARED, MM_BROKER_OP_PROTECT, MM_BROKER_OP_QUERY_LAYOUT, MM_BROKER_OP_QUERY_CONFLICT,
    MM_BROKER_OP_UNMAP, MM_BROKER_PATH_CAPACITY, VFS_DEVICE_ACCESS_DRM_COMPAT,
    VFS_IPC_PAYLOAD_CAPACITY, RustosMmLibraryBindBrokerResult,
};

use rustos_user_abi::pager::MM_BROKER_OP_SET_ANON_POLICY;

use crate::user::handles::{KernelHandle, LibraryBinding, RemoteVfsHandleKind};
use crate::library_cache::{decide_bind, BindDecision, KernelSources, REGISTRY};
use crate::user::memfd::{MemfdError, MemfdHandle};
use super::mm_request_lease::{self, MmTarget};

const PAGE_SIZE: u64 = 4096;
const FILE_COPY_CHUNK: usize = VFS_IPC_PAYLOAD_CAPACITY;

#[derive(Clone)]
enum FileMappingSource {
    Remote { remote_id: u64, path: String },
}

pub(super) fn syscall_linux_rustos_mm_broker(args_ptr: u64) -> u64 {
    // Authority is per operation, and neither gate admits the other's
    // operations. Mapping operations act on another process's address space
    // and stay gated on syscalld's mapping-policy capability. Publishing the
    // anonymous policy is the pager stating its own arbitration rules, so it
    // is gated on owning the pager endpoint instead - syscalld may not set it
    // and the pager may not map through this broker. A caller holding neither
    // is rejected before its arguments are read, exactly as before.
    let has_mapping_policy =
        ipc_ops::current_process_has_service_capability(ServiceCapability::LinuxSyscallPolicy);
    let owns_pager_endpoint = current_process_owns_pager_endpoint();
    if !has_mapping_policy && !owns_pager_endpoint {
        return linux_errno(LINUX_EPERM);
    }

    let args = match usermem::read_current_user_struct::<RustosMmBrokerArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if args.abi_version != MM_BROKER_ABI_VERSION {
        return linux_errno(LINUX_EINVAL);
    }
    let authorized = if args.op == MM_BROKER_OP_SET_ANON_POLICY {
        owns_pager_endpoint
    } else {
        has_mapping_policy
    };
    if !authorized {
        return linux_errno(LINUX_EPERM);
    }

    // Every operation except the pager's own policy publication acts on a
    // process on behalf of one live request. The kernel, not syscalld's
    // `target_pid`, decides which MM that is: the guard holds the request's
    // lease open (the caller cannot observe the end of its call mid-effect)
    // and yields the process/MM generations captured when the caller asked.
    let result = if args.op == MM_BROKER_OP_SET_ANON_POLICY {
        broker_set_anon_policy(&args)
    } else {
        match mm_request_lease::begin_apply(args.ticket, args.target_pid) {
            Err(mm_request_lease::LeaseRefusal::NoRequest) => Err(LINUX_ESRCH),
            Err(mm_request_lease::LeaseRefusal::WrongProcess) => Err(LINUX_EPERM),
            Ok(apply) => {
                let t = apply.target();
                match args.op {
                    MM_BROKER_OP_QUERY_LAYOUT => broker_query_layout(&args, t),
                    MM_BROKER_OP_QUERY_CONFLICT => broker_query_conflict(&args, t),
                    MM_BROKER_OP_DESCRIBE_FD => broker_describe_fd(&args, t),
                    MM_BROKER_OP_BIND_LIBRARY => broker_bind_library(&args, t),
                    MM_BROKER_OP_MAP_ANON => broker_map_anon(&args, t),
                    MM_BROKER_OP_MAP_FILE_PRIVATE => broker_map_file_private(&args, t),
                    MM_BROKER_OP_MAP_MEMFD_SHARED => broker_map_memfd_shared(&args, t),
                    MM_BROKER_OP_MAP_DEVICE_SHARED => broker_map_device_shared(&args, t),
                    MM_BROKER_OP_PROTECT => broker_protect(&args, t),
                    MM_BROKER_OP_UNMAP => broker_unmap(&args, t),
                    _ => Err(LINUX_EINVAL),
                }
            }
        }
    };

    match result {
        Ok(()) => 0,
        Err(errno) => {
            record_mm_broker_refusal(&args, errno);
            linux_errno(errno)
        }
    }
}

/// Names a refused mapping operation. A failed `mmap` otherwise reaches the
/// guest as a bare errno from a service call; this keeps the target, the
/// operation and the errno together. Bounded so a refusal storm stays cheap.
fn record_mm_broker_refusal(args: &RustosMmBrokerArgs, errno: i64) {
    static RECORDED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // ORDERING: Relaxed; a counter bounding telemetry volume.
    if RECORDED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= 64 {
        return;
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "mm-broker-refused",
        (u64::from(args.op) << 32) | (args.target_pid & 0xffff_ffff),
        errno.unsigned_abs(),
    );
}

fn broker_query_conflict(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let end = args.addr.checked_add(args.len).ok_or(LINUX_EINVAL)?;
    let conflict = multitask::first_user_mapping_conflict(t.pid, Some(t.expected()), args.addr, end)
        .map_err(|error| match error {
            multitask::PagerVmaError::Malformed => LINUX_EINVAL,
            multitask::PagerVmaError::Pressure => LINUX_ENOMEM,
            multitask::PagerVmaError::Stale => LINUX_ESRCH,
            _ => LINUX_EAGAIN,
        })?;
    let result = conflict.map_or(RustosMmConflictBrokerResult::default(), |(start, end)|
        RustosMmConflictBrokerResult { start, end });
    write_out(args, &result)
}

fn broker_query_layout(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
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
            user_range_end: linux.brk_limit(),
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    write_out(args, &result?)
}

fn broker_describe_fd(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
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
            KernelHandle::DeviceBuffer(buffer) => {
                result.kind = MM_BROKER_FD_KIND_DEVICE_BUFFER;
                result.len = buffer.len();
                copy_path(&mut result, "device-buffer");
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

/// Whether the calling process owns the published pager service endpoint.
fn current_process_owns_pager_endpoint() -> bool {
    multitask::current_user_process_id().is_some_and(|process_id| {
        ipc_ops::process_owns_published_service_endpoint(
            process_id,
            rustos_user_abi::syscall::IPC_SERVICE_PAGERD,
        )
    })
}

/// Records the pager's anonymous-mapping policy.
///
/// Ring0 keeps the enforcement and applies its own default until this lands,
/// so a pager that never publishes changes nothing. Publication uses an
/// odd/even coherent snapshot. Normal readers retry; the only IRQ-off reader
/// tries twice and falls back to the compiled-in fault-around length.
fn broker_set_anon_policy(args: &RustosMmBrokerArgs) -> Result<(), i64> {
    if args.addr == 0 {
        return Err(LINUX_EINVAL);
    }
    let policy = usermem::read_current_user_struct::<
        rustos_user_abi::pager::PagerAnonymousPolicyWire,
    >(args.addr)
    .map_err(address_space_error_to_linux_errno)?;
    if !crate::pager_policy::publish_anonymous_policy(policy) {
        // Malformed, or a policy is already in force. Both are refusals, not
        // partial applications: the policy that was in force still is.
        return Err(LINUX_EINVAL);
    }
    Ok(())
}

/// Publishes every VMA inherited by a suspended fork child.
///
/// What a child inherits is the complete reservation set, including untouched,
/// reserved, deny-all, file-backed, and image-section ranges. Resident leaves
/// were already cloned by `clone_user_space_cow`, so this fork-only publication
/// deliberately does not require an empty target page table. Any partial
/// publication is revoked before the caller destroys the still-suspended child.
pub(super) fn inherit_pager_vmas(
    child_pid: u64,
    parent_regions: &[rustos_user_abi::pager::PagerVmRegionWire],
) -> Result<(), i64> {
    let mut published: Vec<(u64, u64)> = Vec::new();
    published
        .try_reserve_exact(parent_regions.len())
        .map_err(|_| LINUX_ENOMEM)?;
    for region in parent_regions {
        match pager_admission::admit_inherited_region(child_pid, *region) {
            Ok(child_region) => published.push((child_region.start, child_region.vma_generation)),
            Err(errno) => {
                revoke_inherited_pager_vmas(child_pid, &published);
                return Err(errno);
            }
        }
    }
    Ok(())
}

/// Withdraws exactly the regions this fork published, newest first.
fn revoke_inherited_pager_vmas(child_pid: u64, published: &[(u64, u64)]) {
    for &(start, vma_generation) in published.iter().rev() {
        let _ = multitask::revoke_pager_vma_for_process(child_pid, start, vma_generation);
    }
}

fn broker_map_anon(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_count = page_count(len)?;
    let mapping_end = checked_mapping_end(start, page_count)?;
    let page_flags = protection_to_page_flags(args.prot)?;

    // Every post-bootstrap anonymous mapping is first represented in the
    // process VMA authority. Policy decides only whether committed pages are
    // populated eagerly or on first touch.
    let mut admission =
        pager_admission::admit_anonymous_region(t, start, mapping_end, args.prot);
    if matches!(
        admission,
        pager_admission::AnonymousAdmission::Eager(
            pager_admission::EagerByContract::StaleRegionOverlap
        )
    ) {
        // MAP_FIXED replacement withdraws the old reservation and its present
        // leaves, then makes exactly one fresh admission attempt.
        let _ = multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
        admission =
            pager_admission::admit_anonymous_region(t, start, mapping_end, args.prot);
        if matches!(
            admission,
            pager_admission::AnonymousAdmission::Eager(
                pager_admission::EagerByContract::StaleRegionOverlap
            )
        ) {
            return Err(LINUX_ENOMEM);
        }
    }

    match admission {
        pager_admission::AnonymousAdmission::Demand => {
            let Some(result) =
                multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
                    if !process_state.mapping_below_stack_guard(start, mapping_end) {
                        return Err(LINUX_ENOMEM);
                    }
                    process_state.set_mapping_cursor(mapping_end);
                    Ok::<_, i64>(RustosMmMapBrokerResult {
                        addr: start,
                        len: args.len,
                    })
                })
            else {
                let _ = multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
                return Err(LINUX_ESRCH);
            };
            let result = match result {
                Ok(result) => result,
                Err(errno) => {
                    let _ =
                        multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
                    return Err(errno);
                }
            };
            return publish_map_result(args, t, &result);
        }
        pager_admission::AnonymousAdmission::Eager(_) => {}
        pager_admission::AnonymousAdmission::Failed(errno) => return Err(errno),
    }

    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
        if !process_state.mapping_below_stack_guard(start, mapping_end) {
            return Err(LINUX_ENOMEM);
        }
        if args.prot != 0 {
            process_state
                .address_space_mut()
                .map_zeroed_user_pages_at(VirtAddr::new(start), page_count, page_flags)
                .map_err(address_space_error_to_linux_errno)?;
        }
        process_state.set_mapping_cursor(mapping_end);
        Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
            addr: start,
            len: args.len,
        })
    }) else {
        let _ = multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
        return Err(LINUX_ESRCH);
    };
    let mapped = match result {
        Ok(mapped) => mapped,
        Err(errno) => {
            let _ = multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
            return Err(errno);
        }
    };
    publish_map_result(args, t, &mapped)
}

fn broker_map_file_private(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let mapping_end = checked_mapping_end(start, page_count(len)?)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let source = file_mapping_source(t, args.fd)?;

    // A description bound to a sealed shared library object is served by
    // aliasing the object's immutable frames copy-on-write. Anything that
    // cannot be served completely falls through to the eager copy below.
    if let Some(result) = map_library_alias(args, t, start, len, mapping_end)? {
        return publish_map_result(args, t, &result);
    }

    let Some(map_result) =
        multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
            if !process_state.mapping_below_stack_guard(start, mapping_end) {
                return Err(LINUX_ENOMEM);
            }
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

    if let Err(errno) = copy_file_mapping(t, start, len, args.offset, &source) {
        let _ = broker_unmap(args, t);
        return Err(errno);
    }
    publish_map_result(args, t, &map_result)
}

/// The registry object a file descriptor is currently bound to, if the
/// descriptor is an immutable-mount file that vfsd attested equal to a sealed
/// object. Verification happens here, under no lock but the registry's own and
/// without any IPC; the binding itself was created by `broker_bind_library`.
fn bound_library_object(t: MmTarget, fd: u64) -> Option<(u64, u64)> {
    let (object, len) = multitask::with_process_state_by_generation_mut(
        t.pid,
        t.process_generation,
        t.mm_generation,
        |process_state| {
            let entry = process_state.handles().get_entry(fd)?;
            if !entry.rights().allows_read() {
                return None;
            }
            match entry.handle() {
                KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                    match remote.library_binding() {
                        LibraryBinding::Bound { object } => Some((object, remote.len())),
                        _ => None,
                    }
                }
                _ => None,
            }
        },
    )??;
    let view = REGISTRY.lock().view(object).ok()?;
    // The guest-visible length is vfsd's; a length that no longer agrees means
    // this description is not the object's file.
    (view.len == len).then_some((object, len))
}

/// Binds a descriptor's open description to a sealed library object.
///
/// Only syscalld's mapping-policy capability reaches this, and the identity
/// comes from vfsd, asked about the descriptor's own `remote_id`; neither the
/// guest nor syscalld names an object. The vfsd round trip happens with no
/// process-state lock held, so the handle is re-checked (same handle token,
/// same remote id) before the binding is recorded.
fn broker_bind_library(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    struct Snapshot {
        token: u64,
        remote_id: u64,
        len: u64,
        binding: LibraryBinding,
    }
    let Some(snapshot) = multitask::with_process_state_by_generation_mut(
        t.pid,
        t.process_generation,
        t.mm_generation,
        |process_state| {
            let entry = process_state.handles().get_entry(args.fd).ok_or(LINUX_EBADF)?;
            if !entry.rights().allows_read() {
                return Err(LINUX_EACCES);
            }
            match entry.handle() {
                KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                    Ok(Snapshot {
                        token: remote.token_id(),
                        remote_id: remote.remote_id(),
                        len: remote.len(),
                        binding: remote.library_binding(),
                    })
                }
                _ => Err(LINUX_EINVAL),
            }
        },
    ) else {
        return Err(LINUX_ESRCH);
    };
    let snapshot = snapshot?;
    let not_bound = RustosMmLibraryBindBrokerResult::default();

    // Already attested: revalidate the generation and answer without IPC.
    match snapshot.binding {
        LibraryBinding::Bound { object } => {
            if let Ok(view) = REGISTRY.lock().view(object) {
                if view.len == snapshot.len {
                    return write_out(
                        args,
                        &RustosMmLibraryBindBrokerResult { bound: 1, object, len: view.len },
                    );
                }
            }
        }
        LibraryBinding::Ineligible { registry_epoch } => {
            if registry_epoch == REGISTRY.lock().epoch() {
                return write_out(args, &not_bound);
            }
        }
        LibraryBinding::Unknown => {}
    }
    // Nothing sealed means nothing can match; do not spend a vfsd round trip.
    if !REGISTRY.lock().has_sealed_objects() {
        crate::library_cache::note_decline(crate::library_cache::Decline::NothingSealed, 0);
        return write_out(args, &not_bound);
    }

    // vfsd attestation failing (timeout, ENOENT, a non-volume file) is "not
    // eligible", never an error for the mmap that follows.
    let decision = match offload_ops::call_remote_vfs_file_identity(snapshot.remote_id, t.pid) {
        Ok(file) => {
            let decision = decide_bind(&REGISTRY.lock(), &file, snapshot.len);
            if matches!(decision, BindDecision::NotEligible { .. }) {
                // Name the attested identity (volume, object, length) once.
                nucleus_core::debug::record_milestone(
                    nucleus_core::debug::LogCategory::Compat,
                    "library-bind-mismatch",
                    file.object,
                    file.len ^ (snapshot.len << 32),
                );
            }
            decision
        }
        Err(errno) => {
            crate::library_cache::note_decline(
                crate::library_cache::Decline::AttestFailed,
                errno as u64,
            );
            return write_out(args, &not_bound);
        }
    };
    if matches!(decision, BindDecision::NotEligible { .. }) {
        crate::library_cache::note_decline(
            crate::library_cache::Decline::NoMatchingObject,
            snapshot.len,
        );
    }
    let binding = match decision {
        BindDecision::Bound(object) => LibraryBinding::Bound { object },
        BindDecision::NotEligible { registry_epoch } => {
            LibraryBinding::Ineligible { registry_epoch }
        }
    };
    let Some(recorded) = multitask::with_process_state_by_generation_mut(
        t.pid,
        t.process_generation,
        t.mm_generation,
        |process_state| {
            let slot = process_state.handles_mut().get_mut(args.fd).ok_or(LINUX_EBADF)?;
            match slot {
                KernelHandle::RemoteVfs(remote)
                    if remote.token_id() == snapshot.token
                        && remote.remote_id() == snapshot.remote_id =>
                {
                    remote.set_library_binding(binding);
                    Ok(())
                }
                // The descriptor was closed or replaced while vfsd answered.
                _ => Err(LINUX_EBADF),
            }
        },
    ) else {
        return Err(LINUX_ESRCH);
    };
    recorded?;
    match decision {
        BindDecision::Bound(object) => write_out(
            args,
            &RustosMmLibraryBindBrokerResult { bound: 1, object, len: snapshot.len },
        ),
        BindDecision::NotEligible { .. } => write_out(args, &not_bound),
    }
}

fn library_error_detail(error: crate::library_cache::LibraryError) -> u64 {
    use crate::library_cache::LibraryError;
    match error {
        LibraryError::Invalid => 1,
        LibraryError::Exists => 2,
        LibraryError::NoSlots => 3,
        LibraryError::Budget => 4,
        LibraryError::Stale => 5,
        LibraryError::Authority => 6,
        LibraryError::Incomplete => 7,
        LibraryError::Sealed => 8,
        LibraryError::Source(_) => 9,
    }
}

/// Maps a bound description by aliasing the sealed object's immutable frames.
///
/// `Ok(None)` means "not served here" and nothing was changed: the caller then
/// performs the ordinary eager copy. `Ok(Some(_))` is a complete mapping with
/// its VMA published. All pages are pinned before the first PTE is written, the
/// VMA is published before any leaf exists, and a failure after that point
/// withdraws both, so no partial range survives.
fn map_library_alias(
    args: &RustosMmBrokerArgs,
    t: MmTarget,
    start: u64,
    len: usize,
    mapping_end: u64,
) -> Result<Option<RustosMmMapBrokerResult>, i64> {
    // Only read-capable, page-aligned private mappings are served; PROT_NONE
    // reservations and unaligned offsets use the existing path.
    if args.prot & linux_abi::PROT_READ == 0 || !args.offset.is_multiple_of(PAGE_SIZE) {
        return Ok(None);
    }
    let Some((object, file_len)) = bound_library_object(t, args.fd) else {
        crate::library_cache::note_decline(crate::library_cache::Decline::Unbound, 0);
        return Ok(None);
    };
    let pages = page_count(len)? as u64;
    let first_page = args.offset / PAGE_SIZE;
    // The mapping must lie inside the file's pages. A mapping that reaches
    // past EOF's page has bytes the object does not hold, so it is not served.
    let Some(end_byte) = first_page.checked_add(pages).and_then(|p| p.checked_mul(PAGE_SIZE)) else {
        return Ok(None);
    };
    if end_byte > file_len.next_multiple_of(PAGE_SIZE) {
        crate::library_cache::note_decline(crate::library_cache::Decline::OutOfRange, end_byte);
        return Ok(None);
    }
    let page_flags = protection_to_page_flags(args.prot)?;
    let (view, pins) = {
        let registry = REGISTRY.lock();
        let Ok(view) = registry.view(object) else {
            return Ok(None);
        };
        let pins = match registry.pin_range(&KernelSources, object, first_page, pages) {
            Ok(pins) => pins,
            Err(error) => {
                drop(registry);
                crate::library_cache::note_decline(
                    crate::library_cache::Decline::PinFailed,
                    library_error_detail(error),
                );
                return Ok(None);
            }
        };
        (view, pins)
    };

    match pager_admission::admit_library_region(t, start, mapping_end, args.prot, &view, args.offset) {
        Ok(()) => {}
        Err(pager_admission::LibraryAdmission::Unavailable) => {
            crate::library_cache::note_decline(
                crate::library_cache::Decline::AdmissionUnavailable,
                0,
            );
            return Ok(None);
        }
        Err(pager_admission::LibraryAdmission::Refused(errno)) => return Err(errno),
    }

    let Some(mapped) = multitask::with_process_state_by_generation_mut(
        t.pid,
        t.process_generation,
        t.mm_generation,
        |process_state| {
            if !process_state.mapping_below_stack_guard(start, mapping_end) {
                return Err(LINUX_ENOMEM);
            }
            for (index, pin) in pins.iter().enumerate() {
                process_state
                    .address_space_mut()
                    .map_cached_private_file_cow_at(
                        VirtAddr::new(start + index as u64 * PAGE_SIZE),
                        page_flags,
                        pin,
                    )
                    .map_err(address_space_error_to_linux_errno)?;
            }
            process_state.set_mapping_cursor(mapping_end);
            Ok(RustosMmMapBrokerResult { addr: start, len: args.len })
        },
    ) else {
        let _ = multitask::unmap_pager_vma_for_process(t.pid, Some(t.expected()), start, mapping_end);
        return Err(LINUX_ESRCH);
    };
    match mapped {
        Ok(result) => {
            crate::library_cache::note_alias_map(pages);
            Ok(Some(result))
        }
        Err(errno) => {
            // Withdraws the VMA and any leaf installed before the failure.
            let _ = broker_unmap(args, t);
            Err(errno)
        }
    }
}

fn broker_map_memfd_shared(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let mapping_end = checked_mapping_end(start, page_count(len)?)?;
    let memfd = memfd_source(t, args.fd)?;
    let writable = args.prot & linux_abi::PROT_WRITE != 0;
    let (frames, hold) = memfd
        .acquire_mapping(
            usize::try_from(args.offset).map_err(|_| LINUX_EINVAL)?,
            len,
            writable,
        )
        .map_err(memfd_error_to_errno)?;
    let page_flags = protection_to_page_flags(args.prot)?;

    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
        if !process_state.mapping_below_stack_guard(start, mapping_end) {
            return Err(LINUX_ENOMEM);
        }
        // The Job's mapping-record budget refuses before any page is mapped.
        if !process_state.admit_mapping_records(1) {
            return Err(LINUX_ENOMEM);
        }
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
    publish_map_result(args, t, &result?)
}

/// Maps an authenticated app DMA-BUF capability. `args.offset` is the byte
/// offset inside the buffer; the physical address derives only from the
/// kernel-held allocation record and the verified aperture, never from a
/// descriptor or caller-supplied address.
fn broker_map_device_buffer(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_flags = surface_page_flags(args.prot)?;
    let write = page_flags.contains(PageTableFlags::WRITABLE);
    let page_count = page_count(len)?;
    let mapping_end = checked_mapping_end(start, page_count)?;
    let map_len = (page_count as u64).checked_mul(PAGE_SIZE).ok_or(LINUX_EINVAL)?;
    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
        if !process_state.mapping_below_stack_guard(start, mapping_end) {
            return Err(LINUX_ENOMEM);
        }
        let entry = process_state.handles().get_entry(args.fd).ok_or(LINUX_EBADF)?;
        if !entry.rights().allows_shared_map() || (write && !entry.rights().allows_write()) {
            return Err(LINUX_EACCES);
        }
        let KernelHandle::DeviceBuffer(capability) = entry.handle() else {
            return Err(LINUX_ENODEV);
        };
        let hold = capability
            .admit_mapping(args.offset, map_len, write)
            .map_err(|error| match error {
                kernel_ps::api::handles::BufferError::Permission => LINUX_EACCES,
                kernel_ps::api::handles::BufferError::Stale => LINUX_ESTALE,
                kernel_ps::api::handles::BufferError::Exhausted => LINUX_ENOMEM,
                _ => LINUX_EINVAL,
            })?;
        let aperture_offset = capability
            .aperture_offset()
            .checked_add(args.offset)
            .ok_or(LINUX_EINVAL)?;
        let phys = kernel_io_manager::api::app_buffer::aperture_phys(aperture_offset, map_len)
            .ok_or(LINUX_ENODEV)?;
        let mut frames = Vec::new();
        frames.try_reserve_exact(page_count).map_err(|_| LINUX_ENOMEM)?;
        for index in 0..page_count {
            frames.push(phys + (index as u64) * PAGE_SIZE);
        }
        if !process_state.admit_mapping_records(1) {
            return Err(LINUX_ENOMEM);
        }
        // Any failure below drops `hold`, which is correct: no PTE was left.
        let region = process_state
            .address_space_mut()
            .map_existing_user_pages_at(VirtAddr::new(start), &frames, page_flags)
            .map_err(address_space_error_to_linux_errno)?;
        process_state.record_device_buffer_mapping(region.start.as_u64(), map_len, hold);
        process_state.set_mapping_cursor(region.end().as_u64());
        Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
            addr: region.start.as_u64(),
            len: args.len,
        })
    }) else {
        return Err(LINUX_ESRCH);
    };
    publish_map_result(args, t, &result?)
}

fn broker_map_device_shared(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let is_device_buffer = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |state| {
        matches!(
            state.handles().get_entry(args.fd).map(|entry| entry.handle()),
            Some(KernelHandle::DeviceBuffer(_))
        )
    })
    .unwrap_or(false);
    if is_device_buffer {
        return broker_map_device_buffer(args, t);
    }
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_flags = surface_page_flags(args.prot)?;
    let page_count = page_count(len)?;
    let mapping_end = checked_mapping_end(start, page_count)?;

    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
        if !process_state.mapping_below_stack_guard(start, mapping_end) {
            return Err(LINUX_ENOMEM);
        }
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
            // Already mapped by an earlier call: reporting it again publishes
            // nothing new, so a failed result write must not tear it down.
            return Ok::<(RustosMmMapBrokerResult, bool), i64>((
                RustosMmMapBrokerResult {
                    addr: region.start.as_u64(),
                    len: region.end().as_u64().saturating_sub(region.start.as_u64()),
                },
                false,
            ));
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
        if !process_state.admit_mapping_records(1) {
            return Err(LINUX_ENOMEM);
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
        if external_mapping.is_some() {
            // The surface handle's own record dies with the fd (close, dup2,
            // exec-cloexec) while the PTEs stay; this per-process record keeps
            // them recognizable as external backing so munmap can retire them.
            process_state.record_external_physical_mapping(region.start.as_u64(), args.len);
        }
        surface.set_mapped_region(region);
        let slot = process_state
            .handles_mut()
            .get_mut(surface_fd)
            .ok_or(LINUX_EBADF)?;
        *slot = KernelHandle::DisplaySurface(surface);
        process_state.set_mapping_cursor(region.end().as_u64());
        Ok::<(RustosMmMapBrokerResult, bool), i64>((
            RustosMmMapBrokerResult {
                addr: region.start.as_u64(),
                len: args.len,
            },
            true,
        ))
    }) else {
        return Err(LINUX_ESRCH);
    };
    let (result, fresh) = result?;
    if fresh {
        publish_map_result(args, t, &result)
    } else {
        write_out(args, &result)
    }
}

fn broker_protect(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let page_count = page_count(len)?;
    let end = checked_mapping_end(start, page_count)?;
    let page_flags = protection_to_page_flags(args.prot)?;
    let pager_prot = pager_admission::pager_protection(args.prot).ok_or(LINUX_EINVAL)?;

    // A shared memfd mapping admitted read-only was never counted as writable
    // nor checked against F_SEAL_WRITE; granting write later would bypass both.
    // Refuse before any pager-VMA rewrite so a mixed range changes nothing.
    if page_flags.contains(PageTableFlags::WRITABLE) {
        let Some(forbidden) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |state| {
            state.shared_memfd_range_forbids_write_upgrade(start, end)
        }) else {
            return Err(LINUX_ESRCH);
        };
        if forbidden {
            return Err(LINUX_EACCES);
        }
    }

    match retry_transient_range_edit("mprotect", || {
        multitask::protect_pager_vma_for_process(
            t.pid,
            Some(t.expected()),
            start,
            end,
            pager_prot,
            page_flags,
        )
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
        Err(refusal) => {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-vma-unmap-refused",
                args.target_pid,
                refusal as u64,
            );
            return Err(LINUX_EFAULT);
        }
    }

    let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
        // Re-check under the lock that performs the PTE rewrite.
        if page_flags.contains(PageTableFlags::WRITABLE)
            && process_state.shared_memfd_range_forbids_write_upgrade(start, end)
        {
            return Err(LINUX_EACCES);
        }
        if !process_state.admit_immutable_mapping_edit(start, end) {
            return Err(LINUX_ENOMEM);
        }
        let result = process_state
            .address_space_mut()
            .protect_user_bytes(VirtAddr::new(start), len, page_flags)
            .map_err(address_space_error_to_linux_errno);
        process_state.sync_mapping_kmem();
        result
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

fn broker_unmap(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let end = checked_mapping_end(start, page_count(len)?)?;
    let mut mutation_errno = None;
    let result = retry_transient_range_edit("munmap", || {
        multitask::unmap_pager_vma_for_process_with(
            t.pid,
            Some(t.expected()),
            start,
            end,
            |state| {
                unmap_process_range(state, start, end).map_err(|errno| {
                    mutation_errno = Some(errno);
                    if errno == LINUX_ENOMEM {
                        multitask::PagerVmaError::Pressure
                    } else {
                        multitask::PagerVmaError::Stale
                    }
                })
            },
            |state| {
                state.release_shared_memfd_mappings_in_range(start, end);
                state.release_shared_region_mappings_in_range(start, end);
                state.release_device_buffer_mappings_in_range(start, end);
                state.release_external_physical_mappings_in_range(start, end);
                // Drop a surface's mapping record only when this munmap
                // covers its whole region. A partial unmap leaves PTEs of the
                // rest present; dropping the record then would hide them from
                // `unmap_process_range`'s external-segment list, and every later
                // munmap of the remainder is refused as a foreign leaf.
                let covered: Vec<(u64, u64)> = state
                    .handles()
                    .surface_overlap_segments(0, u64::MAX)
                    .into_iter()
                    .filter(|&(seg_start, seg_len)| {
                        seg_start >= start && seg_start.saturating_add(seg_len) <= end
                    })
                    .collect();
                for (seg_start, seg_len) in covered {
                    state
                        .handles_mut()
                        .clear_surface_mappings_in_range(seg_start, seg_len);
                }
            },
        )
    });
    if let Some(errno) = mutation_errno {
        nucleus_core::debug::record_milestone(
            nucleus_core::debug::LogCategory::Compat,
            "mm-unmap-leaf-refused",
            start,
            errno as u64,
        );
        return Err(errno);
    }
    match result {
        Ok(_) => Ok(()),
        Err(multitask::PagerVmaError::Pressure) => Err(LINUX_ENOMEM),
        Err(multitask::PagerVmaError::Malformed) => Err(LINUX_EINVAL),
        Err(refusal) => {
            nucleus_core::debug::record_milestone(
                nucleus_core::debug::LogCategory::Compat,
                "pager-vma-unmap-refused",
                args.target_pid,
                refusal as u64,
            );
            Err(LINUX_EFAULT)
        }
    }
}

fn unmap_process_range(
    process_state: &mut multitask::UserProcessState,
    start: u64,
    end: u64,
) -> Result<(), i64> {
    let pages = page_count(usize::try_from(end - start).map_err(|_| LINUX_EINVAL)?)?;
    if !process_state.admit_immutable_mapping_edit(start, end) {
        return Err(LINUX_ENOMEM);
    }
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
    external_segments.extend(process_state.external_physical_overlap_segments(start, end));
    external_segments.extend(process_state.device_buffer_overlap_segments(start, end));
    external_segments.extend(
        process_state
            .handles()
            .surface_overlap_segments(start, end)
            .into_iter()
            .map(|(segment_start, segment_len)| {
                (segment_start, segment_start.saturating_add(segment_len))
            }),
    );
    // One preflight/commit over the ENTIRE range: per-segment mutation
    // could remove a pager prefix, then fail allocation/ownership checks
    // on a later borrowed segment and restore misleading VMA authority.
    let result = process_state
        .address_space_mut()
        .unmap_present_user_pages_with_external_at(VirtAddr::new(start), pages, &external_segments)
        .map(|_| ())
        .map_err(address_space_error_to_linux_errno);
    process_state.sync_mapping_kmem();
    result
}

fn file_mapping_source(t: MmTarget, fd: u64) -> Result<FileMappingSource, i64> {
    let Some(source) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
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

fn memfd_source(t: MmTarget, fd: u64) -> Result<MemfdHandle, i64> {
    let Some(source) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
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
    t: MmTarget,
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
                            t.pid,
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
        let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {
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

/// Reports a freshly published mapping to the broker caller.
///
/// The mapping is already live in the target when the result is written, and
/// the caller only learns of it through that result. A failed write (bad or
/// short out buffer, unmapped page) would therefore leave a mapping nobody
/// knows about, so it is retired through the ordinary unmap path before the
/// error is returned.
fn publish_map_result(
    args: &RustosMmBrokerArgs,
    t: MmTarget,
    result: &RustosMmMapBrokerResult,
) -> Result<(), i64> {
    let Err(errno) = write_out(args, result) else {
        return Ok(());
    };
    let undo = RustosMmBrokerArgs {
        addr: result.addr,
        len: args.len,
        ..*args
    };
    // `broker_unmap` names its own refusal; the write error is the one the
    // caller must see.
    let _ = broker_unmap(&undo, t);
    Err(errno)
}

fn write_out<T: rustos_user_abi::wire::UserWire>(args: &RustosMmBrokerArgs, value: &T) -> Result<(), i64> {
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
    if prot == 0 {
        flags |= paging::USER_INACCESSIBLE_LEAF;
    }
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

    /// The broker may act only on the incarnation its request lease captured.
    /// A by-PID primitive would let a late or replayed syscalld call land in
    /// whatever image `exec` installed under the same PID.
    #[test]
    fn mapping_effects_use_only_lease_bound_generation_exact_primitives() {
        let source = include_str!("mm_broker_ops.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "with_process_state_by_pid_mut",
            "unmap_pager_vma_for_process(args.target_pid",
            "protect_pager_vma_for_process(\n            args.target_pid",
            "first_user_mapping_conflict(args.target_pid",
        ] {
            assert!(
                !production.contains(forbidden),
                "by-PID mutation primitive reintroduced: {forbidden}"
            );
        }
        let dispatch = production
            .split("let result = if args.op == MM_BROKER_OP_SET_ANON_POLICY")
            .nth(1)
            .unwrap()
            .split("match result {")
            .next()
            .unwrap();
        assert!(dispatch.contains("mm_request_lease::begin_apply(args.ticket, args.target_pid)"));
        // Every non-policy operation sits behind the lease guard.
        for op in [
            "MM_BROKER_OP_QUERY_LAYOUT",
            "MM_BROKER_OP_QUERY_CONFLICT",
            "MM_BROKER_OP_DESCRIBE_FD",
            "MM_BROKER_OP_MAP_ANON",
            "MM_BROKER_OP_MAP_FILE_PRIVATE",
            "MM_BROKER_OP_MAP_MEMFD_SHARED",
            "MM_BROKER_OP_MAP_DEVICE_SHARED",
            "MM_BROKER_OP_PROTECT",
            "MM_BROKER_OP_UNMAP",
        ] {
            let at = dispatch.find(op).unwrap_or_else(|| panic!("{op} not dispatched"));
            assert!(
                at > dispatch.find("begin_apply").unwrap(),
                "{op} must be dispatched only after the lease is validated"
            );
        }
    }

    #[test]
    fn immutable_edit_reservations_are_reconciled_on_success_and_failure() {
        let source = include_str!("mm_broker_ops.rs");
        let protect = source.split_once("let Some(result) = multitask::with_process_state_by_generation_mut(t.pid, t.process_generation, t.mm_generation, |process_state| {").unwrap().1;
        let protect = protect.split_once("    result\n}").unwrap().0;
        let protect_result = protect.find("let result = process_state").unwrap();
        let protect_sync = protect.find("process_state.sync_mapping_kmem();").unwrap();
        assert!(protect_result < protect_sync);
        assert!(!protect[protect_result..protect_sync].contains("?;"));
        let unmap = source.split_once("fn unmap_process_range(").unwrap().1;
        let unmap = unmap.split_once("fn file_mapping_source(").unwrap().0;
        let count = unmap.find("let pages = page_count").unwrap();
        let reserve = unmap.find("admit_immutable_mapping_edit").unwrap();
        let result = unmap.find("let result = process_state").unwrap();
        let sync = unmap.find("process_state.sync_mapping_kmem();").unwrap();
        assert!(count < reserve && reserve < result && result < sync);
        assert!(!unmap[result..sync].contains("?;"));
    }

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
