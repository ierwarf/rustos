#[test]
fn raced_wake_never_validates_a_consumed_current_frame() {
    let mut scheduler = boxed_scheduler();
    let slot = 1;
    scheduler.contexts[slot] = Some(TaskContext {
        // Dispatch consumed this frame. Deliberately leave an address that
        // could never be validated as a published continuation.
        saved_rsp: 0,
        ready: false,
        ready_since_ticks: 0,
        blocked: false,
        blocked_since_ticks: 0,
        wake_armed: true,
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
    assert!(!context.ready);
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
    assert!(!context.ready);
    assert!(!context.blocked);
    assert!(!context.wake_armed);
}

#[test]
fn wake_transition_publishes_one_owner_before_commit_and_claims_once_after() {
    let _process_table = process_table::tests::isolate_process_table();
    let _publication_lock = super::super::cpu_local::test_publication_lock();
    let _runqueue_serial = super::runqueue::test_serial_guard();
    let _runqueue_reset = RunqueuePublicationReset;
    super::runqueue::reset_before_publication();
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
    context.ready = false;
    context.blocked = true;
    context.wake_armed = true;
    scheduler.current_task = super::ROOT_TASK_SLOT;
    super::runqueue::admit_blocked(slot);
    let transition = super::super::cpu_local::install_test_transition_owner(1, 2, slot);

    assert!(scheduler.wake_task(task_id));
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

#[test]
fn strict_class_requires_explicit_admission_not_a_large_cfs_weight() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let broker = test_process(69);
    let interactive = test_process(70);

    let mut broker_context = test_user_context(broker);
    broker_context.weight = 4 * NICE_0_LOAD;
    let mut interactive_context = test_user_context(interactive);
    interactive_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(broker_context);
    scheduler.contexts[2] = Some(interactive_context);

    assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));
}

#[test]
fn self_demotion_removes_base_system_class_and_caps_fair_weight() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let helper = test_process(73);
    let donor = test_process(74);

    let mut helper_context = test_user_context(helper);
    helper_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | (4 * NICE_0_LOAD);
    let mut donor_context = test_user_context(donor);
    donor_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(helper_context);
    scheduler.contexts[2] = Some(donor_context);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 702,
    });
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 701,
    });
    scheduler.current_task = 1;

    assert!(scheduler.demote_current_user_task_to_user_class());
    assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));
    assert_eq!(
        scheduler.contexts[1].expect("current context").weight,
        NICE_0_LOAD
    );

    // A synchronous reply donation is a separate, capability-scoped
    // source of priority.  Demotion must not turn a pending interactive
    // request into an unbounded priority inversion.
    assert!(scheduler.inherit_ipc_priority(13, 701, 702));
    assert_eq!(scheduler.slot_class(1), Some(SchedClass::System));
    assert!(scheduler.demote_current_user_task_to_user_class());
    assert_eq!(scheduler.slot_class(1), Some(SchedClass::System));
    assert!(scheduler.release_ipc_priority(13));
    assert_eq!(scheduler.slot_class(1), Some(SchedClass::User));

    // The syscall is surrender-only. A task already below the nominal
    // share must not be able to raise its weight by invoking it.
    scheduler.contexts[1]
        .as_mut()
        .expect("current context")
        .weight = SYSTEM_CLASS_WEIGHT_FLAG | MIN_LOAD_WEIGHT;
    assert!(scheduler.demote_current_user_task_to_user_class());
    assert_eq!(
        scheduler.contexts[1].expect("current context").weight,
        MIN_LOAD_WEIGHT
    );
}

