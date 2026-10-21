//! Owner: same-CPU synchronous handoff that carries its frame loan into selection.
//! Boundary: call/reply publication -> DirectSwitch -> dispatcher::fast_switch -> claim.
//! Lifecycle: the publication's reader becomes the selected frame; no FIFO record exists.
//! Concurrency: one masked CPU; an unused loan restores fair custody when dropped.
//! Failure: older FIFO work, an exhausted chain or a long sleeper decline the shortcut.
//! Forbidden: a DirectHandoff owner without FIFO record, queue membership or dispatcher.
//! Evidence: production publication, dispatcher, claim and architecture; RSP ACK is modeled.
use super::*;

const ENDPOINT: u64 = 131030;
const REPLY: u64 = 131031;

/// Another native fixture may have left stale FIFO records; consume them so
/// each witness starts from the empty FIFO the shortcut requires.
fn quiet_fifo() {
    while sync_handoff::take_next_ready(0, false, |_| false).is_some() {}
    assert!(!sync_handoff::pending(0));
}

fn block_peer_on_receive(peer: usize) {
    runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT);
    runqueue::wait::set_blocked(peer, true);
    runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
    frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
}

fn arm_call(entry: &DispatchEntry) {
    assert!(donation_publication::reserve_call_with(entry.preemption(), ID).donation_reserved);
    // SAFETY: the original masked entry retains the actual Current bank.
    let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
    assert!(wait.arm(BlockReason::EndpointReply(REPLY)));
}

#[test]
fn actual_call_switch_carries_the_receiver_reader_and_publishes_no_fifo_record() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        block_peer_on_receive(peer);
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        arm_call(&entry);
        let mut switch = None;
        assert_eq!(ipc_publication::call_switching(entry.preemption(), ENDPOINT, REPLY, ID + 1,
            &mut switch), FastIpcCallHandoffOutcome::CommittedSameCpu);
        assert!(switch.is_some(), "an idle FIFO and unexhausted chain must take the shortcut");
        assert!(!sync_handoff::pending(0), "the shortcut must not also publish a FIFO record");
        assert_eq!(runqueue::owner(peer).state, runqueue::RunOwnerState::DirectHandoff);
        let (dispatch, fast) = entry.dispatch_switching(runqueue::saved_rsp(slot), true,
            Some(ID + 1), switch.take());
        assert!(fast);
        assert_eq!(dispatch.next_slot, peer);
        assert_eq!(runqueue::owner(peer).state, runqueue::RunOwnerState::Running);
        assert!(stack_resources::cpu_leases::owns_prepared(0, peer));
        install_model(&mut entry, dispatch, fast);
        drop(entry);
        assert_eq!(published_current_slot(0), Some(peer));
        commit_context_switch();
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        assert!(matches!(execution_interval::consume(0, peer, ID + 1),
            execution_interval::ConsumedInterval::Active(_)));
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(peer);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}

#[test]
fn actual_unused_call_switch_restores_fair_custody_when_dropped() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        block_peer_on_receive(peer);
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        arm_call(&entry);
        let mut switch = None;
        assert_eq!(ipc_publication::call_switching(entry.preemption(), ENDPOINT, REPLY, ID + 1,
            &mut switch), FastIpcCallHandoffOutcome::CommittedSameCpu);
        assert_eq!(runqueue::owner(peer).state, runqueue::RunOwnerState::DirectHandoff);
        drop(switch.take());
        let owner = runqueue::owner(peer);
        assert_eq!(owner.state, runqueue::RunOwnerState::Local,
            "a DirectHandoff receiver with no FIFO record must rejoin the fair queue");
        assert!(owner.runnable && owner.cpu == Some(0));
        assert!(frame_publication::local_frame_token(peer, 0).is_some());
        // Ordinary fair selection now finds the receiver; nothing was stranded.
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(slot), true, None);
        assert!(!fast);
        assert_eq!(dispatch.next_slot, peer);
        install_model(&mut entry, dispatch, fast);
        drop(entry);
        commit_context_switch();
        assert!(matches!(execution_interval::consume(0, peer, ID + 1),
            execution_interval::ConsumedInterval::Active(_)));
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(peer);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}

#[test]
fn actual_queued_fifo_work_and_exhausted_chain_decline_the_shortcut() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        assert!(!sync_handoff::pending(0));
        assert!(!sync_handoff::chain::chain_exhausted(0));
        assert!(sync_handoff::enqueue(0, sync_handoff::SyncHandoffRecord::new(slot, ID)));
        assert!(sync_handoff::pending(0));
        assert!(!ipc_publication::direct_switch_admitted(0),
            "older queued FIFO work must keep its place ahead of a new handoff");
        sync_handoff::remove_slot_all_cpus(slot);
        assert!(!sync_handoff::pending(0) || sync_handoff::take_next_ready(0, false, |_| false).is_none());
        let mut now = 1;
        while !sync_handoff::chain::chain_exhausted(0) {
            sync_handoff::record_dispatch(0, true, now);
            now += 1;
            assert!(now < 64, "chain accounting never exhausted");
        }
        // The runnable sibling is a fair competitor, so the bound is owed to it.
        assert!(local_dispatch::local_fair_competitor_ready(0));
        assert!(!ipc_publication::direct_switch_admitted(0),
            "an exhausted chain must yield to a ready fair competitor");
        sync_handoff::record_dispatch(0, false, now);
        assert!(!sync_handoff::chain::chain_exhausted(0));
        let _ = peer;
    });
}

