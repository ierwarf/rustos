//! `mremap(2)` mapping policy (a child of `linux_policy`, sharing its MM lease).
//!
//! - **Owner:** `syscalld` owns the address-space policy: where the new range
//!   goes, what `MREMAP_FIXED` replaces, which protection an extension takes and
//!   the `VmaMap` records. Ring0 owns the mechanism, `MM_BROKER_OP_MOVE`.
//! - **Boundary:** every address, length and flag is untrusted. The compact
//!   frame carries `prot` as ring0's evidence about a source this service may
//!   never have recorded (a fork child, an image); it can only make a request
//!   more conservative than the records alone, never grant a growth.
//! - **Lifecycle:** validate, plan (pure), execute: extension first (fresh free
//!   range), then the atomic move, then the policy records. Only a completed
//!   broker transaction edits the `VmaMap`; a failed move retires the extension.
//! - **Concurrency:** one `MmLease` serializes the process's policy state; the
//!   broker serializes the page tables. Another thread sees the old mapping or
//!   the new one, never a copy.
//! - **Failure:** `ENOMEM` for growth or movement of backing that cannot be
//!   relocated, `EFAULT` for a source hole, `EINVAL` for malformed arguments.
//!   `MREMAP_FIXED` unmaps its destination first, as Linux does, so a later
//!   refusal leaves that destination unmapped.
//! - **Forbidden:** no byte copy, no growth of file/device/shared backing, no
//!   `MREMAP_DONTUNMAP` (it needs a clone primitive this kernel does not have).

use super::*;
use rustos_user_abi::syscall::{
    MM_BROKER_OP_MOVE, MREMAP_SOURCE_ANON_PRIVATE, MREMAP_SOURCE_HAS_HOLES,
};

const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;
const MREMAP_DONTUNMAP: u64 = 4;

/// A validated request, lengths rounded up to pages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MremapRequest {
    old_addr: u64,
    old_len: u64,
    new_len: u64,
    may_move: bool,
    fixed: Option<u64>,
}

/// What the plan may assume about the source range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceFacts {
    /// The source touches the `brk` heap, whose labels and limits ring0 owns.
    has_heap: bool,
    /// Every source byte is private anonymous memory (or an address-space
    /// reservation), the only backing whose extension is a fresh zero page.
    anonymous: bool,
    /// The bytes just past the source are free of mappings and policy records.
    extension_free: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MremapPlan {
    Keep,
    /// Unmap `[start, end)`, the tail beyond the new length.
    ShrinkTail { start: u64, end: u64 },
    /// Map a fresh anonymous extension `[start, end)` right after the source.
    GrowInPlace { start: u64, end: u64 },
    /// Atomic relocation. `target` is `None` when policy must choose.
    Relocate {
        target: Option<u64>,
        /// Unmap the destination first (`MREMAP_FIXED`).
        replace_destination: bool,
        /// Source tail dropped before the move when `MREMAP_FIXED` shrinks.
        drop_tail: Option<(u64, u64)>,
        moved_len: u64,
        extension_len: u64,
    },
}