/// A donation chain must not turn userspace topology into kernel-stack
/// depth. `visiting` breaks cycles; only the depth bound stops a long
/// acyclic chain, and seL4 proves the two separately for this reason.
#[test]
fn ipc_donation_chain_depth_is_bounded() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();

    // Slot 1 is the System donor; slots 2.. are User links, each inheriting
    // from the one before it.
    const LINKS: u64 = super::MAX_IPC_DONATION_CHAIN_DEPTH as u64 + 2;
    let mut donor_context = test_user_context(test_process(950));
    donor_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(donor_context);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 950,
    });
    for link in 0..LINKS {
        let slot = 2 + link as usize;
        let id = 960 + link;
        scheduler.contexts[slot] = Some(test_user_context(test_process(id)));
        scheduler.starts[slot] = Some(TaskStart {
            entry: noop_task_entry,
            id,
        });
    }
    for link in 0..LINKS {
        let receiver = 960 + link;
        let donor = if link == 0 { 950 } else { receiver - 1 };
        assert!(
            scheduler.inherit_ipc_priority(link + 1, donor, receiver),
            "donation {link} must be installed"
        );
    }

    // The near end inherits System, as the nested-broker test requires.
    assert_eq!(scheduler.slot_class(2), Some(SchedClass::System));

    // Past the bound propagation stops and the link keeps its base class.
    // Under-promoting is the safe direction: a donation only ever raises
    // urgency, so truncation can never grant authority it should not have.
    assert_eq!(
        scheduler.slot_class(2 + super::MAX_IPC_DONATION_CHAIN_DEPTH),
        Some(SchedClass::User),
        "a chain deeper than the donation bound must not propagate"
    );
}

#[test]
fn bounded_system_burst_reserves_a_ready_user_turn() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let system = test_process(71);
    let user = test_process(72);

    let mut system_context = test_user_context(system);
    system_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(system_context);
    scheduler.contexts[2] = Some(test_user_context(user));
    scheduler
        .current_dispatch_policy_mut()
        .system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;

    assert!(Scheduler::user_reservation_due(
        &scheduler.current_dispatch_policy()
    ));
    scheduler.record_dispatch_class(2);
    assert_eq!(
        scheduler.current_dispatch_policy().system_dispatch_streak,
        0
    );
    assert!(!Scheduler::user_reservation_due(
        &scheduler.current_dispatch_policy()
    ));

    scheduler.record_dispatch_class(1);
    assert_eq!(
        scheduler.current_dispatch_policy().system_dispatch_streak,
        1
    );
}

#[test]
fn user_reservation_obeys_vruntime_without_a_wall_clock_bypass() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let mut allocate = |task_id, offset, weight| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                weight,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("user slot")
    };
    let current = allocate(
        75,
        0x2_000,
        crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
    );
    let newer_user = allocate(76, 0x4_000, crate::arch::pit::divisor_from_micros(100));
    let older_user = allocate(77, 0x6_000, crate::arch::pit::divisor_from_micros(100));
    scheduler.contexts[newer_user]
        .as_mut()
        .expect("newer user context")
        .ready_since_ticks = 2;
    scheduler.contexts[newer_user]
        .as_mut()
        .expect("newer user context")
        .vruntime_ns = 10;
    scheduler.contexts[older_user]
        .as_mut()
        .expect("older user context")
        .ready_since_ticks = 1;
    scheduler.contexts[older_user]
        .as_mut()
        .expect("older user context")
        .vruntime_ns = 20;

    assert!(!Scheduler::user_reservation_due(
        &scheduler.current_dispatch_policy()
    ));
    assert_eq!(
        scheduler.reserved_user_pick(&scheduler.current_dispatch_policy(), current),
        None
    );

    scheduler
        .current_dispatch_policy_mut()
        .system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;
    assert_eq!(
        scheduler.reserved_user_pick(&scheduler.current_dispatch_policy(), current),
        Some(newer_user)
    );
}

