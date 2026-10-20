//! Owner: actual born sender/receiver and original Current allocation.
//! Boundary: actual typed IPC/FIFO/mailbox path; earlier IRQ/topology are MODELS.
//! Lifecycle: original binding, reader and funding loans survive final effects.
//! Concurrency: forced original sender/wait/owner races and FIFO capacity refusal.
//! Failure: refusal preserves winning ownership and typed phase without fallback.
//! Forbidden: scalar generation as backing, re-resolution or modeled hardware claims.
//! Evidence: actual production calls and physical loans; not full joint deletion.
use super::*;
use crate::user::abi::UserAbi;
const SENDER: u64 = 130800;
const RECEIVER: u64 = 130801;
const ENDPOINT: u64 = 0xabc;
const REPLY: u64 = 0xdef;
fn peers(f: impl FnOnce(usize, usize, &mut DispatchPreemption<'_>)) {
    peers_with_scope(true, f)
}
fn peers_with_scope(mask: bool, f: impl FnOnce(usize, usize, &mut DispatchPreemption<'_>)) {
    peers_with_policy(mask, 1, f)
}
pub(in crate::multitask::scheduler) fn peers_with_policy(mask: bool, cpus: u64, f: impl FnOnce(usize, usize, &mut DispatchPreemption<'_>)) {
    let child = core::cell::Cell::new(None);
    let policy = crate::multitask::SchedulingContextAdmission {
        budget_ns: 4000, period_ns: 10000, refill_capacity: 4, cpu_mask: cpus,
        criticality: 1, domain: 120700, policy_epoch: 1, timeout_endpoint_cap: 0,
    };
    root_creation::tests::exec_root_with_admission(SENDER, true,
        root_creation::tests::bootstrap(UserAbi::Linux), policy, |sender, handle| {
            // Serialized native namespace: clear only the previous fixture's
            // modeled FIFO storage, not a production pending/owner recovery.
            assert!(sync_handoff::take_next_ready(0, false, |_| false).is_none());
            runqueue::test_running_claim(sender, 0, runqueue::weight::value(sender));
            let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
            boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x1000);
            boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x4000);
            let receiver = thread_creation::tests::active_sibling(sender, RECEIVER, boot);
            child.set(Some((receiver, handle)));
            // Model only the earlier receive wait, not backing/frame lifetime.
            runqueue::wait::set_reason(receiver, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT);
            runqueue::wait::set_blocked(receiver, true);
            runqueue::publish_blocked(receiver, 0, runqueue::weight::value(receiver));
            frame_publication::bind_local_owner_generation(receiver, runqueue::owner(receiver).generation);
            let mut scope = DispatchPreemption::enter_model_with_enabled_irqs(0);
            if mask { scope.mask_irqs(); }
            f(sender, receiver, &mut scope);
            for cpu in 0..2 { sync_handoff::remove_slot_all_cpus(receiver);
                assert!(sync_handoff::take_next_ready(cpu, false, |_| false).is_none()); }
            donation_ledger::release_task(SENDER);
            donation_ledger::release_task(RECEIVER);
            sync_handoff::remove_slot_all_cpus(receiver);
            assert!(sync_handoff::take_next_ready(0, false, |_| false).is_none());
        }, |sender, id, handle| {
            let (receiver, child_handle) = child.get().unwrap();
            thread_creation::tests::release_born(receiver, RECEIVER, child_handle);
            thread_creation::tests::release_born(sender, id, handle);
        });
}
#[test]
pub(in crate::multitask::scheduler) fn actual_cold_call_keeps_sender_unarmed_and_original_loans_through_receiver_fifo() {
    peers_with_policy(true, 3, |sender, receiver, scope| {
        // Earlier receive locality differs from the admitted caller CPU. The
        // actual cold call must prefer CPU0, not merely keep an already-local
        // receiver where a disabled preference would be indistinguishable.
        runqueue::affinity_payload::record_last_cpu(receiver, 1);
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        let sender_owner = runqueue::owner(sender);
        let sender_refs = current_identity::execution::count(sender);
        let receiver_refs = current_identity::execution::count(receiver);
        let result = call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER, &|_| 1, || {
            assert_eq!(runqueue::owner(sender), sender_owner);
            assert!(!runqueue::wait::armed(sender));
            assert!(!runqueue::wait::blocked(sender));
            assert_eq!(runqueue::owner(receiver).state, runqueue::RunOwnerState::Local);
            assert!(sync_handoff::pending(0));
            assert!(current_identity::execution::count(sender) >= sender_refs + 2);
            assert!(current_identity::execution::count(receiver) > receiver_refs);
            // Cleared reason is not phase release: this same original pending
            // loan still excludes the receiver's next incarnation through FIFO.
            assert!(!runqueue::wait::try_publish_arm(receiver,
                runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT + 1));
        }).unwrap();
        assert!(result.inherited && result.woke && result.hinted);
        assert_eq!(current_identity::execution::count(sender), sender_refs);
        assert_eq!(current_identity::execution::count(receiver), receiver_refs);
        assert_eq!(runqueue::wait::reason(receiver).0, runqueue::wait::REASON_NONE);
        let record = sync_handoff::take_next_ready(0, false, |_| true).unwrap();
        assert_eq!(record.task_id(), RECEIVER);
        // FIFO urgency is NOT execution/lifetime authority; actual selection
        // binds its own original physical frame and owner generation later.
        assert_eq!(record.claim_generation(), None);
        // SAFETY: same original masked CPU and born Current bank enclose this
        // later wait arm, reverse reply CAS, commit and original loan Drop.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReply(REPLY)));
        assert!(wake_publication::reply_with(scope, REPLY, SENDER, &|_| 1));
        assert!(!wait.commit());
        assert!(!runqueue::wait::blocked(sender));
    });
}

