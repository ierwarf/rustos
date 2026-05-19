use core::sync::atomic::{AtomicU64, Ordering};

use kernel_ps::api as ps_api;

use crate::{boot, flow_debug, flow_info, io_services};

static HOUSEKEEPING_ITER_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn housekeeping_iter_count() -> u64 {
    HOUSEKEEPING_ITER_COUNT.load(Ordering::Acquire)
}

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
        HOUSEKEEPING_ITER_COUNT.fetch_add(1, Ordering::AcqRel);
        ps_api::yield_now();
        if work == 0 {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}
