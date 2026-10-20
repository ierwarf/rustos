//! Native fixture isolation only, never a production execution serializer.
//! Actual concurrent owner actors within each witness remain concurrent.
use super::*;

#[test]
fn original_birth_holds_the_same_namespace_as_standalone_identity_witnesses() {
    use std::{string::String, sync::TryLockError};
    use crate::multitask::{current_identity as identity, scheduler::root_creation};
    root_creation::tests::exec_root(97101, true,
        root_creation::tests::bootstrap(crate::multitask::UserAbi::Linux), |slot, _| {
            let before = identity::read(slot).unwrap();
            assert_eq!(before.task_id, Some(97101));
            // Unlike a timing-based receive timeout, try_lock deterministically
            // proves a foreign test cannot enter the actual constructor's
            // namespace. It does not acquire or simulate any production loan.
            std::thread::scope(|scope| {
                scope.spawn(|| match TEST_GUARD.try_lock() {
                    Err(TryLockError::WouldBlock) => (),
                    _ => panic!("birth and standalone identity fixture namespaces diverged"),
                }).join().unwrap();
            });
            assert_eq!(identity::read(slot), Some(before));
            // Deterministic reproduction of the old INTER-TEST collision:
            // an unisolated scalar fixture publisher could replace the row
            // between two real birth/terminal operations. The Identity leaf
            // serializes each write, not a fixture's enclosing lifetime. This
            // deliberate negative descriptor input grants no execution loan.
            let lifecycle = identity::lifecycle(slot);
            identity::publish_test_identity(slot, identity::TaskIdentity {
                task_id: Some(97102), ..Default::default()
            });
            let refused = std::panic::catch_unwind(|| {
                identity::publish_lifecycle_change(slot, Some(before),
                    identity::LifecycleGate::Suspended, false);
            });
            // Restore under the retained namespace BEFORE assertion/teardown;
            // no scalar metadata replacement is used to grant physical custody.
            identity::publish_state(slot, before, lifecycle);
            let error = refused.expect_err("foreign fixture row admitted a real lifecycle write");
            let message = error.downcast_ref::<&str>().copied().or_else(||
                error.downcast_ref::<String>().map(String::as_str)).unwrap_or("");
            assert!(message.contains("bound execution identity changed before clear"));
        });
    // Original resource teardown and namespace guard release must precede a
    // new independent fixture. Another parallel witness may legitimately own
    // the namespace now, so WouldBlock here is not evidence of a leaked birth.
    // Acquiring the same mutex still requires teardown to release its guard;
    // the scoped foreign try_lock above proves the enclosing lifetime exactly.
    let _next_fixture = TEST_GUARD.lock().unwrap_or_else(|error| error.into_inner());
}
