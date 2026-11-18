//! Linux memory-introspection syscalls: `mincore`, `mlock*`, `msync`,
//! `membarrier` and `mremap`.
//!
//! - **Owner:** `kernel-compat` owns the argument envelope and the read-only
//!   queries. `syscalld` owns mapping policy: it picks every `mremap` target
//!   and edits its `VmaMap`, and ring0's `MM_BROKER_OP_MOVE` (see
//!   `mm_broker_ops`) is the only thing that relocates page-table leaves. The
//!   per-process lock ledger (`kernel-ps` `MemlockLedger`) is owned by the
//!   process state and mutated here under its lock.
//! - **Boundary:** Addresses, lengths and flags arrive from an untrusted guest
//!   and are validated against Linux's order of checks before any query.
//! - **Mapped-ness:** a page is mapped when a pager VMA covers it (ring0's
//!   published region table, which also covers never-touched reservations) or
//!   when it is resident in the page tables (ELF image, heap and stack leaves
//!   that never had a VMA). Residency is read from the live leaves, never
//!   inferred.
//! - **Lifecycle:** query the current address space only, including deferred VMAs.
//! - **Evidence:** unit tests cover range tiling and syscall argument admission.
//! - **Concurrency:** the region snapshot takes the pager-VMA writer lock and no
//!   process-state lock; the leaf scans take the process-state read section for
//!   one bounded chunk and never call out of it. Nothing here nests the two.
//! - **Failure:** a refused sub-request returns its own errno; a refused move
//!   changes neither address range (the broker rolls back, `syscalld` retires
//!   the extension it mapped for the move).
//! - **mlock:** this kernel never evicts, swaps or reclaims a resident user
//!   page (memory pressure only ever kills a whole process through the OOM
//!   watchdog), so a lock is a truthful promise for every mapping kind and is
//!   recorded, not enforced by pinning. Pages are not pre-faulted: a locked page
//!   becomes resident at its first touch and stays resident. `MCL_ONFAULT` and
//!   `MLOCK_ONFAULT` therefore behave exactly as the default and are accepted.
//! - **Forbidden:** no guest-selected frame, no W+X, and no success for an
//!   operation whose effect the kernel cannot honestly provide.

use alloc::vec::Vec;

use rustos_user_abi::pager::{
    PagerVmRegionWire, PAGER_PAGE_BYTES, VM_OBJECT_ANONYMOUS, VM_SHARING_PRIVATE,
};
use kernel_ps::user::memlock::{FutureLock, LockRefusal};
use rustos_user_abi::syscall::{
    MREMAP_SOURCE_ANON_PRIVATE, MREMAP_SOURCE_HAS_HOLES, SYSCALL_OFFLOAD_OP_LINUX_MREMAP,
};
use x86_64::VirtAddr;

use super::*;

const PAGE: u64 = PAGER_PAGE_BYTES;
/// Pages inspected per process-state section and per `mincore` output write.
const SCAN_CHUNK_PAGES: usize = 4096;

/// One published mapping, reduced to what this module decides on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Span {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    /// Anonymous, process-private memory: the only kind whose contents can be
    /// reproduced by copying resident pages.
    pub anonymous_private: bool,
}

fn span_of(region: &PagerVmRegionWire) -> Span {
    Span {
        start: region.start,
        end: region.end,
        prot: region.prot,
        anonymous_private: region.object.object_type == VM_OBJECT_ANONYMOUS
            && region.sharing == VM_SHARING_PRIVATE,
    }
}

/// The current process's published mappings, ascending and non-overlapping.
fn current_spans() -> Result<Vec<Span>, i64> {
    let pid = multitask::current_user_process_id().ok_or(LINUX_ESRCH)?;
    let regions = multitask::pager_vma_regions_for_process(pid).map_err(|_| LINUX_ENOMEM)?;
    let mut spans: Vec<Span> = regions.iter().map(span_of).collect();
    spans.sort_unstable_by_key(|span| span.start);
    Ok(spans)
}

fn span_containing(spans: &[Span], addr: u64) -> Option<&Span> {
    let index = spans.partition_point(|span| span.start <= addr);
    index
        .checked_sub(1)
        .map(|i| &spans[i])
        .filter(|span| addr < span.end)
}

