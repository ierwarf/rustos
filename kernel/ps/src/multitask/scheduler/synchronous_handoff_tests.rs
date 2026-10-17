//! Host witnesses for synchronous IPC scheduler custody.
//!
//! - **Owner:** `kernel-ps` scheduler tests own call/reply FIFO and fairness
//!   evidence.
//! - **Boundary:** Tests retain original born allocations/bindings; IRQ and
//!   earlier FIFO inputs are modeled, not execution or hardware grants.
//! - **Lifecycle:** Complete, deduplicate, dispatch FIFO, force fairness,
//!   resume, and remove an exact retired peer.
//! - **Concurrency:** The SAME independent dispatcher, FIFO leaf and per-CPU
//!   execution-chain clock are exercised without catalog consumers.
//! - **Failure:** Overwrite, reordering, unbounded bursts, or stale survival
//!   fails the witness.
//! - **Forbidden:** No host timing assumptions or allocation in production.
//! - **Evidence:** `synchronous-ipc-handoff/SynchronousIpcHandoff`.

use super::{runqueue, root_creation, wake_publication, ReplyWakeHandoff};
use crate::multitask::current_identity as identity;
use crate::user::abi::UserAbi;

#[test]
fn synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded() {
    super::local_dispatch::policy_witnesses::overdue_system_task_is_forced_after_latency_bound();
    super::dispatcher::fifo_witnesses::synchronous_fifo();
}

#[test]
fn reply_wake_token_mint_requires_exact_task_and_dispatch_custody() {
    root_creation::tests::exec_root(921, false,
        root_creation::tests::bootstrap(UserAbi::Linux), |slot, _| {
        let mut scope = nucleus_core::util::lockdep::DispatchPreemption::enter_model(0);
        scope.mask_irqs();
        let refs = identity::execution::count(slot);
        let binding = wake_publication::Binding::capture(slot, 921).unwrap();
        assert_eq!(identity::execution::count(slot), refs + 1);
        assert!(wake_publication::Binding::capture(slot, 922).is_none(),
            "foreign UID cannot borrow backing or mint original reply custody");
        let live = runqueue::RunOwnerSnapshot {
            state: runqueue::RunOwnerState::RemoteQueued, cpu: Some(2),
            generation: 7, runnable: true, wait_reason_kind: 0, wait_armed: false,
        };
        // Scalar snapshot is only the modeled earlier token input. This original
        // physical binding, not that value, owns backing; it is NOT execution.
        assert!(ReplyWakeHandoff::from_owner(binding.slot(), binding.id(), live).is_some());
        assert!(ReplyWakeHandoff::from_owner(binding.slot(), binding.id(),
            runqueue::RunOwnerSnapshot { state: runqueue::RunOwnerState::Running, ..live })
            .is_none(), "executing ownership cannot mint dispatch-handoff custody");
        drop(binding);
        assert_eq!(identity::execution::count(slot), refs);
    });
}
