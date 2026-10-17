use super::super::tests::admit;
use super::super::{close_for_reclaim, pin_view, take_storage};
use super::*;

fn first_entry(_: u64) {}
fn exec_entry(id: u64) {
    assert_eq!(read(93).unwrap().id, id);
}

#[test]
fn execution_pin_blocks_descriptor_clear_and_recycled_identity() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 93;
    let view = admit(slot, 95001);
    let pin = pin_view(slot, 95001, view).unwrap();
    assert!(!close_for_reclaim(slot));
    assert!(std::panic::catch_unwind(|| clear(slot)).is_err());
    assert!(
        std::panic::catch_unwind(|| publish(
            slot,
            TaskStart {
                entry: first_entry,
                id: 95002
            }
        ))
        .is_err()
    );
    assert_eq!(read(slot).unwrap().id, 95001);
    drop(pin);
    assert!(close_for_reclaim(slot));
    clear(slot);
    assert!(read(slot).is_none());
    drop(take_storage(slot));
    let fresh = admit(slot, 95002);
    assert_eq!(read(slot).unwrap().id, 95002);
    assert!(pin_view(slot, 95001, fresh).is_none());
    assert!(close_for_reclaim(slot));
    clear(slot);
    drop(take_storage(slot));
}

#[test]
fn same_identity_exec_changes_entry_without_spending_a_reference() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 93;
    let view = admit(slot, 95101);
    let pin = pin_view(slot, 95101, view).unwrap();
    publish(
        slot,
        TaskStart {
            entry: exec_entry,
            id: 95101,
        },
    );
    let start = read(slot).unwrap();
    // The callback itself reads its owner: no owner lock crosses entry code.
    (start.entry)(start.id);
    assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 1);
    assert!(!close_for_reclaim(slot));
    assert!(std::panic::catch_unwind(|| publish(slot, start)).is_err());
    assert_eq!(read(slot).unwrap().id, 95101);
    drop(pin);
    clear(slot);
    drop(take_storage(slot));
}

#[test]
fn live_descriptor_and_allocation_binding_cannot_be_rebound_without_reset() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 93;
    let view = admit(slot, 95201);
    assert!(std::panic::catch_unwind(|| take_storage(slot)).is_err());
    assert!(super::super::allocated(slot));
    assert!(
        std::panic::catch_unwind(|| publish(
            slot,
            TaskStart {
                entry: first_entry,
                id: 95202
            }
        ))
        .is_err()
    );
    assert!(std::panic::catch_unwind(|| clear(slot)).is_err());
    assert_eq!(read(slot).unwrap().id, 95201);
    assert!(close_for_reclaim(slot));
    clear(slot);
    assert!(pin_view(slot, 95201, view).is_none());
    assert!(
        std::panic::catch_unwind(|| publish(
            slot,
            TaskStart {
                entry: first_entry,
                id: 95202
            }
        ))
        .is_err()
    );
    drop(take_storage(slot));
    publish(
        slot,
        TaskStart {
            entry: first_entry,
            id: 95202,
        },
    );
    assert_eq!(read(slot).unwrap().id, 95202);
    assert!(close_for_reclaim(slot));
    clear(slot);
    drop(take_storage(slot));
}

#[test]
fn descriptor_identity_is_immutable_even_before_frame_binding() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 94;
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
    publish(
        slot,
        TaskStart {
            entry: first_entry,
            id: 95401,
        },
    );
    assert!(
        std::panic::catch_unwind(|| publish(
            slot,
            TaskStart {
                entry: first_entry,
                id: 95402
            }
        ))
        .is_err()
    );
    assert_eq!(read(slot).unwrap().id, 95401);
    assert!(close_for_reclaim(slot));
    assert!(
        std::panic::catch_unwind(|| publish(
            slot,
            TaskStart {
                entry: first_entry,
                id: 95401
            }
        ))
        .is_err()
    );
    clear(slot);
    drop(take_storage(slot));
}

#[test]
fn running_entry_requires_current_custody_and_exact_complete_identity() {
    let start = TaskStart {
        entry: first_entry,
        id: 95501,
    };
    assert_eq!(
        qualified_running_start(Some(start), Some(95501), true, true)
            .unwrap()
            .id,
        95501
    );
    assert!(qualified_running_start(Some(start), Some(95501), true, false).is_none());
    assert!(qualified_running_start(Some(start), Some(95501), false, true).is_none());
    assert!(qualified_running_start(Some(start), None, true, true).is_none());
    assert!(qualified_running_start(Some(start), Some(95502), true, true).is_none());
    assert!(qualified_running_start(None, Some(95501), true, true).is_none());
    let root = TaskStart {
        entry: first_entry,
        id: 0,
    };
    assert_eq!(
        qualified_running_start(Some(root), Some(0), true, true)
            .unwrap()
            .id,
        0
    );
}

#[test]
fn concurrent_entry_refresh_never_changes_the_monotonic_task_binding() {
    let _serial = super::super::super::scheduling_runtime::test_serial_guard();
    let slot = 93;
    let view = admit(slot, 95301);
    let pin = pin_view(slot, 95301, view).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..1000 {
                publish(
                    slot,
                    TaskStart {
                        entry: first_entry,
                        id: 95301,
                    },
                );
            }
        });
        for _ in 0..1000 {
            assert_eq!(read(slot).unwrap().id, 95301);
        }
    });
    assert!(!close_for_reclaim(slot));
    drop(pin);
    clear(slot);
    drop(take_storage(slot));
}
