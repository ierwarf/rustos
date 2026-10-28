use kernel_ps::api as ps_api;

use crate::{boot, flow_debug, flow_info, io_services};

pub(super) fn init_bootstrap_task(_id: u64) {
    flow_debug(50, "init bootstrap task entered");
    ps_api::yield_now();
    io_services::enter_userspace_runtime();
    flow_info(51, "userspace runtime phase entered");
    boot::bootstrap_init_process();
}

pub(super) fn nucleus_housekeeping_task(_id: u64) {
    flow_debug(60, "housekeeping task entered");
    ps_api::register_housekeeping_task();
    loop {
        x86_64::instructions::interrupts::enable();
        // Clear before scanning: work published after this point re-arms it.
        ps_api::take_housekeeping_kick();
        let work = boot::housekeeping_once();
        finish_background_turn(work, ps_api::yield_now, housekeeping_idle_wait);
    }
}

/// Dedicated normal-context producer for the IRQ-off anonymous-fault reserve.
///
/// It is deliberately not folded into generic housekeeping: a low-water bit
/// forces a safe user-frame scheduling boundary, this task refills the complete
/// bounded pool, and only then yields.  Faulting tasks have already resumed
/// through their prepared-leaf CAS before this task can run.
pub(super) fn nucleus_pager_fault_refill_task(_id: u64) {
    flow_debug(61, "pager fault refill task entered");
    let mut passes = 0_u64;
    let mut oom_watchdog = kernel_compat::api::oom::OomWatchdog::new();
    loop {
        x86_64::instructions::interrupts::enable();
        let mut work = kernel_mm::api::frame_capability::service_pager_fault_frame_refill();
        // The ring0 fallback for rootd's memory-pressure policy. It runs here
        // because this is the one normal-context task that already wakes when
        // faults are refused frames; it rate-limits itself to one look per
        // 100 ms and does nothing unless the refusals persist.
        work += kernel_compat::api::oom::service(&mut oom_watchdog);
        // One periodic census from the one task that already wakes on fault
        // pressure. The individual fault counters each report on their own
        // first occurrence and then at a stride, which cannot distinguish "the
        // path served nothing" from "the path was never entered" - and that
        // ambiguity cost a whole debugging pass.
        passes = passes.wrapping_add(1);
        if passes.is_multiple_of(256) {
            kernel_compat::api::pager::record_anonymous_fault_census();
        }
        finish_background_turn(work, ps_api::yield_now, idle_tick_wait);
    }
}

fn housekeeping_idle_wait() {
    // Sleep a tick, or less when a producer kicks: retirement, reap and thread
    // join work should not wait for the next timer edge.
    kernel_hal::api::arch::rtc::sleep_unless(1, ps_api::housekeeping_kicked);
}

fn idle_tick_wait() {
    // This is an ordinary runnable worker, not the CPU's idle owner. HLT here
    // can stop a CPU with eligible user work, and the halted interval is billed
    // to this worker's System context. The existing exact RTC arm/commit/wake
    // parks only this continuation, leaving the processor to the scheduler.
    kernel_hal::api::arch::rtc::sleep(1);
}

fn finish_background_turn(work: usize, yield_now: impl FnOnce(), idle_wait: impl FnOnce()) {
    if work == 0 {
        idle_wait();
    } else {
        yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::finish_background_turn;
    use core::cell::Cell;

    #[test]
    fn background_idle_wait_is_the_only_idle_action_not_a_post_yield_cpu_halt() {
        let actions = Cell::new(0_u8);
        finish_background_turn(
            0,
            || actions.set(actions.get() * 10 + 1),
            || actions.set(actions.get() * 10 + 2),
        );
        assert_eq!(
            actions.get(),
            2,
            "idle continuation must park without an earlier runnable yield"
        );
        actions.set(0);
        finish_background_turn(
            1,
            || actions.set(actions.get() * 10 + 1),
            || actions.set(actions.get() * 10 + 2),
        );
        assert_eq!(
            actions.get(),
            1,
            "a productive worker yields instead of sleeping"
        );
    }
}
