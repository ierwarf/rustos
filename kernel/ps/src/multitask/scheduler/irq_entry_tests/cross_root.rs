//! Owner: original dispatcher custody across independent MM roots and full subsequent returns.
//! Boundary: actual process births, typed IPC, claims, register-effect order and both CPU banks.
//! Lifecycle: donated interval survives reply revocation; terminal Current leaves by real switch.
//! Concurrency: same masked CPU, unique RSP ACK; no manual bank withdrawal or interval settlement.
//! Failure: reset refuses before outgoing ACK and funding settlement; duplicate ACK preserves Current.
//! Forbidden: interpreting modeled page tables/register effects/RSP arrival as hardware evidence.
//! Evidence: production constructors/dispatcher/architecture/return/terminal/reset, external ACK only.
use super::*;

const ENDPOINT: u64 = 131020;
const REPLY: u64 = 131021;

fn changed(entry: &mut DispatchEntry, dispatch: SchedulerDispatch, fast: bool, root: u64) {
    let effects = install_model(entry, dispatch, fast);
    assert_eq!(effects.len(), 5, "cross-MM fast return must not take the same-root TLS shortcut");
    assert_eq!(effects[0], return_architecture::ArchitectureEffect::AddressSpace(root));
    assert!(matches!(effects[1], return_architecture::ArchitectureEffect::PrivilegeStack(_)));
    assert!(matches!(effects[2], return_architecture::ArchitectureEffect::SyscallStack(_)));
    assert!(matches!(effects[3], return_architecture::ArchitectureEffect::Segments(_, _)));
}

fn acknowledge(from: usize, to: usize) {
    assert_eq!(task_execution_owner(from), Some(TaskExecutionOwner::Transition(0)));
    assert!(current_identity::execution::count(from) != 0);
    assert!(stack_resources::cpu_leases::owns_current(0, to));
    assert!(current_identity::execution::banks::owns_current(0, to));
    assert_eq!(dispatch_turn::test_phase(0), 3);
    // ONLY arrival on the incoming RSP is modeled. Real one-shot callback drains
    // physical/MM and UID outgoing custody before reopening this CPU's turn.
    commit_context_switch();
    assert_eq!(task_execution_owner(from), None);
    assert_eq!(current_identity::execution::count(from), 0);
    assert_eq!(dispatch_turn::test_phase(0), 0);
    commit_context_switch();
    assert!(stack_resources::cpu_leases::owns_current(0, to));
    assert!(current_identity::execution::banks::owns_current(0, to));
}

