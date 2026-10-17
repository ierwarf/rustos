use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
const SLOT: usize = 117;
const ID: u64 = 117001;
const ROOT: u64 = 0x3000;
fn stamp() -> ExecReservation {
    crate::multitask::process_table::test_exec_stamp(1, 3, 11)
}
fn with_lease(f: impl FnOnce(&ExecImageLease<'_>, context_validation::PublishedFrameView)) {
    runqueue::reset_before_publication();
    let view = stack_resources::admit_observation_test(SLOT, ID);
    stack_resources::install_exec_context_fixture(SLOT, stamp().process_handle());
    current_identity::clear(SLOT);
    current_identity::publish_lifecycle_change(
        SLOT,
        None,
        current_identity::LifecycleGate::ExecQuiesced,
        true,
    );
    runqueue::address_space::initialize(SLOT, 0x2000);
    runqueue::admit_blocked(SLOT);
    stack_resources::frame_access::with_exec(SLOT, ID, |writer| {
        let fixture_mm = ProcessAddressSpace::modeled_root_for_host(ROOT);
        let pending = crate::multitask::process_table::test_execution_mm(stamp(), &fixture_mm);
        let activation = pending.try_activate().unwrap();
        let lease = ExecImageLease {
            census: None,
            intent: None,
            writer,
            slot: SLOT,
            task_id: ID,
            reservation: stamp(),
            root: ROOT,
            current: false,
            published: Cell::new(false),
            pending_mm: RefCell::new(Some(pending)),
            pending_activation: RefCell::new(Some(activation)),
            pending_placement: RefCell::new(None),
            _fixture_mm: fixture_mm,
        };
        f(&lease, view);
    })
    .unwrap();
    assert!(stack_resources::close_for_reclaim(SLOT));
    stack_resources::task_start::clear(SLOT);
    stack_resources::context::clear(SLOT);
    drop(stack_resources::take_storage(SLOT));
    current_identity::clear(SLOT);
    runqueue::address_space::clear(SLOT);
}
#[test]
fn image_custody_spans_staging_root_publication_and_finalization_not_just_directory_commit() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    with_lease(|lease, view| {
        assert!(stack_resources::pin_view(SLOT, ID, view).is_none()); // staging
        assert!(
            catch_unwind(AssertUnwindSafe(
                || lease.publication(stamp().process_handle())
            ))
            .is_err()
        );
        runqueue::address_space::replace(SLOT, ROOT);
        let publication = lease.publication(stamp().process_handle());
        assert!(stack_resources::pin_view(SLOT, ID, view).is_none()); // visibility finalization
        publication.assert_matches(stamp(), ROOT);
        assert!(
            catch_unwind(AssertUnwindSafe(
                || lease.publication(stamp().process_handle())
            ))
            .is_err()
        );
        assert!(!lease.matches(SLOT, ID, stamp().process_handle(), ROOT, false));
    });
}
#[test]
fn publication_receipt_rejects_old_mm_transaction_foreign_process_generation_and_wrong_root() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    with_lease(|lease, _| {
        runqueue::address_space::replace(SLOT, ROOT);
        let publication = lease.publication(stamp().process_handle());
        for bad in [
            crate::multitask::process_table::test_exec_stamp(2, 3, 11),
            crate::multitask::process_table::test_exec_stamp(1, 4, 11),
            crate::multitask::process_table::test_exec_stamp(1, 3, 12),
        ] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| publication.assert_matches(bad, ROOT))).is_err()
            );
        }
        assert!(
            catch_unwind(AssertUnwindSafe(
                || publication.assert_matches(stamp(), ROOT + 0x1000)
            ))
            .is_err()
        );
        publication.assert_matches(stamp(), ROOT);
    });
}
#[test]
fn image_lease_credentials_cannot_publish_a_foreign_slot_identity_handle_root_or_mode() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    with_lease(|lease, _| {
        let handle = stamp().process_handle();
        assert!(lease.matches(SLOT, ID, handle, ROOT, false));
        assert!(!lease.matches(SLOT + 1, ID, handle, ROOT, false));
        assert!(!lease.matches(SLOT, ID + 1, handle, ROOT, false));
        assert!(!lease.matches(
            SLOT,
            ID,
            crate::multitask::process_table::test_exec_stamp(2, 3, 11).process_handle(),
            ROOT,
            false
        ));
        assert!(!lease.matches(SLOT, ID, handle, ROOT + 0x1000, false));
        assert!(!lease.matches(SLOT, ID, handle, ROOT, true));
    });
}
#[test]
fn incomplete_joined_target_refuses_before_process_staging_callback() {
    let _serial = scheduling_runtime::test_serial_guard();
    let _queue = runqueue::test_serial_guard();
    with_lease(|_, _| {
        assert!(
            with_image(SLOT, ID, stamp(), ROOT, false, |_| panic!(
                "MM staging preceded joined admission"
            ))
            .is_none()
        );
    });
}

#[path = "tests/census.rs"]
mod census;