/// Sub-ranges of `[start, end)` that no span covers, ascending.
pub(super) fn uncovered_ranges(spans: &[Span], start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut cursor = start;
    for span in spans {
        if span.end <= cursor {
            continue;
        }
        if span.start >= end {
            break;
        }
        if span.start > cursor {
            gaps.push((cursor, span.start.min(end)));
        }
        cursor = cursor.max(span.end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        gaps.push((cursor, end));
    }
    gaps
}

/// The spans that exactly tile `[start, end)`, clipped to it; `None` if any
/// byte of the range is not covered.
pub(super) fn tile_range(spans: &[Span], start: u64, end: u64) -> Option<Vec<Span>> {
    if !uncovered_ranges(spans, start, end).is_empty() {
        return None;
    }
    Some(
        spans
            .iter()
            .filter(|span| span.start < end && span.end > start)
            .map(|span| Span {
                start: span.start.max(start),
                end: span.end.min(end),
                ..*span
            })
            .collect(),
    )
}

/// Per-page residency of `pages` pages from `start`, read from the live leaves.
fn resident_pages(start: u64, pages: usize) -> Option<Vec<bool>> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let space = process_state.address_space();
        (0..pages)
            .map(|index| {
                space
                    .translate_user(VirtAddr::new(start.wrapping_add(index as u64 * PAGE)))
                    .is_some()
            })
            .collect::<Vec<bool>>()
    })
}

/// Sub-ranges of `[start, end)` that are neither covered by a published region
/// nor resident, ascending.
fn unmapped_ranges(start: u64, end: u64) -> Result<Vec<(u64, u64)>, i64> {
    let spans = current_spans()?;
    let mut holes: Vec<(u64, u64)> = Vec::new();
    for (gap_start, gap_end) in uncovered_ranges(&spans, start, end) {
        let mut cursor = gap_start;
        while cursor < gap_end {
            let pages = (((gap_end - cursor) / PAGE) as usize).min(SCAN_CHUNK_PAGES);
            let resident = resident_pages(cursor, pages).ok_or(LINUX_ESRCH)?;
            for (index, present) in resident.iter().copied().enumerate() {
                if present {
                    continue;
                }
                let page = cursor + index as u64 * PAGE;
                match holes.last_mut() {
                    Some(last) if last.1 == page => last.1 = page + PAGE,
                    _ => holes.push((page, page + PAGE)),
                }
            }
            cursor += pages as u64 * PAGE;
        }
    }
    Ok(holes)
}

/// Whether every page of `[start, end)` is mapped: covered by a published
/// region or resident.
fn range_fully_mapped(start: u64, end: u64) -> Result<bool, i64> {
    Ok(unmapped_ranges(start, end)?.is_empty())
}

fn align_up(value: u64) -> Option<u64> {
    value.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1))
}

// ---- mincore ---------------------------------------------------------------

pub(super) fn syscall_linux_mincore(start: u64, len: u64, vec_ptr: u64) -> u64 {
    if start & (PAGE - 1) != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    let Some(aligned_len) = align_up(len) else {
        return linux_errno(LINUX_ENOMEM);
    };
    if aligned_len > paging::USER_SPACE_END_EXCLUSIVE.saturating_sub(start) {
        return linux_errno(LINUX_ENOMEM);
    }
    let pages = (aligned_len / PAGE) as usize;
    if pages == 0 {
        return 0;
    }
    if let Err(err) = usermem::validate_current_user_write_buffer(vec_ptr, pages) {
        return linux_errno(user_pointer_error_to_linux_errno(err));
    }
    let spans = match current_spans() {
        Ok(spans) => spans,
        Err(errno) => return linux_errno(errno),
    };
    let mut done = 0_usize;
    while done < pages {
        let chunk = (pages - done).min(SCAN_CHUNK_PAGES);
        let chunk_start = start + done as u64 * PAGE;
        let Some(resident) = resident_pages(chunk_start, chunk) else {
            return linux_errno(LINUX_ESRCH);
        };
        let mut bytes = Vec::with_capacity(chunk);
        let mut hole = false;
        for (index, present) in resident.iter().copied().enumerate() {
            let addr = chunk_start + index as u64 * PAGE;
            if !present && span_containing(&spans, addr).is_none() {
                hole = true;
                break;
            }
            bytes.push(u8::from(present));
        }
        if !bytes.is_empty()
            && let Err(err) = usermem::write_current_user_bytes(vec_ptr + done as u64, &bytes)
        {
            return linux_errno(user_pointer_error_to_linux_errno(err));
        }
        if hole {
            return linux_errno(LINUX_ENOMEM);
        }
        done += chunk;
    }
    0
}

