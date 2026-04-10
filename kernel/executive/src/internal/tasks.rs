use super::*;

pub(super) fn init_bootstrap_task(_id: u64) {
    super::flow_debug(50, "init bootstrap task entered");
    debug::println!("init bootstrap task: entered");
    ps_api::api::yield_now();
    io_manager_api::api::enter_userspace_runtime();
    super::flow_info(51, "userspace runtime phase entered");
    super::boot::bootstrap_init_process();
}

pub(super) fn nucleus_housekeeping_task(_id: u64) {
    super::flow_debug(60, "housekeeping task entered");
    debug::println!("nucleus loop: housekeeping task entered");
    loop {
        x86_64::instructions::interrupts::enable();
        let work = super::boot::housekeeping_once();
        ps_api::api::yield_now();
        if work == 0 {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }
}
