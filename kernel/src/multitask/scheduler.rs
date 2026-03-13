use alloc::boxed::Box;
use core::{mem, ptr};

use x86_64::PhysAddr;

use crate::asmtools::{FxSaveArea, restore_fxstate, save_fxstate};
use crate::debug;
use crate::paging::ProcessAddressSpace;

use super::context::{SAVED_CONTEXT_BYTES, SavedContext};
use super::UserTaskBootstrap;

pub(super) const MAX_TASK: usize = 32;
const TASK_STACK_SIZE: usize = 16 * 1024;
const TASK_ENTRY_STACK_RESERVE_QWORDS: usize = 3;

#[derive(Clone, Copy)]
enum TaskRetireReason {
    UserFault {
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
    },
    CorruptedContext {
        saved_rsp: usize,
    },
    Exited,
}

#[derive(Clone, Copy)]
struct TaskContext {
    saved_rsp: usize,
    ready: bool,
    pit_divisor: u16,
    address_space_root: u64,
    kernel_stack_top: u64,
    user_mode: bool,
    address_space_owner: *mut ProcessAddressSpace,
}

#[derive(Clone, Copy)]
pub(super) struct TaskStart {
    pub(super) entry: fn(u64),
    pub(super) id: u64,
}

pub(super) struct Scheduler {
    contexts: [Option<TaskContext>; MAX_TASK],
    retired: [bool; MAX_TASK],
    retire_reasons: [Option<TaskRetireReason>; MAX_TASK],
    last_errors: [u32; MAX_TASK],
    fx_states: [FxSaveArea; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    current_task: usize,
    stacks: [[u8; TASK_STACK_SIZE]; MAX_TASK],
}

impl Scheduler {
    pub(super) const fn new() -> Self {
        Self {
            contexts: [None; MAX_TASK],
            retired: [false; MAX_TASK],
            retire_reasons: [None; MAX_TASK],
            last_errors: [0; MAX_TASK],
            fx_states: [FxSaveArea::new(); MAX_TASK],
            starts: [None; MAX_TASK],
            current_task: 0,
            stacks: [[0; TASK_STACK_SIZE]; MAX_TASK],
        }
    }

    pub(super) fn reset(&mut self, main_thread_pit_divisor: u16) {
        for slot in 0..MAX_TASK {
            self.clear_slot(slot);
        }

        self.fx_states = [FxSaveArea::new(); MAX_TASK];
        self.retired = [false; MAX_TASK];
        self.retire_reasons = [None; MAX_TASK];
        self.last_errors = [0; MAX_TASK];
        self.current_task = 0;
        self.contexts[0] = Some(TaskContext {
            saved_rsp: 0,
            ready: true,
            pit_divisor: main_thread_pit_divisor,
            address_space_root: crate::paging::kernel_root_phys().as_u64(),
            kernel_stack_top: 0,
            user_mode: false,
            address_space_owner: ptr::null_mut(),
        });

        unsafe {
            save_fxstate(&mut self.fx_states[0]);
        }
    }

    pub(super) fn clear_slot(&mut self, slot: usize) {
        if let Some(context) = self.contexts[slot] {
            if !context.address_space_owner.is_null() {
                unsafe {
                    drop(Box::from_raw(context.address_space_owner));
                }
            }
        }

        self.contexts[slot] = None;
        self.retired[slot] = false;
        self.retire_reasons[slot] = None;
        self.last_errors[slot] = 0;
        self.fx_states[slot] = FxSaveArea::new();
        self.starts[slot] = None;
    }

    fn stack_bounds(&self, slot: usize) -> (usize, usize) {
        let base = self.stacks[slot].as_ptr() as usize;
        (base, base + TASK_STACK_SIZE)
    }

    fn stack_top(&self, slot: usize) -> usize {
        self.stack_bounds(slot).1 & !0xF
    }

    fn is_valid_saved_rsp(&self, slot: usize, saved_rsp: usize) -> bool {
        if saved_rsp == 0 {
            return false;
        }

        let align_mask = mem::align_of::<SavedContext>() - 1;
        if (saved_rsp & align_mask) != 0 {
            return false;
        }

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

    pub(super) fn allocate_kernel_slot(
        &mut self,
        entry: fn(u64),
        id: u64,
        pit_divisor: u16,
        cs: u64,
        ss: u64,
        rflags: u64,
        kernel_task_entry_rip: u64,
    ) -> Option<usize> {
        for slot in 1..MAX_TASK {
            if self.contexts[slot].is_none() {
                let kernel_stack_top = self.stack_top(slot) as u64;
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self.init_kernel_task_context(
                        slot,
                        cs,
                        ss,
                        rflags,
                        kernel_task_entry_rip,
                    ),
                    ready: true,
                    pit_divisor,
                    address_space_root: crate::paging::kernel_root_phys().as_u64(),
                    kernel_stack_top,
                    user_mode: false,
                    address_space_owner: ptr::null_mut(),
                });
                self.fx_states[slot] = FxSaveArea::new();
                self.starts[slot] = Some(TaskStart { entry, id });
                return Some(slot);
            }
        }

