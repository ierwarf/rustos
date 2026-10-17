use super::super::tests::admit;
use super::super::{close_for_reclaim, pin_view, take_storage};
use super::*;

fn release(bank: &CpuLeases, slots: &[usize]) {
    drop(CpuLeases::take(&bank.prepared));
    drop(CpuLeases::take(&bank.outgoing));
    drop(CpuLeases::take(&bank.current));
    for &slot in slots {
        assert!(close_for_reclaim(slot));
        drop(take_storage(slot));
    }
}

#[test]
fn claim_to_assembly_commit_retains_both_allocations() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let (from, to) = (90, 91);
    let old = admit(from, 94001);
    let next = admit(to, 94002);
    let bank = CpuLeases::new();
    bank.initialize(pin_view(from, 94001, old).unwrap());
    bank.prepare(pin_view(to, 94002, next).unwrap());
    assert!(bank.owns_current(from));
    assert!(
        !bank.owns_current(to),
        "prepared entry is not executing Current"
    );
    // Reaper absence observations cannot release either descriptor/allocation
    // during the claim-to-Current publication gap or the old-RSP interval.
    assert!(!close_for_reclaim(from));
    assert!(!close_for_reclaim(to));
    bank.publish(from, to);
    assert_eq!(bank.current.load(Ordering::Acquire), to + 1);
    assert!(bank.owns_current(to));
    assert!(
        !bank.owns_current(from),
        "outgoing stack custody is not current entry authority"
    );
    assert_eq!(bank.outgoing.load(Ordering::Acquire), from + 1);
    assert!(!close_for_reclaim(from));
    assert!(!close_for_reclaim(to));
    bank.commit();
    assert!(close_for_reclaim(from));
    assert!(!close_for_reclaim(to));
    bank.commit(); // no repeated release, and Current remains pinned
    assert!(bank.owns_current(to));
    assert!(!bank.owns_current(from));
    assert!(!close_for_reclaim(to));
    release(&bank, &[from, to]);
}

#[test]
fn simd_replacement_requires_exclusive_current_or_terminal_custody() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 98;
    let view = admit(slot, 98701);
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    let bank = &CPU_LEASES[cpu];
    let remote = &CPU_LEASES[(cpu + 1) % CPU_LEASES.len()];
    assert_eq!(bank.current.load(Ordering::Acquire), 0);
    assert_eq!(remote.current.load(Ordering::Acquire), 0);
    assert!(!simd_unpublished_custody(slot));
    assert_eq!(simd_reset_custody(slot), None);
    bank.initialize(pin_view(slot, 98701, view).unwrap());
    assert_eq!(simd_reset_custody(slot), Some(false));
    bank.prepare(pin_view(slot, 98701, view).unwrap());
    assert!(owns_prepared(cpu, slot));
    assert_eq!(simd_reset_custody(slot), None);
    // Counterfactual incomplete count evidence cannot override the independent
    // prepared-bank refusal. Restore it before any reference destruction.
    let pins = &super::super::RESOURCES[slot].pins;
    let complete_count = pins.swap(1, Ordering::AcqRel);
    let incomplete = simd_reset_custody(slot);
    pins.store(complete_count, Ordering::Release);
    assert_eq!(incomplete, None);
    drop(CpuLeases::take(&bank.prepared));
    remote.initialize(pin_view(slot, 98701, view).unwrap());
    assert_eq!(simd_reset_custody(slot), None);
    drop(CpuLeases::take(&remote.current));
    assert_eq!(simd_reset_custody(slot), Some(false));
    drop(CpuLeases::take(&bank.current));
    assert_eq!(simd_reset_custody(slot), None);
    assert!(close_for_reclaim(slot));
    assert_eq!(simd_reset_custody(slot), Some(true));
    super::super::task_start::clear(slot);
    drop(take_storage(slot));
    assert!(simd_unpublished_custody(slot));
}

#[test]
fn replaced_selection_releases_only_its_prepared_reference() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let (from, rejected, to) = (90, 91, 92);
    let a = admit(from, 94101);
    let b = admit(rejected, 94102);
    let c = admit(to, 94103);
    let bank = CpuLeases::new();
    bank.initialize(pin_view(from, 94101, a).unwrap());
    bank.prepare(pin_view(rejected, 94102, b).unwrap());
    assert!(!close_for_reclaim(rejected));
    bank.prepare(pin_view(to, 94103, c).unwrap());
    assert!(close_for_reclaim(rejected));
    assert!(!close_for_reclaim(from));
    assert!(!close_for_reclaim(to));
    bank.publish(from, to);
    bank.commit();
    release(&bank, &[from, rejected, to]);
}

#[test]
fn retained_current_refresh_never_releases_its_final_reference() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 90;
    let view = admit(slot, 94201);
    let bank = CpuLeases::new();
    bank.initialize(pin_view(slot, 94201, view).unwrap());
    bank.publish(slot, slot); // non-dispatch directory access
    bank.prepare(pin_view(slot, 94201, view).unwrap());
    bank.publish(slot, slot); // same-task validated continuation
    assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 1);
    assert_eq!(bank.outgoing.load(Ordering::Acquire), 0);
    assert!(!close_for_reclaim(slot));
    bank.commit();
    assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 1);
    release(&bank, &[slot]);
}

#[test]
fn foreign_selection_cannot_replace_current_custody() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let old = admit(90, 94301);
    let wrong = admit(92, 94302);
    let bank = CpuLeases::new();
    bank.initialize(pin_view(90, 94301, old).unwrap());
    bank.prepare(pin_view(92, 94302, wrong).unwrap());
    assert!(std::panic::catch_unwind(|| bank.publish(90, 91)).is_err());
    assert_eq!(bank.current.load(Ordering::Acquire), 91);
    assert_eq!(bank.outgoing.load(Ordering::Acquire), 0);
    assert!(close_for_reclaim(92));
    assert!(!close_for_reclaim(90));
    release(&bank, &[90, 92]);
}

#[test]
fn publication_and_remote_close_cannot_free_the_outgoing_rsp_early() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let old = admit(90, 94401);
    let next = admit(91, 94402);
    let bank = CpuLeases::new();
    bank.initialize(pin_view(90, 94401, old).unwrap());
    bank.prepare(pin_view(91, 94402, next).unwrap());
    std::thread::scope(|scope| {
        let remote = scope.spawn(|| {
            for _ in 0..1000 {
                assert!(!close_for_reclaim(90));
                assert!(!close_for_reclaim(91));
            }
        });
        bank.publish(90, 91);
        remote.join().unwrap();
    });
    bank.commit();
    assert!(close_for_reclaim(90));
    assert!(!close_for_reclaim(91));
    release(&bank, &[90, 91]);
}