#[test]
fn actual_withdrawn_receive_retains_exact_queued_reply_without_wake_or_early_worker_bind() {
    peers(|sender, receiver, scope| {
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        // Real original ordinary wake models the timer winning before runtime
        // withdraws its receiver waiter. No scalar wait/owner reset is used.
        // SAFETY: the fixture retains the original masked CPU scope and born
        // Current/receiver allocation through the entire production wake.
        assert!(unsafe { wake_publication::guarded(receiver, RECEIVER, &|_| 1) });
        let receiver_owner = runqueue::owner(receiver);
        assert_eq!(runqueue::wait::reason(receiver), (runqueue::wait::REASON_NONE, 0));
        assert_eq!(call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER, &|_| 1, || {}).err(),
            Some(FastIpcCallHandoffOutcome::DonationUnavailable),
            "withdrawn waiter is not proof of a published exact reply");
        assert!(donation_ledger::attach(REPLY, ipc_donation::DonationNamespace::IpcReply, SENDER));
        assert_eq!(call_before_arm_with(scope, ENDPOINT, REPLY + 1, SENDER, RECEIVER, &|_| 1, || {}).err(),
            Some(FastIpcCallHandoffOutcome::DonationUnavailable));
        let result = call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER, &|_| 1, || {
            assert!(!runqueue::wait::armed(sender));
            assert_eq!(runqueue::owner(receiver), receiver_owner);
            assert!(donation_ledger::borrowed_context_owner_slot(receiver).is_none());
            assert!(!sync_handoff::pending(0));
        }).unwrap();
        assert!(!result.woke && !result.inherited && !result.hinted);
        assert_eq!(runqueue::owner(receiver), receiver_owner);
        assert!(!sync_handoff::pending(0));
        assert!(donation_ledger::borrowed_context_owner_slot(receiver).is_none());
        // The actual receive-side producer, not the stale endpoint waiter,
        // binds the SAME reserved root funding later and releases it once.
        assert!(donation_publication::bind_with(scope, REPLY, SENDER, RECEIVER, false));
        assert_eq!(donation_ledger::borrowed_context_owner_slot(receiver), Some(sender));
        assert!(donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
        assert!(!donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
    });
}

