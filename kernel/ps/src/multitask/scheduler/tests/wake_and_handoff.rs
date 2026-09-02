#[test]
fn raced_wake_never_validates_a_consumed_current_frame() {
    let mut scheduler = boxed_scheduler();
    let slot = 1;
    scheduler.contexts[slot] = Some(TaskContext {
        scheduling_context: crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(
            slot, 691,
        ),
        // Dispatch consumed this frame. Deliberately leave an address that
        // could never be validated as a published continuation.
        saved_rsp: 0,
        test_ready: false,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: true,
        block_reason: BlockReason::Generic,
        weight: NICE_0_LOAD,
        vruntime_ns: 0,
        exec_start_ticks: 0,
        address_space_root: 0,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_kernel_stack_base: 0,
        alternate_kernel_stack_top: 0,
        user_mode: true,
        user_abi: Some(UserAbi::Linux),
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    });
    scheduler.starts[slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: 691,
    });
    scheduler.current_task = slot;

    assert!(scheduler.wake_task(691));
    let context = scheduler.contexts[slot].expect("running task survived raced wake");
    assert!(!scheduler.retired[slot]);
    assert!(!context.test_ready);
    assert!(!context.blocked);
    assert!(!context.wake_armed);
    assert_eq!(scheduler.commit_block_current_task(), Some(false));

    // Commit publishes `blocked` before the caller enters its software
    // schedule trap. A remote CPU can wake in that exact interval while
    // the current stack frame is still consumed and intentionally invalid.
    assert!(scheduler.arm_block_current_task());
    assert_eq!(scheduler.commit_block_current_task(), Some(true));
    assert!(scheduler.contexts[slot].expect("committed block").blocked);
    assert!(scheduler.wake_task(691));
    let context = scheduler.contexts[slot].expect("post-commit wake survived");
    assert!(!scheduler.retired[slot]);
    assert!(!context.test_ready);
    assert!(!context.blocked);
    assert!(!context.wake_armed);
    assert_eq!(context.block_reason, BlockReason::None);

    assert!(scheduler.arm_block_current_task_on_endpoint(0xfeed));
    assert_eq!(
        scheduler.contexts[slot].expect("typed endpoint wait armed").block_reason,
        BlockReason::EndpointReceive(0xfeed)
    );
    assert!(scheduler.cancel_block_current_task());
    assert_eq!(
        scheduler.contexts[slot].expect("typed endpoint wait cancelled").block_reason,
        BlockReason::None
    );
    assert!(scheduler.arm_block_current_task_on_reply(0xcafe));
    assert_eq!(
        scheduler.contexts[slot].expect("typed reply wait armed").block_reason,
        BlockReason::EndpointReply(0xcafe)
    );
    assert!(scheduler.cancel_block_current_task());
    assert_eq!(
        scheduler.contexts[slot].expect("typed reply wait cancelled").block_reason,
        BlockReason::None
    );
}

