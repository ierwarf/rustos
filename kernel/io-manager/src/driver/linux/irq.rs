use core::ffi::{c_char, c_void};

use crate::driver::irq::LinuxIrqHandler;

const IRQ_NONE: i32 = 0;

pub(crate) unsafe extern "C" fn request_threaded_irq(
    irq: u32,
    handler: Option<LinuxIrqHandler>,
    thread_fn: Option<LinuxIrqHandler>,
    flags: u64,
    _name: *const c_char,
    dev_id: *mut c_void,
) -> i32 {
    crate::driver::symbol_events::record_irq_symbol(
        "request_threaded_irq",
        dev_id as usize,
        irq,
        flags,
    );
    crate::driver::irq::request_threaded_irq(irq, handler, thread_fn, flags, dev_id)
}

pub(crate) unsafe extern "C" fn request_any_context_irq(
    irq: u32,
    handler: Option<LinuxIrqHandler>,
    flags: u64,
    _name: *const c_char,
    dev_id: *mut c_void,
) -> i32 {
    crate::driver::symbol_events::record_irq_symbol(
        "request_any_context_irq",
        dev_id as usize,
        irq,
        flags,
    );
    crate::driver::irq::request_any_context_irq(irq, handler, flags, dev_id)
}

pub(crate) unsafe extern "C" fn free_irq(irq: u32, dev_id: *mut c_void) -> *const c_void {
    crate::driver::symbol_events::record_irq_symbol("free_irq", dev_id as usize, irq, 0);
    crate::driver::irq::free_irq(irq, dev_id)
}

pub(crate) unsafe extern "C" fn devm_request_threaded_irq(
    dev: *mut c_void,
    irq: u32,
    handler: Option<LinuxIrqHandler>,
    thread_fn: Option<LinuxIrqHandler>,
    flags: u64,
    name: *const c_char,
    dev_id: *mut c_void,
) -> i32 {
    crate::driver::symbol_events::record_irq_symbol(
        "devm_request_threaded_irq",
        dev as usize,
        irq,
        flags,
    );
    let status = unsafe { request_threaded_irq(irq, handler, thread_fn, flags, name, dev_id) };
    if status == 0 {
        crate::driver::devres::register_irq(dev, irq, dev_id);
    }
    status
}

pub(crate) unsafe extern "C" fn devm_free_irq(dev: *mut c_void, irq: u32, dev_id: *mut c_void) {
    crate::driver::symbol_events::record_irq_symbol("devm_free_irq", dev as usize, irq, 0);
    crate::driver::devres::forget_irq(dev, irq, dev_id);
    let _ = crate::driver::irq::free_irq(irq, dev_id);
}

pub(crate) unsafe extern "C" fn irq_wake_thread(irq: u32, dev_id: *mut c_void) {
    crate::driver::irq::wake_thread(irq, dev_id);
}

pub(crate) unsafe extern "C" fn enable_irq(irq: u32) {
    crate::driver::irq::enable_irq(irq);
}

pub(crate) unsafe extern "C" fn disable_irq(irq: u32) {
    crate::driver::irq::disable_irq(irq);
}

pub(crate) unsafe extern "C" fn disable_irq_nosync(irq: u32) {
    crate::driver::irq::disable_irq(irq);
}

pub(crate) unsafe extern "C" fn synchronize_irq(_irq: u32) {}

pub(crate) unsafe extern "C" fn no_action(_irq: i32, _dev_id: *mut c_void) -> i32 {
    IRQ_NONE
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    match name {
        "request_threaded_irq" => Some(request_threaded_irq as *const () as usize),
        "request_any_context_irq" => Some(request_any_context_irq as *const () as usize),
        "free_irq" => Some(free_irq as *const () as usize),
        "devm_request_threaded_irq" => Some(devm_request_threaded_irq as *const () as usize),
        "devm_free_irq" => Some(devm_free_irq as *const () as usize),
        "irq_wake_thread" => Some(irq_wake_thread as *const () as usize),
        "enable_irq" => Some(enable_irq as *const () as usize),
        "disable_irq" => Some(disable_irq as *const () as usize),
        "disable_irq_nosync" => Some(disable_irq_nosync as *const () as usize),
        "synchronize_irq" => Some(synchronize_irq as *const () as usize),
        "no_action" => Some(no_action as *const () as usize),
        _ => None,
    }
}