#[test]
fn fair_locality_is_bounded_by_class_and_vruntime_lag() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let mut allocate = |task_id, offset| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("user slot")
    };
    let current = allocate(781, 0x2_000);
    let global_min = allocate(782, 0x4_000);
    let local = allocate(783, 0x6_000);
    scheduler.current_task = current;
    scheduler.task_last_cpu[current] = 0;
    scheduler.task_last_cpu[global_min] = 1;
    scheduler.task_last_cpu[local] = 0;
    scheduler.contexts[current]
        .as_mut()
        .expect("current context")
        .vruntime_ns = 10_000_000;
    scheduler.contexts[global_min]
        .as_mut()
        .expect("global context")
        .vruntime_ns = 1_000_000;
    scheduler.contexts[local]
        .as_mut()
        .expect("local context")
        .vruntime_ns = 1_000_000 + SCHED_CPU_LOCALITY_LAG_NS;

    assert_eq!(scheduler.pick_min_vruntime(current), Some(local));

    scheduler.contexts[local]
        .as_mut()
        .expect("local context")
        .vruntime_ns = 1_000_001 + SCHED_CPU_LOCALITY_LAG_NS;
    assert_eq!(scheduler.pick_min_vruntime(current), Some(global_min));

    scheduler.contexts[global_min]
        .as_mut()
        .expect("global context")
        .weight |= SYSTEM_CLASS_WEIGHT_FLAG;
    scheduler.contexts[global_min]
        .as_mut()
        .expect("global context")
        .vruntime_ns = u64::MAX / 2;
    assert_eq!(scheduler.pick_min_vruntime(current), Some(global_min));
}

#[test]
fn overdue_system_task_is_forced_after_latency_bound() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let bootstrap = |offset| {
        UserTaskBootstrap::new(
            UserAbi::Linux,
            x86_64::VirtAddr::new(base + offset),
            x86_64::VirtAddr::new(base + offset + 0x1_000),
        )
    };
    let current = scheduler
        .allocate_user_slot(
            701,
            ProcessAddressSpace::empty_for_tests(),
            bootstrap(0x2_000),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("current slot");
    let interactive = scheduler
        .allocate_user_slot(
            702,
            ProcessAddressSpace::empty_for_tests(),
            bootstrap(0x4_000),
            None,
            crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("interactive slot");
    scheduler.contexts[interactive]
        .as_mut()
        .expect("interactive context")
        .ready_since_ticks = 1;

    let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
    assert_eq!(
        scheduler.overdue_system_pick(current, now_ticks),
        Some(interactive)
    );
}

#[test]
fn overdue_system_continuation_precedes_unrelated_ipc_hint_without_losing_it() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let mut allocate = |task_id, offset| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("System task slot")
    };
    let current = allocate(811, 0x2_000);
    let overdue = allocate(812, 0x4_000);
    let hinted = allocate(813, 0x6_000);
    scheduler.contexts[overdue]
        .as_mut()
        .expect("overdue context")
        .ready_since_ticks = 1;
    scheduler.contexts[hinted]
        .as_mut()
        .expect("hinted context")
        .ready_since_ticks = 0;
    scheduler.current_task = current;
    scheduler.set_next_pick_hint(813);

    // The chain offers overdue System work once, ahead of the hint, and
    // leaves the hint pending when it wins. The second half also pins the
    // premise that lets the chain scan once instead of twice: with the
    // overdue task gone the same scan returns None, so re-running it before
    // the hint could never have changed the outcome.
    let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        Some(overdue)
    );
    assert_eq!(
        scheduler.current_dispatch_policy().next_pick_hint,
        Some(hinted)
    );

    scheduler.contexts[overdue]
        .as_mut()
        .expect("overdue context")
        .ready = false;
    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        None
    );
    assert_eq!(
        scheduler.take_next_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(hinted)
    );
    assert_eq!(scheduler.current_dispatch_policy().next_pick_hint, None);
}

