use core::{cell::Cell, mem, ptr};
use x86_64::VirtAddr;
use x86_64::instructions::{hlt, interrupts};
use x86_64::registers::rflags::RFlags;
use x86_64::registers::segmentation::SegmentSelector;
use x86_64::registers::segmentation::{CS, SS, Segment};
use x86_64::structures::idt::InterruptStackFrameValue;

mod register_macros;

const MAX_TASK: usize = 200;
const TASK_STACK_SIZE: usize = 4 * 1024;

const SAVED_GPR_BYTES: usize = 15 * 8;
const SAVED_XMM_BYTES: usize = 16 * 16;
const CONTEXT_PREFIX_BYTES: usize = SAVED_GPR_BYTES + SAVED_XMM_BYTES; // 0x178

const _: [(); 0x78] = [(); SAVED_GPR_BYTES];
const _: [(); 0x100] = [(); SAVED_XMM_BYTES];
const _: [(); 0x178] = [(); CONTEXT_PREFIX_BYTES];

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct InterruptContext {
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
    frame: InterruptStackFrameValue,
}

const _: [(); 0x78] = [(); mem::offset_of!(InterruptContext, xmm)];
const _: [(); 0x178] = [(); mem::offset_of!(InterruptContext, frame)];
const _: [(); 0x1a0] = [(); mem::size_of::<InterruptContext>()];

impl InterruptContext {
    const fn zeroed() -> Self {
        unsafe { mem::zeroed() }
    }
}

#[derive(Clone, Copy)]
struct TaskContext {
    regs: InterruptContext,
    ready: bool,
}

impl TaskContext {
    const fn empty() -> Self {
        Self {
            regs: InterruptContext::zeroed(),
            ready: false,
        }
    }

    fn for_entry(entry_rip: u64, stack_top: u64, cs: SegmentSelector, ss: SegmentSelector) -> Self {
        let mut context = Self::empty();
        context.regs.frame = InterruptStackFrameValue::new(
            VirtAddr::new(entry_rip),
            cs,
            initial_task_rflags(),
            VirtAddr::new(stack_top),
            ss,
        );
        context.ready = true;
        context
    }
}

#[derive(Clone, Copy)]
struct TaskStart {
    entry: fn(u16),
    id: u16,
}

static mut CONTEXTS: [Option<TaskContext>; MAX_TASK] = [None; MAX_TASK];
static mut START_INFO: [Option<TaskStart>; MAX_TASK] = [None; MAX_TASK];
static mut CURRENT_TASK: usize = 0;
static mut TASK_STACKS: [[u8; TASK_STACK_SIZE]; MAX_TASK] = [[0; TASK_STACK_SIZE]; MAX_TASK];

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
        interrupts::without_interrupts(|| {
            if self.slot.get().is_some() {
                return;
            }

            let slot = allocate_slot(self.entry, self.id).expect("No free task slot");
            self.slot.set(Some(slot));
        });
    }

    #[allow(dead_code)]
    pub fn stop(&self) {
        interrupts::without_interrupts(|| {
            let Some(slot) = self.slot.replace(None) else {
                return;
            };

            unsafe {
                CONTEXTS[slot] = None;
                START_INFO[slot] = None;
            }
        });
    }
}

fn find_next_task_index(current: usize) -> Option<usize> {
    for offset in 1..=MAX_TASK {
        let idx = (current + offset) % MAX_TASK;
        unsafe {
            if let Some(ctx) = CONTEXTS[idx] {
                if ctx.ready {
                    return Some(idx);
                }
            }
        }
    }
    None
}

fn allocate_slot(entry: fn(u16), id: u16) -> Option<usize> {
    unsafe {
        let cs = CS::get_reg();
        let ss = SS::get_reg();

        for slot in 1..MAX_TASK {
            if CONTEXTS[slot].is_none() {
                CONTEXTS[slot] = Some(TaskContext::for_entry(
                    task_entry_trampoline as *const () as usize as u64,
                    init_task_stack(slot),
                    cs,
                    ss,
                ));
                START_INFO[slot] = Some(TaskStart { entry, id });
                return Some(slot);
            }
        }
    }
    None
}

fn initial_task_rflags() -> RFlags {
    const RESERVED_BIT_1: u64 = 1 << 1;
    RFlags::from_bits_retain(RESERVED_BIT_1 | RFlags::INTERRUPT_FLAG.bits())
}

fn init_task_stack(slot: usize) -> u64 {
    unsafe {
        let base = TASK_STACKS[slot].as_ptr() as u64;
        let mut top = base + TASK_STACK_SIZE as u64;
        top &= !0xF;
        top -= 8;
        ptr::write(top as *mut u64, 0);
        top
    }
}

extern "C" fn task_entry_trampoline() -> ! {
    let (entry, id) = unsafe {
        let task = START_INFO[CURRENT_TASK].expect("missing task start info");
        (task.entry, task.id)
    };

    entry(id);
    exit_current_task();
}

fn exit_current_task() -> ! {
    interrupts::without_interrupts(|| unsafe {
        CONTEXTS[CURRENT_TASK] = None;
        START_INFO[CURRENT_TASK] = None;
    });

    loop {
        hlt();
    }
}

pub fn init(timer_interval_ms: f64) {
    unsafe {
        CURRENT_TASK = 0;
        CONTEXTS[0] = Some(TaskContext::empty());
        START_INFO[0] = None;
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
extern "C" fn timer_interrupt_dispatch(context_ptr: *mut InterruptContext) {
    unsafe {
        let context = &mut *context_ptr;

        if let Some(current) = CONTEXTS[CURRENT_TASK].as_mut() {
            current.regs = *context;
            current.ready = true;
        }

        if let Some(next_idx) = find_next_task_index(CURRENT_TASK) {
            CURRENT_TASK = next_idx;
            if let Some(next) = CONTEXTS[next_idx] {
                *context = next.regs;
            }
        }
    }

    crate::pic::send_eoi(crate::pic::PIC_1_OFFSET);
}