#[test]
fn actual_cold_publication_raced_receive_preserves_exact_bound_reply_and_newer_foreign_wait() {
    for late in [false, true] {
        peers(|sender, receiver, scope| {
            assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
            assert!(donation_ledger::attach(REPLY, ipc_donation::DonationNamespace::IpcReply, SENDER));
            let bind_and_rearm = || {
                // Actual receive-side original funding admission. Only the
                // earlier foreign CPU's newer wait phase is a native input;
                // this does not manufacture Current or frame execution custody.
                assert!(donation_publication::bind_with(scope, REPLY, SENDER, RECEIVER, false));
                runqueue::wait::set_reason(receiver, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT + 1);
            };
            if !late { bind_and_rearm(); }
            let owner = runqueue::owner(receiver);
            let effects = core::cell::Cell::new(0);
            let outcome = call_before_arm_observing(scope, ENDPOINT, REPLY, SENDER, RECEIVER,
                &|_| 1, || { if late { bind_and_rearm(); } }, || {
                    effects.set(effects.get() + 1);
                    assert!(!runqueue::wait::armed(sender));
                    assert_eq!(runqueue::owner(receiver), owner);
                    assert_eq!(runqueue::wait::reason(receiver),
                        (runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT + 1));
                    assert_eq!(donation_ledger::borrowed_context_owner_slot(receiver), Some(sender));
                    assert!(!sync_handoff::pending(0));
                }).unwrap();
            assert!(!outcome.inherited && !outcome.woke && !outcome.hinted);
            assert_eq!(effects.get(), 1);
            let foreign = donation_publication::participant(RECEIVER).unwrap();
            assert!(!donation_ledger::reply_already_dispatched(&foreign, REPLY));
            drop(foreign);
            assert!(call_before_arm_with(scope, ENDPOINT, REPLY + 1, SENDER, RECEIVER,
                &|_| 1, || {}).is_err(), "another reply cannot borrow this dispatch proof");
            assert!(donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
            assert!(!donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
            assert!(call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER,
                &|_| 1, || {}).is_err(), "released custody cannot certify a later pending call");
            assert_eq!(runqueue::owner(receiver), owner);
            assert!(!sync_handoff::pending(0));
        });
    }
}

#[test]
fn actual_cold_call_own_funding_bind_cannot_certify_a_refused_receiver_wake() {
    let refused = core::cell::Cell::new(false);
    let effects = core::cell::Cell::new(0);
    peers(|sender, receiver, scope| {
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        assert!(donation_ledger::attach(REPLY, ipc_donation::DonationNamespace::IpcReply, SENDER));
        let sender_owner = runqueue::owner(sender);
        let receiver_owner = runqueue::owner(receiver);
        // Negative native input: model the earlier owner/frame publication
        // overlap, NOT hardware execution or a frame grant. Both original born
        // allocations and the actual typed wait/funding producer remain real.
        frame_publication::bind_local_owner_generation(receiver, receiver_owner.generation + 1);
        assert!(frame_publication::observed_frame_token(receiver, 0, false).is_none());
        let outcome = call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER, &|_| 1,
            || effects.set(effects.get() + 1));
        frame_publication::bind_local_owner_generation(receiver, receiver_owner.generation);
        // This producer bound funding BEFORE its frame admission refused. That
        // side effect is not evidence that another CPU actually took a request.
        assert_eq!(donation_ledger::borrowed_context_owner_slot(receiver), Some(sender));
        assert_eq!(runqueue::owner(sender), sender_owner);
        assert_eq!(runqueue::owner(receiver), receiver_owner);
        assert_eq!(runqueue::wait::reason(receiver),
            (runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT));
        assert!(!runqueue::wait::armed(sender));
        assert!(!sync_handoff::pending(0));
        assert!(donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
        assert!(!donation_ledger::release_reply(REPLY, ipc_donation::DonationNamespace::IpcReply));
        refused.set(outcome.err() == Some(FastIpcCallHandoffOutcome::ReceiverMismatch));
    });
    // Check after normal fixture teardown, including the negative/mutant case.
    assert!(refused.get(), "the sender must cancel, not sleep on its own failed wake's binding");
    assert_eq!(effects.get(), 0, "refused wake must not publish final effects");
}

#[test]
fn actual_cold_call_cross_cpu_keeps_exact_receive_funding_and_remote_queue() {
    peers_with_policy(true, 3, |sender, receiver, scope| {
        runqueue::affinity_payload::set_affinity(receiver, 2, u64::MAX, false);
        runqueue::affinity_payload::record_last_cpu(receiver, 1);
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        // Only earlier online topology is modeled; funding admits both CPUs.
        let result = call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER,
            &|_| 3, || {}).unwrap();
        assert!(result.inherited && result.woke && result.hinted);
        let remote = runqueue::owner(receiver);
        assert_eq!(remote.state, runqueue::RunOwnerState::RemoteQueued);
        assert_eq!(remote.cpu, Some(1));
        assert!(remote.runnable);
        assert!(sync_handoff::pending(1));
        assert!(!runqueue::wait::armed(sender));
        assert!(!runqueue::wait::blocked(sender));
        assert_eq!(runqueue::drain_remote_wakes(1), 1);
        assert!(frame_publication::local_frame_token(receiver, 1).is_some());
    });
}