/// Linux's argument checks, in Linux's order.
fn validate(
    old_addr: u64,
    old_len: u64,
    new_len: u64,
    flags: u64,
    new_addr: u64,
    user_end: u64,
) -> Result<MremapRequest, i32> {
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP) != 0
        || flags & MREMAP_DONTUNMAP != 0
    {
        return Err(errno::EINVAL);
    }
    let may_move = flags & MREMAP_MAYMOVE != 0;
    let fixed = flags & MREMAP_FIXED != 0;
    if fixed && !may_move {
        return Err(errno::EINVAL);
    }
    if !old_addr.is_multiple_of(PAGE_SIZE) {
        return Err(errno::EINVAL);
    }
    let old_len = align_up(old_len).ok_or(errno::EINVAL)?;
    let new_len = align_up(new_len).ok_or(errno::EINVAL)?;
    if old_len == 0 || new_len == 0 {
        return Err(errno::EINVAL);
    }
    let old_end = old_addr
        .checked_add(old_len)
        .filter(|end| *end <= user_end)
        .ok_or(errno::EFAULT)?;
    let fixed = if fixed {
        if !new_addr.is_multiple_of(PAGE_SIZE) || new_addr < LINUX_USER_SPACE_BASE {
            return Err(errno::EINVAL);
        }
        let new_end = new_addr
            .checked_add(new_len)
            .filter(|end| *end <= user_end)
            .ok_or(errno::EINVAL)?;
        if new_addr < old_end && new_end > old_addr {
            return Err(errno::EINVAL);
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

/// Pure decision: how a validated request is carried out.
fn plan_request(request: &MremapRequest, facts: &SourceFacts) -> Result<MremapPlan, i32> {
    let old_end = request.old_addr + request.old_len;
    if request.fixed.is_none() && request.new_len == request.old_len {
        return Ok(MremapPlan::Keep);
    }
    if facts.has_heap {
        return Err(errno::ENOMEM);
    }
    let Some(target) = request.fixed else {
        if request.new_len < request.old_len {
            return Ok(MremapPlan::ShrinkTail {
                start: request.old_addr + request.new_len,
                end: old_end,
            });
        }
        if !facts.anonymous {
            return Err(errno::ENOMEM);
        }
        if facts.extension_free {
            return Ok(MremapPlan::GrowInPlace {
                start: old_end,
                end: request.old_addr + request.new_len,
            });
        }
        if !request.may_move {
            return Err(errno::ENOMEM);
        }
        return Ok(MremapPlan::Relocate {
            target: None,
            replace_destination: false,
            drop_tail: None,
            moved_len: request.old_len,
            extension_len: request.new_len - request.old_len,
        });
    };
    if request.new_len > request.old_len && !facts.anonymous {
        return Err(errno::ENOMEM);
    }
    Ok(MremapPlan::Relocate {
        target: Some(target),
        replace_destination: true,
        drop_tail: (request.old_len > request.new_len)
            .then_some((request.old_addr + request.new_len, old_end)),
        moved_len: request.old_len.min(request.new_len),
        extension_len: request.new_len.saturating_sub(request.old_len),
    })
}

/// Shifts `vma` (already clipped to the moved range) to the destination.
fn translated(vma: &MmVma, old_addr: u64, target: u64) -> MmVma {
    let mut moved = vma.clone();
    moved.start = vma.start - old_addr + target;
    moved.end = vma.end - old_addr + target;
    moved
}

pub fn handle_mremap(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let validated = validate(
        request.arg0,
        request.arg1,
        mmap_offset(request),
        request.flags,
        request.dirfd,
        LINUX_USER_SPACE_END,
    );
    let mremap = match validated {
        Ok(mremap) => mremap,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    let target = MmTarget::of(request);
    let mut state = match mm_state_for(target) {
        Ok(state) => state,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    if state.vmas.len().saturating_add(2 + VMA_SPLIT_HEADROOM) > MAX_VMA_RECORDS {
        response.status = errno::ENOMEM;
        return;
    }
    match execute(target, &mut state, &mremap, request.mask) {
        Ok(addr) => copy_payload(response, &addr),
        Err(errno) => response.status = errno,
    }
}

fn execute(
    target: MmTarget,
    state: &mut MmPolicyState,
    request: &MremapRequest,
    hint: u32,
) -> Result<u64, i32> {
    let old_addr = request.old_addr;
    let old_end = old_addr + request.old_len;

    // The source must be mapped: policy records, or (for a range this service
    // never recorded) the kernel's own geometry.
    require_mapped(target, state, old_addr, old_end)?;
    let facts = source_facts(target, state, request, hint);
    match plan_request(request, &facts)? {
        MremapPlan::Keep => Ok(old_addr),
        MremapPlan::ShrinkTail { start, end } => {
            unmap_populated_range(target, state, start, end)?;
            Ok(old_addr)
        }
        MremapPlan::GrowInPlace { start, end } => {
            let (prot, reserved) = extension_protection(state, old_end, hint);
            map_extension(target, state, start, end - start, prot, reserved)?;
            state.mmap_next = state.mmap_next.max(end);
            Ok(old_addr)
        }
        MremapPlan::Relocate {
            target: destination,
            replace_destination,
            drop_tail,
            moved_len,
            extension_len,
        } => {
            let total = moved_len + extension_len;
            let destination = match destination {
                Some(destination) => destination,
                None => find_current_free_region(target, state, 0, total)?,
            };
            if !mmap_range_below_ceiling(state, destination, total) {
                return Err(errno::ENOMEM);
            }
            if replace_destination {
                unmap_populated_range(target, state, destination, destination + total)?;
            }
            if let Some((start, end)) = drop_tail {
                unmap_populated_range(target, state, start, end)?;
            }
            let (prot, reserved) = extension_protection(state, old_end, hint);
            // Extension first, in a range nobody else can have claimed since
            // the free-region choice (the lease serializes this process's
            // policy): a refused move can then retire it and change nothing.
            if extension_len != 0 {
                map_extension(
                    target,
                    state,
                    destination + moved_len,
                    extension_len,
                    prot,
                    reserved,
                )?;
            }
            if let Err(errno) = broker_simple(
                target,
                MM_BROKER_OP_MOVE,
                old_addr,
                moved_len,
                0,
                0,
                u64::MAX,
                destination,
            ) {
                if extension_len != 0 {
                    let _ = unmap_populated_range(
                        target,
                        state,
                        destination + moved_len,
                        destination + total,
                    );
                }
                return Err(errno);
            }
            // Commit: the kernel owns the move now, so the records follow it.
            let moved: alloc::vec::Vec<MmVma> = state
                .vmas
                .overlapping(old_addr, old_addr + moved_len)
                .map(|vma| {
                    let mut clipped = vma.clone();
                    clipped.start = vma.start.max(old_addr);
                    clipped.end = vma.end.min(old_addr + moved_len);
                    translated(&clipped, old_addr, destination)
                })
                .collect();
            remove_vma_range(state, old_addr, old_addr + moved_len);
            for vma in moved {
                insert_vma(state, vma);
            }
            state.mmap_next = state.mmap_next.max(destination + total);
            Ok(destination)
        }
    }
}

/// Every byte of `[start, end)` is recorded here or mapped according to ring0.
fn require_mapped(
    target: MmTarget,
    state: &MmPolicyState,
    start: u64,
    end: u64,
) -> Result<(), i32> {
    let mut cursor = start;
    for vma in state.vmas.overlapping(start, end) {
        if vma.start > cursor {
            require_mapped_by_kernel(target, cursor, vma.start)?;
        }
        cursor = cursor.max(vma.end);
    }
    if cursor < end {
        require_mapped_by_kernel(target, cursor, end)?;
    }
    Ok(())
}

/// A gap in the records is acceptable only if ring0 reports the whole gap
/// mapped. The conflict query returns the first mapped span intersecting the
/// probe, so the gap is mapped exactly when that span covers the probe start
/// and advancing over it reaches the gap end.
fn require_mapped_by_kernel(target: MmTarget, start: u64, end: u64) -> Result<(), i32> {
    let mut cursor = start;
    while cursor < end {
        match query_conflict(target, cursor, end - cursor) {
            Ok(Some((span_start, span_end))) if span_start <= cursor && span_end > cursor => {
                cursor = span_end;
            }
            Ok(_) => return Err(errno::EFAULT),
            Err(errno) => return Err(errno),
        }
    }
    Ok(())
}

fn source_facts(
    target: MmTarget,
    state: &MmPolicyState,
    request: &MremapRequest,
    hint: u32,
) -> SourceFacts {
    let old_addr = request.old_addr;
    let old_end = old_addr + request.old_len;
    let mut has_heap = false;
    let mut recorded_anonymous = true;
    for vma in state.vmas.overlapping(old_addr, old_end) {
        match vma.kind {
            MmVmaKind::Heap => has_heap = true,
            MmVmaKind::Anonymous | MmVmaKind::Reserved => {}
            _ => recorded_anonymous = false,
        }
    }
    let unrecorded = !state.vmas.is_covered(old_addr, old_end);
    // Bytes this service never recorded are classified by ring0's own geometry:
    // provably anonymous only when every kernel VMA over the source is private
    // anonymous and none of it is image/stack/legacy memory without a VMA.
    let kernel_vouches =
        hint & MREMAP_SOURCE_ANON_PRIVATE != 0 && hint & MREMAP_SOURCE_HAS_HOLES == 0;
    let anonymous = recorded_anonymous && (!unrecorded || kernel_vouches);

    let extension_free = request.new_len > request.old_len && request.fixed.is_none() && {
        let ext_end = old_addr + request.new_len;
        mmap_range_below_ceiling(state, old_addr, request.new_len)
            && state.vmas.overlapping(old_end, ext_end).next().is_none()
            && matches!(query_conflict(target, old_end, ext_end - old_end), Ok(None))
    };
    SourceFacts {
        has_heap,
        anonymous,
        extension_free,
    }
}

/// Protection (and reservation-ness) an extension inherits from the byte just
/// below it, as in Linux where the grown area belongs to the same VMA. For an
/// unrecorded source, ring0's evidence about the last page stands in.
fn extension_protection(state: &MmPolicyState, old_end: u64, hint: u32) -> (u64, bool) {
    match state.vmas.overlapping(old_end - 1, old_end).next() {
        Some(vma) => (vma.prot, vma.kind == MmVmaKind::Reserved),
        None => {
            let prot = u64::from(hint & 7);
            (prot, prot == 0)
        }
    }
}

fn map_extension(
    target: MmTarget,
    state: &mut MmPolicyState,
    start: u64,
    len: u64,
    prot: u64,
    reserved: bool,
) -> Result<(), i32> {
    if !reserved {
        broker_simple(
            target,
            MM_BROKER_OP_MAP_ANON,
            start,
            len,
            prot,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        )?;
    }
    insert_vma(
        state,
        MmVma {
            start,
            end: start + len,
            prot,
            mmap_flags: MAP_PRIVATE | MAP_ANONYMOUS,
            kind: if reserved {
                MmVmaKind::Reserved
            } else {
                MmVmaKind::Anonymous
            },
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u64 = LINUX_USER_SPACE_BASE + 0x10_0000;
    const P: u64 = PAGE_SIZE;
    const END: u64 = LINUX_USER_SPACE_END;

    fn facts(anonymous: bool, extension_free: bool) -> SourceFacts {
        SourceFacts {
            has_heap: false,
            anonymous,
            extension_free,
        }
    }

    fn req(old: u64, new: u64, may_move: bool, fixed: Option<u64>) -> MremapRequest {
        MremapRequest {
            old_addr: A,
            old_len: old,
            new_len: new,
            may_move,
            fixed,
        }
    }

    #[test]
    fn validation_follows_linux_order() {
        let ok = |o, os, ns, f, n| validate(o, os, ns, f, n, END);
        assert_eq!(ok(A, P, 2 * P, 0, 0).unwrap().new_len, 2 * P);
        assert_eq!(ok(A, 1, 1, 0, 0).unwrap().old_len, P);
        assert_eq!(ok(A + 1, P, P, 0, 0), Err(errno::EINVAL));
        assert_eq!(ok(A, P, 0, 0, 0), Err(errno::EINVAL));
        assert_eq!(ok(A, 0, P, 0, 0), Err(errno::EINVAL));
        assert_eq!(ok(A, P, P, 0x80, 0), Err(errno::EINVAL));
        assert_eq!(ok(A, P, P, MREMAP_FIXED, A + 8 * P), Err(errno::EINVAL));
        assert_eq!(
            ok(A, P, P, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0),
            Err(errno::EINVAL)
        );
        assert_eq!(
            ok(A, P, P, MREMAP_MAYMOVE | MREMAP_FIXED, A + 8 * P + 1),
            Err(errno::EINVAL)
        );
        assert_eq!(
            ok(A, 4 * P, 4 * P, MREMAP_MAYMOVE | MREMAP_FIXED, A + 2 * P),
            Err(errno::EINVAL),
            "destination overlapping the source"
        );
        assert_eq!(
            ok(A, 4 * P, 8 * P, MREMAP_MAYMOVE | MREMAP_FIXED, A - 2 * P),
            Err(errno::EINVAL),
            "the grown destination overlaps the source"
        );
        assert_eq!(
            ok(A, P, P, MREMAP_MAYMOVE | MREMAP_FIXED, A + 8 * P)
                .unwrap()
                .fixed,
            Some(A + 8 * P)
        );
        assert_eq!(ok(END - P, 4 * P, P, 0, 0), Err(errno::EFAULT));
    }

    #[test]
    fn plan_never_loses_a_store_it_cannot_prove() {
        let p = |r: MremapRequest, f: SourceFacts| plan_request(&r, &f);
        assert_eq!(p(req(2 * P, 2 * P, false, None), facts(false, false)), Ok(MremapPlan::Keep));
        assert_eq!(
            p(req(2 * P, P, false, None), facts(false, false)),
            Ok(MremapPlan::ShrinkTail { start: A + P, end: A + 2 * P })
        );
        assert_eq!(
            p(req(2 * P, 3 * P, false, None), facts(true, true)),
            Ok(MremapPlan::GrowInPlace { start: A + 2 * P, end: A + 3 * P })
        );
        assert_eq!(p(req(2 * P, 3 * P, false, None), facts(true, false)), Err(errno::ENOMEM));
        assert_eq!(
            p(req(4 * P, 16 * P, true, None), facts(true, false)),
            Ok(MremapPlan::Relocate {
                target: None,
                replace_destination: false,
                drop_tail: None,
                moved_len: 4 * P,
                extension_len: 12 * P,
            }),
            "a blocked neighbour forces an atomic move, never a copy"
        );
        assert_eq!(p(req(2 * P, 3 * P, true, None), facts(false, true)), Err(errno::ENOMEM));
        assert_eq!(p(req(2 * P, 3 * P, true, None), facts(false, false)), Err(errno::ENOMEM));
        let heap = SourceFacts { has_heap: true, ..facts(true, true) };
        assert_eq!(p(req(2 * P, P, false, None), heap), Err(errno::ENOMEM));
        assert_eq!(p(req(2 * P, 2 * P, false, None), heap), Ok(MremapPlan::Keep));
    }

    #[test]
    fn fixed_replaces_its_destination_and_orders_shrink_before_move() {
        let p = |r: MremapRequest, f: SourceFacts| plan_request(&r, &f);
        let fixed = Some(A + 64 * P);
        assert_eq!(
            p(req(2 * P, 2 * P, true, fixed), facts(false, false)),
            Ok(MremapPlan::Relocate {
                target: fixed,
                replace_destination: true,
                drop_tail: None,
                moved_len: 2 * P,
                extension_len: 0,
            }),
            "a same-size FIXED move works for any movable backing"
        );
        assert_eq!(
            p(req(4 * P, 2 * P, true, fixed), facts(false, false)),
            Ok(MremapPlan::Relocate {
                target: fixed,
                replace_destination: true,
                drop_tail: Some((A + 2 * P, A + 4 * P)),
                moved_len: 2 * P,
                extension_len: 0,
            })
        );
        assert_eq!(
            p(req(2 * P, 5 * P, true, fixed), facts(true, false)),
            Ok(MremapPlan::Relocate {
                target: fixed,
                replace_destination: true,
                drop_tail: None,
                moved_len: 2 * P,
                extension_len: 3 * P,
            })
        );
        assert_eq!(p(req(2 * P, 5 * P, true, fixed), facts(false, false)), Err(errno::ENOMEM));
    }

    #[test]
    fn records_follow_a_move_and_extensions_inherit_protection() {
        let vma = |start, end, prot, kind| MmVma {
            start,
            end,
            prot,
            mmap_flags: MAP_PRIVATE | MAP_ANONYMOUS,
            kind,
        };
        let moved = translated(
            &vma(A + P, A + 3 * P, PROT_READ, MmVmaKind::Anonymous),
            A,
            A + 64 * P,
        );
        assert_eq!((moved.start, moved.end), (A + 65 * P, A + 67 * P));
        assert_eq!(moved.prot, PROT_READ);
        let mut state = mm_state_from_layout(RustosMmLayoutBrokerResult::default());
        insert_vma(
            &mut state,
            vma(A, A + 2 * P, PROT_READ | PROT_WRITE, MmVmaKind::Anonymous),
        );
        assert_eq!(
            extension_protection(&state, A + 2 * P, 0),
            (PROT_READ | PROT_WRITE, false)
        );
        insert_vma(&mut state, vma(A + 2 * P, A + 4 * P, 0, MmVmaKind::Reserved));
        assert_eq!(extension_protection(&state, A + 4 * P, 0), (0, true));
        assert_eq!(extension_protection(&state, A + 9 * P, 3), (3, false));
        assert_eq!(extension_protection(&state, A + 9 * P, 0), (0, true));
    }
}