// ---- mlock family ----------------------------------------------------------
//
// Admission follows Linux: `EPERM` when the caller has neither privilege nor a
// nonzero `RLIMIT_MEMLOCK`; `ENOMEM` when the cumulative locked total would
// exceed the limit or part of the range is unmapped; `EAGAIN` when the ledger
// cannot record the lock. Ledger edits and the mapped check are ordered lock,
// then check, then repair: a concurrent `munmap` releases its locks under the
// same process-state lock, so a lock recorded after the unmap is the only stale
// one and the repair below removes exactly that.

fn memlock_limit() -> Result<u64, i64> {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
    request.flags = linux_abi::RLIMIT_MEMLOCK;
    request.mask = 0x2;
    let response = call_syscalld(request)?;
    ensure_syscalld_status(&response)?;
    if response.payload_len as usize != LINUX_RLIMIT_SIZE {
        return Err(LINUX_EINVAL);
    }
    Ok(u64::from_le_bytes(response.payload[..8].try_into().unwrap()))
}

fn caller_is_privileged() -> bool {
    multitask::with_current_process_credentials(|security| security.euid() == 0).unwrap_or(false)
}

/// Linux `can_do_mlock`: privileged, or a nonzero `RLIMIT_MEMLOCK`.
fn lock_admission(limit: u64, privileged: bool) -> Result<(), i64> {
    if !privileged && limit == 0 {
        return Err(LINUX_EPERM);
    }
    Ok(())
}

/// The byte budget the ledger enforces: unlimited for a privileged caller.
fn lock_budget(limit: u64, privileged: bool) -> u64 {
    if privileged { u64::MAX } else { limit }
}

fn refusal_errno(refusal: LockRefusal) -> i64 {
    match refusal {
        LockRefusal::Limit => LINUX_ENOMEM,
        LockRefusal::NoMemory => LINUX_EAGAIN,
    }
}

/// `[start, start + len)` widened to whole pages, as `do_mlock` does.
pub(super) fn lock_range(start: u64, len: u64) -> Result<(u64, u64), i64> {
    let offset = start & (PAGE - 1);
    let aligned = align_up(len.checked_add(offset).ok_or(LINUX_ENOMEM)?).ok_or(LINUX_ENOMEM)?;
    let base = start & !(PAGE - 1);
    let end = base.checked_add(aligned).ok_or(LINUX_ENOMEM)?;
    if end > paging::USER_SPACE_END_EXCLUSIVE {
        return Err(LINUX_ENOMEM);
    }
    Ok((base, end))
}

fn mlock_common(start: u64, len: u64) -> u64 {
    let (base, end) = match lock_range(start, len) {
        Ok(range) => range,
        Err(errno) => return linux_errno(errno),
    };
    let limit = match memlock_limit() {
        Ok(limit) => limit,
        Err(errno) => return linux_errno(errno),
    };
    let privileged = caller_is_privileged();
    if let Err(errno) = lock_admission(limit, privileged) {
        return linux_errno(errno);
    }
    if end == base {
        return 0;
    }
    match range_fully_mapped(base, end) {
        Ok(true) => {}
        Ok(false) => return linux_errno(LINUX_ENOMEM),
        Err(errno) => return linux_errno(errno),
    }
    let budget = lock_budget(limit, privileged);
    let locked = multitask::with_current_user_process_state_mut(|_, _, state| {
        state.memlock_mut().lock(base, end, budget)
    });
    match locked {
        None => return linux_errno(LINUX_ESRCH),
        Some(Err(refusal)) => return linux_errno(refusal_errno(refusal)),
        Some(Ok(_)) => {}
    }
    // A thread that unmapped part of the range between the check and the lock
    // left a lock on nothing; drop exactly those holes.
    match unmapped_ranges(base, end) {
        Ok(holes) if holes.is_empty() => 0,
        Ok(holes) => {
            let _ = multitask::with_current_user_process_state_mut(|_, _, state| {
                for (hole_start, hole_end) in holes {
                    state.memlock_mut().release_mapping(hole_start, hole_end);
                }
            });
            linux_errno(LINUX_ENOMEM)
        }
        Err(errno) => linux_errno(errno),
    }
}