#[test]
fn actual_cold_call_full_fifo_keeps_typed_wake_and_original_unarmed_sender() {
    peers(|sender, receiver, scope| {
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        sync_handoff::with_full_fifo_model(0, || {
            let result = call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER,
                &|_| 1, || {}).unwrap();
            assert!(result.inherited && result.woke);
            assert!(!result.hinted);
            assert_eq!(runqueue::owner(receiver).state, runqueue::RunOwnerState::Local);
            assert!(runqueue::owner(receiver).runnable);
            assert!(!runqueue::wait::armed(sender));
            assert!(!runqueue::wait::blocked(sender));
            assert_eq!(runqueue::wait::reason(receiver).0, runqueue::wait::REASON_NONE);
        });
    });
}

#[test]
fn actual_cold_call_foreign_endpoint_or_sender_wait_refuses_before_bind_and_wake() {
    peers(|sender, receiver, scope| {
        assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
        let before = runqueue::owner(receiver);
        assert_eq!(call_before_arm_with(scope, ENDPOINT + 1, REPLY, SENDER, RECEIVER, &|_| 1, || {}).err(),
            Some(FastIpcCallHandoffOutcome::DonationUnavailable));
        assert_eq!(runqueue::owner(receiver), before);
        assert!(donation_ledger::borrowed_context_owner_slot(receiver).is_none());
        assert!(!sync_handoff::pending(0));
        // SAFETY: original masked CPU and actual Current bank survive the
        // refused cold producer, wait cancellation and this same loan's Drop.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReply(REPLY)));
        assert_eq!(call_before_arm_with(scope, ENDPOINT, REPLY, SENDER, RECEIVER, &|_| 1, || {}).err(),
            Some(FastIpcCallHandoffOutcome::SenderMismatch));
        assert!(runqueue::wait::armed(sender));
        assert_eq!(runqueue::owner(receiver), before);
        assert!(wait.cancel());
        assert_eq!(call_before_arm_with(scope, ENDPOINT, REPLY, SENDER + 1, RECEIVER, &|_| 1, || {}).err(),
            Some(FastIpcCallHandoffOutcome::SenderMismatch));
    });
}

