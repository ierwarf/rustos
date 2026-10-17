use super::super::{scheduling_runtime, stack_resources};
use super::*;
use crate::multitask::cpu_local;
const SLOT: usize = 118;
const ID: u64 = 118001;
const CPU: usize = 6;
fn image() -> (context_validation::PublishedFrameView, usize) {
    let view = stack_resources::admit_observation_test(SLOT, ID);
    current_identity::clear(SLOT);
    current_identity::publish_metadata(
        SLOT,
        Some(current_identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
    );
    let rsp = view.kernel_stack_top as usize - SAVED_CONTEXT_BYTES;
    // Initialized POD bytes under isolated fixture's exclusion, before claim.
    let mut saved: SavedContext = unsafe { core::mem::zeroed() };
    saved.rip = 0x1234;
    saved.cs = 0x33;
    saved.rflags = 2;
    unsafe { core::ptr::write(rsp as *mut SavedContext, saved) };
    runqueue::admit_blocked(SLOT);
    runqueue::publish_local(SLOT, CPU, 100);
    frame_publication::publish(
        SLOT,
        rsp,
        view.kernel_stack_base,
        view.kernel_stack_top,
        0,
        0,
        true,
        false,
        true,
        0,
        0,
    );
    frame_publication::bind_local_owner_generation(SLOT, runqueue::owner(SLOT).generation);
    (view, rsp)
}
fn release() {
    assert!(stack_resources::close_for_reclaim(SLOT));
    stack_resources::task_start::clear(SLOT);
    drop(stack_resources::take_storage(SLOT));
    current_identity::clear(SLOT);
    frame_publication::clear(SLOT);
}
#[test]
fn copied_local_observation_never_escapes_backing_or_classifies_unknown_as_zero() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let (_, rsp) = image();
    let snapshot = unsafe { observe(SLOT, CPU, rsp, false) }.unwrap();
    assert_eq!(snapshot.saved.rip, 0x1234);
    assert!(!snapshot.all_zero);
    assert!(unsafe { observe(SLOT, CPU, rsp + 8, false) }.is_none());
    assert!(unsafe { observe(SLOT, CPU, rsp - 8, false) }.is_none());
    assert!(unsafe { observe(SLOT, CPU, usize::MAX - 7, false) }.is_none());
    assert!(unsafe { observe(SLOT, CPU, rsp, true) }.is_none());
    release();
    assert_eq!(snapshot.saved.rip, 0x1234); // independent copy after real release
    assert!(unsafe { observe(SLOT, CPU, rsp, false) }.is_none());
}
#[test]
fn executing_observation_needs_exact_cpu_current_or_prepared_and_refuses_foreign_current() {
    // Match the shared fixture's CPU → runtime → runqueue order.
    let _cpu = cpu_local::test_publication_lock();
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let execution_restore = crate::multitask::current_identity::execution::banks::isolate(CPU);
    let (view, rsp) = image();
    assert!(runqueue::claim_dispatch(SLOT, CPU, 100));
    // Assert token admission independently: the new reader guard must not mask
    // a broken publication qualifier with its second bank/foreign-CPU check.
    assert!(frame_publication::observed_frame_token(SLOT, CPU, true).is_none());
    assert!(unsafe { observe(SLOT, CPU, rsp, true) }.is_none());
    for prepared in [true, false] {
        let pin = stack_resources::pin_view(SLOT, ID, view).unwrap();
        stack_resources::cpu_leases::with_test_bank(CPU, pin, prepared, || {
            assert!(unsafe { observe(SLOT, CPU, rsp, false) }.is_none());
            assert!(unsafe { observe(SLOT, CPU + 1, rsp, true) }.is_none());
            let snapshot = unsafe { observe(SLOT, CPU, rsp, true) }.unwrap();
            assert_eq!(snapshot.saved.cs, 0x33);
            let foreign = cpu_local::install_test_current_owner(CPU + 1, SLOT);
            assert!(frame_publication::observed_frame_token(SLOT, CPU, true).is_none());
            assert!(unsafe { observe(SLOT, CPU, rsp, true) }.is_none());
            drop(foreign);
        });
    }
    drop(execution_restore);
    release();
}
#[test]
fn observed_epoch_and_identity_cannot_recover_replaced_or_writing_frame_authority() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    runqueue::reset_before_publication();
    let (view, rsp) = image();
    current_identity::clear(SLOT);
    current_identity::publish_metadata(
        SLOT,
        Some(current_identity::TaskIdentity {
            task_id: Some(ID + 1),
            ..Default::default()
        }),
    );
    assert!(unsafe { observe(SLOT, CPU, rsp, false) }.is_none());
    current_identity::clear(SLOT);
    current_identity::publish_metadata(
        SLOT,
        Some(current_identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
    );
    current_identity::publish_lifecycle_change(
        SLOT,
        Some(current_identity::TaskIdentity {
            task_id: Some(ID),
            ..Default::default()
        }),
        current_identity::LifecycleGate::ExecQuiesced,
        true,
    );
    stack_resources::frame_access::with_exec(SLOT, ID, |_| {
        assert!(unsafe { observe(SLOT, CPU, rsp, false) }.is_none());
    })
    .unwrap();
    frame_publication::publish(
        SLOT,
        8,
        view.kernel_stack_base,
        view.kernel_stack_top,
        0,
        0,
        true,
        false,
        true,
        0,
        0,
    );
    assert!(unsafe { observe(SLOT, CPU, 8, false) }.is_none());
    release();
}
