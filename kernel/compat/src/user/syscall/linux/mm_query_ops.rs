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
//!   recorded in `MemlockLedger`. `mlock`/`mlock2` pre-faults anonymous private
//!   pages unless `MLOCK_ONFAULT` is set (`MCL_ONFAULT` still defers population
//!   to first touch). `MADV_POPULATE_*` and `MADV_DONTNEED*`/`MADV_FREE` also
//!   run here in the caller's address-space context.
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

/// Fills `out` with per-page `0`/`1` residency from `start`, read from the live leaves.
fn resident_pages(start: u64, out: &mut [u8]) -> Option<()> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let space = process_state.address_space();
        for (index, slot) in out.iter_mut().enumerate() {
            let present = space
                .translate_user(VirtAddr::new(start.wrapping_add(index as u64 * PAGE)))
                .is_some();
            *slot = u8::from(present);
        }
    })
}

/// Sub-ranges of `[start, end)` that are neither covered by `spans`
/// nor resident, ascending.
fn unmapped_ranges_with_spans(
    spans: &[Span],
    start: u64,
    end: u64,
) -> Result<Vec<(u64, u64)>, i64> {
    let mut holes: Vec<(u64, u64)> = Vec::new();
    let mut buf = Vec::new();
    for (gap_start, gap_end) in uncovered_ranges(spans, start, end) {
        let mut cursor = gap_start;
        while cursor < gap_end {
            let pages = (((gap_end - cursor) / PAGE) as usize).min(SCAN_CHUNK_PAGES);
            buf.resize(pages, 0);
            resident_pages(cursor, &mut buf).ok_or(LINUX_ESRCH)?;
            for (index, &present) in buf.iter().enumerate() {
                if present != 0 {
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

/// Sub-ranges of `[start, end)` that are neither covered by a published region
/// nor resident, ascending.
fn unmapped_ranges(start: u64, end: u64) -> Result<Vec<(u64, u64)>, i64> {
    let spans = current_spans()?;
    unmapped_ranges_with_spans(&spans, start, end)
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
    linux_unit_result((|| {
        if start & (PAGE - 1) != 0 {
            return Err(LINUX_EINVAL);
        }
        let aligned_len = align_up(len).ok_or(LINUX_ENOMEM)?;
        if aligned_len > paging::USER_SPACE_END_EXCLUSIVE.saturating_sub(start) {
            return Err(LINUX_ENOMEM);
        }
        let pages = (aligned_len / PAGE) as usize;
        if pages == 0 {
            return Ok(());
        }
        validate_user_write(vec_ptr, pages)?;
        let spans = current_spans()?;
        let mut buf = alloc::vec![0_u8; pages.min(SCAN_CHUNK_PAGES)];
        let mut done = 0_usize;
        while done < pages {
            let chunk = (pages - done).min(SCAN_CHUNK_PAGES);
            let chunk_start = start + done as u64 * PAGE;
            let chunk_buf = &mut buf[..chunk];
            resident_pages(chunk_start, chunk_buf).ok_or(LINUX_ESRCH)?;
            let mut valid = chunk;
            let mut hole = false;
            for (index, &present) in chunk_buf.iter().enumerate() {
                let addr = chunk_start + index as u64 * PAGE;
                if present == 0 && span_containing(&spans, addr).is_none() {
                    valid = index;
                    hole = true;
                    break;
                }
            }
            if valid > 0 {
                write_user_bytes(vec_ptr + done as u64, &chunk_buf[..valid])?;
            }
            if hole {
                return Err(LINUX_ENOMEM);
            }
            done += chunk;
        }
        Ok(())
    })())
}

// ---- mlock family ----------------------------------------------------------
//
// Admission follows Linux: `EPERM` when the caller has neither privilege nor a
// nonzero `RLIMIT_MEMLOCK`; `ENOMEM` when the cumulative locked total would
// exceed the limit or part of the range is unmapped; `EAGAIN` when the ledger
// cannot record the lock. Mapped pieces are computed from the published spans
// and live leaves before locking.

fn memlock_limit() -> Result<u64, i64> {
    let mut request = new_syscalld_request(SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64);
    request.flags = linux_abi::RLIMIT_MEMLOCK;
    request.mask = 0x2;
    let payload = call_syscalld_payload(&request, LINUX_RLIMIT_SIZE)?;
    Ok(u64::from_le_bytes(payload[..8].try_into().unwrap()))
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

/// `[start, end)` minus `holes` (ascending, disjoint, inside the range): the
/// mapped pieces, ascending.
pub(super) fn complement_of_holes(start: u64, end: u64, holes: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut pieces = Vec::new();
    let mut cursor = start;
    for &(hole_start, hole_end) in holes {
        if hole_start > cursor {
            pieces.push((cursor, hole_start.min(end)));
        }
        cursor = cursor.max(hole_end);
    }
    if cursor < end {
        pieces.push((cursor, end));
    }
    pieces
}

/// Tries to make `[start, end)` resident by faulting its anonymous pages in.
///
/// Only pages a published anonymous region grants access to are touched: the
/// access is a write exactly when the region is writable (Linux populates a
/// private writable page with a write fault so it owns a private frame), a read
/// when it is only readable, and a `PROT_NONE` page is skipped. Pages of every
/// other backing are resident by construction (eager private-file and image
/// leaves, shared frames) and are not touched. `access` of `Some(write)`
/// forces that access for `MADV_POPULATE_*`.
fn populate_resident(
    spans: &[Span],
    start: u64,
    end: u64,
    forced_write: Option<bool>,
) -> Result<(), i64> {
    use crate::pager::{PopulateOutcome, populate_current_anonymous_page};
    use rustos_user_abi::pager::{VM_PROT_READ, VM_PROT_WRITE};

    const RETRIES: usize = 256;
    let mut page = start;
    while page < end {
        let Some(span) = span_containing(spans, page) else {
            page += PAGE;
            continue;
        };
        if !span.anonymous_private {
            page = span.end.min(end);
            continue;
        }
        let write = match forced_write {
            Some(write) => write,
            None if span.prot & VM_PROT_WRITE != 0 => true,
            None if span.prot & VM_PROT_READ != 0 => false,
            None => {
                page = span.end.min(end);
                continue;
            }
        };
        let mut attempts = 0;
        loop {
            match populate_current_anonymous_page(page, write) {
                PopulateOutcome::Resident => break,
                PopulateOutcome::Retry if attempts < RETRIES => {
                    attempts += 1;
                    core::hint::spin_loop();
                }
                PopulateOutcome::Retry | PopulateOutcome::NoMemory => return Err(LINUX_ENOMEM),
                // A concurrent unmap or protection change removed the right to
                // populate this page; it is no longer something to make resident.
                PopulateOutcome::NotPopulatable => break,
            }
        }
        page += PAGE;
    }
    Ok(())
}

/// `mlock`/`mlock2`. Linux admits the whole request against the limit, locks
/// every mapped piece, and only afterwards reports `ENOMEM` for an unmapped
/// part: the locked pieces stay locked. `populate` is false for
/// `MLOCK_ONFAULT`, whose pages become resident (and stay) at first touch.
fn mlock_common(start: u64, len: u64, populate: bool) -> u64 {
    linux_unit_result((|| {
        let (base, end) = lock_range(start, len)?;
        let privileged = caller_is_privileged();
        let limit = if privileged { 0 } else { memlock_limit()? };
        lock_admission(limit, privileged)?;
        if end == base {
            return Ok(());
        }
        let spans = current_spans()?;
        let holes = unmapped_ranges_with_spans(&spans, base, end)?;
        let pieces = complement_of_holes(base, end, &holes);
        let budget = lock_budget(limit, privileged);
        multitask::with_current_user_process_state_mut(|_, _, state| {
            state.memlock_mut().lock_all(&pieces, budget)
        })
        .ok_or(LINUX_ESRCH)?
        .map_err(refusal_errno)?;
        if populate {
            for &(piece_start, piece_end) in &pieces {
                populate_resident(&spans, piece_start, piece_end, None)?;
            }
        }
        if holes.is_empty() { Ok(()) } else { Err(LINUX_ENOMEM) }
    })())
}

pub(super) fn syscall_linux_mlock(start: u64, len: u64) -> u64 {
    mlock_common(start, len, true)
}

pub(super) fn syscall_linux_mlock2(start: u64, len: u64, flags: u64) -> u64 {
    if flags & !linux_abi::MLOCK_ONFAULT != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    mlock_common(start, len, flags & linux_abi::MLOCK_ONFAULT == 0)
}

/// `munlock`: unlocks every mapped piece, then reports `ENOMEM` for a hole.
pub(super) fn syscall_linux_munlock(start: u64, len: u64) -> u64 {
    linux_unit_result((|| {
        let (base, end) = lock_range(start, len)?;
        if end == base {
            return Ok(());
        }
        let holes = unmapped_ranges(base, end)?;
        let pieces = complement_of_holes(base, end, &holes);
        multitask::with_current_user_process_state_mut(|_, _, state| {
            pieces
                .iter()
                .try_for_each(|&(piece_start, piece_end)| {
                    state.memlock_mut().unlock(piece_start, piece_end).map(|_| ())
                })
        })
        .ok_or(LINUX_ESRCH)?
        .map_err(refusal_errno)?;
        if holes.is_empty() { Ok(()) } else { Err(LINUX_ENOMEM) }
    })())
}

fn validate_madvise_range(start: u64, len: u64) -> Result<Option<(u64, u64)>, i64> {
    if start & (PAGE - 1) != 0 {
        return Err(LINUX_EINVAL);
    }
    let Some(aligned) = align_up(len) else {
        return Err(LINUX_EINVAL);
    };
    if len != 0 && aligned == 0 {
        return Err(LINUX_EINVAL);
    }
    let Some(end) = start.checked_add(aligned) else {
        return Err(LINUX_EINVAL);
    };
    if end == start {
        return Ok(None);
    }
    if end > paging::USER_SPACE_END_EXCLUSIVE {
        return Err(LINUX_ENOMEM);
    }
    Ok(Some((start, end)))
}

/// `madvise(MADV_POPULATE_READ | MADV_POPULATE_WRITE)`, which must run in the
/// caller's own context to fault pages in (see `populate_current_anonymous_page`)
/// and is therefore answered here rather than by `syscalld`. Linux order:
/// alignment, then the unmapped hole (`ENOMEM`, reported after the mapped part
/// was populated), and `EFAULT` where the access is not permitted.
pub(super) fn syscall_linux_madvise_populate(start: u64, len: u64, write: bool) -> u64 {
    use rustos_user_abi::pager::{VM_PROT_READ, VM_PROT_WRITE};
    linux_unit_result((|| {
        let Some((start, end)) = validate_madvise_range(start, len)? else {
            return Ok(());
        };
        let spans = current_spans()?;
        let needed = if write { VM_PROT_WRITE } else { VM_PROT_READ };
        if spans
            .iter()
            .filter(|span| span.start < end && span.end > start)
            .any(|span| span.prot & needed == 0)
        {
            return Err(LINUX_EFAULT);
        }
        let holes = unmapped_ranges_with_spans(&spans, start, end)?;
        populate_resident(&spans, start, end, Some(write))?;
        if holes.is_empty() { Ok(()) } else { Err(LINUX_ENOMEM) }
    })())
}

fn anonymous_private_pieces(spans: &[Span], start: u64, end: u64) -> Vec<(u64, u64)> {
    spans
        .iter()
        .filter(|span| span.anonymous_private && span.start < end && span.end > start)
        .map(|span| (span.start.max(start), span.end.min(end)))
        .collect()
}

fn madvise_discard_range(
    start: u64,
    len: u64,
    allow_locked: bool,
    require_anon_private: bool,
) -> u64 {
    linux_unit_result((|| {
        let Some((start, end)) = validate_madvise_range(start, len)? else {
            return Ok(());
        };
        let spans = current_spans()?;
        if require_anon_private
            && spans
                .iter()
                .filter(|span| span.start < end && span.end > start)
                .any(|span| !span.anonymous_private)
        {
            return Err(LINUX_EINVAL);
        }
        let pieces = anonymous_private_pieces(&spans, start, end);
        multitask::with_current_user_process_state_mut(|_, _, process_state| {
            if !allow_locked && process_state.memlock().locked_bytes_in(start, end) > 0 {
                return Err(LINUX_EINVAL);
            }
            for (piece_start, piece_end) in &pieces {
                let page_count = usize::try_from((*piece_end - *piece_start) / PAGE)
                    .map_err(|_| LINUX_EINVAL)?;
                process_state
                    .address_space_mut()
                    .unmap_present_user_pages_at(VirtAddr::new(*piece_start), page_count)
                    .map_err(address_space_error_to_linux_errno)?;
            }
            process_state.sync_mapping_kmem();
            Ok(())
        })
        .ok_or(LINUX_ESRCH)??;
        // Fast path: if published Ring0 `pager_vma` spans alone already cover
        // `[start, end)`, every page is mapped and we avoid scanning all page-table
        // leaves or querying `syscalld` across multi-megabyte V8/PartitionAlloc cages.
        if uncovered_ranges(&spans, start, end).is_empty() {
            return Ok(());
        }
        let holes = unmapped_ranges_with_spans(&spans, start, end)?;
        if holes.is_empty() || syscalld_vma_covers_range(start, end - start) {
            Ok(())
        } else {
            Err(LINUX_ENOMEM)
        }
    })())
}

/// Checks with `syscalld` whether `[start, start + len)` is covered by tracked
/// VMAs (including `MmVmaKind::Reserved` `PROT_NONE` reservations that have not
/// been materialized in Ring0's `pager_vma` table).
fn syscalld_vma_covers_range(start: u64, len: u64) -> bool {
    let mut request = new_syscalld_request(
        rustos_user_abi::syscall::SYSCALL_OFFLOAD_OP_LINUX_MADVISE,
    );
    request.arg0 = start;
    request.arg1 = len;
    request.flags = linux_abi::MADV_DONTNEED | (1_u64 << 63);
    call_syscalld(&request)
        .and_then(|response| ensure_empty_syscalld_response(&response))
        .is_ok()
}

pub(super) fn syscall_linux_madvise_dontneed(start: u64, len: u64, allow_locked: bool) -> u64 {
    madvise_discard_range(start, len, allow_locked, false)
}

pub(super) fn syscall_linux_madvise_free(start: u64, len: u64) -> u64 {
    madvise_discard_range(start, len, false, true)
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
    linux_unit_result((|| {
        if !mlockall_flags_valid(flags) {
            return Err(LINUX_EINVAL);
        }
        let privileged = caller_is_privileged();
        let limit = if privileged { 0 } else { memlock_limit()? };
        lock_admission(limit, privileged)?;
        let budget = lock_budget(limit, privileged);
        let ranges = if flags & linux_abi::MCL_CURRENT != 0 {
            all_mapped_ranges()?
        } else {
            Vec::new()
        };
        multitask::with_current_user_process_state_mut(|_, _, state| {
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
        })
        .ok_or(LINUX_ESRCH)?
        .map_err(refusal_errno)
    })())
}

pub(super) fn syscall_linux_munlockall() -> u64 {
    linux_unit_result(
        multitask::with_current_user_process_state_mut(|_, _, state| {
            let ledger = state.memlock_mut();
            ledger.release_mapping(0, paging::USER_SPACE_END_EXCLUSIVE);
            ledger.set_future(None);
        })
        .ok_or(LINUX_ESRCH),
    )
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
    linux_unit_result((|| {
        let Some((base, end)) = msync_range(start, len, flags)? else {
            return Ok(());
        };
        if !range_fully_mapped(base, end)? {
            return Err(LINUX_ENOMEM);
        }
        let object_ids = multitask::with_current_user_process_state(|_, _, state| {
            state.shared_memfd_object_ids_in_range(base, end)
        })
        .unwrap_or_default();
        for object_id in object_ids {
            let mut request = new_vfs_request(rustos_user_abi::syscall::VFS_IPC_OP_SHARED_BACKING);
            request.arg0 = rustos_user_abi::syscall::VFS_SHARED_BACKING_FLAG_SYNC;
            request.arg1 = object_id;
            call_vfs_status(&request)?;
        }
        Ok(())
    })())
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

pub(super) fn syscall_linux_pkey_alloc(flags: u64, access_rights: u64) -> u64 {
    if flags != 0 || (access_rights & !0x3) != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    // No PKU protection keys exist in hardware/CR4.PKE.
    linux_errno(LINUX_ENOSPC)
}

pub(super) fn syscall_linux_pkey_free(_pkey: u64) -> u64 {
    linux_errno(LINUX_EINVAL)
}

fn pkey_mprotect_key_valid(pkey: u64) -> bool {
    let pkey_i32 = pkey as i32;
    pkey_i32 == 0 || pkey_i32 == -1
}

pub(super) fn syscall_linux_pkey_mprotect(
    start: u64,
    len: u64,
    prot: u64,
    pkey: u64,
) -> u64 {
    if pkey_mprotect_key_valid(pkey) {
        return super::memory_ops::syscall_linux_mprotect(start, len, prot);
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
    linux_result((|| {
        let request = validate_mremap(old_addr, old_size, new_size, flags, new_addr)?;
        let old_end = request.old_addr + request.old_len;
        let hint = source_hint(&current_spans()?, request.old_addr, old_end);
        let grows = request.new_len > request.old_len;
        let extension_budget = if grows {
            let (is_locked, locked_bytes) =
                multitask::with_current_user_process_state(|_, _, state| {
                    let ledger = state.memlock();
                    (ledger.is_locked(old_end - 1), ledger.locked_bytes())
                })
                .unwrap_or((false, 0));
            if is_locked {
                let privileged = caller_is_privileged();
                let limit = if privileged { 0 } else { memlock_limit()? };
                let budget = lock_budget(limit, privileged);
                let extension = request.new_len - request.old_len;
                if locked_bytes.saturating_add(extension) > budget {
                    return Err(LINUX_ENOMEM);
                }
                Some(budget)
            } else {
                None
            }
        } else {
            None
        };
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
                return Err(LINUX_ENOSYS);
            }
            Err(errno) => return Err(errno),
        };
        let result = decode_exact_payload::<u64>(&response.payload, response.payload_len)?;
        if let Some(budget) = extension_budget {
            let start = result + request.old_len;
            let end = result + request.new_len;
            // Admitted above; a concurrent locker that used the budget since leaves
            // the extension unlocked, which is the conservative outcome.
            let _ = multitask::with_current_user_process_state_mut(|_, _, state| {
                let _ = state.memlock_mut().lock(start, end, budget);
            });
        }
        Ok(result)
    })())
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

    #[test]
    fn madvise_range_and_piece_helpers_follow_linux() {
        assert_eq!(validate_madvise_range(A + 1, 0), Err(LINUX_EINVAL));
        assert_eq!(validate_madvise_range(A, 0), Ok(None));
        assert_eq!(validate_madvise_range(A, 1), Ok(Some((A, A + PAGE))));
        assert_eq!(validate_madvise_range(A, u64::MAX), Err(LINUX_EINVAL));
        assert_eq!(
            validate_madvise_range(paging::USER_SPACE_END_EXCLUSIVE, PAGE),
            Err(LINUX_ENOMEM)
        );

        let spans = [
            span(A, A + 3 * PAGE, true, 3),
            span(A + 3 * PAGE, A + 5 * PAGE, false, 3),
            span(A + 6 * PAGE, A + 8 * PAGE, true, 1),
        ];
        assert_eq!(
            anonymous_private_pieces(&spans, A + PAGE, A + 7 * PAGE),
            [(A + PAGE, A + 3 * PAGE), (A + 6 * PAGE, A + 7 * PAGE)]
        );

        assert_eq!(
            complement_of_holes(A, A + 6 * PAGE, &[(A + PAGE, A + 2 * PAGE), (A + 4 * PAGE, A + 6 * PAGE)]),
            [(A, A + PAGE), (A + 2 * PAGE, A + 4 * PAGE)]
        );
        assert_eq!(
            complement_of_holes(A, A + 2 * PAGE, &[]),
            [(A, A + 2 * PAGE)]
        );
    }

    #[test]
    fn pkey_mprotect_accepts_zero_and_minus_one_ignoring_upper_32_bits() {
        assert!(pkey_mprotect_key_valid(0));
        assert!(pkey_mprotect_key_valid(u64::MAX));
        assert!(pkey_mprotect_key_valid(0xFFFF_FFFF));
        assert!(pkey_mprotect_key_valid(0xDEAD_0000_0000_0000));
        assert!(pkey_mprotect_key_valid(0xDEAD_0000_FFFF_FFFF));
        assert!(!pkey_mprotect_key_valid(1));
        assert!(!pkey_mprotect_key_valid(0x1_0000_0001));
        assert!(!pkey_mprotect_key_valid((-2_i32 as u32) as u64));
        assert_eq!(syscall_linux_pkey_mprotect(A, PAGE, 0, 1), linux_errno(LINUX_EINVAL));
        assert_eq!(
            syscall_linux_pkey_mprotect(A, PAGE, 0, 0x1_0000_0001),
            linux_errno(LINUX_EINVAL)
        );
    }
}
