//! Hierarchical resident-page charge groups.
//!
//! - **Owner:** `kernel-mm`. The frame-descriptor ledger charges and uncharges
//!   exactly where it changes a bound root's data-leaf count, so a group's usage
//!   is by construction the sum of the counts of the roots bound to it and to its
//!   descendants. `kernel-ps` maps each job to one group and sets limits.
//! - **Boundary:** a group is a small index. A root is bound to at most one group
//!   for its life; binding catches up the pages the root already holds.
//! - **Lifecycle:** groups are created with the job that owns them and stay
//!   valid after it dies until their usage drains to zero, which the job layer
//!   waits for before reusing the slot.
//! - **Concurrency:** lock-free. Claims reserve usage before publication and
//!   cancellation refunds it. The hierarchy is charged speculatively with
//!   rollback on refusal; usage includes in-flight claims.
//! - **Failure:** admission false refuses the allocation like a watermark
//!   denial. Overflow refuses speculative charges.
//! - **Forbidden:** allocating, taking a lock, or sleeping; charging a root that
//!   is not bound; unbinding a root that still owns data leaves.
//! - **Evidence:** `memory-charge-conservation`, ledger unit tests.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Charge groups. Equal to the job slab size; `kernel-ps` asserts the equality.
pub const CHARGE_GROUPS: usize = 64;
/// No limit on a group.
pub const UNLIMITED: u64 = u64::MAX;
const NO_PARENT: u32 = u32::MAX;
/// Deepest chain a walk follows; bounds every operation even on corruption.
const MAX_DEPTH: usize = 9;

struct Group {
    usage: AtomicU64,
    limit: AtomicU64,
    high: AtomicU64,
    min: AtomicU64,
    high_events: AtomicU64,
    parent: AtomicU32,
}

impl Group {
    const fn new() -> Self {
        Self {
            usage: AtomicU64::new(0),
            limit: AtomicU64::new(UNLIMITED),
            high: AtomicU64::new(UNLIMITED),
            min: AtomicU64::new(0),
            high_events: AtomicU64::new(0),
            parent: AtomicU32::new(NO_PARENT),
        }
    }
}

static GROUPS: [Group; CHARGE_GROUPS] = [const { Group::new() }; CHARGE_GROUPS];

fn group(index: usize) -> Option<&'static Group> {
    GROUPS.get(index)
}

/// Visits `index` and each ancestor, root last, at most `MAX_DEPTH` groups.
fn walk(index: usize, mut visit: impl FnMut(&'static Group) -> bool) -> bool {
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(current) = group(cursor) else {
            return true;
        };
        if !visit(current) {
            return false;
        }
        let parent = current.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            return true;
        }
        cursor = parent as usize;
    }
    true
}

/// Makes `index` a fresh group below `parent` with no limit. Its usage is left
/// alone: pages of a previous tenant may still be draining.
pub fn init_group(index: usize, parent: Option<usize>) {
    let Some(cell) = group(index) else { return };
    cell.limit.store(UNLIMITED, Ordering::Relaxed);
    cell.high.store(UNLIMITED, Ordering::Relaxed);
    cell.min.store(0, Ordering::Relaxed);
    cell.high_events.store(0, Ordering::Relaxed);
    cell.parent
        .store(parent.map_or(NO_PARENT, |p| p as u32), Ordering::Relaxed);
}

/// Sets the group's own limit in pages (`UNLIMITED` for none). Lowering it
/// below usage evicts nothing; it only stops admission.
pub fn set_limit(index: usize, pages: u64) {
    if let Some(cell) = group(index) {
        cell.limit.store(pages, Ordering::Relaxed);
    }
}

pub fn limit(index: usize) -> u64 {
    group(index).map_or(UNLIMITED, |cell| cell.limit.load(Ordering::Relaxed))
}

/// The tightest limit on the chain from `index` to the root.
pub fn effective_limit(index: usize) -> u64 {
    let mut effective = UNLIMITED;
    walk(index, |cell| {
        effective = effective.min(cell.limit.load(Ordering::Relaxed));
        true
    });
    effective
}

/// Slow-path job memory QoS policy. A high crossing emits an event without
/// refusing page faults or scheduling a worker in ring0.
pub fn set_qos(index: usize, high: u64, min: u64) {
    if let Some(cell) = group(index) {
        cell.high.store(high, Ordering::Relaxed);
        cell.min.store(min, Ordering::Relaxed);
    }
}

pub fn high(index: usize) -> u64 {
    group(index).map_or(UNLIMITED, |cell| cell.high.load(Ordering::Relaxed))
}

pub fn minimum(index: usize) -> u64 {
    group(index).map_or(0, |cell| cell.min.load(Ordering::Relaxed))
}

pub fn effective_high(index: usize) -> u64 {
    let mut effective = UNLIMITED;
    walk(index, |cell| {
        effective = effective.min(cell.high.load(Ordering::Relaxed));
        true
    });
    effective
}

pub fn high_events(index: usize) -> u64 {
    group(index).map_or(0, |cell| cell.high_events.load(Ordering::Relaxed))
}

