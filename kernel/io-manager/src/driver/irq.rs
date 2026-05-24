use alloc::vec::Vec;
use core::ffi::c_void;

use crate::sync::KernelSpinLock as Mutex;
use x86_64::instructions::interrupts;

const PIC_IRQ_COUNT: usize = 16;
const PENDING_THREADED_IRQ_CAPACITY: usize = 64;
const IRQF_SHARED: u64 = 0x0000_0080;
const IRQF_NO_AUTOEN: u64 = 0x0008_0000;
const IRQ_NOTCONNECTED: u32 = 1_u32 << 31;

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

#[derive(Clone, Copy)]
struct PendingThreadedIrq {
    irq: u32,
    thread_fn: LinuxIrqHandler,
    dev_id: usize,
}

struct PendingThreadedIrqQueue {
    entries: [Option<PendingThreadedIrq>; PENDING_THREADED_IRQ_CAPACITY],
    head: usize,
    len: usize,
}

impl PendingThreadedIrqQueue {
    const fn new() -> Self {
        Self {
            entries: [None; PENDING_THREADED_IRQ_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push_back(&mut self, pending: PendingThreadedIrq) -> bool {
        if self.len == self.entries.len() {
            return false;
        }
        let tail = (self.head + self.len) % self.entries.len();
        self.entries[tail] = Some(pending);
        self.len += 1;
        true
    }

    fn pop_front(&mut self) -> Option<PendingThreadedIrq> {
        if self.len == 0 {
            return None;
        }
        let pending = self.entries[self.head].take();
        self.head = (self.head + 1) % self.entries.len();
        self.len -= 1;
        pending
    }

    fn remove_matching(&mut self, irq: u32, dev_id: usize) {
        let mut retained = [None; PENDING_THREADED_IRQ_CAPACITY];
        let mut retained_len = 0usize;
        while let Some(pending) = self.pop_front() {
            if pending.irq == irq && pending.dev_id == dev_id {
                continue;
            }
            retained[retained_len] = Some(pending);
            retained_len += 1;
        }
        self.entries = retained;
        self.head = 0;
        self.len = retained_len;
    }
}

static IRQ_ACTIONS: Mutex<[Vec<IrqAction>; PIC_IRQ_COUNT]> =
    Mutex::new([const { Vec::new() }; PIC_IRQ_COUNT]);
static PENDING_THREADED_IRQS: Mutex<PendingThreadedIrqQueue> =
    Mutex::new(PendingThreadedIrqQueue::new());
static THREAD_QUEUE_FULL_DISABLED_IRQS: Mutex<[bool; PIC_IRQ_COUNT]> =
    Mutex::new([false; PIC_IRQ_COUNT]);

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
        remove_pending_threaded_irq(irq, dev_id as usize);
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
        enqueue_threaded_irq(irq, thread, dev_id as usize);
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
                handled |= enqueue_threaded_irq(irq as u32, thread_fn, action.dev_id);
            }
        } else {
            handled |= status == IRQ_HANDLED;
        }
    }
    handled
}

pub(crate) fn service_threaded_irqs() -> usize {
    let mut work = 0usize;
    while let Some(pending) = PENDING_THREADED_IRQS.lock().pop_front() {
        let _status =
            unsafe { (pending.thread_fn)(pending.irq as i32, pending.dev_id as *mut c_void) };
        work += 1;
    }
    reenable_thread_queue_full_irqs();
    work
}

fn enqueue_threaded_irq(irq: u32, thread_fn: LinuxIrqHandler, dev_id: usize) -> bool {
    if irq as usize >= PIC_IRQ_COUNT {
        return false;
    }
    let pending = PendingThreadedIrq {
        irq,
        thread_fn,
        dev_id,
    };
    if PENDING_THREADED_IRQS.lock().push_back(pending) {
        return true;
    }

    crate::arch::pic::disable_irq(irq as u8);
    THREAD_QUEUE_FULL_DISABLED_IRQS.lock()[irq as usize] = true;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "threaded-irq-queue-full",
        irq as u64,
        dev_id as u64,
    );
    crate::debug::error!(
        driver,
        "threaded irq queue full: irq={} dev_id={:#x}; irq disabled",
        irq,
        dev_id
    );
    true
}

fn reenable_thread_queue_full_irqs() {
    let mut disabled = THREAD_QUEUE_FULL_DISABLED_IRQS.lock();
    for irq in 0..PIC_IRQ_COUNT {
        if !disabled[irq] {
            continue;
        }
        disabled[irq] = false;
        crate::arch::pic::enable_irq(irq as u8);
    }
}

fn remove_pending_threaded_irq(irq: u32, dev_id: usize) {
    PENDING_THREADED_IRQS.lock().remove_matching(irq, dev_id);
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
