//! Focused scheduler proof for atomic child activation and first-turn custody.

use super::tests::boxed_scheduler;
use super::{INTERACTIVE_PIT_DIVISOR_FLAG, RFLAGS_RESERVED_BIT_1, Scheduler};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, noop_task_entry};
use crate::user::abi::UserAbi;
use core::sync::atomic::{AtomicBool, Ordering};

#[test]
fn spawn_handoff_is_fifo_deduplicated_and_precedes_ipc_handoff() {
    let mut scheduler = boxed_scheduler();
    let base = crate::memory::paging::USER_SPACE_BASE;
    let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
    let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
    let allocate = |scheduler: &mut Scheduler, task_id, offset, suspended| {
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
                crate::arch::pit::divisor_from_micros(2_000) | INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                RFLAGS_RESERVED_BIT_1,
                suspended,
                noop_task_entry,
            )
            .expect("task slot")
    };
    let parent = allocate(&mut scheduler, 801, 0x2_000, false);
    let first = allocate(&mut scheduler, 802, 0x4_000, true);
    let second = allocate(&mut scheduler, 803, 0x6_000, true);
    let ordinary = allocate(&mut scheduler, 804, 0x8_000, false);
    scheduler.current_task = parent;

    assert!(!scheduler.activate_suspended_user_tasks(&[802, 999]));
    assert!(scheduler.start_suspended[first]);
    assert!(scheduler.start_suspended[second]);
    assert_eq!(
        scheduler.current_dispatch_policy().spawn_pick_hints.len(),
        0
    );
    assert!(!scheduler.activate_suspended_user_tasks(&[802, 802]));
    assert!(scheduler.start_suspended[first]);
    assert!(scheduler.start_suspended[second]);
    assert_eq!(
        scheduler.current_dispatch_policy().spawn_pick_hints.len(),
        0
    );
    let authority_commit_started = AtomicBool::new(false);
    let failed_commit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.activate_suspended_user_tasks_with_commit(&[802, 803], || {
            authority_commit_started.store(true, Ordering::Release);
            panic!("injected authority commit failure");
        })
    }));
    assert!(failed_commit.is_err());
    assert!(authority_commit_started.load(Ordering::Acquire));
    assert!(scheduler.start_suspended[first]);
    assert!(scheduler.start_suspended[second]);
    assert_eq!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_handoff_remaining,
        0
    );
    assert!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_pick_hints
            .is_empty()
    );
    scheduler.set_next_spawn_pick_hint(804);
    assert!(scheduler.activate_suspended_user_tasks(&[802, 803]));
    assert_eq!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_handoff_remaining,
        2
    );
    assert_eq!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_pick_hints
            .len(),
        2
    );
    assert_eq!(
        scheduler.current_dispatch_policy().spawn_pick_hints.len(),
        1
    );
    assert!(scheduler.set_next_synchronous_pick_hint(801));
    scheduler.set_next_spawn_pick_hint(802);
    scheduler.set_next_pick_hint(802);
    assert_eq!(
        scheduler.current_dispatch_policy().spawn_pick_hints.len(),
        2
    );
    assert_eq!(
        scheduler.take_next_atomic_activation_handoff_ready_slot(),
        Some(first)
    );
    assert_eq!(
        scheduler.take_next_atomic_activation_handoff_ready_slot(),
        Some(second)
    );
    assert_eq!(
        scheduler.take_next_atomic_activation_handoff_ready_slot(),
        None
    );
    assert_eq!(
        scheduler.take_next_synchronous_pick_hint_ready_slot(),
        Some(parent)
    );
    assert_eq!(scheduler.take_next_pick_hint_ready_slot(), Some(first));
    assert_eq!(
        scheduler.take_next_spawn_pick_hint_ready_slot(),
        Some(ordinary)
    );
    scheduler.set_next_spawn_pick_hint(802);
    scheduler.set_next_spawn_pick_hint(803);
    scheduler.clear_slot(first);
    assert_eq!(
        scheduler.take_next_spawn_pick_hint_ready_slot(),
        Some(second)
    );
}

#[test]
fn authority_commit_is_checked_while_the_complete_cohort_is_still_suspended() {
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
                crate::arch::pit::divisor_from_micros(2_000) | INTERACTIVE_PIT_DIVISOR_FLAG,
                user_cs,
                user_ss,
                RFLAGS_RESERVED_BIT_1,
                true,
                noop_task_entry,
            )
            .expect("suspended task slot")
    };
    let first = allocate(&mut scheduler, 901, 0x2_000);
    let second = allocate(&mut scheduler, 902, 0x4_000);
    let authority_consumed = AtomicBool::new(false);

    assert!(
        scheduler.activate_suspended_user_tasks_with_commit(&[901, 902], || {
            authority_consumed.store(true, Ordering::Release);
        })
    );

    // The activation helper executes its post-callback suspension assertion
    // before publication. A regression that publishes before consuming the
    // authority therefore panics in this call rather than merely changing a
    // syntactic source-order test.
    assert!(authority_consumed.load(Ordering::Acquire));
    assert!(!scheduler.start_suspended[first]);
    assert!(!scheduler.start_suspended[second]);
    assert_eq!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_handoff_remaining,
        2
    );
    assert_eq!(
        scheduler
            .current_dispatch_policy()
            .atomic_activation_pick_hints
            .len(),
        2
    );
}
