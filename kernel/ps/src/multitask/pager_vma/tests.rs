use super::*;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::UserProcessState;
use rustos_user_abi::pager::{VM_OBJECT_ANONYMOUS, VM_PROT_READ, VM_PROT_WRITE};
use std::sync::Mutex;

/// The publication table is a process-wide bounded array whose free slots
/// are shared by every test in this module. Two tests publishing at once
/// can land on the same slot and make a healthy reader observe the other
/// writer's odd invalidation as `Unstable`, so the suite serializes here.
static TEST_GUARD: Mutex<()> = Mutex::new(());

fn guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Fake handles in this suite name the first process chunk. Standalone
    // witnesses must not depend on another test having grown its side tables.
    assert!(ensure_slot_storage(crate::multitask::slot_dir::SLOT_CHUNK - 1));
    guard
}

fn identity(generation: u32, mm_generation: u32) -> ProcessIdentity {
    ProcessIdentity::from_parts(41, generation, mm_generation)
}

fn template(start: u64) -> PagerVmRegionWire {
    PagerVmRegionWire {
        start,
        end: start + PAGER_PAGE_BYTES * 2,
        object: PagerObjectIdentityWire {
            object_type: VM_OBJECT_ANONYMOUS,
            rights: VM_PROT_READ | VM_PROT_WRITE,
            backing_service: 0,
            slot: 17,
            generation: 19,
            pager_epoch: 23,
            backing_generation: 29,
            ..PagerObjectIdentityWire::default()
        },
        object_offset: 0,
        prot: VM_PROT_READ | VM_PROT_WRITE,
        sharing: VM_SHARING_PRIVATE,
        commit_state: rustos_user_abi::pager::VM_COMMIT_COMMITTED,
        fault_endpoint: PagerEndpointCapabilityWire {
            slot: 31,
            generation: 37,
            rights: 1,
        },
        ..PagerVmRegionWire::default()
    }
}

fn process_state() -> UserProcessState {
    UserProcessState::new(
        ProcessAddressSpace::empty_for_tests(),
        None,
        None,
        None,
        None,
        false,
        "/pager-vma-test.elf",
    )
}

#[test]
fn extent_prefilter_cannot_reject_from_bounds_republished_during_its_read() {
    let _guard = guard();
    let slot = PublishedPagerVma::empty();
    let address = 0x8_000;
    assert!(!slot.may_cover(address));
    assert!(!slot.may_overlap(address, address + PAGER_PAGE_BYTES));
    assert!(!slot.may_overlap_or_touch(address, address + PAGER_PAGE_BYTES));

    // Deterministic interleaving: the reader sampled an empty *even* slot;
    // the writer committed the VMA before the reader checked its sequence.
    // Old one-sided filters concluded the address was absent and returned
    // Stale despite the later full snapshot being able to see this region.
    let before = slot.sequence.load(Ordering::Acquire);
    let old_start = slot.start.load(Ordering::Relaxed);
    let old_end = slot.end.load(Ordering::Relaxed);
    slot.publish(Some(template(address))).unwrap();
    let after = slot.sequence.load(Ordering::Acquire);
    assert_ne!(before, after);
    assert_eq!(
        PublishedPagerVma::stable_extent_from_samples(before, after, old_start, old_end),
        None,
        "stale bounds may not reject a concurrently published VMA"
    );
    assert!(slot.may_cover(address));
    assert!(slot.may_overlap(address, address + PAGER_PAGE_BYTES));
    assert!(slot.may_overlap_or_touch(address, address + PAGER_PAGE_BYTES));
    assert!(!slot.may_cover(address + PAGER_PAGE_BYTES * 2));
}