pub(super) fn syscall_linux_mlock(start: u64, len: u64) -> u64 {
    mlock_common(start, len)
}

pub(super) fn syscall_linux_mlock2(start: u64, len: u64, flags: u64) -> u64 {
    if flags & !linux_abi::MLOCK_ONFAULT != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    mlock_common(start, len)
}

pub(super) fn syscall_linux_munlock(start: u64, len: u64) -> u64 {
    let (base, end) = match lock_range(start, len) {
        Ok(range) => range,
        Err(errno) => return linux_errno(errno),
    };
    if end == base {
        return 0;
    }
    match range_fully_mapped(base, end) {
        Ok(true) => {}
        Ok(false) => return linux_errno(LINUX_ENOMEM),
        Err(errno) => return linux_errno(errno),
    }
    match multitask::with_current_user_process_state_mut(|_, _, state| {
        state.memlock_mut().unlock(base, end)
    }) {
        None => linux_errno(LINUX_ESRCH),
        Some(Err(refusal)) => linux_errno(refusal_errno(refusal)),
        Some(Ok(_)) => 0,
    }
}

pub(super) fn mlockall_flags_valid(flags: u64) -> bool {
    let known = linux_abi::MCL_CURRENT | linux_abi::MCL_FUTURE | linux_abi::MCL_ONFAULT;
    flags != 0 && flags & !known == 0 && flags != linux_abi::MCL_ONFAULT
}

