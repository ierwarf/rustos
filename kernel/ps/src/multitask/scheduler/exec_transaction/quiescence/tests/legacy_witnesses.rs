//! Historical exec obligations now run the actual frozen-census engine over
//! independently born resources. Fault injection is refusal-only, not a UID,
//! terminal producer, architectural grant or hardware completion substitute.
use super::*;

pub(in crate::multitask::scheduler) fn recycled_uid() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let before = runqueue::owner(sibling);
        let context = stack_resources::context::read(sibling).unwrap();
        // The ORIGINAL census forbids real physical reuse. Prove that as well
        // as the exact copied descriptor check, rather than replacing its pin.
        assert!(stack_resources::admission::reserve(sibling, ID + 2).is_none());
        stack_resources::task_start::test_mismatched_start(sibling, ID + 2, || {
            assert_eq!(check(&intent, &census), Some(true));
            assert_eq!(runqueue::owner(sibling), before);
            assert!(!identity::lifecycle(sibling).retired);
            assert_eq!(retirement_cleanup::reason(sibling), None);
            assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (false, false));
            assert_eq!(stack_resources::context::read(sibling).unwrap().process_handle, context.process_handle);
        });
        assert_eq!(stack_resources::task_start::read(sibling).unwrap().id, ID + 1);
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}

pub(in crate::multitask::scheduler) fn live_identity() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let before = runqueue::owner(sibling);
        stack_resources::task_start::test_mismatched_start(target, ID + 99, || {
            assert_eq!(check(&intent, &census), None);
            assert!(!identity::lifecycle(target).retired);
            assert!(!identity::lifecycle(sibling).retired);
            assert_eq!(runqueue::owner(sibling), before);
            assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (false, false));
        });
        assert_eq!(stack_resources::task_start::read(target).unwrap().id, ID);
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}

pub(in crate::multitask::scheduler) fn pending_kernel() {
    fixture(|target, sibling, handle| {
        let reservation = process_table::begin_exec(handle).unwrap();
        let intent = identity::exec_intent::begin(target, ID, reservation, true).unwrap();
        let census = ExecMembership::begin(&intent).unwrap();
        let before = runqueue::owner(sibling);
        let context = stack_resources::context::read(target).unwrap();
        retirement_cleanup::test_with_pending_kernel(target, ID, || {
            assert!(!identity::lifecycle(target).retired);
            assert_eq!(retirement_cleanup::reason(target), None);
            assert_eq!(check(&intent, &census), None);
            assert!(retirement_cleanup::kernel_pending(target));
            assert_eq!(runqueue::owner(sibling), before);
            assert_eq!(retirement_cleanup::test_consumer_publication(sibling), (false, false));
            assert!(!identity::lifecycle(sibling).retired);
            let after = stack_resources::context::read(target).unwrap();
            assert_eq!(after.address_space_root, context.address_space_root);
            assert_eq!(after.saved_rsp, context.saved_rsp);
            assert_eq!(after.process_handle, context.process_handle);
            assert_eq!(stack_resources::task_start::read(target).unwrap().id, ID);
        });
        cancel(intent, census);
        thread_creation::tests::release_born(sibling, ID + 1, handle);
    });
}