#[test]
fn actual_range_rewrite_gap_is_retryable_until_commit_or_original_rollback() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 91);
    let identity = identity(91, 97);
    for refuse_mutation in [false, true] {
        let region = publish(
            handle,
            identity,
            template(0x30_000),
            MAX_PAGER_VMAS_PER_PROCESS,
        )
        .unwrap();
        let untouched_page = region.start + PAGER_PAGE_BYTES;
        let mut during = None;
        let result = rewrite_attenuated_range(
            handle,
            identity,
            region.start,
            untouched_page,
            Some(0),
            || {
                // Execute the production lookup at the real withdrawal/PTE
                // callback boundary. This page is OUTSIDE the edited range;
                // its original VMA is temporarily absent, not unmapped.
                during = Some(lookup(handle, identity, untouched_page, VM_ACCESS_WRITE));
                if refuse_mutation {
                    Err(PagerVmaError::Pressure)
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(
            result,
            if refuse_mutation {
                Err(PagerVmaError::Pressure)
            } else {
                Ok(true)
            }
        );
        let retained = lookup(handle, identity, untouched_page, VM_ACCESS_WRITE).unwrap();
        assert_eq!(retained.vma_generation, region.vma_generation);
        assert_eq!(
            lookup(handle, identity, region.start, VM_ACCESS_WRITE).is_ok(),
            refuse_mutation
        );
        for live in snapshot_regions(handle, identity).unwrap() {
            revoke(handle, identity, live.start, live.vma_generation).unwrap();
        }
        assert_eq!(
            during,
            Some(Err(PagerVmaError::Unstable)),
            "a temporary publication hole must not terminate a valid first touch"
        );
        assert_eq!(
            lookup(handle, identity, untouched_page, VM_ACCESS_WRITE),
            Err(PagerVmaError::Stale),
            "a committed real unmap must still fail"
        );
    }
}

#[test]
fn actual_lookup_rechecks_whole_rewrite_epoch_after_the_scan() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 93);
    let identity = identity(93, 99);
    let original = publish(
        handle,
        identity,
        template(0x34_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    for old_result in [
        Ok(original),
        Err(PagerVmaError::Stale),
        Err(PagerVmaError::Denied),
    ] {
        let observed = read_rewrite_epoch(handle, || {
            // The same production read envelope is used around lookup_slot's
            // entire scan. Complete a real range edit between its two samples.
            assert_eq!(
                rewrite_attenuated_range(
                    handle,
                    identity,
                    original.start,
                    original.end,
                    Some(VM_PROT_READ | VM_PROT_WRITE),
                    || Ok(())
                ),
                Ok(true)
            );
            old_result
        });
        assert_eq!(observed, Err(PagerVmaError::Unstable));
        assert_eq!(
            lookup(handle, identity, original.start, VM_ACCESS_WRITE),
            Ok(original)
        );
    }
    revoke(handle, identity, original.start, original.vma_generation).unwrap();
}

#[test]
fn actual_exhausted_range_rewrite_epoch_refuses_before_withdrawal_or_pte_mutation() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 95);
    let identity = identity(95, 101);
    let original = publish(
        handle,
        identity,
        template(0x38_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    // Native exhausted-counter input only, not a production reset/reuse path.
    let sequence = &rewrite_epoch::REWRITE_EPOCHS[handle.index()];
    let before = sequence.swap(u64::MAX - 1, Ordering::AcqRel);
    let mut mutated = false;
    let result = rewrite_attenuated_range(
        handle,
        identity,
        original.start,
        original.end,
        Some(0),
        || {
            mutated = true;
            Ok(())
        },
    );
    sequence.store(before, Ordering::Release);
    assert_eq!(result, Err(PagerVmaError::Pressure));
    assert!(!mutated);
    assert_eq!(
        lookup(handle, identity, original.start, VM_ACCESS_WRITE),
        Ok(original)
    );
    revoke(handle, identity, original.start, original.vma_generation).unwrap();
}

#[test]
fn publication_stamps_exact_process_mm_and_nonzero_vma_generation() {
    let _guard = guard();
    let handle = ProcessHandle::new(29, 43);
    let identity = identity(43, 47);
    let published = publish(
        handle,
        identity,
        template(0x4000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    assert_eq!(published.process_handle, 30);
    assert_eq!(published.process_generation, 43);
    assert_eq!(published.mm_generation, 47);
    assert_ne!(published.vma_generation, 0);
    assert_eq!(
        lookup(handle, identity, 0x5000, VM_ACCESS_WRITE),
        Ok(published)
    );
    revoke(handle, identity, published.start, published.vma_generation).unwrap();
}

#[test]
fn overlap_and_permission_escalation_fail_closed() {
    let _guard = guard();
    let handle = ProcessHandle::new(28, 53);
    let identity = identity(53, 59);
    let published = publish(
        handle,
        identity,
        template(0x8000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    assert_eq!(
        publish(
            handle,
            identity,
            template(0x9000),
            MAX_PAGER_VMAS_PER_PROCESS
        ),
        Err(PagerVmaError::Overlap)
    );
    assert_eq!(
        lookup(handle, identity, 0x8000, VM_ACCESS_EXECUTE),
        Err(PagerVmaError::Denied)
    );
    revoke(handle, identity, published.start, published.vma_generation).unwrap();
}

#[test]
fn protection_attenuation_and_unmap_rewrite_before_mutation() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 61);
    let identity = identity(61, 67);
    let published = publish(
        handle,
        identity,
        template(0x20_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    let mut protection_mutated = false;
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            published.start,
            published.start + PAGER_PAGE_BYTES,
            Some(0),
            || {
                protection_mutated = true;
                Ok(())
            },
        ),
        Ok(true)
    );
    assert!(protection_mutated);
    assert_eq!(
        lookup(handle, identity, published.start, VM_ACCESS_READ),
        Err(PagerVmaError::Denied)
    );
    let right = lookup(
        handle,
        identity,
        published.start + PAGER_PAGE_BYTES,
        VM_ACCESS_WRITE,
    )
    .unwrap();
    assert_eq!(right.vma_generation, published.vma_generation);
    assert_eq!(right.object_offset, PAGER_PAGE_BYTES);
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            right.start,
            right.end,
            Some(VM_PROT_READ | VM_PROT_WRITE | VM_PROT_EXECUTE),
            || Ok(()),
        ),
        Err(PagerVmaError::Denied)
    );

    let mut unmapped = false;
    assert_eq!(
        rewrite_attenuated_range(handle, identity, right.start, right.end, None, || {
            unmapped = true;
            Ok(())
        }),
        Ok(true)
    );
    assert!(unmapped);
    assert_eq!(
        lookup(handle, identity, right.start, VM_ACCESS_READ),
        Err(PagerVmaError::Stale)
    );
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            published.start,
            published.start + PAGER_PAGE_BYTES,
            None,
            || Ok(()),
        ),
        Ok(true)
    );
}

