use core::{cell::Cell, mem, ptr};
use x86_64::instructions::{hlt, interrupts};
use x86_64::registers::rflags::RFlags;
use x86_64::registers::segmentation::{CS, SS, Segment};

const MAX_TASK: usize = 32;
const TASK_STACK_SIZE: usize = 16 * 1024;

const SAVED_GPR_BYTES: usize = 15 * 8;
const SAVED_XMM_BYTES: usize = 16 * 16;
const CONTEXT_PREFIX_BYTES: usize = SAVED_GPR_BYTES + SAVED_XMM_BYTES; // 0x178
const IRET_FRAME_BYTES: usize = 3 * 8;
const SAVED_CONTEXT_BYTES: usize = CONTEXT_PREFIX_BYTES + IRET_FRAME_BYTES; // 0x190
const TASK_ENTRY_STACK_RESERVE_QWORDS: usize = 3;

const _: [(); 0x78] = [(); SAVED_GPR_BYTES];
const _: [(); 0x100] = [(); SAVED_XMM_BYTES];
const _: [(); 0x178] = [(); CONTEXT_PREFIX_BYTES];
const _: [(); 0x18] = [(); IRET_FRAME_BYTES];
const _: [(); 0x190] = [(); SAVED_CONTEXT_BYTES];

#[repr(C)]
#[derive(Clone, Copy)]
struct SavedContext {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
    xmm: [[u8; 16]; 16],
    rip: u64,
    cs: u64,
    rflags: u64,
}

const _: [(); 0x78] = [(); mem::offset_of!(SavedContext, xmm)];
const _: [(); 0x178] = [(); mem::offset_of!(SavedContext, rip)];
const _: [(); 0x180] = [(); mem::offset_of!(SavedContext, cs)];
const _: [(); 0x188] = [(); mem::offset_of!(SavedContext, rflags)];
const _: [(); 0x190] = [(); mem::size_of::<SavedContext>()];

#[derive(Clone, Copy)]
struct TaskContext {
    saved_rsp: usize,
    ready: bool,
}

#[derive(Clone, Copy)]
struct TaskStart {
    entry: fn(u16),
    id: u16,
}

struct Scheduler {
    contexts: [Option<TaskContext>; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    current_task: usize,
    stacks: [[u8; TASK_STACK_SIZE]; MAX_TASK],
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            contexts: [None; MAX_TASK],
            starts: [None; MAX_TASK],
            current_task: 0,
            stacks: [[0; TASK_STACK_SIZE]; MAX_TASK],
        }
    }

    fn reset(&mut self) {
        self.contexts = [None; MAX_TASK];
        self.starts = [None; MAX_TASK];
        self.current_task = 0;
        self.contexts[0] = Some(TaskContext {
            saved_rsp: 0,
            ready: true,
        });
    }

    fn clear_slot(&mut self, slot: usize) {
        self.contexts[slot] = None;
        self.starts[slot] = None;
    }

    fn stack_bounds(&self, slot: usize) -> (usize, usize) {
        let base = self.stacks[slot].as_ptr() as usize;
        (base, base + TASK_STACK_SIZE)
    }

    fn is_valid_saved_rsp(&self, slot: usize, saved_rsp: usize) -> bool {
        if saved_rsp == 0 {
            return false;
        }

        let align_mask = mem::align_of::<SavedContext>() - 1;
        if (saved_rsp & align_mask) != 0 {
            return false;
        }

        // Slot 0 uses inherited boot stack.
        if slot == 0 {
            return true;
        }

        if slot >= MAX_TASK {
            return false;
        }

        let (base, top) = self.stack_bounds(slot);
        let Some(frame_end) = saved_rsp.checked_add(SAVED_CONTEXT_BYTES) else {
            return false;
        };

        saved_rsp >= base && frame_end <= top
    }

    fn next_ready_task_index(&self, current: usize) -> Option<usize> {
        for offset in 1..=MAX_TASK {
            let idx = (current + offset) % MAX_TASK;
            if let Some(ctx) = self.contexts[idx] {
                if ctx.ready && self.is_valid_saved_rsp(idx, ctx.saved_rsp) {
                    return Some(idx);
                }
            }
        }
        None
    }

    fn allocate_slot(
        &mut self,
        entry: fn(u16),
        id: u16,
        cs: u64,
        ss: u64,
        rflags: u64,
    ) -> Option<usize> {
        for slot in 1..MAX_TASK {
            if self.contexts[slot].is_none() {
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self.init_task_context(slot, cs, ss, rflags),
                    ready: true,
                });
                self.starts[slot] = Some(TaskStart { entry, id });
                return Some(slot);
            }
        }
        None
    }

    fn init_task_context(&mut self, slot: usize, cs: u64, ss: u64, rflags: u64) -> usize {
        let (_, top) = self.stack_bounds(slot);
        let stack_top = top & !0xF;

        // Reserve 24 bytes so task entry starts with SysV 16-byte alignment expectations.
        // The first 16 bytes also serve as optional iret RSP/SS slots when needed.
        let task_rsp = stack_top - TASK_ENTRY_STACK_RESERVE_QWORDS * mem::size_of::<u64>();
        unsafe {
            let stack_slots = task_rsp as *mut u64;
            ptr::write(stack_slots, task_rsp as u64);
            ptr::write(stack_slots.add(1), ss);
            ptr::write(stack_slots.add(2), 0);
        }

        let context_ptr = task_rsp - mem::size_of::<SavedContext>();
        let context = context_ptr as *mut SavedContext;

        unsafe {
            ptr::write_bytes(context as *mut u8, 0, mem::size_of::<SavedContext>());
            (*context).rip = task_entry_trampoline as *const () as usize as u64;
            (*context).cs = cs;
            (*context).rflags = rflags;
        }

        context_ptr
    }

    fn on_timer_interrupt(&mut self, current_rsp: usize) -> usize {
        let current_slot = self.current_task;
        let current_ready = self.is_valid_saved_rsp(current_slot, current_rsp);
        if let Some(current) = self.contexts[current_slot].as_mut() {
            current.saved_rsp = current_rsp;
            current.ready = current_ready;
        }

        let next_idx = self
            .next_ready_task_index(self.current_task)
            .unwrap_or(self.current_task);

        if let Some(next) = self.contexts[next_idx] {
            if self.is_valid_saved_rsp(next_idx, next.saved_rsp) {
                self.current_task = next_idx;
                return next.saved_rsp;
            }
        }

        current_rsp
    }

    fn current_task_start(&self) -> Option<TaskStart> {
        self.starts[self.current_task]
    }

    fn exit_current_task(&mut self) {
        self.clear_slot(self.current_task);
    }
}