#[test]
fn stale_pick_hint_falls_through_without_mutating_task_state() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let hinted = scheduler
        .allocate_user_slot(
            0x8131,
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
        .expect("hinted task");
    scheduler.contexts[hinted]
        .as_mut()
        .expect("hinted context")
        .blocked = true;
    let before = scheduler.contexts[hinted].expect("stale hinted context");
    scheduler.current_dispatch_policy_mut().next_pick_hint = Some(hinted);

    assert_eq!(scheduler.pick_hint_candidate_slot(Some(hinted)), None);
    assert_eq!(
        scheduler.current_dispatch_policy().next_pick_hint,
        Some(hinted),
        "candidate validation must not turn a stale hint into task authority"
    );
    let after = scheduler.contexts[hinted].expect("post-validation hinted context");
    assert_eq!(after.ready, before.ready);
    assert_eq!(after.blocked, before.blocked);
    assert_eq!(after.wake_armed, before.wake_armed);
    assert_eq!(after.ready_since_ticks, before.ready_since_ticks);
}

#[test]
fn overdue_system_continuation_precedes_a_fresh_latency_handoff() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let mut allocate = |task_id, offset, system| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(if system { 2_000 } else { 100 })
                    | if system {
                        super::INTERACTIVE_PIT_DIVISOR_FLAG
                    } else {
                        0
                    },
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("task slot")
    };
    let current = allocate(821, 0x2_000, true);
    let overdue = allocate(822, 0x4_000, true);
    let hinted = allocate(823, 0x6_000, false);
    let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
    scheduler.contexts[overdue]
        .as_mut()
        .expect("overdue context")
        .ready_since_ticks = 1;
    scheduler.contexts[hinted]
        .as_mut()
        .expect("hinted context")
        .ready_since_ticks = now_ticks;
    scheduler.current_task = current;
    assert!(scheduler.set_next_latency_pick_hint(823));

    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        Some(overdue)
    );
    assert_eq!(scheduler.current_dispatch_policy().latency_pick_hint_len, 1);

    scheduler.contexts[overdue]
        .as_mut()
        .expect("overdue context")
        .ready = false;
    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        None
    );
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(hinted)
    );
}

#[test]
fn event_wait_handoff_is_fifo_deduplicated_and_burst_bounded() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let bootstrap = |offset| {
        UserTaskBootstrap::new(
            UserAbi::Linux,
            x86_64::VirtAddr::new(base + offset),
            x86_64::VirtAddr::new(base + offset + 0x1_000),
        )
    };
    let user_slot = scheduler
        .allocate_user_slot(
            901,
            ProcessAddressSpace::empty_for_tests(),
            bootstrap(0x2_000),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("user slot");
    let system_slot = scheduler
        .allocate_user_slot(
            902,
            ProcessAddressSpace::empty_for_tests(),
            bootstrap(0x4_000),
            None,
            crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("system slot");
    let second_user_slot = scheduler
        .allocate_user_slot(
            903,
            ProcessAddressSpace::empty_for_tests(),
            bootstrap(0x6_000),
            None,
            crate::arch::pit::divisor_from_micros(100),
            user_cs,
            user_ss,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("second user slot");

    assert_eq!(scheduler.slot_class(user_slot), Some(SchedClass::User));
    assert_eq!(scheduler.slot_class(system_slot), Some(SchedClass::System));
    assert_eq!(
        scheduler.slot_class(second_user_slot),
        Some(SchedClass::User)
    );
    assert!(scheduler.set_next_latency_pick_hint(901));
    assert!(!scheduler.set_next_latency_pick_hint(902));
    assert!(scheduler.set_next_latency_pick_hint(903));
    assert!(scheduler.set_next_latency_pick_hint(901));
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(user_slot)
    );
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(second_user_slot)
    );
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        None
    );

    assert!(scheduler.set_next_latency_pick_hint(901));
    scheduler
        .current_dispatch_policy_mut()
        .latency_handoff_streak = super::MAX_CONSECUTIVE_LATENCY_HANDOFFS;
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        None
    );
    scheduler.record_latency_handoff(false);
    assert_eq!(
        scheduler.take_next_latency_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(user_slot)
    );
}