#[test]
fn fast_ipc_commit_requires_exact_typed_waits_and_mutates_both_peers_once() {
    let mut scheduler = boxed_scheduler();
    let sender_slot = 1;
    let receiver_slot = 2;
    let sender_task_id = 701;
    let receiver_task_id = 702;
    let sender = TaskContext {
        scheduling_context: crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(
            sender_slot,
            sender_task_id,
        ),
        saved_rsp: 0,
        test_ready: false,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: false,
        block_reason: BlockReason::None,
        weight: NICE_0_LOAD,
        vruntime_ns: 0,
        exec_start_ticks: 0,
        address_space_root: 0,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_kernel_stack_base: 0,
        alternate_kernel_stack_top: 0,
        user_mode: true,
        user_abi: Some(UserAbi::Linux),
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    };
    let mut receiver = sender;
    receiver.scheduling_context =
        crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(
            receiver_slot,
            receiver_task_id,
        );
    receiver.blocked = true;
    receiver.blocked_since_ticks = 1;
    receiver.block_reason = BlockReason::EndpointReceive(0xabc);
    scheduler.contexts[sender_slot] = Some(sender);
    scheduler.contexts[receiver_slot] = Some(receiver);
    scheduler.starts[sender_slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: sender_task_id,
    });
    scheduler.starts[receiver_slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: receiver_task_id,
    });
    let sender_context_identity = scheduler.contexts[sender_slot]
        .expect("sender context")
        .scheduling_context
        .identity();
    assert!(scheduler.scheduling_context_matches(sender_task_id, sender_context_identity));
    assert!(!scheduler.scheduling_context_matches(receiver_task_id, sender_context_identity));
    let wrong_slot_identity = kernel_object::api::identity::ObjectIdentity::new(
        kernel_object::api::identity::ObjectOwner::Ps,
        kernel_object::api::identity::ObjectKind::SchedulingContext,
        receiver_slot as u64 + 1,
        sender_task_id + 1,
    )
    .expect("nonzero malformed identity");
    assert!(!scheduler.scheduling_context_matches(sender_task_id, wrong_slot_identity));
    scheduler.task_affinity_masks[sender_slot] = 1;
    scheduler.process_affinity_masks[sender_slot] = 1;
    scheduler.task_affinity_masks[receiver_slot] = 1;
    scheduler.process_affinity_masks[receiver_slot] = 1;
    scheduler.current_task = sender_slot;
    assert!(
        scheduler
            .reserve_ipc_call_donation(sender_task_id)
            .donation_reserved
    );
    assert!(scheduler.arm_block_current_task_on_reply(0xdef));

    assert_eq!(
        scheduler.commit_fast_ipc_call_handoff(0xabe, 0xdef, receiver_task_id),
        FastIpcCallHandoffOutcome::ReceiverMismatch
    );
    assert!(!scheduler.contexts[sender_slot].expect("sender retained").blocked);
    assert!(scheduler.contexts[receiver_slot].expect("receiver retained").blocked);

    assert_eq!(
        scheduler.commit_fast_ipc_call_handoff(0xabc, 0xdef, receiver_task_id),
        FastIpcCallHandoffOutcome::CommittedSameCpu
    );
    let sender = scheduler.contexts[sender_slot].expect("sender committed");
    let receiver = scheduler.contexts[receiver_slot].expect("receiver committed");
    assert!(sender.blocked);
    assert!(!sender.wake_armed);
    assert_eq!(sender.block_reason, BlockReason::EndpointReply(0xdef));
    assert!(!receiver.blocked);
    assert_eq!(receiver.block_reason, BlockReason::None);
    assert!(receiver.test_ready);

    scheduler.current_task = receiver_slot;
    assert_eq!(
        scheduler.complete_fast_ipc_reply_handoff(0xdef, sender_task_id),
        FastIpcReplyHandoffOutcome::Direct
    );
    let sender = scheduler.contexts[sender_slot].expect("caller returned");
    assert!(!sender.blocked);
    assert_eq!(sender.block_reason, BlockReason::None);
    assert!(sender.test_ready);

    assert!(scheduler.release_ipc_priority(0xdef, crate::multitask::scheduler::ipc_donation::DonationNamespace::IpcReply));
    {
        let cross_receiver = scheduler.contexts[sender_slot]
            .as_mut()
            .expect("cross-CPU receiver context");
        cross_receiver.blocked = true;
        cross_receiver.test_ready = false;
        cross_receiver.block_reason = BlockReason::EndpointReceive(0xbee);
    }
    scheduler.task_last_cpu[sender_slot] = 1;
    scheduler.current_task = receiver_slot;
    assert!(
        scheduler
            .reserve_ipc_call_donation(receiver_task_id)
            .donation_reserved
    );
    assert!(scheduler.arm_block_current_task_on_reply(0xfee));
    assert_eq!(
        scheduler.commit_fast_ipc_call_handoff(0xbee, 0xfee, sender_task_id),
        FastIpcCallHandoffOutcome::CommittedCrossCpu
    );
    assert!(
        scheduler.contexts[receiver_slot]
            .expect("cross-CPU caller")
            .blocked
    );
    assert!(
        !scheduler.contexts[sender_slot]
            .expect("cross-CPU receiver")
            .blocked
    );
}