static mut SCHEDULER: Scheduler = Scheduler::new();

#[inline(always)]
unsafe fn scheduler_mut() -> &'static mut Scheduler {
    unsafe { &mut *ptr::addr_of_mut!(SCHEDULER) }
}

#[inline(always)]
unsafe fn scheduler_ref() -> &'static Scheduler {
    unsafe { &*ptr::addr_of!(SCHEDULER) }
}

pub struct Thread {
    entry: fn(u16),
    id: u16,
    slot: Cell<Option<usize>>,
}

impl Thread {
    pub fn new(entry: fn(u16), id: u16) -> Self {
        Self {
            entry,
            id,
            slot: Cell::new(None),
        }
    }

    pub fn start(&self) {
        interrupts::without_interrupts(|| unsafe {
            if self.slot.get().is_some() {
                return;
            }

            let cs = CS::get_reg().0 as u64;
            let ss = SS::get_reg().0 as u64;
            let rflags = initial_task_rflags().bits();
            let slot = scheduler_mut()
                .allocate_slot(self.entry, self.id, cs, ss, rflags)
                .expect("No free task slot");
            self.slot.set(Some(slot));
        });
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        interrupts::without_interrupts(|| unsafe {
            let Some(slot) = self.slot.replace(None) else {
                return;
            };

            scheduler_mut().clear_slot(slot);
        });
    }
}

fn initial_task_rflags() -> RFlags {
    const RESERVED_BIT_1: u64 = 1 << 1;
    RFlags::from_bits_retain(RESERVED_BIT_1 | RFlags::INTERRUPT_FLAG.bits())
}

extern "C" fn task_entry_trampoline() -> ! {
    let task = interrupts::without_interrupts(|| unsafe { scheduler_ref().current_task_start() });
    let Some(task) = task else {
        exit_current_task();
    };

    (task.entry)(task.id);
    exit_current_task();
}

fn exit_current_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        scheduler_mut().exit_current_task();
    });

    loop {
        hlt();
    }
}

pub fn init(timer_interval_ms: f64) {
    unsafe {
        scheduler_mut().reset();
    }

    crate::pit::start(0, timer_interval_ms);
}

unsafe extern "C" {
    fn timer_interrupt_handler();
}

pub fn timer_interrupt_handler_addr() -> u64 {
    timer_interrupt_handler as *const () as usize as u64
}

#[unsafe(no_mangle)]
extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    let current_rsp = context_ptr as usize;
    let next_rsp = unsafe { scheduler_mut().on_timer_interrupt(current_rsp) };

    crate::pic::send_eoi(crate::pic::PIC_1_OFFSET);
    next_rsp as *mut SavedContext
}
