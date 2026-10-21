//! Owner: same-CPU lane call switch between two resident IPC tasks.
//! Boundary: `ipc_lane::call` -> owner CASes, banks, interval, registers -> RSP ACK.
//! Lifecycle: the receiver resumes from its parked cell; the caller parks at the ACK.
//! Concurrency: one masked CPU and no `CpuDispatchTurn` at all.
//! Failure: any unmet precondition declines with no state changed.
//! Forbidden: a switch that leaves the receiver without banks, interval or Current.
//! Evidence: production lane, owner words, banks and ACK; register effects are modeled.
use super::*;

const ENDPOINT: u64 = 131040;
const REPLY: u64 = 131041;

fn quiet_fifo() {
    while sync_handoff::take_next_ready(0, false, |_| false).is_some() {}
    assert!(!sync_handoff::pending(0));
}

/// A task blocked inside a syscall resumes a KERNEL continuation; the fixture
/// models an earlier user-mode frame, so rewrite its code selector.
fn make_kernel_frame(slot: usize) {
    // SAFETY: the fixture owns this slot's actual born stack and no reader is open.
    unsafe {
        let frame = &mut *(runqueue::saved_rsp(slot) as *mut crate::multitask::context::SavedContext);
        frame.cs = crate::arch::gdt::kernel_code_selector().0 as u64;
        frame.rip = 0xffff_8000_0000_1000;
        // The software-interrupt stub normalizes a kernel frame to rsp = 1, ss = 0.
        frame.rsp = 1;
        frame.ss = 0;
    }
}

/// Park `peer` exactly as a blocking return's RSP ACK would: its banked pin and
/// permit become a resident cell, and the CPU remembers the caller's registers.
fn park_receiver(slot: usize, peer: usize) {
    runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT);
    runqueue::wait::set_blocked(peer, true);
    runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
    frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
    let view = frame_publication::return_view(slot).unwrap();
    let arch = stack_resources::context::retained_architecture(0, slot, ID, view).unwrap();
    ipc_lane::note_installed(0, slot, arch);
    let token = frame_publication::observed_frame_token(peer, 0, false).unwrap();
    let mut peer_arch = arch;
    peer_arch.stack_top = token.view.kernel_stack_top;
    let pin = stack_resources::cpu_leases::pin_initial(peer, ID + 1).expect("peer allocation pin");
    let permit = current_identity::execution::initial(peer, ID + 1).expect("peer execution permit");
    assert!(resident::park(peer, runqueue::owner(peer).generation, resident::Loans {
        pin, permit, saved_rsp: token.saved_rsp, arch: peer_arch, cpu: 0 }));
}

#[test]
fn actual_lane_call_switches_to_the_parked_receiver_and_parks_the_caller_at_the_ack() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        make_kernel_frame(slot);
        make_kernel_frame(peer);
        park_receiver(slot, peer);
        execution_interval::test_open_captured(slot, 2);
        let masked = scope();
        assert!(donation_publication::reserve_call_with(&masked, ID).donation_reserved);
        // SAFETY: `masked` is the original own-CPU scope over the actual Current bank.
        assert!(unsafe { current_wait::CurrentWait::capture() }.unwrap().arm(BlockReason::EndpointReply(REPLY)));
        let receiver_generation = runqueue::owner(peer).generation;
        return_architecture::LANE_EFFECTS.with(|log| log.borrow_mut().clear());
        let rsp = match ipc_lane::call(&masked, ENDPOINT, REPLY, ID + 1, runqueue::saved_rsp(slot)) {
            ipc_lane::Outcome::Switched(rsp) => rsp,
            ipc_lane::Outcome::Declined => panic!("lane declined a fully qualified call"),
            ipc_lane::Outcome::Generic => panic!("lane fell back on a fully qualified call"),
        };
        assert_eq!(rsp, runqueue::saved_rsp(peer), "the receiver resumes at its parked frame");
        let receiver = runqueue::owner(peer);
        assert_eq!((receiver.state, receiver.cpu), (runqueue::RunOwnerState::Running, Some(0)));
        assert_eq!(receiver.generation, receiver_generation + 1);
        assert!(!resident::is_resident(peer, receiver_generation));
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Blocked);
        assert_eq!(published_current_slot(0), Some(peer));
        assert_eq!(task_execution_owner(slot), Some(TaskExecutionOwner::Transition(0)));
        assert!(stack_resources::cpu_leases::owns_current(0, peer));
        assert!(stack_resources::cpu_leases::owns_outgoing(0, slot));
        assert!(current_identity::execution::banks::owns_current(0, peer));
        assert_eq!(donation_ledger::borrowed_context_charge_token(peer).map(|(_, reply)| reply), Some(REPLY));
        let effects = return_architecture::LANE_EFFECTS.with(|log| log.borrow().clone());
        assert!(matches!(effects.first(), Some(return_architecture::ArchitectureEffect::PrivilegeStack(_))));
        assert_eq!(effects.last(), Some(&return_architecture::ArchitectureEffect::Simd(peer)));
        // The incoming RSP is where the stub runs the ACK: Transition clears and
        // the blocked caller parks its loans for the lane.
        commit_context_switch();
        assert_eq!(task_execution_owner(slot), None);
        assert!(frame_publication::observed_frame_token(slot, 0, false).is_some(),
            "the published frame becomes readable once the ACK clears Transition");
        let caller = runqueue::owner(slot);
        assert!(resident::is_resident(slot, caller.generation), "the blocked caller must park");
        assert_eq!(current_identity::execution::count(peer), 1);
        assert!(matches!(execution_interval::consume(0, peer, ID + 1),
            execution_interval::ConsumedInterval::Active(_)));
        drop(masked);
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}

