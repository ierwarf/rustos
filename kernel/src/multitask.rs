use core::{cell::Cell, ptr};
use x86_64::instructions::{hlt, interrupts};
use x86_64::structures::idt::InterruptStackFrame;
use x86_64::VirtAddr;

const MAX_TASK: usize = 200;
const TASK_STACK_SIZE: usize = 4 * 1024;

#[derive(Clone, Copy)]
struct TaskContext {
    rsp: u64,
    rip: u64,
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

pub fn task_switch(mut stack_frame: InterruptStackFrame, vector: u8, _error_code: Option<u64>) {
    unsafe {
        if let Some(current) = CONTEXTS[CURRENT_TASK].as_mut() {
            current.rsp = stack_frame.stack_pointer.as_u64();
            current.rip = stack_frame.instruction_pointer.as_u64();
        }

        if let Some(next_idx) = find_next_task_index(CURRENT_TASK) {
            CURRENT_TASK = next_idx;

            if let Some(next) = CONTEXTS[next_idx] {
                stack_frame.as_mut().update(|frame| {
                    frame.stack_pointer = VirtAddr::new(next.rsp);
                    frame.instruction_pointer = VirtAddr::new(next.rip);
                });
            }
        }
    }

    crate::pic::send_eoi(vector);
}

fn find_next_task_index(current: usize) -> Option<usize> {
    for offset in 1..=MAX_TASK {
        let idx = (current + offset) % MAX_TASK;
        unsafe {
            if CONTEXTS[idx].is_some() {
                return Some(idx);
            }
        }
    }
    None
}

fn allocate_slot(entry: fn(u16), id: u16) -> Option<usize> {
    unsafe {
        for slot in 1..MAX_TASK {
            if CONTEXTS[slot].is_none() {
                CONTEXTS[slot] = Some(TaskContext {
                    rsp: init_task_stack(slot),
                    rip: task_entry_trampoline as *const () as usize as u64,
                });
                START_INFO[slot] = Some(TaskStart { entry, id });
                return Some(slot);
            }
        }
    }
    None
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

pub fn init() {
    unsafe {
        CURRENT_TASK = 0;
        CONTEXTS[0] = Some(TaskContext { rsp: 0, rip: 0 });
        START_INFO[0] = None;
    }
}

pub fn start_scheduler(timer_interval_ms: u8) {
    crate::pic::enable_irq(0);
    crate::pit::start(0, timer_interval_ms);
    interrupts::enable();
}