#[test]
fn dispatch_fairness_and_handoff_state_is_cpu_isolated() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let user = test_process(904);
    scheduler.contexts[1] = Some(test_user_context(user));
    {
        let mut policy = scheduler.cpu_dispatch[1].lock();
        policy.system_dispatch_streak = MAX_CONSECUTIVE_SYSTEM_DISPATCHES;
        policy.next_pick_hint = Some(7);
    }

    scheduler.record_dispatch_class(1);
    scheduler.current_dispatch_policy_mut().next_pick_hint = None;

    assert_eq!(scheduler.cpu_dispatch[0].lock().system_dispatch_streak, 0);
    assert_eq!(
        scheduler.cpu_dispatch[1].lock().system_dispatch_streak,
        MAX_CONSECUTIVE_SYSTEM_DISPATCHES
    );
    assert_eq!(scheduler.cpu_dispatch[1].lock().next_pick_hint, Some(7));
}

/// Authority confinement: a donation edge must never be installable
/// where the donor and receiver are the same task, and it must never
/// bind a receiver that is already retired. `bind_reserved_ipc_priority`
/// and `inherit_ipc_priority` each carry an independent guard for this;
/// this test drives both call paths so a guard removed from either one
/// is caught here rather than only through the other's redundancy.
#[test]
fn ipc_donation_rejects_self_referential_and_retired_targets() {
    let _process_table = process_table::tests::isolate_process_table();
    let mut scheduler = boxed_scheduler();
    let owner = test_process(64);

    let mut donor_context = test_user_context(owner);
    donor_context.weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[1] = Some(donor_context);
    scheduler.starts[1] = Some(TaskStart {
        entry: noop_task_entry,
        id: 640,
    });

    // A task cannot donate priority to itself, even with a live
    // reservation already pending for that exact donor (which is what
    // `bind_reserved_ipc_priority` would otherwise happily match).
    assert!(!scheduler.inherit_ipc_priority(20, 640, 640));
    assert!(scheduler.reserve_ipc_priority(640));
    assert!(
        !scheduler.bind_reserved_ipc_priority(20, 640, 640),
        "self-referential donation must be rejected even with a live reservation"
    );
    assert!(scheduler.cancel_ipc_priority_reservation(640));

    // A retired receiver can never gain a bound donation. `find_task_slot`
    // already hides retired slots, so a retired receiver id resolves to
    // no slot at all and `inherit_ipc_priority` must fail closed rather
    // than silently install an orphaned donation via its unreserved
    // upsert fallback.
    scheduler.contexts[2] = Some(test_user_context(owner));
    scheduler.starts[2] = Some(TaskStart {
        entry: noop_task_entry,
        id: 641,
    });
    scheduler.retired[2] = true;

    assert!(
        !scheduler.inherit_ipc_priority(22, 640, 641),
        "donation must not bind a retired receiver"
    );
    assert!(
        !scheduler
            .ipc_priority_donations
            .iter()
            .flatten()
            .any(|entry| entry.reply == 22),
        "a rejected donation must not leak an entry into the table"
    );
}

/// Bounded donation: the fixed-capacity donation table must reject
/// admission at exactly `MAX_TASK` live entries, never one past it -
/// the backing array has no slot beyond that bound.
#[test]
fn ipc_priority_donation_capacity_is_bounded_by_max_task() {
    let mut scheduler = boxed_scheduler();
    assert!(scheduler.ipc_priority_donation_capacity_available());
    scheduler.ipc_priority_donation_len = MAX_TASK - 1;
    assert!(scheduler.ipc_priority_donation_capacity_available());
    scheduler.ipc_priority_donation_len = MAX_TASK;
    assert!(!scheduler.ipc_priority_donation_capacity_available());
}

