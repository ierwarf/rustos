//! Owner: historical directory and terminal IDs bound to original production owners.
//! Boundary: actual root/thread births, physical storage, full process handles and kernel Executor.
//! Lifecycle: original terminal producer -> both exact consumer ACKs -> sealed reaper completion.
//! Concurrency: no raw leaf or IRQ scope spans external endpoint/process/MM effects.
//! Failure: stale hints, retired identities and foreign cleanup payloads cannot grant custody.
//! Forbidden: native IRQ/MM/hardware hooks as architecture ACK or a copied catalog backend.
//! Evidence: actual endpoint PeerClosed, exact ACK exclusion, detach counts and physical reuse seals.
use super::*;
use crate::multitask::scheduler::{self as s, retirement_cleanup, root_creation, terminal_quarantine};

fn boot() -> UserTaskBootstrap {
    let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
    boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x2000);
    boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x6000);
    boot.linux_thread_state = Some(LinuxThreadState::default());
    boot
}

pub(super) fn identity() {
    root_creation::tests::exec_root(0x1a2b, false, boot(), |root, handle| {
        let tid = 0x3c4d;
        let peer = s::thread_creation::tests::active_sibling(root, tid, boot());
        assert_eq!(s::task_directory::published_user_log_ids(tid), Some(Some((0x1a2b, tid))));
        assert_eq!(s::task_directory::published_user_log_ids(0x1a2b), Some(Some((0x1a2b, 0x1a2b))));
        s::thread_creation::tests::release_born(peer, tid, handle);
    });
}

fn assert_binding(id: u64, slot: usize) {
    let original = s::wake_publication::Binding::resolve(id).unwrap();
    assert_eq!(original.slot(), slot);
    assert_eq!(original.id(), id);
}

pub(super) fn hint_collision() {
    root_creation::tests::exec_root(5, false, boot(), |root, handle| {
        let tid = 5 + MAX_TASK as u64;
        let peer = s::thread_creation::tests::active_sibling(root, tid, boot());
        // Real physical publications intentionally share the directory bucket.
        assert_binding(5, root);
        assert_binding(tid, peer);
        s::task_directory::record(5, peer);
        assert_binding(5, root);
        s::task_directory::record(tid, root);
        assert_binding(tid, peer);
        // Negative lifecycle input only; no terminal/reuse ACK is inferred.
        let identity = &crate::multitask::current_identity::LifecycleGate::Retired;
        crate::multitask::current_identity::update_live_lifecycle(root, 5, *identity, true);
        let refused = s::wake_publication::Binding::resolve(5).is_none();
        crate::multitask::current_identity::update_live_lifecycle(root, 5, *identity, false);
        assert!(refused);
        assert_binding(tid, peer);
        s::thread_creation::tests::release_born(peer, tid, handle);
    });
}

pub(super) fn endpoint_revocation() {
    const PID: u64 = 94001;
    root_creation::tests::exec_root_with_release(PID, false, boot(), |slot, _| {
        let task = kernel_ipc_runtime::api::create_endpoint_for_task(PID).unwrap();
        let process = kernel_ipc_runtime::api::create_endpoint_for_process(PID).unwrap();
        let (task_reply, _) = kernel_ipc_runtime::api::enqueue_endpoint_call(task, 951, b"task").unwrap();
        let (process_reply, _) = kernel_ipc_runtime::api::enqueue_endpoint_call(process, 952, b"process").unwrap();
        let producer = terminal_quarantine::begin(slot, Some(PID),
            s::TaskRetireReason::Terminated { requested_by_pid: None }, true).unwrap();
        // SAFETY: fixture retains the original masked native entry and real
        // kernel Current backing; no external consumer runs inside this scope.
        unsafe { s::terminal_publication::guarded(producer, &|_| 1); }
        assert_eq!(retirement_cleanup::test_consumer_publication(slot), (true, true));
        let executor = retirement_cleanup::executor::capture_current().unwrap();
        retirement_cleanup::claim_next_kernel(&executor).unwrap().complete(|id| {
            let _ = s::wake_publication::wake_task(id);
        });
        assert!(!retirement_cleanup::kernel_pending(slot));
        for reply in [task_reply, process_reply] {
            assert!(matches!(kernel_ipc_runtime::api::take_endpoint_response_detailed(reply, 0),
                Ok(EndpointResponseTake::Error { error: IpcError::PeerClosed,
                    discarded_request_handles }) if discarded_request_handles.is_empty()));
        }
        assert_eq!(kernel_ipc_runtime::api::enqueue_endpoint_call(task, 953, b"late-task"), Err(IpcError::InvalidHandle));
        assert_eq!(kernel_ipc_runtime::api::enqueue_endpoint_call(process, 954, b"late-process"), Err(IpcError::InvalidHandle));
    }, s::terminal_publication::tests::release_after_kernel_ack);
}

pub(super) fn exact_cleanup() {
    const PID: u64 = 96001;
    // Actual INITIAL owned MM differs from the retained kernel executor's MM;
    // shared-active-MM exclusion must not be bypassed by a scalar rewrite.
    root_creation::tests::exec_root_with_backing(PID, false, boot(),
        root_creation::tests::policy(), root_creation::tests::state_with_root(0x9000), |slot, _| {
        let stop = terminal_quarantine::begin(slot, Some(PID), s::TaskRetireReason::Exited, false).unwrap();
        // SAFETY: original modeled native entry and actual nonexecuting root.
        unsafe { s::terminal_publication::guarded(stop, &|_| 1); }
        let executor = retirement_cleanup::executor::capture_current().unwrap();
        assert!(s::reaper::claim_next(&executor).is_none());
        retirement_cleanup::claim_next_kernel(&executor).unwrap().complete(|id| {
            let _ = s::wake_publication::wake_task(id);
        });
        // Kernel ACK is actually complete: ONLY the runtime ACK now excludes
        // physical detachment, not a second unconsumed side-effect gate.
        assert!(!retirement_cleanup::kernel_pending(slot));
        // Pending and Claimed runtime cleanup each independently prohibit reuse
        // AFTER the original kernel effect ACK, not behind another guard.
        assert!(s::reaper::claim_next(&executor).is_none());
        let cleanup = retirement_cleanup::claim_next().unwrap();
        assert_eq!((cleanup.task_id(), cleanup.process_id(), cleanup.process_terminal()), (PID, PID, true));
        assert_eq!(retirement_cleanup::claim_next(), None);
        assert!(s::reaper::claim_next(&executor).is_none());
        assert!(s::stack_resources::context::read(slot).is_some());
        let foreign = crate::multitask::RetiredTaskCleanup { task_id: PID + 1,
            process_id: PID, process_terminal: true, user_fault_vector: None, clear_child_tid: 0,
            robust_list_head: 0, robust_list_len: 0 };
        assert!(!retirement_cleanup::complete(foreign));
        assert!(s::reaper::claim_next(&executor).is_none());
        assert!(retirement_cleanup::complete(cleanup));
        let reclaim = s::reaper::claim_next(&executor).unwrap();
        assert!(s::stack_resources::context::read(slot).is_none());
        assert_eq!(process_table::thread_count_by_pid(PID), Some(1));
        reclaim.complete();
        assert_eq!(process_table::thread_count_by_pid(PID), Some(0));
        assert_eq!(process_table::reap_exited_processes(), 1);
    }, |slot, _, _| assert!(s::stack_resources::context::read(slot).is_none()));
}