#[test]
fn pager_fault_handoff_requires_exact_waits_and_donates_after_worker_wake() {
    let mut scheduler = boxed_scheduler();
    let sender_slot = 1;
    let receiver_slot = 2;
    let sender_task_id = 801;
    let receiver_task_id = 802;
    let token = 0x51;

    let sender = TaskContext {
        scheduling_context: crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(
            sender_slot,
            sender_task_id,
        ),
        saved_rsp: 0,
        test_ready: false,
        ready_since_ticks: 0,
        blocked: true,
        blocked_since_ticks: 1,
        wake_armed: false,
        block_reason: BlockReason::PagerFault(token),
        weight: NICE_0_LOAD,
        vruntime_ns: crate::multitask::scheduler::IPC_DONATION_BONUS_NS.saturating_mul(4),
        exec_start_ticks: 0,
        address_space_root: 0,
        kernel_stack_base: 0,
        kernel_stack_top: 0,
        alternate_kernel_stack_base: 0,
        alternate_kernel_stack_top: 0,
        user_mode: true,
        user_abi: Some(UserAbi::Linux),
        console_session: ConsoleSessionHandle::SYSTEM,
        process_handle: None,
        process_id: None,
        user_stack: None,
        windows_thread_state: None,
    };
    let mut receiver = sender;
    receiver.scheduling_context =
        crate::multitask::scheduler::scheduling_context::SchedulingContext::bind(
            receiver_slot,
            receiver_task_id,
        );
    receiver.wake_armed = true;
    receiver.block_reason = BlockReason::PagerService;
    receiver.vruntime_ns = crate::multitask::scheduler::IPC_DONATION_BONUS_NS.saturating_mul(8);

    scheduler.contexts[sender_slot] = Some(sender);
    scheduler.contexts[receiver_slot] = Some(receiver);
    scheduler.starts[sender_slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: sender_task_id,
    });
    scheduler.starts[receiver_slot] = Some(TaskStart {
        entry: noop_task_entry,
        id: receiver_task_id,
    });
    scheduler.task_affinity_masks[sender_slot] = 1;
    scheduler.process_affinity_masks[sender_slot] = 1;
    scheduler.task_affinity_masks[receiver_slot] = 1;
    scheduler.process_affinity_masks[receiver_slot] = 1;
    scheduler.current_task = sender_slot;

    assert_eq!(
        scheduler.handoff_pager_fault_to_waiter(token + 1, receiver_task_id),
        crate::multitask::scheduler::PagerFaultHandoffOutcome::SenderMismatch
    );
    assert_eq!(
        scheduler.handoff_pager_fault_to_waiter(token, receiver_task_id),
        crate::multitask::scheduler::PagerFaultHandoffOutcome::DirectSameCpu
    );

    let sender = scheduler.contexts[sender_slot].expect("fault donor retained");
    let receiver = scheduler.contexts[receiver_slot].expect("pager worker retained");
    assert!(sender.blocked);
    assert_eq!(sender.block_reason, BlockReason::PagerFault(token));
    assert!(!receiver.blocked);
    assert!(!receiver.wake_armed);
    assert_eq!(receiver.block_reason, BlockReason::None);
    assert!(receiver.test_ready);
    assert!(
        receiver.vruntime_ns < crate::multitask::scheduler::IPC_DONATION_BONUS_NS.saturating_mul(8),
        "pager worker did not receive the one-shot vruntime donation"
    );

    // Host schedulers deliberately leave the global runqueue owner words
    // unpublished, so only the post-wake production token is unavailable
    // here; the wake transition itself is the contract under test.
    assert!(
        scheduler
            .complete_pager_fault_wake_handoff(token + 1, sender_task_id)
            .is_none(),
        "a mismatched reply token woke the fault owner"
    );
    let sender = scheduler.contexts[sender_slot].expect("fault donor retained");
    assert!(sender.blocked, "a mismatched reply token unblocked the owner");
    assert_eq!(
        sender.block_reason,
        BlockReason::PagerFault(token),
        "a mismatched reply token consumed the owner's wait"
    );

    let _ = scheduler.complete_pager_fault_wake_handoff(token, sender_task_id);
    let sender = scheduler.contexts[sender_slot].expect("fault donor retained");
    assert!(
        !sender.blocked,
        "the exact pager reply did not wake the fault owner"
    );
    assert_eq!(
        sender.block_reason,
        BlockReason::None,
        "the exact pager reply left a stale wait reason behind"
    );

    let sender = scheduler.contexts[sender_slot]
        .as_mut()
        .expect("fault donor retained for stale-reply witness");
    sender.blocked = true;
    sender.test_ready = false;
    sender.block_reason = BlockReason::PagerFault(token + 1);
    assert!(
        scheduler
            .complete_pager_fault_wake_handoff(token, sender_task_id)
            .is_none(),
        "a stale pager reply woke a later fault generation"
    );
    let sender = scheduler.contexts[sender_slot].expect("fault donor retained");
    assert!(sender.blocked);
    assert_eq!(sender.block_reason, BlockReason::PagerFault(token + 1));
    assert!(scheduler.scheduling_context_owner_is_live(sender_task_id));
    scheduler.retired[sender_slot] = true;
    assert!(
        !scheduler.scheduling_context_owner_is_live(sender_task_id),
        "a retired-but-not-reaped slot retained live scheduling-context custody"
    );
}