/// Bounded donation: a direct handoff floors the target's vruntime
/// toward the caller's (so it is picked promptly) but must never raise
/// it above what it already had - a donation can only help a receiver,
/// never push it backward in the fair queue.
#[test]
fn ipc_donation_floors_target_vruntime_and_never_raises_it() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let allocate = |scheduler: &mut Scheduler, task_id, offset| {
        scheduler
            .allocate_user_slot(
                task_id,
                ProcessAddressSpace::empty_for_tests(),
                UserTaskBootstrap::new(
                    UserAbi::Linux,
                    x86_64::VirtAddr::new(base + offset),
                    x86_64::VirtAddr::new(base + offset + 0x1_000),
                ),
                None,
                crate::arch::pit::divisor_from_micros(100),
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("user slot")
    };
    let caller = allocate(&mut scheduler, 661, 0x2_000);
    let target = allocate(&mut scheduler, 662, 0x4_000);
    scheduler.current_task = caller;
    scheduler.contexts[caller]
        .as_mut()
        .expect("caller context")
        .vruntime_ns = 10_000_000;
    scheduler.contexts[target]
        .as_mut()
        .expect("target context")
        .vruntime_ns = 50_000_000;

    scheduler.apply_ipc_donation(target);
    let floored = scheduler.contexts[target]
        .expect("target context")
        .vruntime_ns;
    assert!(
        floored <= 10_000_000,
        "donation must floor the target toward the caller, got {floored}"
    );

    // Whatever the computed floor is, a donation is a `min()` against the
    // target's current vruntime: it can only hold it steady or lower it,
    // never raise it above where it already stood.
    scheduler.contexts[target]
        .as_mut()
        .expect("target context")
        .vruntime_ns = 1_000;
    scheduler.apply_ipc_donation(target);
    assert!(
        scheduler.contexts[target]
            .expect("target context")
            .vruntime_ns
            <= 1_000,
        "donation must never raise a target's vruntime above its prior value"
    );

    // The donated floor is the *tighter* (smaller) of the caller's own
    // floor and the target class's floor, not the looser one: swapping
    // `min` for `max` here cannot be caught by a bound that only checks
    // "never raised", because the outer `.min()` against the target's
    // prior value still absorbs an over-large floor. Isolate the two
    // floor sources (caller is System class so it drops out of the
    // target's own User-class scan) and pin the exact result.
    scheduler.contexts[caller]
        .as_mut()
        .expect("caller context")
        .weight = SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    scheduler.contexts[caller]
        .as_mut()
        .expect("caller context")
        .vruntime_ns = 20_000_000;
    scheduler.contexts[target]
        .as_mut()
        .expect("target context")
        .vruntime_ns = 100_000_000;
    scheduler.apply_ipc_donation(target);
    assert_eq!(
        scheduler.contexts[target]
            .expect("target context")
            .vruntime_ns,
        18_000_000,
        "the donated floor must be the tighter (smaller) of the caller and class floors"
    );

    // A task cannot donate to itself: `target_slot == current_task_slot()`
    // must short-circuit before any vruntime mutation.
    let self_vruntime_before = scheduler.contexts[caller]
        .expect("caller context")
        .vruntime_ns;
    scheduler.apply_ipc_donation(caller);
    assert_eq!(
        scheduler.contexts[caller]
            .expect("caller context")
            .vruntime_ns,
        self_vruntime_before,
        "a task must never donate to itself"
    );

    // Idle-thread invariant: an idle-classed slot can never receive a
    // donation, even if it otherwise looks runnable.
    scheduler.idle_cpu[target] = 0;
    scheduler.contexts[target]
        .as_mut()
        .expect("target context")
        .vruntime_ns = 999_999;
    scheduler.apply_ipc_donation(target);
    assert_eq!(
        scheduler.contexts[target]
            .expect("target context")
            .vruntime_ns,
        999_999,
        "an idle-classed slot must never be donated to"
    );
}