#[test]
fn sparse_unmap_is_idempotent_and_protect_commit_still_require_coverage() {
    let _guard = guard();
    let handle = ProcessHandle::new(25, 201);
    let identity = identity(201, 301);
    let mut region = template(0x40_000);
    region.end = region.start + 3 * PAGER_PAGE_BYTES;
    let region = publish(handle, identity, region, MAX_PAGER_VMAS_PER_PROCESS).unwrap();
    let hole_start = region.start + PAGER_PAGE_BYTES;
    let hole_end = hole_start + PAGER_PAGE_BYTES;
    assert_eq!(
        rewrite_attenuated_range(handle, identity, hole_start, hole_end, None, || Ok(())),
        Ok(true)
    );
    assert_eq!(
        rewrite_attenuated_range(handle, identity, hole_start, hole_end, None, || panic!(
            "touching neighbors do not authorize mutation of the hole"
        )),
        Ok(false)
    );
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            region.start,
            region.end,
            Some(VM_PROT_READ),
            || panic!("protection must reject the gap before mutation")
        ),
        Err(PagerVmaError::Stale)
    );
    assert_eq!(
        rewrite_range(
            handle,
            identity,
            region.start,
            region.end,
            None,
            Some(VM_COMMIT_RESERVED),
            || panic!("commit state still requires complete coverage")
        ),
        Err(PagerVmaError::Stale)
    );
    for address in [region.start, hole_end] {
        assert_eq!(
            lookup(handle, identity, address, VM_ACCESS_WRITE)
                .unwrap()
                .prot,
            VM_PROT_READ | VM_PROT_WRITE
        );
    }
    let mut mutations = 0;
    assert_eq!(
        rewrite_attenuated_range(handle, identity, region.start, region.end, None, || {
            mutations += 1;
            Ok(())
        }),
        Ok(true)
    );
    assert_eq!(
        mutations, 1,
        "one whole-range preflight/commit, not partial segment mutations"
    );
    for address in [region.start, hole_start, hole_end] {
        assert_eq!(
            lookup(handle, identity, address, VM_ACCESS_READ),
            Err(PagerVmaError::Stale)
        );
    }
}