/// Pages charged to this group and every descendant.
pub fn usage(index: usize) -> u64 {
    group(index).map_or(0, |cell| cell.usage.load(Ordering::Relaxed))
}

/// Whether `pages` more would fit under every limit on the chain right now.
/// Advisory: concurrent admissions can each pass before any is charged.
pub fn would_admit(index: usize, pages: u64) -> bool {
    walk(index, |cell| {
        let limit = cell.limit.load(Ordering::Relaxed);
        cell.usage
            .load(Ordering::Relaxed)
            .checked_add(pages)
            .is_some_and(|total| limit == UNLIMITED || total <= limit)
    })
}

/// Atomically reserve pages on the whole hierarchy, rolling back on refusal.
pub fn try_charge(index: usize, pages: u64) -> bool {
    if pages == 0 {
        return true;
    }
    let mut visited: [Option<&'static Group>; MAX_DEPTH] = [None; MAX_DEPTH];
    let mut count = 0;
    let mut cursor = index;
    for _ in 0..MAX_DEPTH {
        let Some(cell) = group(cursor) else {
            return true;
        };
        let charged = cell
            .usage
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |usage| {
                let total = usage.checked_add(pages)?;
                (total <= cell.limit.load(Ordering::Relaxed)).then_some(total)
            });
        let Ok(old) = charged else {
            for cell in visited[..count].iter().flatten() {
                cell.usage.fetch_sub(pages, Ordering::Relaxed);
            }
            return false;
        };
        let high = cell.high.load(Ordering::Relaxed);
        if high != UNLIMITED && old <= high && old.saturating_add(pages) > high {
            cell.high_events.fetch_add(1, Ordering::Relaxed);
        }
        visited[count] = Some(cell);
        count += 1;
        let parent = cell.parent.load(Ordering::Relaxed);
        if parent == NO_PARENT {
            return true;
        }
        cursor = parent as usize;
    }
    true
}

/// Charges `pages` to the group and every ancestor.
pub fn charge(index: usize, pages: u64) {
    walk(index, |cell| {
        let old = cell
            .usage
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |usage| {
                usage.checked_add(pages)
            })
            .expect("charge group usage overflow");
        let high = cell.high.load(Ordering::Relaxed);
        if high != UNLIMITED && old <= high && old.saturating_add(pages) > high {
            cell.high_events.fetch_add(1, Ordering::Relaxed);
        }
        true
    });
}

/// Returns `pages` to the group and every ancestor.
pub fn uncharge(index: usize, pages: u64) {
    walk(index, |cell| {
        // Checked update: an over-return leaves the counter untouched rather
        // than wrapped, so a caller that survives the panic (host tests, a
        // contained fault) never sees a huge usage that blocks admission.
        let updated = cell
            .usage
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |usage| {
                usage.checked_sub(pages)
            });
        assert!(updated.is_ok(), "charge group usage underflow");
        true
    });
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    // The groups are process-wide statics, so each test takes its own indices.

    #[test]
    fn charges_and_returns_propagate_to_every_ancestor() {
        init_group(40, None);
        init_group(41, Some(40));
        init_group(42, Some(41));
        charge(42, 5);
        assert_eq!((usage(42), usage(41), usage(40)), (5, 5, 5));
        charge(41, 2);
        assert_eq!((usage(42), usage(41), usage(40)), (5, 7, 7));
        uncharge(42, 5);
        uncharge(41, 2);
        assert_eq!((usage(42), usage(41), usage(40)), (0, 0, 0));
    }

    #[test]
    fn admission_checks_every_limit_on_the_chain() {
        init_group(43, None);
        init_group(44, Some(43));
        set_limit(43, 10);
        set_limit(44, 100);
        assert_eq!(effective_limit(44), 10);
        assert!(would_admit(44, 10));
        assert!(!would_admit(44, 11), "the parent's limit binds the child");
        charge(44, 7);
        assert!(would_admit(44, 3));
        assert!(!would_admit(44, 4));
        uncharge(44, 7);
        set_limit(43, UNLIMITED);
        assert!(would_admit(44, 100));
        assert!(!would_admit(44, 101), "the child's own limit still binds");
    }

    #[test]
    fn lowering_a_limit_below_usage_evicts_nobody_and_only_stops_admission() {
        init_group(45, None);
        charge(45, 8);
        set_limit(45, 3);
        assert_eq!(usage(45), 8);
        assert!(!would_admit(45, 1));
        uncharge(45, 6);
        assert!(would_admit(45, 1));
        uncharge(45, 2);
    }

    #[test]
    fn a_reinitialised_group_drops_its_limit_but_keeps_draining_usage() {
        init_group(46, None);
        set_limit(46, 4);
        charge(46, 3);
        init_group(46, None);
        assert_eq!(limit(46), UNLIMITED);
        assert_eq!(
            usage(46),
            3,
            "pages of the previous tenant still count until freed"
        );
        uncharge(46, 3);
    }

    #[test]
    #[should_panic(expected = "underflow")]
    fn returning_more_than_was_charged_is_a_hard_failure() {
        init_group(47, None);
        uncharge(47, 1);
    }

    #[test]
    fn admission_rejects_usage_overflow_even_without_a_limit() {
        init_group(48, None);
        charge(48, u64::MAX);
        assert!(!would_admit(48, 1));
        let result = std::panic::catch_unwind(|| charge(48, 1));
        assert!(result.is_err());
        assert_eq!(usage(48), u64::MAX);
        uncharge(48, u64::MAX);
    }

    #[test]
    fn out_of_range_groups_are_inert() {
        assert!(would_admit(CHARGE_GROUPS, u64::MAX));
        charge(CHARGE_GROUPS + 5, 1);
        assert_eq!(usage(CHARGE_GROUPS), 0);
    }
}