        None
    }

    pub(super) fn allocate_user_slot(
        &mut self,
        id: u64,
        address_space: ProcessAddressSpace,
        bootstrap: UserTaskBootstrap,
        pit_divisor: u16,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
        idle_entry: fn(u64),
    ) -> Option<usize> {
        for slot in 1..MAX_TASK {
            if self.contexts[slot].is_none() {
                let boxed_space = Box::new(address_space);
                let root_phys = boxed_space.root_phys().as_u64();
                let raw_space = Box::into_raw(boxed_space);
                let kernel_stack_top = self.stack_top(slot) as u64;

                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self.init_user_task_context(
                        slot,
                        bootstrap,
                        user_cs,
                        user_ss,
                        rflags,
                    ),
                    ready: true,
                    pit_divisor,
                    address_space_root: root_phys,
                    kernel_stack_top,
                    user_mode: true,
                    address_space_owner: raw_space,
                });
                self.fx_states[slot] = FxSaveArea::new();
                self.starts[slot] = Some(TaskStart {
                    entry: idle_entry,
                    id,
                });
                return Some(slot);
            }
        }

        None
    }

    fn init_kernel_task_context(
        &mut self,
        slot: usize,
        cs: u64,
        ss: u64,
        rflags: u64,
        kernel_task_entry_rip: u64,
    ) -> usize {
        let stack_top = self.stack_top(slot);

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
            (*context).rsp = task_rsp as u64;
            (*context).ss = ss;
            (*context).rip = kernel_task_entry_rip;
            (*context).cs = cs;
            (*context).rflags = rflags;
        }

        context_ptr
    }

    fn init_user_task_context(
        &mut self,
        slot: usize,
        bootstrap: UserTaskBootstrap,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
    ) -> usize {
        let stack_top = self.stack_top(slot);
        let context_ptr = stack_top - mem::size_of::<SavedContext>();
        let context = context_ptr as *mut SavedContext;

        unsafe {
            ptr::write_bytes(context as *mut u8, 0, mem::size_of::<SavedContext>());
            (*context).rax = bootstrap.registers.rax;
            (*context).rbx = bootstrap.registers.rbx;
            (*context).rcx = bootstrap.registers.rcx;
            (*context).rdx = bootstrap.registers.rdx;
            (*context).rsi = bootstrap.registers.rsi;
            (*context).rdi = bootstrap.registers.rdi;
            (*context).rbp = bootstrap.registers.rbp;
            (*context).r8 = bootstrap.registers.r8;
            (*context).r9 = bootstrap.registers.r9;
            (*context).r10 = bootstrap.registers.r10;
            (*context).r11 = bootstrap.registers.r11;
            (*context).r12 = bootstrap.registers.r12;
            (*context).r13 = bootstrap.registers.r13;
            (*context).r14 = bootstrap.registers.r14;
            (*context).r15 = bootstrap.registers.r15;
            (*context).rsp = bootstrap.stack_pointer.as_u64();
            (*context).ss = user_ss;
            (*context).rip = bootstrap.entry.as_u64();
            (*context).cs = user_cs;
            (*context).rflags = rflags;
        }

        context_ptr
    }

    fn retire_slot_due_to_invalid_context(&mut self, slot: usize, saved_rsp: usize) {
        let Some(_context) = self.contexts[slot] else {
            return;
        };

        if slot == 0 {
            panic!("scheduler bootstrap task saved rsp is invalid: {saved_rsp:#x}");
        }

        if let Some(current) = self.contexts[slot].as_mut() {
            current.saved_rsp = saved_rsp;
            current.ready = false;
        }
        self.retired[slot] = true;
        self.retire_reasons[slot] = Some(TaskRetireReason::CorruptedContext { saved_rsp });
    }

    fn retire_invalid_ready_tasks(&mut self) {
        for slot in 0..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.ready {
                continue;
            }
            if self.is_valid_saved_rsp(slot, context.saved_rsp) {
                continue;
            }

            self.retire_slot_due_to_invalid_context(slot, context.saved_rsp);
        }
    }

    pub(super) fn on_timer_interrupt(&mut self, current_rsp: usize) -> (usize, u16) {
        let current_slot = self.current_task;
        if self.retired[current_slot] {
            if let Some(current) = self.contexts[current_slot].as_mut() {
                current.saved_rsp = current_rsp;
                current.ready = false;
            }
        } else if self.is_valid_saved_rsp(current_slot, current_rsp) {
            if let Some(current) = self.contexts[current_slot].as_mut() {
                current.saved_rsp = current_rsp;
                current.ready = true;
            }
        } else {
            self.retire_slot_due_to_invalid_context(current_slot, current_rsp);
        }

        self.retire_invalid_ready_tasks();

        let next_idx = self.next_ready_task_index(self.current_task).unwrap_or(0);

        if let Some(next) = self.contexts[next_idx] {
            if self.is_valid_saved_rsp(next_idx, next.saved_rsp) {
                self.current_task = next_idx;
                return (next.saved_rsp, next.pit_divisor);
            }
        }

        let pit_divisor = self.contexts[current_slot]
            .map(|ctx| ctx.pit_divisor)
            .or_else(|| self.contexts[0].map(|ctx| ctx.pit_divisor))
            .expect("scheduler must keep slot 0 alive");

        (current_rsp, pit_divisor)
    }

    pub(super) fn current_task_start(&self) -> Option<TaskStart> {
        self.starts[self.current_task].filter(|_| {
            self.contexts[self.current_task]
                .map(|ctx| !ctx.user_mode)
                .unwrap_or(false)
        })
    }

    pub(super) fn prepare_current_task_execution(&self) {
        let current = self.contexts[self.current_task].expect("current task context missing");
        crate::paging::load_address_space_phys(PhysAddr::new(current.address_space_root));
        if current.kernel_stack_top != 0 {
            crate::gdt::set_privilege_stack(current.kernel_stack_top);
            crate::syscall::set_kernel_stack_top(current.kernel_stack_top);
        }
    }

    pub(super) fn reap_inactive_retired_slots(&mut self) {
        let active_root = self.contexts[self.current_task].map(|ctx| ctx.address_space_root);

        for slot in 1..MAX_TASK {
            if !self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                self.retired[slot] = false;
                continue;
            };

            if context.user_mode && Some(context.address_space_root) == active_root {
                continue;
            }

            self.log_retired_slot(slot, context);
            self.clear_slot(slot);
        }
    }

    pub(super) fn save_current_fx_state(&mut self) {
        unsafe {
            save_fxstate(&mut self.fx_states[self.current_task]);
        }
    }

    pub(super) fn restore_current_fx_state(&self) {
        unsafe {
            restore_fxstate(&self.fx_states[self.current_task]);
        }
    }

    pub(super) fn current_user_address_space(&self) -> Option<&ProcessAddressSpace> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode || context.address_space_owner.is_null() {
            return None;
        }

        Some(unsafe { &*context.address_space_owner })
    }

    pub(super) fn retire_current_user_task_due_to_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
    ) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot] else {
            return false;
        };
        if !context.user_mode {
            return false;
        }

        self.retired[slot] = true;
        if let Some(current) = self.contexts[slot].as_mut() {
            current.ready = false;
        }
        self.retire_reasons[slot] = Some(TaskRetireReason::UserFault {
            vector,
            error_code,
            cr2,
            rip,
        });
        true
    }

    pub(super) fn current_last_error(&self) -> u32 {
        self.last_errors[self.current_task]
    }

    pub(super) fn set_current_last_error(&mut self, value: u32) {
        self.last_errors[self.current_task] = value;
    }

    pub(super) fn exit_current_task(&mut self) {
        if self.contexts[self.current_task]
            .map(|ctx| ctx.user_mode)
            .unwrap_or(false)
        {
            self.retired[self.current_task] = true;
            if let Some(current) = self.contexts[self.current_task].as_mut() {
                current.ready = false;
            }
            if self.retire_reasons[self.current_task].is_none() {
                self.retire_reasons[self.current_task] = Some(TaskRetireReason::Exited);
            }
            return;
        }

        self.clear_slot(self.current_task);
    }

    fn log_retired_slot(&self, slot: usize, context: TaskContext) {
        let id = self.starts[slot]
            .map(|start| start.id)
            .unwrap_or(slot as u64);
        match self.retire_reasons[slot] {
            Some(TaskRetireReason::UserFault {
                vector,
                error_code,
                cr2,
                rip,
            }) => {
                debug::println!(
                    "scheduler: reaped user task pid={} slot={} vector={} error={:?} cr2={:#x} rip={:#x}",
                    id,
                    slot,
                    vector,
                    error_code,
                    cr2,
                    rip,
                );
            }
            Some(TaskRetireReason::CorruptedContext { saved_rsp }) => {
                let (stack_base, stack_top) = self.stack_bounds(slot);
                debug::println!(
                    "scheduler: reaped corrupted task pid={} slot={} user_mode={} saved_rsp={:#x} stack=[{:#x}, {:#x})",
                    id,
                    slot,
                    context.user_mode,
                    saved_rsp,
                    stack_base,
                    stack_top,
                );
            }
            Some(TaskRetireReason::Exited) => {
                debug::println!(
                    "scheduler: reaped exited user task pid={} slot={}",
                    id,
                    slot
                );
            }
            None => {
                debug::println!(
                    "scheduler: reaped retired task pid={} slot={} user_mode={}",
                    id,
                    slot,
                    context.user_mode,
                );
            }
        }
    }
}