#[test]
fn wake_transition_publishes_one_owner_before_commit_and_claims_once_after() {
    let mut scheduler = boxed_scheduler();
    let task_id = 0xec02;
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let slot = scheduler
        .allocate_user_slot(
            task_id,
            ProcessAddressSpace::empty_for_tests(),
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + 0x10_000),
                x86_64::VirtAddr::new(base + 0x11_000),
            ),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("transition wake task");
    let weight = scheduler.contexts[slot]
        .expect("transition wake context")
        .weight;
    let context = scheduler.contexts[slot]
        .as_mut()
        .expect("transition wake context");
    context.test_ready = false;
    context.blocked = true;
    context.wake_armed = true;
    scheduler.current_task = super::ROOT_TASK_SLOT;
    super::runqueue::admit_blocked(slot);
    let transition = super::super::cpu_local::install_test_transition_owner(1, 2, slot);

    assert!(scheduler.wake_task(task_id));
    assert_ne!(
        scheduler.contexts[slot]
            .expect("transition wake context")
            .ready_since_ticks,
        0,
        "transition wake must publish a queued-age timestamp"
    );
    let published = super::runqueue::owner(slot);
    assert_eq!(
        published.state,
        super::runqueue::RunOwnerState::RemoteQueued,
        "a blocked transition wake must publish exact mailbox custody before assembly commit"
    );
    assert_eq!(published.cpu, Some(1));
    assert!(
        scheduler.pick_hint_candidate_slot(Some(slot)).is_none(),
        "the production candidate gate must reject a frame retained by an active transition"
    );
    assert!(
        scheduler.wake_task(task_id),
        "a stale duplicate wake remains a hint after exact ownership exists"
    );
    assert_eq!(
        super::runqueue::owner(slot),
        published,
        "a repeated transition wake must not duplicate or advance mailbox ownership"
    );

    transition.commit_assembly();
    assert!(
        scheduler.pick_hint_candidate_slot(Some(slot)).is_some(),
        "the same queued frame becomes eligible only after assembly releases the transition"
    );
    assert_eq!(super::runqueue::drain_remote_wakes(1), 1);
    assert!(super::runqueue::is_local_dispatchable(slot, 1));
    assert!(
        super::runqueue::claim_dispatch(slot, 1, weight),
        "the committed mailbox owner must be claimable once"
    );
    assert!(
        !super::runqueue::claim_dispatch(slot, 1, weight),
        "a consumed queued owner must not be claimable twice"
    );
}
