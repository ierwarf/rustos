//! Owner: actual durable thread materialization component witnesses.
//! Boundary: real Ticket/MM/Executor and producer row, modeled MM root not hardware.
//! Lifecycle: transfer, exact exclusive claim, cancellation/publication before ACK.
//! Concurrency: canonical native fixture; no altered runner or production gates.
//! Failure: stale receipt, wrong current MM, abandonment and terminal caller refuse.
//! Forbidden: native Current publication as assembly switch or physical TEB proof.
//! Evidence: shared row/qualification/worker and real resource/terminal owners.
use super::super::tests::{bootstrap, release_born, reserve_actual};
use super::*;
use crate::multitask::scheduler::process_affinity::tests::affinity_fixture;
const ROOT: usize = 124;
const ID: u64 = 1245501;
const KERNEL: usize = 123;
const KID: u64 = 123011;
fn kernel(f: impl FnOnce(&Executor)) {
    let ticket = stack_resources::admission::reserve(KERNEL, KID).unwrap();
    root_slots::prepare_storage(&ticket).unwrap();
    ticket.begin_commit();
    let mut context = stack_resources::context::read(ROOT).unwrap();
    context.scheduling_context = scheduling_context::SchedulingContext::bind(KERNEL, KID);
    context.user_mode = false;
    context.user_abi = None;
    context.process_handle = None;
    context.process_id = None;
    ticket.install_context(context);
    runqueue::weight::initialize(KERNEL, NICE_0_LOAD);
    runqueue::address_space::initialize(KERNEL, 0x4000);
    ticket.publish_start(TaskStart {
        entry: crate::multitask::noop_task_entry,
        id: KID,
    });
    let base = stack_resources::storage_base(KERNEL);
    let view = PublishedFrameView {
        user_mode_task: false,
        kernel_process_task: false,
        kernel_stack_base: (base + TASK_STACK_GUARD_BYTES) as u64,
        kernel_stack_top: align_kernel_stack_top(base + TASK_STACK_SIZE) as u64,
        alternate_stack_base: 0,
        alternate_stack_top: 0,
        stack_canary_intact: true,
        scheduler_storage_base: 0,
        scheduler_storage_end: 0,
    };
    stack_resources::publish_binding(KERNEL, KID, view);
    identity::publish_metadata(
        KERNEL,
        Some(TaskIdentity {
            task_id: Some(KID),
            ..Default::default()
        }),
    );
    for gate in [
        identity::LifecycleGate::Retired,
        identity::LifecycleGate::Suspended,
        identity::LifecycleGate::JobStopped,
        identity::LifecycleGate::ExecQuiesced,
    ] {
        identity::update_live_lifecycle(KERNEL, KID, gate, false);
    }
    runqueue::affinity_payload::initialize_affinity(KERNEL, 1, 1);
    runqueue::admit_blocked(KERNEL);
    ticket.finish_commit();
    let current = crate::multitask::cpu_local::install_test_current_owner(0, KERNEL);
    let pin = stack_resources::pin_view(KERNEL, KID, view).unwrap();
    stack_resources::cpu_leases::with_test_bank(0, pin, false, || {
        let executor =
            super::super::super::retirement_cleanup::executor::capture_binding(KERNEL, KID)
                .unwrap();
        f(&executor);
    });
    drop(current);
    identity::update_live_lifecycle(KERNEL, KID, identity::LifecycleGate::Retired, true);
    runqueue::retire(KERNEL, NICE_0_LOAD);
    assert!(scheduling_runtime::close_for_reclaim(
        KERNEL,
        context.scheduling_context.identity()
    ));
    let mut reuse = stack_resources::reuse::begin_retired(KERNEL, KID).unwrap();
    runqueue::release_retired(KERNEL);
    scheduling_runtime::release(KERNEL, context.scheduling_context.identity());
    stack_resources::context::clear(KERNEL);
    stack_resources::task_start::clear(KERNEL);
    drop(reuse.take_storage());
    reuse.finish();
}
fn pending() -> Receipt {
    enqueue(Work {
        reservation: Some(reserve_actual()),
        bootstrap: bootstrap(UserAbi::Linux),
        selectors: (0x23, 0x1b, 0x202),
    })
    .unwrap_or_else(|_| panic!("real durable transfer refused"))
}
#[test]
fn original_mm_qualification_refuses_wrong_root_and_retains_actual_activation_through_bytes() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let r = reserve_actual();
        assert!(with_current_mm(&r, 0x8000, || panic!("foreign root wrote TEB")).is_none());
        assert_eq!(with_current_mm(&r, 0x4000, || 42), Some(42));
        cancel(r);
    });
}
#[test]
fn equal_root_different_original_mm_generation_refuses_materialization_and_final_membership() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |handle| {
        let mut r = reserve_actual();
        let original = r.mm.take().unwrap();
        let modeled = crate::memory::paging::ProcessAddressSpace::modeled_root_for_host(0x4000);
        let stamp = process_table::test_exec_stamp(handle.generation(), 99, 1);
        r.mm = Some(process_table::test_execution_mm(stamp, &modeled));
        // Native negative MM-generation injection; not a physical MM replacement.
        assert!(
            with_current_mm(&r, 0x4000, || panic!(
                "same-root stale generation wrote bytes"
            ))
            .is_none()
        );
        assert!(Publication::capture(&r, UserAbi::Linux).is_none());
        r.mm = Some(original);
        cancel(r);
    });
}
#[test]
fn exhausted_durable_nonce_cannot_transfer_or_drop_actual_reserved_ticket() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let work = Work {
            reservation: Some(reserve_actual()),
            bootstrap: bootstrap(UserAbi::Linux),
            selectors: (0x23, 0x1b, 0x202),
        };
        let mut row = Row::new();
        row.nonce = u64::MAX;
        let mut refused = enqueue_into(&mut row, work)
            .err()
            .expect("exhausted nonce wrapped");
        assert_eq!(row.nonce, u64::MAX);
        assert_eq!(row.phase, Phase::Vacant);
        assert!(row.work.is_none());
        assert!(
            refused
                .reservation
                .as_ref()
                .unwrap()
                .ticket()
                .is_reserved_current()
        );
        cancel(refused.reservation.take().unwrap());
    });
}
#[test]
fn abandoned_actual_kernel_claim_quarantines_capsule_without_reopen_or_implied_ack() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let receipt = pending();
        kernel(|executor| {
            drop(claim_next(executor).unwrap());
            assert!(claim_next(executor).is_none());
            assert_eq!(poll(receipt), Some(false));
            let row = ROWS[receipt.slot].lock();
            assert_eq!(row.phase, Phase::Poisoned);
            assert!(
                row.work
                    .as_ref()
                    .unwrap()
                    .reservation
                    .as_ref()
                    .unwrap()
                    .ticket()
                    .is_reserved_current()
            );
            assert!(stack_resources::admission::reserve(receipt.slot, receipt.id + 1).is_none());
        });
        // Native injected abandonment recovery only, never a production reopening:
        // perform the real cancellation ACK before restoring this test row phase.
        let mut work = ROWS[receipt.slot].lock().work.take().unwrap();
        cancel(work.reservation.take().unwrap());
        let mut row = ROWS[receipt.slot].lock();
        assert_eq!(row.nonce, receipt.nonce);
        row.phase = Phase::Complete;
        row.succeeded = false;
    });
}
#[test]
fn real_kernel_worker_publishes_slot_before_durable_ack_and_duplicate_claim_or_stale_receipt_refuses()
 {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |handle| {
        let receipt = pending();
        assert_eq!(poll(receipt), None);
        assert_eq!(
            poll(Receipt {
                nonce: receipt.nonce + 1,
                ..receipt
            }),
            Some(false)
        );
        assert_eq!(
            poll(Receipt {
                id: receipt.id + 1,
                ..receipt
            }),
            Some(false)
        );
        assert!(stack_resources::admission::is_reserved(receipt.slot));
        kernel(|executor| {
            let claim = claim_next(executor).unwrap();
            assert!(claim_next(executor).is_none());
            assert_eq!(poll(receipt), None);
            assert!(stack_resources::admission::is_reserved(receipt.slot));
            claim.complete();
            assert_eq!(poll(receipt), Some(true));
            assert!(!stack_resources::admission::is_reserved(receipt.slot));
            assert!(identity::lifecycle(receipt.slot).suspended);
            assert!(claim_next(executor).is_none());
        });
        release_born(receipt.slot, receipt.id, handle);
    });
}
#[test]
fn same_uid_slot_reuse_spends_new_ticket_and_durable_nonce_and_old_receipt_cannot_observe_next_ack()
{
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |handle| {
        let old = pending();
        let epoch = ROWS[old.slot]
            .lock()
            .work
            .as_ref()
            .unwrap()
            .reservation
            .as_ref()
            .unwrap()
            .ticket()
            .generation();
        kernel(|executor| {
            service(executor);
        });
        assert_eq!(poll(old), Some(true));
        release_born(old.slot, old.id, handle);
        let next = pending();
        assert_eq!((next.slot, next.id), (old.slot, old.id));
        assert!(next.nonce > old.nonce);
        assert!(
            ROWS[next.slot]
                .lock()
                .work
                .as_ref()
                .unwrap()
                .reservation
                .as_ref()
                .unwrap()
                .ticket()
                .generation()
                > epoch
        );
        assert_eq!(poll(old), Some(false));
        assert_eq!(poll(next), None);
        kernel(|executor| {
            service(executor);
        });
        assert_eq!(poll(old), Some(false));
        assert_eq!(poll(next), Some(true));
        release_born(next.slot, next.id, handle);
    });
}
#[test]
fn actual_windows_materialization_error_cancels_reserved_storage_and_attachment_before_receipt_ack()
{
    affinity_fixture(UserAbi::Windows, UserAbi::Windows, false, |_| {
        let count = process_table::thread_count_by_pid(ID).unwrap();
        let reservation = reserve_actual();
        let mut image = bootstrap(UserAbi::Windows);
        // Actual TEB overflow refusal occurs before any invented host page walk.
        image.windows_thread_state = Some(
            crate::user::process_state::WindowsThreadRuntimeState::new(ID + 8, u64::MAX),
        );
        let receipt = enqueue(Work {
            reservation: Some(reservation),
            bootstrap: image,
            selectors: (0x23, 0x1b, 0x202),
        })
        .unwrap_or_else(|_| panic!("durable Windows transfer refused"));
        kernel(|executor| {
            assert_eq!(service(executor), 1);
        });
        assert_eq!(poll(receipt), Some(false));
        assert!(!stack_resources::allocated(receipt.slot));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(count));
    });
}
#[test]
fn dying_original_user_cannot_orphan_real_capsule_and_kernel_cancels_before_failed_ack() {
    affinity_fixture(UserAbi::Linux, UserAbi::Linux, false, |_| {
        let count = process_table::thread_count_by_pid(ID).unwrap();
        let receipt = pending();
        identity::update_live_lifecycle(ROOT, ID, identity::LifecycleGate::Retired, true);
        assert_eq!(poll(receipt), None);
        assert!(
            super::super::super::retirement_cleanup::executor::capture_binding(ROOT, ID).is_none()
        );
        kernel(|executor| {
            assert_eq!(service(executor), 1);
        });
        assert_eq!(poll(receipt), Some(false));
        assert!(!stack_resources::allocated(receipt.slot));
        assert_eq!(process_table::thread_count_by_pid(ID), Some(count));
    });
}