#[test]
fn actual_lane_call_declines_without_changing_state_when_the_receiver_is_not_parked() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT);
        runqueue::wait::set_blocked(peer, true);
        runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
        frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
        // Everything else the lane needs is in place; only residency is missing.
        let view = frame_publication::return_view(slot).unwrap();
        ipc_lane::note_installed(0, slot,
            stack_resources::context::retained_architecture(0, slot, ID, view).unwrap());
        execution_interval::test_open_captured(slot, 2);
        let masked = scope();
        assert!(donation_publication::reserve_call_with(&masked, ID).donation_reserved);
        // SAFETY: `masked` is the original own-CPU scope over the actual Current bank.
        assert!(unsafe { current_wait::CurrentWait::capture() }.unwrap().arm(BlockReason::EndpointReply(REPLY)));
        let (caller, receiver) = (runqueue::owner(slot), runqueue::owner(peer));
        assert!(matches!(ipc_lane::call(&masked, ENDPOINT, REPLY, ID + 1, runqueue::saved_rsp(slot)),
            ipc_lane::Outcome::Declined));
        assert_eq!((runqueue::owner(slot), runqueue::owner(peer)), (caller, receiver));
        assert_eq!(published_current_slot(0), Some(slot));
        assert!(matches!(execution_interval::consume(0, slot, ID), execution_interval::ConsumedInterval::Active(_)));
        // SAFETY: same original scope and Current bank.
        assert!(unsafe { current_wait::CurrentWait::capture() }.unwrap().cancel());
        drop(masked);
        donation_ledger::release_task(ID);
    });
}

