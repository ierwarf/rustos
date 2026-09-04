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
        TEST_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    fn publication_stamps_exact_process_mm_and_nonzero_vma_generation() {
        let _guard = guard();
        let handle = ProcessHandle::new(29, 43);
        let identity = identity(43, 47);
        let published = publish(handle, identity, template(0x4000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
        let published = publish(handle, identity, template(0x8000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
        assert_eq!(
            publish(handle, identity, template(0x9000), MAX_PAGER_VMAS_PER_PROCESS),
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
        let published = publish(handle, identity, template(0x20_000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
    fn per_process_vma_bound_admits_product_envelope_and_rejects_the_next_region() {
        let _guard = guard();
        let handle = ProcessHandle::new(26, 71);
        let identity = identity(71, 73);
        let mut published = [PagerVmRegionWire::default(); MAX_PAGER_VMAS_PER_PROCESS];
        for (index, slot) in published.iter_mut().enumerate() {
            let start = 0x40_000 + (index as u64) * PAGER_PAGE_BYTES * 2;
            *slot = publish(handle, identity, template(start), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
        let published = publish(handle, exact_identity, template(0xc000), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
    fn target_process_publication_is_generation_bound_and_revocable() {
        let _guard = guard();
        let _isolation = super::super::process_table::tests::isolate_process_table();
        let handle = super::super::process_table::create_process(4_242, process_state())
            .expect("test process handle");
        let identity = super::super::process_table::live_process_identity(handle)
            .expect("test process identity");

        // This unit owns only the lock-free VMA publication protocol.  The
        // normal-time facade prepares real page-table leaves first, while this
        // test deliberately uses an empty host-test address space.
        let published = publish(handle, identity, template(0x1_0000), MAX_PAGER_VMAS_PER_PROCESS).expect("target publication");
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

    fn rule_spans(
        region: PagerVmRegionWire,
        edit: PagerRangeEdit,
    ) -> alloc::vec::Vec<(u64, u64, u64)> {
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
            let published = publish(handle, identity, wide_template(base, 4), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
        let published = publish(handle, identity, wide_template(base, 4), MAX_PAGER_VMAS_PER_PROCESS).unwrap();
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
            published.push(publish(handle, identity, wide_template(start, 3), MAX_PAGER_VMAS_PER_PROCESS).unwrap());
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