/// Disjoint ascending page ranges of every mapping the process has: published
/// pager regions plus the labelled image/heap/stack areas.
fn all_mapped_ranges() -> Result<Vec<(u64, u64)>, i64> {
    let mut ranges: Vec<(u64, u64)> = current_spans()?
        .iter()
        .map(|span| (span.start, span.end))
        .collect();
    if let Some(areas) = multitask::with_current_user_process_state(|_, _, state| {
        state
            .linux_memory_map()
            .map(|map| map.areas().iter().map(|a| (a.start, a.end)).collect::<Vec<_>>())
            .unwrap_or_default()
    }) {
        ranges.extend(areas);
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if start >= end || end > paging::USER_SPACE_END_EXCLUSIVE {
            continue;
        }
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    Ok(merged)
}

pub(super) fn syscall_linux_mlockall(flags: u64) -> u64 {
    if !mlockall_flags_valid(flags) {
        return linux_errno(LINUX_EINVAL);
    }
    let limit = match memlock_limit() {
        Ok(limit) => limit,
        Err(errno) => return linux_errno(errno),
    };
    let privileged = caller_is_privileged();
    if let Err(errno) = lock_admission(limit, privileged) {
        return linux_errno(errno);
    }
    let budget = lock_budget(limit, privileged);
    let current = flags & linux_abi::MCL_CURRENT != 0;
    let ranges = if current {
        match all_mapped_ranges() {
            Ok(ranges) => ranges,
            Err(errno) => return linux_errno(errno),
        }
    } else {
        Vec::new()
    };
    let result = multitask::with_current_user_process_state_mut(|_, _, state| {
        let ledger = state.memlock_mut();
        let newly: u64 = ranges
            .iter()
            .map(|&(start, end)| ledger.unlocked_bytes_in(start, end))
            .sum();
        if ledger.locked_bytes().saturating_add(newly) > budget {
            return Err(LockRefusal::Limit);
        }
        for &(start, end) in &ranges {
            ledger.lock(start, end, budget)?;
        }
        // `mlockall` replaces the future-mapping policy: without MCL_FUTURE it
        // clears an earlier one, as Linux rewrites `def_flags`.
        ledger.set_future((flags & linux_abi::MCL_FUTURE != 0).then_some(FutureLock {
            on_fault: flags & linux_abi::MCL_ONFAULT != 0,
            limit_bytes: budget,
        }));
        Ok(())
    });
    match result {
        None => linux_errno(LINUX_ESRCH),
        Some(Err(refusal)) => linux_errno(refusal_errno(refusal)),
        Some(Ok(())) => 0,
    }
}

pub(super) fn syscall_linux_munlockall() -> u64 {
    match multitask::with_current_user_process_state_mut(|_, _, state| {
        let ledger = state.memlock_mut();
        ledger.release_mapping(0, paging::USER_SPACE_END_EXCLUSIVE);
        ledger.set_future(None);
    }) {
        Some(()) => 0,
        None => linux_errno(LINUX_ESRCH),
    }
}

// ---- msync -----------------------------------------------------------------
//
// No mapping kind that `mmap` can create has a backing store to write back:
// private file mappings never write through, memfd and anonymous memory are
// their own backing, and a writable shared TmpFs mapping maps the very frames
// vfsd serves `pread` from (`mm_broker_ops/file_shared.rs`), so a store is
// visible to every reader the moment it retires. Writable shared mappings of
// FAT files are refused (`EACCES`) rather than made to need write-back. A valid
// `msync` therefore has nothing to flush and succeeds once the range is proven
// mapped.

/// Linux's `msync` argument checks. `Ok(None)` means an empty range.
pub(super) fn msync_range(start: u64, len: u64, flags: u64) -> Result<Option<(u64, u64)>, i64> {
    let known = linux_abi::MS_ASYNC | linux_abi::MS_INVALIDATE | linux_abi::MS_SYNC;
    if flags & !known != 0 {
        return Err(LINUX_EINVAL);
    }
    if start & (PAGE - 1) != 0 {
        return Err(LINUX_EINVAL);
    }
    if flags & linux_abi::MS_ASYNC != 0 && flags & linux_abi::MS_SYNC != 0 {
        return Err(LINUX_EINVAL);
    }
    let aligned = align_up(len).ok_or(LINUX_ENOMEM)?;
    let end = start.checked_add(aligned).ok_or(LINUX_ENOMEM)?;
    Ok((end != start).then_some((start, end)))
}

pub(super) fn syscall_linux_msync(start: u64, len: u64, flags: u64) -> u64 {
    let (base, end) = match msync_range(start, len, flags) {
        Ok(Some(range)) => range,
        Ok(None) => return 0,
        Err(errno) => return linux_errno(errno),
    };
    match range_fully_mapped(base, end) {
        Ok(true) => 0,
        Ok(false) => linux_errno(LINUX_ENOMEM),
        Err(errno) => linux_errno(errno),
    }
}

// ---- membarrier ------------------------------------------------------------
//
// Every command except QUERY needs every CPU running a thread of this process
// to execute a full barrier before the call returns, which requires an
// IPI-and-acknowledge primitive. The kernel's IPIs are the private reschedule
// and TLB-shootdown vectors, neither of which acknowledges to a caller, so no
// command is implemented and QUERY reports the empty mask.

pub(super) fn syscall_linux_membarrier(cmd: u64, flags: u64) -> u64 {
    if flags != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if cmd == linux_abi::MEMBARRIER_CMD_QUERY {
        return 0;
    }
    linux_errno(LINUX_EINVAL)
}

// ---- mremap ----------------------------------------------------------------

/// A validated `mremap` request, lengths already rounded up to pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MremapRequest {
    pub old_addr: u64,
    pub old_len: u64,
    pub new_len: u64,
    pub may_move: bool,
    /// Destination for `MREMAP_FIXED`.
    pub fixed: Option<u64>,
}

/// Linux's argument checks, in Linux's order.
///
/// `MREMAP_DONTUNMAP` is refused with `EINVAL`: it leaves a populated-on-demand
/// copy of the old range behind, which needs a VMA-clone primitive this kernel
/// does not have.
pub(super) fn validate_mremap(
    old_addr: u64,
    old_size: u64,
    new_size: u64,
    flags: u64,
    new_addr: u64,
) -> Result<MremapRequest, i64> {
    let known = linux_abi::MREMAP_MAYMOVE | linux_abi::MREMAP_FIXED | linux_abi::MREMAP_DONTUNMAP;
    if flags & !known != 0 || flags & linux_abi::MREMAP_DONTUNMAP != 0 {
        return Err(LINUX_EINVAL);
    }
    let may_move = flags & linux_abi::MREMAP_MAYMOVE != 0;
    let fixed = flags & linux_abi::MREMAP_FIXED != 0;
    if fixed && !may_move {
        return Err(LINUX_EINVAL);
    }
    if old_addr & (PAGE - 1) != 0 {
        return Err(LINUX_EINVAL);
    }
    let old_len = align_up(old_size).ok_or(LINUX_EINVAL)?;
    let new_len = align_up(new_size).ok_or(LINUX_EINVAL)?;
    if new_len == 0 || old_len == 0 {
        return Err(LINUX_EINVAL);
    }
    let end_limit = paging::USER_SPACE_END_EXCLUSIVE;
    old_addr
        .checked_add(old_len)
        .filter(|end| *end <= end_limit)
        .ok_or(LINUX_EFAULT)?;
    let fixed = if fixed {
        if new_addr & (PAGE - 1) != 0 {
            return Err(LINUX_EINVAL);
        }
        let new_end = new_addr
            .checked_add(new_len)
            .filter(|end| *end <= end_limit)
            .ok_or(LINUX_EINVAL)?;
        let old_end = old_addr + old_len;
        if new_addr < old_end && new_end > old_addr {
            return Err(LINUX_EINVAL);
        }
        Some(new_addr)
    } else {
        None
    };
    Ok(MremapRequest {
        old_addr,
        old_len,
        new_len,
        may_move,
        fixed,
    })
}