fn prepare(sender: usize, scope: &DispatchPreemption<'_>) {
    assert!(donation_publication::reserve_call_with(scope, SENDER).donation_reserved);
    // SAFETY: same original masked scope and actual Current bank span arm.
    let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
    assert_eq!(wait.slot(), sender);
    assert!(wait.arm(BlockReason::EndpointReply(REPLY)));
}

/// Original public regression name now reaches this actual physical trace.
pub(in crate::multitask::scheduler) fn original_call_trace() {
    peers_with_policy(true, 3, |sender, receiver, scope| {
        // Earlier receive locality is CPU1, while BOTH CPUs are admitted. A
        // disabled caller preference must not pass as an unchanged-local wake.
        runqueue::affinity_payload::record_last_cpu(receiver, 1);
        prepare(sender, scope);
        let before_sender = runqueue::owner(sender);
        let before_receiver = runqueue::owner(receiver);
        let sender_refs = current_identity::execution::count(sender);
        let receiver_refs = current_identity::execution::count(receiver);
        assert_eq!(call(scope, ENDPOINT + 1, REPLY, RECEIVER),
            FastIpcCallHandoffOutcome::ReceiverMismatch);
        assert_eq!(runqueue::owner(sender), before_sender);
        assert_eq!(runqueue::owner(receiver), before_receiver);
        assert_eq!(current_identity::execution::count(sender), sender_refs);
        assert_eq!(current_identity::execution::count(receiver), receiver_refs);
        assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER),
            FastIpcCallHandoffOutcome::CommittedSameCpu);
        assert!(runqueue::wait::blocked(sender));
        assert!(!runqueue::wait::armed(sender));
        assert_eq!(runqueue::wait::reason(sender), (runqueue::wait::REASON_ENDPOINT_REPLY, REPLY));
        let direct = runqueue::owner(receiver);
        assert_eq!(direct.state, runqueue::RunOwnerState::DirectHandoff);
        assert_eq!(direct.generation, before_receiver.generation + 1);
        assert!(direct.runnable);
        assert_eq!(runqueue::wait::reason(receiver).0, runqueue::wait::REASON_NONE);
        assert!(frame_publication::direct_handoff_frame_token(receiver, 0).is_some());
        let record = sync_handoff::take_next_ready(0, false, |_| true).unwrap();
        assert_eq!(record.task_id(), RECEIVER);
        assert_eq!(record.bound_direct_generation(), Some(direct.generation));
        assert!(!sync_handoff::pending(0));
        assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER), FastIpcCallHandoffOutcome::SenderMismatch);
        assert_eq!(runqueue::owner(receiver), direct);
        assert_eq!(current_identity::execution::count(sender), sender_refs);
        assert_eq!(current_identity::execution::count(receiver), receiver_refs);
    });
}

#[test]
fn actual_raced_owner_cannot_be_reacquired_by_original_typed_handoff_cas() {
    peers(|_, receiver, _scope| {
        let binding = wake_publication::Binding::capture(receiver, RECEIVER).unwrap();
        let typed = runqueue::wait::TypedWake::capture(&binding,
            runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT).unwrap();
        let expected = runqueue::owner(receiver);
        let token = frame_publication::observed_frame_token(receiver, 0, false).unwrap();
        let pin = frame_publication::pin_for_identity(token, RECEIVER).unwrap();
        // SAFETY: actual original masked caller, born allocation and UID pin.
        let reader = unsafe { stack_resources::frame_readers::acquire(&pin, 0) }.unwrap();
        // Actual competing runnable intent changes the authoritative owner word
        // without clearing our original pending typed wait. A fresh-CAS retry
        // must not spend even this same-generation winner as the old claim.
        runqueue::set_runnable(receiver, true);
        let winner = runqueue::owner(receiver);
        let winner_wait = runqueue::wait::reason(receiver);
        assert_eq!(winner.state, runqueue::RunOwnerState::Blocked);
        assert_ne!(winner, expected);
        assert!(typed.publish_and_clear(|| runqueue::claim_blocked_handoff(
            receiver, expected, 0, true)).is_none());
        assert_eq!(runqueue::owner(receiver), winner);
        assert!(!runqueue::published_local_member_for_test(0, receiver));
        assert_eq!(runqueue::wait::reason(receiver), winner_wait);
        drop(reader);
        drop(pin);
    });
}

