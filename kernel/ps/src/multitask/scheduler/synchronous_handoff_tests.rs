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

use alloc::boxed::Box;

use super::{MAX_CONSECUTIVE_SYNC_HANDOFFS, Scheduler};
use crate::memory::paging::ProcessAddressSpace;
use crate::multitask::{UserTaskBootstrap, noop_task_entry};
use crate::user::abi::UserAbi;

static TEST_SCHEDULER_TEMPLATE: Scheduler = Scheduler::new();

fn boxed_scheduler() -> Box<Scheduler> {
    let mut scheduler = Box::<Scheduler>::new_uninit();
    unsafe {
        // SAFETY: the source is one fully initialized immutable Scheduler;
        // destination is a disjoint allocation of exactly the same type.
        core::ptr::copy_nonoverlapping(
            core::ptr::addr_of!(TEST_SCHEDULER_TEMPLATE),
            scheduler.as_mut_ptr(),
            1,
        );
        scheduler.assume_init()
    }
}

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
    assert_eq!(scheduler.current_dispatch_policy().sync_pick_hints.len(), 2);
    assert_eq!(
        scheduler.mandatory_overdue_system_pick(current, now_ticks),
        Some(overdue)
    );
    assert_eq!(
        scheduler
            .take_next_synchronous_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(first)
    );
    scheduler.record_synchronous_handoff(true);
    assert_eq!(
        scheduler
            .take_next_synchronous_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(second)
    );

    assert!(scheduler.set_next_synchronous_pick_hint(912));
    assert!(scheduler.set_next_synchronous_pick_hint(913));
    scheduler.current_dispatch_policy_mut().sync_handoff_streak = MAX_CONSECUTIVE_SYNC_HANDOFFS;
    assert_eq!(
        scheduler
            .take_next_synchronous_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        None
    );
    assert_eq!(scheduler.current_dispatch_policy().sync_pick_hints.len(), 2);
    scheduler.record_synchronous_handoff(false);
    assert_eq!(
        scheduler
            .take_next_synchronous_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        Some(first)
    );
    scheduler.clear_slot(second);
    assert_eq!(
        scheduler
            .take_next_synchronous_pick_hint_ready_slot(&mut scheduler.current_dispatch_policy()),
        None
    );
}