#[test]
fn actual_reply_switch_returns_to_the_caller_without_fifo_record() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        block_peer_on_receive(peer);
        execution_interval::test_open_captured(slot, 2);
        let source_context = stack_resources::context::read(slot).unwrap();
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        arm_call(&entry);
        let mut switch = None;
        assert_eq!(ipc_publication::call_switching(entry.preemption(), ENDPOINT, REPLY, ID + 1,
            &mut switch), FastIpcCallHandoffOutcome::CommittedSameCpu);
        let (dispatch, fast) = entry.dispatch_switching(runqueue::saved_rsp(slot), true,
            Some(ID + 1), switch.take());
        assert!(fast && dispatch.next_slot == peer);
        install_model(&mut entry, dispatch, fast);
        drop(entry);
        commit_context_switch();

        // The server replies and commits its next receive, as the fused trap does.
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        // SAFETY: entry retains the SAME masked CPU and original Current bank.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReceive(ENDPOINT)));
        assert!(wait.commit());
        drop(wait);
        // The host clock reads zero, so commit time is stamped explicitly.
        runqueue::wait::set_blocked_since_ticks(slot, 1);
        let mut switch = None;
        let outcome = ipc_publication::reply_published(entry.preemption(), REPLY, ID, ID,
            source_context.scheduling_context.identity(), &|_| 1, Some(&mut switch)).unwrap();
        assert_eq!(outcome, FastIpcReplyHandoffOutcome::LocalFallback);
        assert!(switch.is_some(), "a caller that just blocked on this CPU takes the shortcut");
        assert!(!sync_handoff::pending(0));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::DirectHandoff);
        let (dispatch, fast) = entry.dispatch_switching(runqueue::saved_rsp(peer), true,
            outcome.dispatch_hint(ID), switch.take());
        assert!(fast);
        assert_eq!(dispatch.next_slot, slot);
        install_model(&mut entry, dispatch, fast);
        drop(entry);
        commit_context_switch();
        assert!(stack_resources::cpu_leases::owns_current(0, slot));
        assert!(matches!(execution_interval::consume(0, slot, ID),
            execution_interval::ConsumedInterval::Active(_)));
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(peer);
        sync_handoff::remove_slot_all_cpus(slot);
        set_scheduler_current_task_scratch_for_cpu(0, slot);
    });
}

#[test]
fn recent_sleeper_requires_a_recorded_commit_within_one_millisecond() {
    let per_second = 4_000_000_000;
    assert!(!ipc_publication::recent_sleeper(0, 10, per_second), "unrecorded commit time");
    assert!(ipc_publication::recent_sleeper(100, 100, per_second));
    assert!(ipc_publication::recent_sleeper(100, 100 + per_second / 1000, per_second));
    assert!(!ipc_publication::recent_sleeper(100, 101 + per_second / 1000, per_second));
    assert!(ipc_publication::recent_sleeper(100, 50, per_second), "clock skew never ages a sleeper");
}

#[test]
fn actual_blocking_ipc_return_parks_its_loans_and_any_owner_transition_evicts_them() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        // The fixture models a user-mode frame; a task that blocks inside a
        // syscall resumes a kernel continuation, which is what parks.
        // SAFETY: the fixture owns this slot's actual born stack and no reader is open.
        unsafe {
            let frame = &mut *(runqueue::saved_rsp(slot) as *mut crate::multitask::context::SavedContext);
            frame.cs = crate::arch::gdt::kernel_code_selector().0 as u64;
            frame.rip = 0xffff_8000_0000_1000;
            frame.rsp = 1;
            frame.ss = 0;
        }
        let view = frame_publication::return_view(slot).unwrap();
        ipc_lane::note_installed(0, slot,
            stack_resources::context::retained_architecture(0, slot, ID, view).unwrap());
        block_peer_on_receive(peer);
        execution_interval::test_open_captured(slot, 2);
        let mut entry = DispatchEntry::from_scope(scope());
        entry.save_simd();
        arm_call(&entry);
        let mut switch = None;
        assert_eq!(ipc_publication::call_switching(entry.preemption(), ENDPOINT, REPLY, ID + 1,
            &mut switch), FastIpcCallHandoffOutcome::CommittedSameCpu);
        let (dispatch, fast) = entry.dispatch_switching(runqueue::saved_rsp(slot), true,
            Some(ID + 1), switch.take());
        assert!(fast && dispatch.next_slot == peer);
        install_model(&mut entry, dispatch, fast);
        let refs = current_identity::execution::count(slot);
        drop(entry);
        // The RSP ACK parks the caller's pin and permit instead of releasing them.
        commit_context_switch();
        let owner = runqueue::owner(slot);
        assert_eq!(owner.state, runqueue::RunOwnerState::Blocked);
        assert!(resident::is_resident(slot, owner.generation),
            "a task blocked on its reply must stay resident for the lane");
        assert_eq!(resident::permit_units(slot), 1);
        assert_eq!(current_identity::execution::count(slot), 0,
            "quiescence observers must not count the parked permit (was {refs} while running)");
        // Any transition out of that generation revokes the cell and releases it.
        let masked = scope();
        // SAFETY: `masked` is the original own-CPU scope for the whole wake.
        assert!(unsafe { wake_publication::guarded(slot, ID, &|_| 1) });
        drop(masked);
        assert_ne!(runqueue::owner(slot).generation, owner.generation);
        assert!(!resident::is_resident(slot, owner.generation));
        assert_eq!(resident::permit_units(slot), 0);
        assert_eq!(current_identity::execution::count(slot), 0);
        assert!(matches!(execution_interval::consume(0, peer, ID + 1),
            execution_interval::ConsumedInterval::Active(_)));
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(peer);
        sync_handoff::remove_slot_all_cpus(slot);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}