#[test]
fn actual_full_fifo_refuses_original_call_before_owner_wait_or_sender_commit() {
    peers(|sender, receiver, scope| {
        prepare(sender, scope);
        let sender_before = runqueue::owner(sender);
        let receiver_before = runqueue::owner(receiver);
        sync_handoff::with_full_fifo_model(0, || {
            assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER),
                FastIpcCallHandoffOutcome::DirectCustodyUnavailable);
            assert_eq!(runqueue::owner(sender), sender_before);
            assert_eq!(runqueue::owner(receiver), receiver_before);
            assert_eq!(runqueue::wait::reason(receiver),
                (runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT));
            let binding = wake_publication::Binding::capture(receiver, RECEIVER).unwrap();
            let typed = runqueue::wait::TypedWake::capture(&binding,
                runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT).unwrap();
            // Refused scalar join MUST NOT consume the original matched epoch.
            assert!(typed.publish_and_clear::<()>(|| None).is_none());
            assert_eq!(runqueue::owner(receiver), receiver_before);
            assert!(typed.clear());
        });
        assert!(!sync_handoff::pending(0));
    });
}

#[test]
pub(in crate::multitask::scheduler) fn actual_cross_cpu_call_keeps_original_funding_wait_fifo_and_mailbox_generation() {
    peers_with_policy(true, 3, |sender, receiver, scope| {
        // Native earlier affinity/last-placement inputs; actual funding has
        // been admitted for BOTH CPUs by the original runtime/domain engine.
        runqueue::affinity_payload::set_affinity(receiver, 2, u64::MAX, false);
        runqueue::affinity_payload::record_last_cpu(receiver, 1);
        prepare(sender, scope);
        let before = runqueue::owner(receiver);
        assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER),
            FastIpcCallHandoffOutcome::CommittedCrossCpu);
        let remote = runqueue::owner(receiver);
        assert_eq!(remote.state, runqueue::RunOwnerState::RemoteQueued);
        assert_eq!(remote.cpu, Some(1));
        assert_eq!(remote.generation, before.generation + 1);
        assert!(remote.runnable);
        assert!(runqueue::wait::blocked(sender));
        assert_eq!(runqueue::wait::reason(receiver).0, runqueue::wait::REASON_NONE);
        assert!(sync_handoff::pending(1));
        assert_eq!(runqueue::drain_remote_wakes(1), 1);
        let local = runqueue::owner(receiver);
        assert_eq!(local.state, runqueue::RunOwnerState::Local);
        assert_eq!(local.cpu, Some(1));
        assert_eq!(local.generation, remote.generation + 1);
        assert!(frame_publication::local_frame_token(receiver, 1).is_some());
        let record = sync_handoff::take_next_ready(1, false, |_| true).unwrap();
        assert_eq!(record.task_id(), RECEIVER);
        assert_eq!(record.claim_generation(), None); // Remote generic hint, not a grant.
        assert_eq!(record.bind_selected_local_owner(local, 1).claim_generation(), Some(local.generation));
    });
}