#[test]
fn unmap_partially_initialized_range_and_touching_only_preserve_exact_authority() {
    let _guard = guard();
    let handle = ProcessHandle::new(25, 202);
    let identity = identity(202, 302);
    let mut region = template(0x48_000);
    region.end = region.start + PAGER_PAGE_BYTES;
    let region = publish(handle, identity, region, MAX_PAGER_VMAS_PER_PROCESS).unwrap();
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            region.end,
            region.end + PAGER_PAGE_BYTES,
            None,
            || panic!("an adjacent range is not an intersection")
        ),
        Ok(false)
    );
    assert_eq!(
        lookup(handle, identity, region.start, VM_ACCESS_WRITE).unwrap(),
        region
    );
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            region.start,
            region.end + 2 * PAGER_PAGE_BYTES,
            None,
            || Ok(())
        ),
        Ok(true)
    );
    assert_eq!(
        lookup(handle, identity, region.start, VM_ACCESS_READ),
        Err(PagerVmaError::Stale)
    );
}

#[test]
fn reservation_commit_state_splits_and_gates_fault_access() {
    let _guard = guard();
    let handle = ProcessHandle::new(28, 62);
    let identity = identity(62, 68);
    let published = publish(
        handle,
        identity,
        template(0x24_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    let first_end = published.start + PAGER_PAGE_BYTES;

    assert_eq!(
        rewrite_range(
            handle,
            identity,
            published.start,
            first_end,
            None,
            Some(VM_COMMIT_RESERVED),
            || Ok(()),
        ),
        Ok(true)
    );
    assert_eq!(
        lookup(handle, identity, published.start, VM_ACCESS_READ),
        Err(PagerVmaError::Denied)
    );
    assert!(lookup(handle, identity, first_end, VM_ACCESS_WRITE).is_ok());

    assert_eq!(
        rewrite_range(
            handle,
            identity,
            published.start,
            first_end,
            None,
            Some(VM_COMMIT_COMMITTED),
            || Ok(()),
        ),
        Ok(true)
    );
    assert!(lookup(handle, identity, published.start, VM_ACCESS_READ).is_ok());
    assert_eq!(published_spans(handle, identity).len(), 1);

    for (start, end, _) in published_spans(handle, identity) {
        assert_eq!(
            rewrite_attenuated_range(handle, identity, start, end, None, || Ok(())),
            Ok(true)
        );
    }
}

#[test]
fn prot_none_reserved_and_committed_regions_publish_but_never_admit_access() {
    let _guard = guard();
    let handle = ProcessHandle::new(29, 63);
    let identity = identity(63, 69);
    for (index, commit_state) in [VM_COMMIT_RESERVED, VM_COMMIT_COMMITTED]
        .into_iter()
        .enumerate()
    {
        let mut guard_region = template(0x2a_000 + index as u64 * 0x20_000);
        guard_region.prot = 0;
        guard_region.commit_state = commit_state;
        let published = publish(handle, identity, guard_region, MAX_PAGER_VMAS_PER_PROCESS)
            .expect("PROT_NONE is still an authoritative VMA");
        assert_eq!(
            lookup(handle, identity, published.start, VM_ACCESS_READ),
            Err(PagerVmaError::Denied)
        );
        assert_eq!(
            lookup(handle, identity, published.start, VM_ACCESS_WRITE),
            Err(PagerVmaError::Denied)
        );
        revoke(handle, identity, published.start, published.vma_generation).unwrap();
    }
}

#[test]
fn per_process_vma_bound_admits_product_envelope_and_rejects_the_next_region() {
    let _guard = guard();
    let handle = ProcessHandle::new(26, 71);
    let identity = identity(71, 73);
    let mut published = [PagerVmRegionWire::default(); MAX_PAGER_VMAS_PER_PROCESS];
    for (index, slot) in published.iter_mut().enumerate() {
        let start = 0x40_000 + (index as u64) * PAGER_PAGE_BYTES * 2;
        *slot = publish(
            handle,
            identity,
            template(start),
            MAX_PAGER_VMAS_PER_PROCESS,
        )
        .unwrap();
    }
    assert_eq!(
        publish(
            handle,
            identity,
            template(0x40_000 + (MAX_PAGER_VMAS_PER_PROCESS as u64) * PAGER_PAGE_BYTES * 2),
            MAX_PAGER_VMAS_PER_PROCESS,
        ),
        Err(PagerVmaError::Pressure)
    );
    for region in published {
        revoke(handle, identity, region.start, region.vma_generation).unwrap();
    }
}

#[test]
fn exec_generation_change_and_revoked_region_never_match() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 61);
    let exact_identity = identity(61, 67);
    let published = publish(
        handle,
        exact_identity,
        template(0xc000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    assert_eq!(
        lookup(handle, identity(61, 68), 0xc000, VM_ACCESS_READ),
        Err(PagerVmaError::Stale)
    );
    assert_eq!(
        revoke(
            handle,
            exact_identity,
            published.start,
            published.vma_generation + 1,
        ),
        Err(PagerVmaError::Stale)
    );
    assert_eq!(
        lookup(handle, exact_identity, 0xc000, VM_ACCESS_READ),
        Ok(published)
    );
    revoke(
        handle,
        exact_identity,
        published.start,
        published.vma_generation,
    )
    .unwrap();
    assert_eq!(
        lookup(handle, exact_identity, 0xc000, VM_ACCESS_READ),
        Err(PagerVmaError::Stale)
    );
}

/// Fork inherits the reservation, so it needs every region at once - and
/// must not pick up residue a previous occupant of these slots left, which
/// process-slot reclamation does not purge.
/// The ceiling is ring3 policy that this table only enforces. It may
/// narrow the fixed array, never widen it.
#[test]
fn a_published_ceiling_narrows_the_fixed_table_and_cannot_widen_it() {
    let _guard = guard();
    let handle = ProcessHandle::new(25, 79);
    let identity = identity(79, 83);
    let first = publish(handle, identity, template(0x60_000), 2).unwrap();
    let second = publish(handle, identity, template(0x70_000), 2).unwrap();
    // Third region refused by policy, not by a full array.
    assert_eq!(
        publish(handle, identity, template(0x80_000), 2),
        Err(PagerVmaError::Pressure)
    );
    // A ceiling past the fixed array is clamped, not honoured, so a caller
    // cannot publish into storage that does not exist.
    assert_eq!(
        publish(
            handle,
            identity,
            template(0x80_000),
            MAX_PAGER_VMAS_PER_PROCESS + 4096,
        )
        .map(|region| region.start),
        Ok(0x80_000)
    );
    let third = lookup(handle, identity, 0x80_000, VM_ACCESS_READ).unwrap();
    // A zero ceiling admits nothing.
    assert_eq!(
        publish(handle, identity, template(0x90_000), 0),
        Err(PagerVmaError::Pressure)
    );
    for region in [first, second, third] {
        revoke(handle, identity, region.start, region.vma_generation).unwrap();
    }
}

#[test]
fn a_reservation_snapshot_returns_every_live_region_and_no_stale_residue() {
    let _guard = guard();
    let handle = ProcessHandle::new(26, 71);
    let live = identity(71, 73);
    let first = publish(handle, live, template(0x40_000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
    let second = publish(handle, live, template(0x50_000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();

    let mut regions = snapshot_regions(handle, live).expect("reservation snapshot");
    regions.sort_by_key(|region| region.start);
    assert_eq!(regions, alloc::vec![first, second]);

    // Same slots, moved-on MM generation: nothing is inheritable.
    assert_eq!(snapshot_regions(handle, identity(71, 74)), Ok(Vec::new()));
    // A handle whose generation disagrees with the identity is refused
    // rather than answered from the slot contents.
    assert_eq!(
        snapshot_regions(ProcessHandle::new(26, 72), live),
        Err(PagerVmaError::Stale)
    );

    revoke(handle, live, first.start, first.vma_generation).unwrap();
    assert_eq!(
        snapshot_regions(handle, live).map(|regions| regions.len()),
        Ok(1)
    );
    revoke(handle, live, second.start, second.vma_generation).unwrap();
    assert_eq!(snapshot_regions(handle, live), Ok(Vec::new()));
}

#[test]
fn fork_hold_makes_fault_readers_retry_without_publishing_a_hole() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 81);
    let live = identity(81, 83);
    let region = publish(handle, live, template(0x60_000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();

    let held = with_fork_held_regions(handle, live, |regions| {
        assert_eq!(regions, &[region]);
        assert_eq!(
            lookup(handle, live, region.start, VM_ACCESS_READ),
            Err(PagerVmaError::Unstable)
        );
        regions.len()
    });
    assert_eq!(held, Ok(1));
    assert_eq!(
        lookup(handle, live, region.start, VM_ACCESS_READ),
        Ok(region)
    );
    revoke(handle, live, region.start, region.vma_generation).unwrap();
}

#[test]
fn old_image_mapping_plan_cannot_publish_over_replacement_stack_guard() {
    use rustos_user_abi::linux::{LinuxMemoryMapState, LinuxProcessState, LinuxRuntimeProfile};
    let _guard = guard();
    let _isolation = super::super::process_table::tests::isolate_process_table();
    let pid = 4_241;
    let old_guard = rustos_user_abi::syscall::PROC_BROKER_USER_SPACE_END_EXCLUSIVE - (128 << 20);
    let new_guard = old_guard - (128 << 20);
    let state = UserProcessState::new(
        ProcessAddressSpace::empty_for_tests(),
        Some(LinuxProcessState {
            mapping_limit: old_guard,
            ..LinuxProcessState::default()
        }),
        None,
        None,
        None,
        false,
        "/old-image.elf",
    );
    let handle = super::super::process_table::create_process(pid, state).unwrap();
    let retained = super::super::process_table::retain_process(handle).unwrap();
    let plan = template(new_guard);
    // Pause a sibling's service-side request after old-image policy admission.
    assert!(
        retained.with_state_mut(|_, state| state.mapping_below_stack_guard(plan.start, plan.end))
    );
    retained.with_state_mut(|_, state| {
        let _retired = state.replace_for_exec(
            ProcessAddressSpace::empty_for_tests(),
            LinuxProcessState {
                mapping_limit: new_guard,
                ..LinuxProcessState::default()
            },
            LinuxMemoryMapState::default(),
            LinuxRuntimeProfile::default(),
            "/replacement.elf",
        );
    });
    // Resume the old plan against actual VMA admission. It must refuse before
    // even inspecting the replacement page tables, let alone publishing.
    assert_eq!(
        super::super::publish_pager_vma_for_process(pid, plan, MAX_PAGER_VMAS_PER_PROCESS),
        Err(PagerVmaError::Denied),
    );
    assert!(
        !retained.with_state_mut(|_, state| state.mapping_below_stack_guard(plan.start, plan.end))
    );
}

#[test]
fn target_process_publication_is_generation_bound_and_revocable() {
    let _guard = guard();
    let _isolation = super::super::process_table::tests::isolate_process_table();
    let handle = super::super::process_table::create_process(4_242, process_state())
        .expect("test process handle");
    let identity =
        super::super::process_table::live_process_identity(handle).expect("test process identity");

    // This unit owns only the lock-free VMA publication protocol.  The
    // normal-time facade prepares real page-table leaves first, while this
    // test deliberately uses an empty host-test address space.
    let published = publish(
        handle,
        identity,
        template(0x1_0000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .expect("target publication");
    assert_eq!(
        published.process_generation,
        u64::from(identity.process_generation())
    );
    assert_eq!(published.mm_generation, u64::from(identity.mm_generation()));
    assert_eq!(
        lookup(handle, identity, 0x1_0000, VM_ACCESS_READ),
        Ok(published)
    );
    assert_eq!(
        super::super::revoke_pager_vma_for_process(
            4_242,
            published.start,
            published.vma_generation,
        ),
        Ok(published)
    );
    assert_eq!(
        lookup(handle, identity, 0x1_0000, VM_ACCESS_READ),
        Err(PagerVmaError::Stale)
    );
}

#[test]
fn publication_reclaims_stale_process_generation_before_slot_reuse() {
    let _guard = guard();
    let _isolation = super::super::process_table::tests::isolate_process_table();
    let stale = super::super::process_table::create_process(5_001, process_state())
        .expect("stale process handle");
    let stale_identity =
        super::super::process_table::live_process_identity(stale).expect("stale process identity");
    publish(
        stale,
        stale_identity,
        template(0x2_0000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .expect("stale generation publication");
    super::super::process_table::detach_task(stale).expect("retire stale process");
    assert_eq!(super::super::process_table::reap_exited_processes(), 1);

    let replacement = super::super::process_table::create_process(5_002, process_state())
        .expect("replacement process handle");
    assert_eq!(replacement.index(), stale.index());
    assert_ne!(replacement.generation(), stale.generation());
    let replacement_identity = super::super::process_table::live_process_identity(replacement)
        .expect("replacement process identity");
    let published = publish(
        replacement,
        replacement_identity,
        template(0x2_0000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .expect("replacement must reclaim stale VMA generation");
    let occupied: Vec<_> = process_slots(replacement)
        .expect("replacement publication slots")
        .iter()
        .filter_map(|slot| slot.snapshot().expect("stable publication slot"))
        .collect();
    assert_eq!(
        occupied.len(),
        1,
        "stale generations must not consume capacity"
    );
    assert_eq!(occupied[0], published);
    assert_eq!(
        published.process_generation,
        replacement.generation() as u64
    );
    revoke(
        replacement,
        replacement_identity,
        published.start,
        published.vma_generation,
    )
    .expect("replacement VMA cleanup");
    super::super::process_table::detach_task(replacement).expect("retire replacement process");
    assert_eq!(super::super::process_table::reap_exited_processes(), 1);
}

/// A wide template, so an edit can land strictly inside it and split.
fn wide_template(start: u64, pages: u64) -> PagerVmRegionWire {
    PagerVmRegionWire {
        end: start + PAGER_PAGE_BYTES * pages,
        ..template(start)
    }
}

fn published_spans(
    handle: ProcessHandle,
    identity: ProcessIdentity,
) -> alloc::vec::Vec<(u64, u64, u64)> {
    let mut spans: alloc::vec::Vec<(u64, u64, u64)> = process_slots(handle)
        .unwrap()
        .iter()
        .filter_map(|slot| slot.snapshot().unwrap())
        .filter(|region| {
            region.process_generation == u64::from(identity.process_generation())
                && region.mm_generation == u64::from(identity.mm_generation())
        })
        .map(|region| (region.start, region.end, region.object_offset))
        .collect();
    spans.sort_unstable();
    spans
}

fn rule_spans(region: PagerVmRegionWire, edit: PagerRangeEdit) -> alloc::vec::Vec<(u64, u64, u64)> {
    let (fragments, len) = apply_region_edit(region, edit).fragments();
    let mut spans: alloc::vec::Vec<(u64, u64, u64)> = fragments[..len]
        .iter()
        .map(|fragment| (fragment.start, fragment.end, fragment.object_offset))
        .collect();
    spans.sort_unstable();
    spans
}

/// Ring0's rewrite must equal the shared ABI rule exactly.
///
/// This is one half of the replica binding: pagerd is tested against the
/// same rule, so proving each side equals it proves the two sides agree.
/// They previously derived their own remainders, and an interior `munmap`
/// left ring0 holding two mappings that pagerd had deleted - so the next
/// fault in a surviving remainder passed ring0's VMA check, matched no
/// pagerd region, and killed the thread.
#[test]
fn ring0_rewrite_matches_the_shared_range_edit_rule() {
    let _guard = guard();
    let handle = ProcessHandle::new(25, 79);
    let identity = identity(79, 83);
    let base = 0x8_0000;
    for (edit_start, edit_end) in [
        (base, base + PAGER_PAGE_BYTES), // trim head
        (base + PAGER_PAGE_BYTES * 3, base + PAGER_PAGE_BYTES * 4), // trim tail
        (base + PAGER_PAGE_BYTES, base + PAGER_PAGE_BYTES * 2), // interior split
        (base, base + PAGER_PAGE_BYTES * 4), // full remove
    ] {
        let published = publish(
            handle,
            identity,
            wide_template(base, 4),
            MAX_PAGER_VMAS_PER_PROCESS,
        )
        .unwrap();
        assert_eq!(
            rewrite_attenuated_range(handle, identity, edit_start, edit_end, None, || Ok(())),
            Ok(true)
        );
        assert_eq!(
            published_spans(handle, identity),
            rule_spans(published, PagerRangeEdit::unmap(edit_start, edit_end)),
            "unmap {edit_start:#x}..{edit_end:#x} must equal the shared rule"
        );
        for (start, end, _) in published_spans(handle, identity) {
            assert_eq!(
                rewrite_attenuated_range(handle, identity, start, end, None, || Ok(())),
                Ok(true)
            );
        }
        assert!(published_spans(handle, identity).is_empty());
    }
}

/// An interior unmap costs exactly one extra VMA slot, and no more - the
/// capacity claim `PAGER_MAX_REGION_GROWTH_PER_UNMAP` states and that
/// pagerd's table is sized against.
#[test]
fn an_interior_unmap_costs_exactly_one_extra_vma_slot() {
    let _guard = guard();
    let handle = ProcessHandle::new(24, 89);
    let identity = identity(89, 97);
    let base = 0x9_0000;
    let published = publish(
        handle,
        identity,
        wide_template(base, 4),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    let before = published_spans(handle, identity).len();
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            base + PAGER_PAGE_BYTES,
            base + PAGER_PAGE_BYTES * 2,
            None,
            || Ok(()),
        ),
        Ok(true)
    );
    let after = published_spans(handle, identity).len();
    assert_eq!(after - before, PAGER_MAX_REGION_GROWTH_PER_UNMAP);
    assert!(after - before <= PAGER_MAX_REGION_GROWTH_PER_PROTECT);
    // Every surviving page still faults, and the removed one does not.
    assert!(lookup(handle, identity, base, VM_ACCESS_WRITE).is_ok());
    assert!(
        lookup(
            handle,
            identity,
            base + PAGER_PAGE_BYTES * 2,
            VM_ACCESS_WRITE
        )
        .is_ok()
    );
    assert_eq!(
        lookup(handle, identity, base + PAGER_PAGE_BYTES, VM_ACCESS_WRITE),
        Err(PagerVmaError::Stale)
    );
    let _ = published;
    for (start, end, _) in published_spans(handle, identity) {
        assert_eq!(
            rewrite_attenuated_range(handle, identity, start, end, None, || Ok(())),
            Ok(true)
        );
    }
}

/// A split with no free VMA slot must refuse before it withdraws anything.
/// Withdrawing first and failing to republish would lose a live mapping.
#[test]
fn a_split_with_no_free_vma_slot_refuses_and_keeps_every_region() {
    let _guard = guard();
    let handle = ProcessHandle::new(23, 101);
    let identity = identity(101, 103);
    let base = 0xa_0000;
    let mut published = alloc::vec::Vec::new();
    for index in 0..MAX_PAGER_VMAS_PER_PROCESS as u64 {
        let start = base + index * PAGER_PAGE_BYTES * 4;
        published.push(
            publish(
                handle,
                identity,
                wide_template(start, 3),
                MAX_PAGER_VMAS_PER_PROCESS,
            )
            .unwrap(),
        );
    }
    let before = published_spans(handle, identity);
    assert_eq!(before.len(), MAX_PAGER_VMAS_PER_PROCESS);
    // Every slot is taken, so an interior split has nowhere to put its
    // second fragment. It must refuse with `Pressure` and leave the whole
    // table exactly as it was; the mutation closure must never run.
    let mut mutated = false;
    assert_eq!(
        rewrite_attenuated_range(
            handle,
            identity,
            base + PAGER_PAGE_BYTES,
            base + PAGER_PAGE_BYTES * 2,
            None,
            || {
                mutated = true;
                Ok(())
            },
        ),
        Err(PagerVmaError::Pressure)
    );
    assert!(!mutated, "no PTE may change before the split is admitted");
    assert_eq!(published_spans(handle, identity), before);
    for region in published {
        revoke(handle, identity, region.start, region.vma_generation).unwrap();
    }
}

#[test]
fn rewrite_reclaims_previous_exec_incarnation_instead_of_mutating_its_range() {
    let _guard = guard();
    let handle = ProcessHandle::new(27, 101);
    let before_exec = identity(101, 103);
    let after_exec = identity(101, 104);
    let stale = publish(
        handle,
        before_exec,
        template(0x50_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();

    // The new image maps the same addresses again. The old incarnation's region
    // has no authority over them, so unmapping must neither run the PTE
    // mutation for it nor report the range as a pager-managed edit.
    let mut mutated = false;
    let result = rewrite_attenuated_range(handle, after_exec, stale.start, stale.end, None, || {
        mutated = true;
        Err(PagerVmaError::Stale)
    });
    assert_eq!(result, Ok(false));
    assert!(
        !mutated,
        "a previous incarnation's region must not drive the mutation"
    );

    // The residue was reclaimed, so the new incarnation can publish the range.
    let fresh = publish(
        handle,
        after_exec,
        template(0x50_000),
        MAX_PAGER_VMAS_PER_PROCESS,
    )
    .unwrap();
    assert_ne!(fresh.mm_generation, stale.mm_generation);
    revoke(handle, after_exec, fresh.start, fresh.vma_generation).unwrap();
}
