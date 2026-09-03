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
    loop {
        x86_64::instructions::interrupts::enable();
        let work = boot::housekeeping_once();
        ps_api::yield_now();
        if work == 0 {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
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
    loop {
        x86_64::instructions::interrupts::enable();
        let work = kernel_mm::api::frame_capability::service_pager_fault_frame_refill();
        // One periodic census from the one task that already wakes on fault
        // pressure. The individual fault counters each report on their own
        // first occurrence and then at a stride, which cannot distinguish "the
        // path served nothing" from "the path was never entered" - and that
        // ambiguity cost a whole debugging pass.
        passes = passes.wrapping_add(1);
        if passes.is_multiple_of(256) {
            kernel_compat::api::pager::record_anonymous_fault_census();
        }
        ps_api::yield_now();
        if work == 0 {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}