#[test]
fn actual_cross_root_ipc_roundtrip_and_terminal_returns_withdraw_every_born_bank_before_real_reuse() {
    with_peer_kind(true, true, |source, peer| {
        let peer = peer.unwrap();
        // Explicit earlier native executor wait, not a hardware ACK. Once the
        // real IPC claim satisfies the spawn promise, its stale FIFO priority
        // must no longer override this otherwise runnable System executor.
        runqueue::test_blocked_return(121, 0, runqueue::weight::value(121));
        let source_context = stack_resources::context::read(source).unwrap();
        let peer_context = stack_resources::context::read(peer).unwrap();
        assert_ne!(source_context.process_handle, peer_context.process_handle);
        assert_ne!(source_context.address_space_root, peer_context.address_space_root);
        runqueue::wait::set_reason(peer, runqueue::wait::REASON_ENDPOINT_RECEIVE, ENDPOINT);
        runqueue::wait::set_blocked(peer, true);
        runqueue::publish_blocked(peer, 0, runqueue::weight::value(peer));
        frame_publication::bind_local_owner_generation(peer, runqueue::owner(peer).generation);
        execution_interval::test_open_captured(source, 2);

        let mut entry = DispatchEntry::from_scope(scope());
        assert!(donation_publication::reserve_call_with(entry.preemption(), ID).donation_reserved);
        // SAFETY: the original masked entry retains actual Current/MM/UID banks.
        let wait = unsafe { current_wait::CurrentWait::capture() }.unwrap();
        assert!(wait.arm(BlockReason::EndpointReply(REPLY)));
        drop(wait);
        assert_eq!(ipc_publication::call(entry.preemption(), ENDPOINT, REPLY, ID + 1),
            FastIpcCallHandoffOutcome::CommittedSameCpu);
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(source), true, Some(ID + 1));
        assert!(fast);
        assert_eq!(dispatch.next_slot, peer);
        changed(&mut entry, dispatch, fast, 0x8000);
        drop(entry);
        acknowledge(source, peer);
        assert_eq!(runqueue::owner(source).state, runqueue::RunOwnerState::Blocked);
        assert!(frame_publication::observed_frame_token(source, 0, false).is_some(),
            "a completed blocked return must retain its SAME committed frame generation");

        // Actual reply cancellation changes metadata, not the already-open
        // original donor FundingPin. The NEXT actual prologue settles it once.
        let mut entry = DispatchEntry::from_scope(scope());
        assert!(release_reply_donation(REPLY, ipc_donation::DonationNamespace::IpcReply));
        let original = current_terminal::OriginalCurrent::borrow(entry.preemption()).unwrap();
        let snapshot = original.runtime_snapshot().unwrap();
        assert_eq!((snapshot.executing_task_id, snapshot.context_owner_task_id), (ID + 1, ID));
        drop(original);
        // The same reply engine receives explicit earlier Online-CPU input;
        // native has no admitted HAL topology. No owner/claim path is replaced.
        assert_eq!(ipc_publication::reply_with(entry.preemption(), REPLY, ID, ID,
            source_context.scheduling_context.identity(), &|_| 1), Some(FastIpcReplyHandoffOutcome::LocalFallback));
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(peer), true, Some(ID));
        assert_eq!(dispatch.next_slot, source);
        changed(&mut entry, dispatch, fast, 0x4000);
        drop(entry);
        acknowledge(peer, source);
        assert!(!release_reply_donation(REPLY, ipc_donation::DonationNamespace::IpcReply));

        // Terminal source still owns its current interval and banks. Its real
        // dispatch prologue settles that interval; no consume()/fixture reset.
        let mut entry = DispatchEntry::from_scope(scope());
        let stop = terminal_quarantine::begin(source, Some(ID), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: same original entry retains Current and the IRQ mask.
        unsafe { terminal_publication::guarded(stop, &|_| 1); }
        assert_eq!(retirement_cleanup::test_consumer_publication(source), (true, true));
        assert!(stack_resources::reuse::begin_retired(source, ID).is_none());
        let token = frame_publication::observed_frame_token(peer, 0, false).unwrap();
        assert!(context_validation::validate_published_saved_context(peer, token.saved_rsp, token.view).is_ok());
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(source), true, Some(ID + 1));
        assert_eq!(dispatch.next_slot, peer);
        changed(&mut entry, dispatch, fast, 0x8000);
        drop(entry);
        assert!(stack_resources::reuse::begin_retired(source, ID).is_none());
        acknowledge(source, peer);

        // Finally retire the OTHER born Current through a complete return to
        // the independently allocated earlier kernel executor, not bank teardown.
        let mut entry = DispatchEntry::from_scope(scope());
        let executor_id = stack_resources::task_start::read(121).unwrap().id;
        // SAFETY: original masked entry and exact retained executor allocation;
        // restore its real queue membership through the production wake engine.
        assert!(unsafe { wake_publication::guarded(121, executor_id, &|_| 1) });
        let stop = terminal_quarantine::begin(peer, Some(ID + 1), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: same original entry retains Current and the IRQ mask.
        unsafe { terminal_publication::guarded(stop, &|_| 1); }
        assert!(stack_resources::reuse::begin_retired(peer, ID + 1).is_none());
        let (dispatch, fast) = entry.dispatch(runqueue::saved_rsp(peer), true, None);
        assert_eq!(dispatch.next_slot, 121);
        changed(&mut entry, dispatch, fast, 0x4000);
        drop(entry);
        assert!(stack_resources::reuse::begin_retired(peer, ID + 1).is_none());
        acknowledge(peer, 121);
        // Both born tasks leave by the actual callbacks BEFORE external ACKs.
        // Keep the new original kernel Current/UID/interval bank throughout
        // cleanup and subsequent same-slot birth; fixture teardown cannot make
        // their reset/resource/MM seals pass accidentally.
        let masked = scope();
        terminal_publication::tests::release_published(source, ID, source_context.process_handle.unwrap());
        terminal_publication::tests::release_published(peer, ID + 1, peer_context.process_handle.unwrap());
        assert!(stack_resources::context::read(source).is_none());
        assert!(current_identity::read(peer).is_none());
        let mut boot = root_creation::tests::bootstrap(UserAbi::Linux);
        boot.entry = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x3000);
        boot.stack_pointer = x86_64::VirtAddr::new(crate::memory::paging::USER_SPACE_BASE + 0x6000);
        let (reused, handle) = root_creation::tests::active_independent_root_with_scope(ID + 2,
            boot, 0xc000, &masked);
        assert_eq!(reused, source);
        assert_ne!(Some(handle), source_context.process_handle);
        assert!(wake_publication::Binding::capture(reused, ID).is_none());
        assert!(wake_publication::Binding::capture(reused, ID + 2).is_some());
        assert!(stack_resources::cpu_leases::owns_current(0, 121));
        assert!(current_identity::execution::banks::owns_current(0, 121));
        let stop = terminal_quarantine::begin(reused, Some(ID + 2), TaskRetireReason::Exited, false).unwrap();
        // SAFETY: the actual original kernel Current and masked scope survive
        // ALL new-process effects; no fixture Current replacement is performed.
        unsafe { terminal_publication::guarded(stop, &|_| 1); }
        terminal_publication::tests::release_published(reused, ID + 2, handle);
        drop(masked);
        // Only final earlier executor teardown remains an explicit fixture input.
        assert!(matches!(execution_interval::consume(0, 121,
            stack_resources::task_start::read(121).unwrap().id), execution_interval::ConsumedInterval::Active(_)));
    }, |slot, _, _| {
        // Enclosing fixture merely audits: actual born cleanup/reuse completed
        // while its real kernel Current bank was still retained above.
        assert_eq!(task_execution_owner(slot), None);
        assert_eq!(current_identity::execution::count(slot), 0);
        assert!(stack_resources::context::read(slot).is_none());
        assert!(current_identity::read(slot).is_none());
    }, true);
}
