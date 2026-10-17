use super::super::super::{current_identity, scheduling_runtime};
use super::super::{
    close_for_reclaim, install_storage, pin_view, take_storage, task_start, tests::admit,
};
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 120;
const ID: u64 = 120001;
fn quiesce(value: bool) {
    current_identity::publish_lifecycle_change(
        SLOT,
        None,
        current_identity::LifecycleGate::ExecQuiesced,
        value,
    );
}
fn finish() {
    assert!(close_for_reclaim(SLOT));
    task_start::clear(SLOT);
    drop(take_storage(SLOT));
    current_identity::clear(SLOT);
    frame_publication::clear(SLOT);
}

#[test]
fn unpublished_writer_owns_bytes_outside_leaf_and_blocks_detach_and_reentry() {
    let _serial = scheduling_runtime::test_serial_guard();
    install_storage(SLOT, alloc::vec![0; super::super::super::TASK_STACK_SIZE]);
    initialize_storage(SLOT, |storage| {
        // A leaf callback would deadlock here. Byte initialization no longer
        // holds the resource lock, yet zero refcount cannot detach the buffer.
        RESOURCES[SLOT].with(|resources| assert!(resources.frame_writing));
        assert!(catch_unwind(AssertUnwindSafe(|| take_storage(SLOT))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| with_unpublished(SLOT, |_| ()))).is_err());
        storage[64] = 0x71;
    });
    assert!(!RESOURCES[SLOT].with(|resources| resources.frame_writing));
    finish();
}

#[test]
fn writer_unwind_releases_admission_without_losing_allocation_custody() {
    let _serial = scheduling_runtime::test_serial_guard();
    install_storage(SLOT, alloc::vec![0; super::super::super::TASK_STACK_SIZE]);
    assert!(
        catch_unwind(AssertUnwindSafe(|| with_unpublished(SLOT, |_| panic!(
            "initializer refusal"
        ))))
        .is_err()
    );
    with_unpublished(SLOT, |writer| {
        assert!(writer.stack_top() > writer.base.as_ptr() as usize)
    });
    finish();
}

#[test]
fn exec_writer_requires_exact_quiesced_identity_and_drained_readers() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    assert!(with_exec(SLOT, ID, |_| ()).is_none());
    quiesce(true);
    assert!(with_exec(SLOT, ID + 1, |_| ()).is_none());
    let reader = pin_view(SLOT, ID, view).unwrap();
    assert!(with_exec(SLOT, ID, |_| ()).is_none());
    drop(reader);
    let mut called = false;
    assert!(
        with_exec(SLOT, ID, |writer| {
            called = true;
            assert_eq!(writer.exec_stack_top(SLOT, ID), writer.stack_top());
            assert!(
                catch_unwind(AssertUnwindSafe(|| writer.exec_stack_top(SLOT + 1, ID))).is_err()
            );
            assert!(
                catch_unwind(AssertUnwindSafe(|| writer.exec_stack_top(SLOT, ID + 1))).is_err()
            );
            assert!(pin_view(SLOT, ID, view).is_none());
            assert!(catch_unwind(AssertUnwindSafe(|| task_start::clear(SLOT))).is_err());
            assert!(catch_unwind(AssertUnwindSafe(|| super::super::context::clear(SLOT))).is_err());
        })
        .is_some()
    );
    assert!(called);
    assert!(pin_view(SLOT, ID, view).is_some());
    finish();
}

#[test]
fn closing_defers_reap_until_the_exclusive_writer_releases() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    quiesce(true);
    super::super::super::retirement_cleanup::take_reap_pending();
    with_exec(SLOT, ID, |_| {
        assert!(!close_for_reclaim(SLOT));
        assert!(pin_view(SLOT, ID, view).is_none());
        assert!(catch_unwind(AssertUnwindSafe(|| take_storage(SLOT))).is_err());
        assert!(!super::super::super::retirement_cleanup::take_reap_pending());
    })
    .unwrap();
    assert!(super::super::super::retirement_cleanup::take_reap_pending());
    assert!(with_exec(SLOT, ID, |_| ()).is_none());
    finish();
}