#[test]
fn actual_reverse_reply_uses_original_typed_wait_fair_owner_and_exact_fifo() {
    peers(|_, receiver, scope| {
        runqueue::wait::set_reason(receiver, runqueue::wait::REASON_ENDPOINT_REPLY, REPLY);
        let context = stack_resources::context::read(receiver).unwrap().scheduling_context.identity();
        let refs = current_identity::execution::count(receiver);
        // Only earlier topology is modeled; actual typed/frame/queue/FIFO path.
        assert_eq!(reply_with(scope, REPLY, RECEIVER, RECEIVER, context, &|_| 1),
            Some(FastIpcReplyHandoffOutcome::LocalFallback));
        let local = runqueue::owner(receiver);
        assert_eq!(local.state, runqueue::RunOwnerState::Local);
        assert!(local.runnable);
        assert_eq!(runqueue::wait::reason(receiver).0, runqueue::wait::REASON_NONE);
        let record = sync_handoff::take_next_ready(0, false, |_| true).unwrap();
        assert_eq!(record.task_id(), RECEIVER);
        assert_eq!(record.claim_generation(), Some(local.generation));
        assert_eq!(current_identity::execution::count(receiver), refs);
        // Duplicate completion may withdraw no new edge or wait incarnation.
        assert_eq!(reply(scope, REPLY, RECEIVER, RECEIVER, context),
            Some(FastIpcReplyHandoffOutcome::Rejected));
        assert_eq!(runqueue::owner(receiver), local);
    });
}

#[test]
fn actual_call_reverse_wake_before_original_sender_commit_cannot_be_overwritten() {
    peers(|sender, receiver, scope| {
        prepare(sender, scope);
        let before_refs = current_identity::execution::count(sender);
        assert_eq!(call_before_sender_commit(scope, ENDPOINT, REPLY, RECEIVER, || {
            assert_eq!(runqueue::owner(receiver).state, runqueue::RunOwnerState::DirectHandoff);
            assert!(sync_handoff::pending(0));
            // All producer-owned sender/root loans are STILL live at this final
            // effect. A generation comparison or fresh scalar read is no loan.
            assert!(current_identity::execution::count(sender) >= before_refs + 2);
            let context = stack_resources::context::read(sender).unwrap().scheduling_context.identity();
            assert_eq!(reply(scope, REPLY, SENDER, SENDER, context),
                Some(FastIpcReplyHandoffOutcome::LocalFallback));
        }), FastIpcCallHandoffOutcome::CommittedSameCpu);
        assert!(!runqueue::wait::blocked(sender));
        assert!(!runqueue::wait::armed(sender));
        assert!(runqueue::owner(sender).runnable);
        assert_eq!(current_identity::execution::count(sender), before_refs);
    });
}

#[test]
fn actual_call_unmasked_or_closed_receiver_refuses_before_any_wait_or_fifo_effect() {
    peers_with_scope(false, |sender, receiver, scope| {
        let before = runqueue::owner(receiver);
        assert!(!scope.is_masked());
        assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER), FastIpcCallHandoffOutcome::SenderMismatch);
        assert_eq!(runqueue::owner(receiver), before);
        scope.mask_irqs();
        prepare(sender, scope);
        let stop = terminal_quarantine::begin(receiver, Some(RECEIVER), TaskRetireReason::Exited, false).unwrap();
        assert_eq!(call(scope, ENDPOINT, REPLY, RECEIVER), FastIpcCallHandoffOutcome::ReceiverMismatch);
        assert_eq!(runqueue::owner(receiver), before);
        assert!(runqueue::wait::armed(sender));
        assert!(!runqueue::wait::blocked(sender));
        assert!(!sync_handoff::pending(0));
        drop(stop);
    });
}

#[test]
fn actual_reply_foreign_context_and_wrong_wait_refuse_without_generic_fallback() {
    peers(|sender, receiver, scope| {
        prepare(sender, scope);
        let context = stack_resources::context::read(sender).unwrap().scheduling_context.identity();
        assert_eq!(reply(scope, REPLY, RECEIVER, RECEIVER, context), None);
        let before = runqueue::owner(receiver);
        assert_eq!(reply(scope, REPLY, RECEIVER, SENDER, context),
            Some(FastIpcReplyHandoffOutcome::Rejected));
        assert_eq!(runqueue::owner(receiver), before);
        assert_eq!(runqueue::wait::reason(receiver), (runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT));
        assert!(!sync_handoff::pending(0));
    });
}
