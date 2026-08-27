//! Host witnesses for synchronous IPC scheduler custody.
//!
//! - **Owner:** `kernel-ps` scheduler tests own call/reply FIFO and fairness
//!   evidence.
//! - **Boundary:** Tests use synthetic published task contexts only.
//! - **Lifecycle:** Complete, deduplicate, dispatch FIFO, force fairness,
//!   resume, and remove an exact retired peer.
//! - **Concurrency:** The checked queue is serialized exactly as production
//!   scheduler mutation is serialized.
//! - **Failure:** Overwrite, reordering, unbounded bursts, or stale survival
//!   fails the witness.
//! - **Forbidden:** No host timing assumptions or allocation in production.
//! - **Evidence:** `synchronous-ipc-handoff/SynchronousIpcHandoff`.

use super::tests::boxed_scheduler;
use super::{
    MAX_CONSECUTIVE_SYNC_HANDOFFS, NICE_0_LOAD, SYSTEM_CLASS_WEIGHT_FLAG, SchedClass, Scheduler,
    runqueue,
};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, noop_task_entry};
use crate::user::abi::UserAbi;

#[test]
fn synchronous_ipc_handoff_is_fifo_deduplicated_and_fairness_bounded() {
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
                crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("System task slot")
    };
    let current = allocate(&mut scheduler, 911, 0x2_000);
    let first = allocate(&mut scheduler, 912, 0x4_000);
    let second = allocate(&mut scheduler, 913, 0x6_000);
    let overdue = allocate(&mut scheduler, 914, 0x8_000);
    let now_ticks = crate::arch::rtc::ticks_per_second().saturating_mul(2);
    scheduler.contexts[overdue]
        .as_mut()
        .expect("overdue context")
        .ready_since_ticks = 1;
    scheduler.current_task = current;

    assert!(scheduler.set_next_synchronous_pick_hint(912));
    assert!(scheduler.set_next_synchronous_pick_hint(912));
    assert!(scheduler.set_next_synchronous_pick_hint(913));
    assert_eq!(scheduler.synchronous_handoff_len_for_tests(), 2);
    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        Some(overdue)
    );
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        Some(first)
    );
    scheduler.record_synchronous_handoff(true);
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        Some(second)
    );

    assert!(scheduler.set_next_synchronous_pick_hint(912));
    assert!(scheduler.set_next_synchronous_pick_hint(913));
    scheduler.set_synchronous_handoff_streak_for_tests(MAX_CONSECUTIVE_SYNC_HANDOFFS);
    assert_eq!(scheduler.take_next_synchronous_pick_hint_ready_slot(), None);
    assert_eq!(scheduler.synchronous_handoff_len_for_tests(), 2);
    scheduler.record_synchronous_handoff(false);
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        Some(first)
    );
    scheduler.clear_slot(second);
    assert_eq!(scheduler.take_next_synchronous_pick_hint_ready_slot(), None);

    assert!(scheduler.set_next_synchronous_pick_hint(912));
    scheduler.starts[first]
        .as_mut()
        .expect("live first start")
        .id = 915;
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        None,
        "the shared record predicate must reject a changed exact task identity"
    );
}

#[test]
fn reply_wake_token_mint_requires_exact_task_and_dispatch_custody() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let slot = scheduler
        .allocate_user_slot(
            921,
            ProcessAddressSpace::empty_for_tests(),
            UserTaskBootstrap::new(
                UserAbi::Linux,
                x86_64::VirtAddr::new(base + 0xa_000),
                x86_64::VirtAddr::new(base + 0xb_000),
            ),
            None,
            crate::arch::pit::divisor_from_micros(2_000) | super::INTERACTIVE_PIT_DIVISOR_FLAG,
            crate::arch::gdt::user_code_selector().0 as u64,
            crate::arch::gdt::user_data_selector().0 as u64,
            super::RFLAGS_RESERVED_BIT_1,
            false,
            noop_task_entry,
        )
        .expect("reply caller slot");
    let live = runqueue::RunOwnerSnapshot {
        state: runqueue::RunOwnerState::RemoteQueued,
        cpu: Some(2),
        generation: 7,
        runnable: true,
        wait_reason_kind: 0,
        wait_armed: false,
    };

    assert!(
        scheduler
            .reply_wake_handoff_from_owner(slot, 921, live)
            .is_some(),
        "the exact live reply caller mints one opaque handoff token"
    );
    assert!(
        scheduler
            .reply_wake_handoff_from_owner(slot, 922, live)
            .is_none(),
        "slot reuse identity substitution cannot mint a token"
    );
    assert!(
        scheduler
            .reply_wake_handoff_from_owner(
                slot,
                921,
                runqueue::RunOwnerSnapshot {
                    state: runqueue::RunOwnerState::Running,
                    ..live
                },
            )
            .is_none(),
        "an executing owner is not dispatch-handoff custody"
    );
}

#[test]
fn terminal_reply_releases_donation_and_wakes_exact_caller_in_one_scheduler_operation() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
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
                crate::arch::pit::divisor_from_micros(2_000),
                crate::arch::gdt::user_code_selector().0 as u64,
                crate::arch::gdt::user_data_selector().0 as u64,
                super::RFLAGS_RESERVED_BIT_1,
                false,
                noop_task_entry,
            )
            .expect("terminal reply task")
    };
    let donor = allocate(&mut scheduler, 931, 0xc_000);
    let caller = allocate(&mut scheduler, 932, 0xe_000);
    scheduler.contexts[donor].as_mut().expect("donor").weight =
        SYSTEM_CLASS_WEIGHT_FLAG | NICE_0_LOAD;
    assert!(scheduler.inherit_ipc_priority(77, 931, 932));
    assert_eq!(scheduler.slot_class(caller), Some(SchedClass::System));
    {
        let context = scheduler.contexts[caller].as_mut().expect("caller");
        context.test_ready = false;
        context.blocked = true;
        context.wake_armed = true;
    }

    assert!(
        scheduler.complete_ipc_reply_wake_handoff(77, 932).is_none(),
        "host runqueue isolation prevents only the post-wake production token"
    );
    assert_eq!(
        scheduler.slot_class(caller),
        Some(SchedClass::User),
        "the terminal reply must revoke its exact donation"
    );
    let context = scheduler.contexts[caller].expect("woken caller");
    assert!(context.test_ready);
    assert!(!context.blocked);
    assert!(!context.wake_armed);
}