/// What ring0 knows about the source that `syscalld` may not have recorded:
/// the `MREMAP_SOURCE_*` bits plus the protection of the last source page.
pub(super) fn source_hint(spans: &[Span], start: u64, end: u64) -> u32 {
    let tiles = tile_range(spans, start, end);
    let mut hint = 0_u32;
    match &tiles {
        None => hint |= MREMAP_SOURCE_HAS_HOLES,
        Some(tiles) => {
            if !tiles.is_empty() && tiles.iter().all(|span| span.anonymous_private) {
                hint |= MREMAP_SOURCE_ANON_PRIVATE;
            }
        }
    }
    if let Some(last) = span_containing(spans, end - 1) {
        hint |= last.prot & 7;
    }
    hint
}

/// `mremap(2)`. `syscalld` plans (target, `MREMAP_FIXED` replacement, shrink,
/// extension) and calls `MM_BROKER_OP_MOVE` when the mapping has to move; the
/// move relocates page-table leaves and frame claims atomically, never copies.
///
/// A growing mapping whose last page is locked stays locked, as Linux extends
/// the locked VMA: the extension is admitted against `RLIMIT_MEMLOCK` first and
/// recorded in the ledger afterwards.
pub(super) fn syscall_linux_mremap(
    old_addr: u64,
    old_size: u64,
    new_size: u64,
    flags: u64,
    new_addr: u64,
) -> u64 {
    let request = match validate_mremap(old_addr, old_size, new_size, flags, new_addr) {
        Ok(request) => request,
        Err(errno) => return linux_errno(errno),
    };
    let old_end = request.old_addr + request.old_len;
    let hint = match current_spans() {
        Ok(spans) => source_hint(&spans, request.old_addr, old_end),
        Err(errno) => return linux_errno(errno),
    };
    let grows = request.new_len > request.old_len;
    let extension_locked = grows
        && multitask::with_current_user_process_state(|_, _, state| {
            state.memlock().is_locked(old_end - 1)
        })
        .unwrap_or(false);
    if extension_locked {
        let extension = request.new_len - request.old_len;
        let admitted = memlock_limit().map(|limit| lock_budget(limit, caller_is_privileged()));
        let within = admitted.map(|budget| {
            multitask::with_current_user_process_state(|_, _, state| {
                state.memlock().locked_bytes().saturating_add(extension) <= budget
            })
            .unwrap_or(false)
        });
        match within {
            Ok(true) => {}
            Ok(false) => return linux_errno(LINUX_ENOMEM),
            Err(errno) => return linux_errno(errno),
        }
    }
    let response = match call_syscalld_fast_mem(
        SYSCALL_OFFLOAD_OP_LINUX_MREMAP,
        request.old_addr,
        request.old_len,
        hint,
        flags,
        request.fixed.unwrap_or(0),
        request.new_len,
    ) {
        Ok(response) => response,
        Err(_) if !ipc_ops::service_registered(linux_abi::IPC_SERVICE_LINUX_SYSCALLD) => {
            return linux_errno(LINUX_ENOSYS);
        }
        Err(errno) => return linux_errno(errno),
    };
    if response.payload_len as usize != size_of::<u64>() {
        return linux_errno(LINUX_EINVAL);
    }
    let result = u64::from_le_bytes(response.payload[..8].try_into().unwrap());
    if extension_locked {
        let start = result + request.old_len;
        let end = result + request.new_len;
        // Admitted above; a concurrent locker that used the budget since leaves
        // the extension unlocked, which is the conservative outcome.
        let budget = memlock_limit().map(|limit| lock_budget(limit, caller_is_privileged()));
        if let Ok(budget) = budget {
            let _ = multitask::with_current_user_process_state_mut(|_, _, state| {
                let _ = state.memlock_mut().lock(start, end, budget);
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = 0x10_0000;

    fn span(start: u64, end: u64, anon: bool, prot: u32) -> Span {
        Span {
            start,
            end,
            prot,
            anonymous_private: anon,
        }
    }

    #[test]
    fn uncovered_ranges_reports_exact_holes() {
        let spans = [span(A, A + 2 * PAGE, true, 3), span(A + 4 * PAGE, A + 6 * PAGE, true, 3)];
        assert_eq!(uncovered_ranges(&spans, A, A + 6 * PAGE), [(A + 2 * PAGE, A + 4 * PAGE)]);
        assert_eq!(uncovered_ranges(&spans, A - PAGE, A + PAGE), [(A - PAGE, A)]);
        assert_eq!(uncovered_ranges(&spans, A + 6 * PAGE, A + 7 * PAGE), [(A + 6 * PAGE, A + 7 * PAGE)]);
        assert!(uncovered_ranges(&spans, A, A + 2 * PAGE).is_empty());
        assert!(tile_range(&spans, A, A + 6 * PAGE).is_none());
        let tiles = tile_range(&spans, A + PAGE, A + 2 * PAGE).unwrap();
        assert_eq!((tiles[0].start, tiles[0].end), (A + PAGE, A + 2 * PAGE));
    }

    #[test]
    fn span_lookup_is_half_open() {
        let spans = [span(A, A + PAGE, true, 3), span(A + 2 * PAGE, A + 3 * PAGE, true, 3)];
        assert!(span_containing(&spans, A).is_some());
        assert!(span_containing(&spans, A + PAGE).is_none());
        assert!(span_containing(&spans, A + 2 * PAGE).is_some());
        assert!(span_containing(&spans, A - 1).is_none());
    }

    #[test]
    fn mlock_argument_rules_follow_linux() {
        assert_eq!(lock_range(A + 5, 1), Ok((A, A + PAGE)));
        assert_eq!(lock_range(A + PAGE - 1, 2), Ok((A, A + 2 * PAGE)));
        assert_eq!(lock_range(A, 0), Ok((A, A)));
        assert_eq!(lock_range(u64::MAX - 10, 100), Err(LINUX_ENOMEM));
        assert!(mlockall_flags_valid(linux_abi::MCL_CURRENT));
        assert!(mlockall_flags_valid(linux_abi::MCL_CURRENT | linux_abi::MCL_FUTURE));
        assert!(mlockall_flags_valid(linux_abi::MCL_CURRENT | linux_abi::MCL_ONFAULT));
        assert!(!mlockall_flags_valid(0));
        assert!(!mlockall_flags_valid(linux_abi::MCL_ONFAULT));
        assert!(!mlockall_flags_valid(8));
        assert_eq!(lock_range(paging::USER_SPACE_END_EXCLUSIVE - PAGE, 2 * PAGE), Err(LINUX_ENOMEM));
        assert_eq!(lock_admission(0, false), Err(LINUX_EPERM));
        assert_eq!(lock_admission(PAGE, false), Ok(()));
        assert_eq!(lock_admission(0, true), Ok(()));
        assert_eq!(lock_budget(PAGE, false), PAGE);
        assert_eq!(lock_budget(PAGE, true), u64::MAX);
        assert_eq!(refusal_errno(LockRefusal::Limit), LINUX_ENOMEM);
        assert_eq!(refusal_errno(LockRefusal::NoMemory), LINUX_EAGAIN);
    }

    #[test]
    fn msync_argument_rules_follow_linux() {
        assert_eq!(msync_range(A, 100, linux_abi::MS_SYNC), Ok(Some((A, A + PAGE))));
        assert_eq!(msync_range(A, 0, linux_abi::MS_ASYNC), Ok(None));
        assert_eq!(msync_range(A + 1, PAGE, linux_abi::MS_SYNC), Err(LINUX_EINVAL));
        assert_eq!(
            msync_range(A, PAGE, linux_abi::MS_SYNC | linux_abi::MS_ASYNC),
            Err(LINUX_EINVAL)
        );
        assert_eq!(msync_range(A, PAGE, 0x40), Err(LINUX_EINVAL));
        assert_eq!(
            msync_range(A, PAGE, linux_abi::MS_SYNC | linux_abi::MS_INVALIDATE),
            Ok(Some((A, A + PAGE)))
        );
        assert_eq!(msync_range(A, u64::MAX, 0), Err(LINUX_ENOMEM));
    }

    #[test]
    fn membarrier_reports_no_commands() {
        assert_eq!(syscall_linux_membarrier(0, 0), 0);
        assert_eq!(syscall_linux_membarrier(0, 1), linux_errno(LINUX_EINVAL));
        // GLOBAL, PRIVATE_EXPEDITED and REGISTER_* are not implemented.
        for cmd in [1_u64, 2, 8, 16, 1 << 31] {
            assert_eq!(syscall_linux_membarrier(cmd, 0), linux_errno(LINUX_EINVAL));
        }
    }

    #[test]
    fn mremap_validation_matches_linux_order() {
        let ok = |o, os, ns, f, n| validate_mremap(o, os, ns, f, n);
        let maymove = linux_abi::MREMAP_MAYMOVE;
        let fixed = linux_abi::MREMAP_FIXED;
        assert_eq!(ok(A, PAGE, 2 * PAGE, 0, 0).unwrap().new_len, 2 * PAGE);
        assert_eq!(ok(A, 1, 1, 0, 0).unwrap().old_len, PAGE);
        assert_eq!(ok(A + 1, PAGE, PAGE, 0, 0), Err(LINUX_EINVAL));
        assert_eq!(ok(A, PAGE, 0, 0, 0), Err(LINUX_EINVAL));
        assert_eq!(ok(A, 0, PAGE, 0, 0), Err(LINUX_EINVAL));
        assert_eq!(ok(A, PAGE, PAGE, 0x80, 0), Err(LINUX_EINVAL));
        assert_eq!(ok(A, PAGE, PAGE, fixed, A + 8 * PAGE), Err(LINUX_EINVAL));
        assert_eq!(ok(A, PAGE, PAGE, maymove | linux_abi::MREMAP_DONTUNMAP, 0), Err(LINUX_EINVAL));
        assert_eq!(ok(A, PAGE, PAGE, maymove | fixed, A + 8 * PAGE + 1), Err(LINUX_EINVAL));
        // Overlapping destination.
        assert_eq!(ok(A, 4 * PAGE, 4 * PAGE, maymove | fixed, A + 2 * PAGE), Err(LINUX_EINVAL));
        assert_eq!(ok(A + 4 * PAGE, 4 * PAGE, 4 * PAGE, maymove | fixed, A + 2 * PAGE), Err(LINUX_EINVAL));
        let far = ok(A, PAGE, PAGE, maymove | fixed, A + 8 * PAGE).unwrap();
        assert_eq!(far.fixed, Some(A + 8 * PAGE));
        assert!(far.may_move);
        assert_eq!(ok(u64::MAX - PAGE + 1, 4 * PAGE, PAGE, 0, 0), Err(LINUX_EFAULT));
    }

    #[test]
    fn mremap_source_hint_reports_only_what_ring0_can_prove() {
        let spans = [
            span(A, A + 2 * PAGE, true, 3),
            span(A + 2 * PAGE, A + 4 * PAGE, true, 1),
            span(A + 8 * PAGE, A + 9 * PAGE, false, 1),
        ];
        // Fully covered, all anonymous private: ring0 vouches, last page is R.
        assert_eq!(
            source_hint(&spans, A, A + 4 * PAGE),
            MREMAP_SOURCE_ANON_PRIVATE | 1
        );
        assert_eq!(source_hint(&spans, A, A + 2 * PAGE), MREMAP_SOURCE_ANON_PRIVATE | 3);
        // A file-backed page withdraws the vouch but not the evidence.
        assert_eq!(source_hint(&spans, A + 8 * PAGE, A + 9 * PAGE), 1);
        // A hole is reported, and nothing is vouched for.
        assert_eq!(
            source_hint(&spans, A + 4 * PAGE, A + 6 * PAGE),
            MREMAP_SOURCE_HAS_HOLES
        );
        assert_eq!(
            source_hint(&spans, A, A + 6 * PAGE),
            MREMAP_SOURCE_HAS_HOLES
        );
    }
}