#[test]
fn actual_lane_reply_returns_to_the_parked_caller_and_both_hops_share_one_funding() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        make_kernel_frame(slot);
        make_kernel_frame(peer);
        park_receiver(slot, peer);
        execution_interval::test_open_captured(slot, 2);
        let source_context = stack_resources::context::read(slot).unwrap();
        let masked = scope();
        assert!(donation_publication::reserve_call_with(&masked, ID).donation_reserved);
        // SAFETY: `masked` is the original own-CPU scope over the actual Current bank.
        assert!(unsafe { current_wait::CurrentWait::capture() }.unwrap().arm(BlockReason::EndpointReply(REPLY)));
        let ipc_lane::Outcome::Switched(rsp) =
            ipc_lane::call(&masked, ENDPOINT, REPLY, ID + 1, runqueue::saved_rsp(slot))
        else { panic!("lane call did not switch") };
        assert_eq!(rsp, runqueue::saved_rsp(peer));
        commit_context_switch();
        // The call forwarded the caller's interval: the server runs on the same
        // funder, labeled with this call's reply, without a second pin.
        let snapshot = {
            let current = execution_interval::CurrentInterval::borrow(&masked, peer, ID + 1)
                .expect("the receiver must hold the forwarded interval");
            current.runtime_snapshot()
        };
        assert!(snapshot.is_some(), "forwarded interval must keep its funding");

        // The server commits its next receive, as the fused reply trap does, and replies.
        // SAFETY: same original scope and Current bank.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReceive(ENDPOINT)));
        assert!(wait.commit());
        drop(wait);
        runqueue::wait::set_blocked_since_ticks(slot, 1); // host clock reads zero
        let caller_generation = runqueue::owner(slot).generation;
        assert!(resident::is_resident(slot, caller_generation));
        let ipc_lane::Outcome::Switched(rsp) = ipc_lane::reply(&masked, REPLY, ID, ID,
            source_context.scheduling_context.identity(), runqueue::saved_rsp(peer))
        else { panic!("lane reply did not switch") };
        assert_eq!(rsp, runqueue::saved_rsp(slot), "the caller resumes at its parked frame");
        assert_eq!(runqueue::owner(slot).state, runqueue::RunOwnerState::Running);
        assert_eq!(runqueue::owner(peer).state, runqueue::RunOwnerState::Blocked);
        assert_eq!(published_current_slot(0), Some(slot));
        assert_eq!(donation_ledger::borrowed_context_charge_token(peer), None,
            "the reply releases the donation the server ran under");
        commit_context_switch();
        let server = runqueue::owner(peer);
        assert!(resident::is_resident(peer, server.generation), "the answering server must park again");
        assert!(matches!(execution_interval::consume(0, slot, ID), execution_interval::ConsumedInterval::Active(_)));
        drop(masked);
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        set_scheduler_current_task_scratch_for_cpu(0, slot);
    });
}

#[test]
fn actual_lane_reply_declines_a_caller_that_slept_longer_than_a_millisecond() {
    current_with_peer(true, |slot, peer| {
        let peer = peer.unwrap();
        quiet_fifo();
        make_kernel_frame(slot);
        make_kernel_frame(peer);
        park_receiver(slot, peer);
        execution_interval::test_open_captured(slot, 2);
        let source_context = stack_resources::context::read(slot).unwrap();
        let masked = scope();
        assert!(donation_publication::reserve_call_with(&masked, ID).donation_reserved);
        // SAFETY: `masked` is the original own-CPU scope over the actual Current bank.
        assert!(unsafe { current_wait::CurrentWait::capture() }.unwrap().arm(BlockReason::EndpointReply(REPLY)));
        assert!(matches!(ipc_lane::call(&masked, ENDPOINT, REPLY, ID + 1, runqueue::saved_rsp(slot)),
            ipc_lane::Outcome::Switched(_)));
        commit_context_switch();
        // SAFETY: same original scope and Current bank.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReceive(ENDPOINT)));
        assert!(wait.commit());
        drop(wait);
        // The host clock reads zero, so no commit time was recorded: the
        // ordinary wake, which lifts a long sleeper's virtual-time floor, must run.
        assert_eq!(runqueue::wait::blocked_since_ticks(slot), 0);
        let (caller, server) = (runqueue::owner(slot), runqueue::owner(peer));
        assert!(matches!(ipc_lane::reply(&masked, REPLY, ID, ID,
            source_context.scheduling_context.identity(), runqueue::saved_rsp(peer)),
            ipc_lane::Outcome::Declined));
        assert_eq!((runqueue::owner(slot), runqueue::owner(peer)), (caller, server));
        assert_eq!(published_current_slot(0), Some(peer));
        // Leave the fixture consistent: wake the caller through the ordinary path.
        // SAFETY: `masked` is the original own-CPU scope for the whole wake.
        assert!(unsafe { wake_publication::guarded(slot, ID, &|_| 1) });
        drop(masked);
        assert!(matches!(execution_interval::consume(0, peer, ID + 1), execution_interval::ConsumedInterval::Active(_)));
        donation_ledger::release_task(ID);
        donation_ledger::release_task(ID + 1);
        sync_handoff::remove_slot_all_cpus(slot);
        set_scheduler_current_task_scratch_for_cpu(0, peer);
    });
}