#[test]
fn remote_close_and_reader_cannot_cross_an_active_byte_writer() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    quiesce(true);
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            with_exec(SLOT, ID, |writer| {
                // No existing reader/CPU reference; touch real owned header bytes.
                unsafe {
                    core::ptr::write_bytes(
                        (writer.stack_top() - SAVED_CONTEXT_BYTES) as *mut u8,
                        0x69,
                        SAVED_CONTEXT_BYTES,
                    );
                }
                barrier.wait();
                barrier.wait();
            })
            .unwrap()
        });
        barrier.wait();
        assert!(pin_view(SLOT, ID, view).is_none());
        assert!(!close_for_reclaim(SLOT));
        barrier.wait();
    });
    assert!(close_for_reclaim(SLOT));
    finish();
}

#[test]
fn self_exec_requires_sole_exact_current_before_any_image_side_effect() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    quiesce(false); // self cannot suspend itself across the exec barrier
    let cpu = 7;
    assert!(with_current_exec_on(SLOT, ID, cpu, |_| ()).is_none());
    let current = pin_view(SLOT, ID, view).unwrap();
    cpu_leases::with_test_bank(cpu, current, false, || {
        assert!(with_current_exec_on(SLOT, ID + 1, cpu, |_| ()).is_none());
        assert!(with_current_exec_on(SLOT, ID, cpu + 1, |_| ()).is_none());
        let reader = pin_view(SLOT, ID, view).unwrap();
        assert!(
            with_current_exec_on(SLOT, ID, cpu, |_| panic!("side effects preceded drain"))
                .is_none()
        );
        drop(reader);
        assert!(
            with_current_exec_on(SLOT, ID, cpu, |writer| {
                assert_eq!(writer.exec_stack_top(SLOT, ID), writer.stack_top());
                assert!(pin_view(SLOT, ID, view).is_none());
                assert!(catch_unwind(AssertUnwindSafe(|| task_start::clear(SLOT))).is_err());
            })
            .is_some()
        );
        assert!(pin_view(SLOT, ID, view).is_some());
    });
    finish();
}

#[test]
fn self_exec_cannot_spend_prepared_custody_as_current_or_leak_writer_after_unwind() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    let cpu = 7;
    cpu_leases::with_test_bank(cpu, pin_view(SLOT, ID, view).unwrap(), true, || {
        assert!(with_current_exec_on(SLOT, ID, cpu, |_| ()).is_none());
    });
    cpu_leases::with_test_bank(cpu, pin_view(SLOT, ID, view).unwrap(), false, || {
        assert!(
            catch_unwind(AssertUnwindSafe(|| with_current_exec_on(
                SLOT,
                ID,
                cpu,
                |_| panic!("exec abort")
            )))
            .is_err()
        );
        assert!(with_current_exec_on(SLOT, ID, cpu, |_| ()).is_some());
    });
    finish();
}

#[test]
fn exec_revokes_old_frame_epoch_before_bytes_and_keeps_readers_out_through_publication() {
    let _serial = scheduling_runtime::test_serial_guard();
    let view = admit(SLOT, ID);
    let publish = || {
        frame_publication::publish(
            SLOT,
            view.kernel_stack_top as usize - SAVED_CONTEXT_BYTES,
            view.kernel_stack_base,
            view.kernel_stack_top,
            0,
            0,
            true,
            false,
            true,
            0,
            0,
        )
    };
    publish();
    let old = frame_publication::test_read(SLOT).unwrap();
    quiesce(true);
    with_exec(SLOT, ID, |_| {
        assert_ne!(
            frame_publication::test_read(SLOT).unwrap().frame_epoch,
            old.frame_epoch
        );
        assert!(frame_publication::pin_for_identity(old, ID).is_none());
        publish();
        let new = frame_publication::test_read(SLOT).unwrap();
        assert!(frame_publication::pin_for_identity(new, ID).is_none());
        quiesce(false); // flags are live, but resource writer still refuses
        assert!(frame_publication::pin_for_identity(new, ID).is_none());
    })
    .unwrap();
    assert!(frame_publication::pin_for_identity(old, ID).is_none());
    let new = frame_publication::test_read(SLOT).unwrap();
    assert!(frame_publication::pin_for_identity(new, ID).is_some());
    finish();
}