#[cfg(test)]
mod admission_wiring_tests {
    //! Source witnesses: the allocation sites have no host fake, so these pin
    //! the order that makes a job limit effective without leaking a frame.

    #[test]
    fn every_root_claiming_site_asks_admission_before_it_claims_or_allocates() {
        let mapping = include_str!("address_space/pager_fault_mapping.rs");
        // Formatting is not admission authority. Preserve exact arguments but
        // ignore line wrapping and an optional trailing call comma.
        let compact: alloc::string::String =
            mapping.chars().filter(|c| !c.is_whitespace()).collect();
        let mapping = compact.replace(",)", ")");
        for (claim, what) in [
            (
                "phys::claim_data_leaf(root_phys,virtual_address,frame_phys)",
                "current-root first touch",
            ),
            ("phys::claim_private_file_data_leaf(", "private file"),
        ] {
            let at = mapping
                .find(claim)
                .unwrap_or_else(|| panic!("{what} claim moved"));
            let admit = mapping[..at]
                .rfind("phys::root_admits(root_phys,1)")
                .unwrap_or_else(|| panic!("{what} has no admission before its claim"));
            assert!(
                at - admit < 400,
                "{what}: admission must sit right before the claim"
            );
        }
        let other = mapping
            .split_once("pubfnmap_prepared_pager_fault_frame_at(")
            .unwrap()
            .1;
        // The VMA extent is checked before anything is admitted or claimed, so
        // an out-of-extent request can neither charge nor leak a frame claim.
        assert!(
            other.find("extent.end().as_u64()").unwrap()
                < other.find("phys::root_admits(root_phys,1)").unwrap()
        );
        assert!(
            other.find("phys::root_admits(root_phys,1)").unwrap()
                < other
                    .find("phys::claim_data_leaf(root_phys,virtual_address,frame_phys)")
                    .unwrap()
        );
        let eager = include_str!("address_space.rs");
        let batch = eager
            .find("phys::alloc_frames_batch_user(&mut frame_buffer[..want])")
            .unwrap();
        let admit = eager[..batch]
            .rfind("phys::root_admits(self.pml4_frame_phys, want as u64)")
            .unwrap();
        assert!(
            batch - admit < 400,
            "the batch is admitted before frames are drawn"
        );
    }

    #[test]
    fn a_refusal_is_an_out_of_frames_error_so_callers_unwind_like_a_watermark_denial() {
        let mapping = include_str!("address_space/pager_fault_mapping.rs");
        let sites: alloc::vec::Vec<_> = mapping
            .match_indices("if !phys::root_admits(root_phys, 1) {")
            .collect();
        assert_eq!(
            sites.len(),
            3,
            "first touch, prepared frame and private file"
        );
        for (at, _) in sites {
            let body = &mapping[at..at + 140];
            assert!(
                body.contains("return Err(AddressSpaceError::OutOfFrames);"),
                "a limit refusal must read as out of frames, not as a bad frame"
            );
        }
        let eager = include_str!("address_space.rs");
        let at = eager
            .find("phys::root_admits(self.pml4_frame_phys, want as u64)")
            .unwrap();
        assert!(eager[at..at + 200].contains("return Err(AddressSpaceError::OutOfFrames);"));
    }
}

#[cfg(test)]
mod reservation_concurrency_tests {
    extern crate std;
    use super::*;
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    #[test]
    fn concurrent_reservations_obey_shared_and_ancestor_limits_and_refund() {
        for (parent, first, second) in [(25, 26, 27), (28, 29, 29)] {
            init_group(parent, None);
            init_group(first, Some(parent));
            if second != first {
                init_group(second, Some(parent));
            }
            set_limit(parent, 1);
            let entered = Arc::new(Barrier::new(3));
            let release = Arc::new(Barrier::new(3));
            let mut tasks = std::vec::Vec::new();
            for index in [first, second] {
                let entered = entered.clone();
                let release = release.clone();
                tasks.push(thread::spawn(move || {
                    let admitted = try_charge(index, 1);
                    entered.wait();
                    release.wait();
                    if admitted {
                        uncharge(index, 1);
                    }
                    admitted
                }));
            }
            entered.wait();
            assert_eq!(usage(parent), 1);
            release.wait();
            let winners = tasks
                .into_iter()
                .map(|t| t.join().unwrap())
                .filter(|won| *won)
                .count();
            assert_eq!(winners, 1);
            assert_eq!(usage(parent), 0);
            assert_eq!(usage(first), 0);
            assert_eq!(usage(second), 0);
        }
    }
}
