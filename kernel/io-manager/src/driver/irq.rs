use alloc::vec::Vec;
use core::ffi::c_void;

use spin::Mutex;
use x86_64::instructions::interrupts;

const PIC_IRQ_COUNT: usize = 16;
const IRQF_SHARED: u64 = 0x0000_0080;
const IRQF_NO_AUTOEN: u64 = 0x0008_0000;
const IRQ_NOTCONNECTED: u32 = 1_u32 << 31;

const IRQ_NONE: i32 = 0;
const IRQ_HANDLED: i32 = 1;
const IRQ_WAKE_THREAD: i32 = 2;

pub(crate) type LinuxIrqHandler = unsafe extern "C" fn(irq: i32, dev_id: *mut c_void) -> i32;

#[derive(Clone, Copy)]
struct IrqAction {
    handler: Option<LinuxIrqHandler>,
    thread_fn: Option<LinuxIrqHandler>,
    flags: u64,
    dev_id: usize,
}

static IRQ_ACTIONS: Mutex<[Vec<IrqAction>; PIC_IRQ_COUNT]> =
    Mutex::new([const { Vec::new() }; PIC_IRQ_COUNT]);

pub(crate) fn request_threaded_irq(
    irq: u32,
    handler: Option<LinuxIrqHandler>,
    thread_fn: Option<LinuxIrqHandler>,
    flags: u64,
    dev_id: *mut c_void,
) -> i32 {
    if irq == IRQ_NOTCONNECTED {
        return -107;
    }
    if irq as usize >= PIC_IRQ_COUNT {
        return -22;
    }
    if handler.is_none() && thread_fn.is_none() {
        return -22;
    }
    if (flags & IRQF_SHARED) != 0 && dev_id.is_null() {
        return -22;
    }

    let action = IrqAction {
        handler,
        thread_fn,
        flags,
        dev_id: dev_id as usize,
    };

    irq_safe(|| {
        let mut actions = IRQ_ACTIONS.lock();
        let slot = &mut actions[irq as usize];
        if !slot.is_empty() {
            let shared = slot
                .iter()
                .all(|existing| (existing.flags & IRQF_SHARED) != 0);
            if !shared || (flags & IRQF_SHARED) == 0 {
                return -16;
            }
            if slot.iter().any(|existing| existing.dev_id == action.dev_id) {
                return 0;
            }
        }
        slot.push(action);
        if (flags & IRQF_NO_AUTOEN) == 0 {
            crate::arch::pic::enable_irq(irq as u8);
        }
        0
    })
}

pub(crate) fn request_any_context_irq(
    irq: u32,
    handler: Option<LinuxIrqHandler>,
    flags: u64,
    dev_id: *mut c_void,
) -> i32 {
    let status = request_threaded_irq(irq, handler, None, flags, dev_id);
    if status == 0 { 0 } else { status }
}

pub(crate) fn free_irq(irq: u32, dev_id: *mut c_void) -> *const c_void {
    if irq as usize >= PIC_IRQ_COUNT {
        return core::ptr::null();
    }

    irq_safe(|| {
        let mut actions = IRQ_ACTIONS.lock();
        let slot = &mut actions[irq as usize];
        let Some(index) = slot
            .iter()
            .position(|action| action.dev_id == dev_id as usize)
        else {
            return core::ptr::null();
        };
        let removed = slot.remove(index);
        if slot.is_empty() {
            crate::arch::pic::disable_irq(irq as u8);
        }
        removed.dev_id as *const c_void
    })
}

pub(crate) fn enable_irq(irq: u32) {
    if irq as usize >= PIC_IRQ_COUNT {
        return;
    }
    crate::arch::pic::enable_irq(irq as u8);
}

pub(crate) fn disable_irq(irq: u32) {
    if irq as usize >= PIC_IRQ_COUNT {
        return;
    }
    crate::arch::pic::disable_irq(irq as u8);
}

pub(crate) fn wake_thread(irq: u32, dev_id: *mut c_void) {
    if irq as usize >= PIC_IRQ_COUNT {
        return;
    }

    let thread = irq_safe(|| {
        let actions = IRQ_ACTIONS.lock();
        actions[irq as usize]
            .iter()
            .find(|action| action.dev_id == dev_id as usize)
            .and_then(|action| action.thread_fn)
    });
    if let Some(thread) = thread {
        unsafe {
            let _ = thread(irq as i32, dev_id);
        }
    }
}

pub fn dispatch_pic_irq(irq: u8) -> bool {
    if irq as usize >= PIC_IRQ_COUNT {
        return false;
    }

    let actions = irq_safe(|| IRQ_ACTIONS.lock()[irq as usize].clone());
    let mut handled = false;
    for action in actions {
        let status = match action.handler {
            Some(handler) => unsafe { handler(irq as i32, action.dev_id as *mut c_void) },
            None => IRQ_WAKE_THREAD,
        };

        if status == IRQ_WAKE_THREAD {
            if let Some(thread_fn) = action.thread_fn {
                let thread_status = unsafe { thread_fn(irq as i32, action.dev_id as *mut c_void) };
                handled |= thread_status != IRQ_NONE;
            }
        } else {
            handled |= status == IRQ_HANDLED;
        }
    }
    handled
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
