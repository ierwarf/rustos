use super::*;

pub(super) fn admit(slot: usize, task_id: u64) -> PublishedFrameView {
    admit_context(slot, task_id, None)
}

pub(super) fn admit_context(
    slot: usize,
    task_id: u64,
    context: Option<super::super::TaskContext>,
) -> PublishedFrameView {
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
    install_storage(slot, alloc::vec![0; super::super::TASK_STACK_SIZE]);
    initialize_storage(slot, |storage| {
        for word in storage[..super::super::TASK_STACK_GUARD_BYTES].chunks_exact_mut(8) {
            word.copy_from_slice(&super::super::STACK_CANARY_WORD.to_ne_bytes());
        }
    });
    let base = storage_base(slot);
    let view = PublishedFrameView {
        user_mode_task: true,
        kernel_process_task: false,
        kernel_stack_base: (base + super::super::TASK_STACK_GUARD_BYTES) as u64,
        kernel_stack_top: super::super::align_kernel_stack_top(base + super::super::TASK_STACK_SIZE)
            as u64,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: true,
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    if let Some(context) = context {
        super::context::install(slot, context);
    }
    task_start::publish(
        slot,
        super::super::TaskStart {
            entry: |_| {},
            id: task_id,
        },
    );
    publish_binding(slot, task_id, view);
    view
}

#[test]
fn allocation_pin_prevents_reset_release_and_identity_reuse() {
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = MAX_TASK - 1;
    let view = admit(slot, 92001);
    let pin = pin_view(slot, 92001, view).unwrap();
    assert!(!close_for_reclaim(slot));
    assert!(pin_view(slot, 92001, view).is_none());
    assert!(std::panic::catch_unwind(|| take_storage(slot)).is_err());
    assert!(std::panic::catch_unwind(|| initialize_storage(slot, |_| {})).is_err());
    assert!(allocated(slot));
    drop(pin);
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
    let fresh = admit(slot, 92002);
    assert!(pin_view(slot, 92001, fresh).is_none());
    let fresh_pin = pin_view(slot, 92002, fresh).unwrap();
    drop(fresh_pin);
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
}

#[test]
fn exact_geometry_and_owned_canary_do_not_follow_foreign_publications() {
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = MAX_TASK - 2;
    let view = admit(slot, 92003);
    assert!(pin_view(slot, 92004, view).is_none());
    let foreign = PublishedFrameView {
        kernel_stack_base: view.kernel_stack_base + 8,
        ..view
    };
    assert!(pin_view(slot, 92003, foreign).is_none());
    assert!(std::panic::catch_unwind(|| publish_binding(slot, 92003, foreign)).is_err());
    assert_eq!(owned_canary_intact(slot), Some(true));
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
}

#[test]
fn concurrent_readers_keep_allocation_until_the_last_pin_is_released() {
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = MAX_TASK - 3;
    let view = admit(slot, 92005);
    let barrier = std::sync::Barrier::new(9);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let barrier = &barrier;
            scope.spawn(move || {
                let pin = pin_view(slot, 92005, view).unwrap();
                barrier.wait();
                barrier.wait();
                drop(pin);
            });
        }
        barrier.wait();
        assert_eq!(RESOURCES[slot].pins.load(Ordering::Acquire), 8);
        assert!(!close_for_reclaim(slot));
        barrier.wait();
    });
    assert!(close_for_reclaim(slot));
    drop(take_storage(slot));
}

#[test]
fn closing_cannot_be_undone_by_metadata_republication() {
    let _serial = super::super::scheduling_runtime::test_serial_guard();
    let slot = MAX_TASK - 4;
    let view = admit(slot, 92006);
    assert!(close_for_reclaim(slot));
    publish_binding(slot, 92006, view);
    assert!(pin_view(slot, 92006, view).is_none());
    drop(take_storage(slot));
}
