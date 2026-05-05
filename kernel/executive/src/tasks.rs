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
