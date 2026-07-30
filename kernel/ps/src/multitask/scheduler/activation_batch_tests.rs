//! Focused scheduler proof for atomic child activation and first-turn custody.

use super::tests::boxed_scheduler;
use super::{INTERACTIVE_PIT_DIVISOR_FLAG, RFLAGS_RESERVED_BIT_1, Scheduler};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, noop_task_entry};
use crate::user::abi::UserAbi;

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
    assert_eq!(scheduler.spawn_pick_hints.len(), 0);
    assert!(!scheduler.activate_suspended_user_tasks(&[802, 802]));
    assert!(scheduler.start_suspended[first]);
    assert!(scheduler.start_suspended[second]);
    assert_eq!(scheduler.spawn_pick_hints.len(), 0);
    scheduler.set_next_spawn_pick_hint(804);
    assert!(scheduler.activate_suspended_user_tasks(&[802, 803]));
    assert_eq!(scheduler.atomic_activation_handoff_remaining, 2);
    assert_eq!(scheduler.atomic_activation_pick_hints.len(), 2);
    assert_eq!(scheduler.spawn_pick_hints.len(), 1);
    assert!(scheduler.set_next_synchronous_pick_hint(801));
    scheduler.set_next_spawn_pick_hint(802);
    scheduler.set_next_pick_hint(802);
    assert_eq!(scheduler.spawn_pick_hints.len(), 2);
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
