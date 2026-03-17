use alloc::boxed::Box;
use core::{mem, ptr};

use x86_64::PhysAddr;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::FsBase;

use crate::asmtools::{FxSaveArea, restore_fxstate, save_fxstate};
use crate::debug;
use crate::paging::ProcessAddressSpace;
use crate::session::ConsoleSessionId;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessState, LinuxThreadState};
use crate::user::process_state::{SharedUserProcessState, UserProcessState};

use super::context::{SAVED_CONTEXT_BYTES, SavedContext};
use super::{UserFaultDisposition, UserStackState, UserTaskBootstrap};

pub(super) const MAX_TASK: usize = 32;
// Kernel worker threads run fairly deep Rust call chains during process/module
// bring-up, so 16 KiB stacks are too tight and can corrupt adjacent task stacks.
const TASK_STACK_SIZE: usize = 64 * 1024;
const TASK_ENTRY_STACK_RESERVE_QWORDS: usize = 3;
const PAGE_FAULT_VECTOR: u8 = 14;

#[derive(Clone, Copy)]
enum TaskRetireReason {
    UserFault {
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
    },
    Terminated {
        requested_by_pid: Option<u64>,
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
    blocked: bool,
    pit_divisor: u16,
    address_space_root: u64,
    kernel_stack_top: u64,
    user_mode: bool,
    user_abi: Option<UserAbi>,
    console_session: ConsoleSessionId,
    process_state_owner: *mut SharedUserProcessState,
    user_stack: Option<UserStackState>,
    linux_thread_state: Option<LinuxThreadState>,
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
    pending_reap: bool,
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
            pending_reap: false,
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
        self.pending_reap = false;
        self.contexts[0] = Some(TaskContext {
            saved_rsp: 0,
            ready: true,
            blocked: false,
            pit_divisor: main_thread_pit_divisor,
            address_space_root: crate::paging::kernel_root_phys().as_u64(),
            kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionId::PRIMARY,
            process_state_owner: ptr::null_mut(),
            user_stack: None,
            linux_thread_state: None,
        });

        unsafe {
            save_fxstate(&mut self.fx_states[0]);
        }
    }

    pub(super) fn bootstrap_context_ready(&self) -> bool {
        self.contexts[0].is_some()
    }

    pub(super) fn clear_slot(&mut self, slot: usize) {
        if let Some(context) = self.contexts[slot] {
            if !context.process_state_owner.is_null() {
                unsafe {
                    if (*context.process_state_owner).release() {
                        drop(Box::from_raw(context.process_state_owner));
                    }
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

    fn mark_slot_ready(&mut self, slot: usize, saved_rsp: usize, ready: bool) {
        let Some(context) = self.contexts[slot].as_mut() else {
            return;
        };

        context.saved_rsp = saved_rsp;
        context.ready = ready;
    }

    fn retire_slot(&mut self, slot: usize, reason: TaskRetireReason) {
        if slot == 0 {
            panic!("scheduler bootstrap task cannot be retired");
        }

        self.retired[slot] = true;
        self.pending_reap = true;
        if let Some(context) = self.contexts[slot].as_mut() {
            context.ready = false;
        }
        self.retire_reasons[slot] = Some(reason);
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
                    blocked: false,
                    pit_divisor,
                    address_space_root: crate::paging::kernel_root_phys().as_u64(),
                    kernel_stack_top,
                    user_mode: false,
                    user_abi: None,
                    console_session: ConsoleSessionId::PRIMARY,
                    process_state_owner: ptr::null_mut(),
                    user_stack: None,
                    linux_thread_state: None,
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
                let boxed_state = UserProcessState::new(
                    address_space,
                    bootstrap.linux_process_state,
                    bootstrap.logical_admin,
                    bootstrap.exec_path(),
                );
                let root_phys = boxed_state.address_space().root_phys().as_u64();
                let raw_state =
                    Box::into_raw(Box::new(SharedUserProcessState::new(id, boxed_state)));
                let kernel_stack_top = self.stack_top(slot) as u64;

                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self
                        .init_user_task_context(slot, bootstrap, user_cs, user_ss, rflags),
                    ready: true,
                    blocked: false,
                    pit_divisor,
                    address_space_root: root_phys,
                    kernel_stack_top,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_state_owner: raw_state,
                    user_stack: bootstrap.user_stack,
                    linux_thread_state: bootstrap.linux_thread_state,
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

    pub(super) fn allocate_user_thread_slot(
        &mut self,
        id: u64,
        bootstrap: UserTaskBootstrap,
        pit_divisor: u16,
        user_cs: u64,
        user_ss: u64,
        rflags: u64,
    ) -> Option<usize> {
        let current = self.contexts[self.current_task]?;
        if !current.user_mode || current.process_state_owner.is_null() {
            return None;
        }

        let root_phys = current.address_space_root;
        let process_state_owner = current.process_state_owner;
        for slot in 1..MAX_TASK {
            if self.contexts[slot].is_none() {
                unsafe {
                    (*process_state_owner).retain();
                }
                let kernel_stack_top = self.stack_top(slot) as u64;
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self
                        .init_user_task_context(slot, bootstrap, user_cs, user_ss, rflags),
                    ready: true,
                    blocked: false,
                    pit_divisor,
                    address_space_root: root_phys,
                    kernel_stack_top,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_state_owner,
                    user_stack: bootstrap.user_stack,
                    linux_thread_state: bootstrap.linux_thread_state,
                });
                self.fx_states[slot] = FxSaveArea::new();
                self.starts[slot] = Some(TaskStart {
                    entry: super::noop_task_entry,
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
        if self.contexts[slot].is_none() {
            return;
        }
        self.mark_slot_ready(slot, saved_rsp, false);
        self.retire_slot(slot, TaskRetireReason::CorruptedContext { saved_rsp });
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
            self.mark_slot_ready(current_slot, current_rsp, false);
        } else if self.contexts[current_slot]
            .map(|ctx| ctx.blocked)
            .unwrap_or(false)
        {
            self.mark_slot_ready(current_slot, current_rsp, false);
        } else if self.is_valid_saved_rsp(current_slot, current_rsp) {
            self.mark_slot_ready(current_slot, current_rsp, true);
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

        let fs_base = current
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);
        FsBase::write(VirtAddr::new(fs_base));
        crate::syscall::prepare_for_context_return(self.context_returns_to_user(current));
    }

    pub(super) fn reap_inactive_retired_slots(&mut self) -> usize {
        if !self.pending_reap {
            return 0;
        }

        let active_root = self.contexts[self.current_task].map(|ctx| ctx.address_space_root);
        let mut still_pending = false;
        let mut reaped = 0;

        for slot in 1..MAX_TASK {
            if !self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                self.retired[slot] = false;
                continue;
            };

            if context.user_mode && Some(context.address_space_root) == active_root {
                still_pending = true;
                continue;
            }

            self.log_retired_slot(slot, context);
            self.clear_slot(slot);
            reaped += 1;
        }

        self.pending_reap = still_pending;
        reaped
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
        if !context.user_mode || context.process_state_owner.is_null() {
            return None;
        }

        Some(unsafe { (*context.process_state_owner).state().address_space() })
    }

    pub(super) fn current_user_abi(&self) -> Option<UserAbi> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode {
            return None;
        }

        context.user_abi
    }

    pub(super) fn current_user_id(&self) -> Option<u64> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode {
            return None;
        }

        self.starts[self.current_task].map(|start| start.id)
    }

    pub(super) fn current_user_process_id(&self) -> Option<u64> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode || context.process_state_owner.is_null() {
            return None;
        }

        Some(unsafe { (*context.process_state_owner).process_id() })
    }

    pub(super) fn current_console_session(&self) -> ConsoleSessionId {
        self.contexts[self.current_task]
            .map(|context| context.console_session)
            .unwrap_or(ConsoleSessionId::PRIMARY)
    }

    pub(super) fn with_current_user_linux_state_mut<R>(
        &mut self,
        f: impl FnOnce(
            u64,
            u64,
            UserAbi,
            &mut ProcessAddressSpace,
            &mut Option<LinuxProcessState>,
            &mut Option<LinuxThreadState>,
        ) -> R,
    ) -> Option<R> {
        let slot = self.current_task;
        let context = self.contexts[slot].as_mut()?;
        if !context.user_mode || context.process_state_owner.is_null() {
            return None;
        }

        let abi = context.user_abi?;
        let tid = self.starts[slot].map(|start| start.id)?;
        let process_id = unsafe { (*context.process_state_owner).process_id() };
        let thread_state_ptr = ptr::addr_of_mut!(context.linux_thread_state);
        let process_state = unsafe { (*context.process_state_owner).state_mut() };
        let (address_space, linux_process_state) =
            process_state.address_space_and_linux_process_state_mut();
        let linux_thread_state = unsafe { &mut *thread_state_ptr };
        Some(f(
            process_id,
            tid,
            abi,
            address_space,
            linux_process_state,
            linux_thread_state,
        ))
    }

    pub(super) fn with_current_user_process_state_mut<R>(
        &mut self,
        f: impl FnOnce(u64, UserAbi, &mut UserProcessState) -> R,
    ) -> Option<R> {
        let slot = self.current_task;
        let context = self.contexts[slot].as_mut()?;
        if !context.user_mode || context.process_state_owner.is_null() {
            return None;
        }

        let abi = context.user_abi?;
        let id = self.starts[slot].map(|start| start.id)?;
        let process_state = unsafe { (*context.process_state_owner).state_mut() };
        Some(f(id, abi, process_state))
    }

    fn try_grow_current_user_stack_on_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rsp: u64,
    ) -> bool {
        if vector != PAGE_FAULT_VECTOR || error_code.unwrap_or(0) & 0x1 != 0 {
            return false;
        }

        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if !context.user_mode || context.process_state_owner.is_null() {
            return false;
        }

        let Some(mut stack_state) = context.user_stack else {
            return false;
        };
        if !stack_state.contains_stack_pointer(rsp) || !stack_state.contains_reserved_address(cr2) {
            return false;
        }

        let Some((growth_start, growth_end, page_count)) = stack_state.grow_to_include_fault(cr2)
        else {
            return false;
        };

        let process_id = self.starts[slot]
            .map(|start| start.id)
            .unwrap_or(slot as u64);
        let process_state = unsafe { (*context.process_state_owner).state_mut() };
        let (address_space, linux_process_state) =
            process_state.address_space_and_linux_process_state_mut();
        let map_result = address_space.map_zeroed_user_pages_at(
            VirtAddr::new(growth_start),
            page_count,
            x86_64::structures::paging::PageTableFlags::WRITABLE
                | x86_64::structures::paging::PageTableFlags::NO_EXECUTE,
        );
        if map_result.is_err() {
            return false;
        }

        if let Some(state) = linux_process_state.as_mut() {
            state
                .release_reserved_range(growth_start, growth_end)
                .expect("user stack reserved range mismatch");
        }

        context.user_stack = Some(stack_state);
        debug::println!(
            "scheduler: grew user stack pid={} slot={} cr2={:#x} rsp={:#x} new_start={:#x} pages={}",
            process_id,
            slot,
            cr2,
            rsp,
            growth_start,
            page_count,
        );
        true
    }

    pub(super) fn retire_current_user_task_due_to_fault(
        &mut self,
        vector: u8,
        error_code: Option<u64>,
        cr2: u64,
        rip: u64,
        rsp: u64,
    ) -> UserFaultDisposition {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot] else {
            return UserFaultDisposition::Unhandled;
        };
        if !context.user_mode {
            return UserFaultDisposition::Unhandled;
        }

        if self.try_grow_current_user_stack_on_fault(vector, error_code, cr2, rsp) {
            return UserFaultDisposition::Resumed;
        }

        self.retire_slot(
            slot,
            TaskRetireReason::UserFault {
                vector,
                error_code,
                cr2,
                rip,
            },
        );
        UserFaultDisposition::Retired
    }

    pub(super) fn current_last_error(&self) -> u32 {
        self.last_errors[self.current_task]
    }

    pub(super) fn is_user_task_alive(&self, task_id: u64) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };

        self.contexts[slot].is_some() && !self.retired[slot]
    }

    pub(super) fn terminate_user_task(
        &mut self,
        task_id: u64,
        requested_by_pid: Option<u64>,
    ) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };
        if self.retired[slot] {
            return false;
        }

        self.retire_slot(slot, TaskRetireReason::Terminated { requested_by_pid });
        true
    }

    pub(super) fn block_current_user_task(&mut self) -> bool {
        let slot = self.current_task;
        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        if !context.user_mode {
            return false;
        }

        context.blocked = true;
        context.ready = false;
        true
    }

    pub(super) fn wake_user_task(&mut self, task_id: u64) -> bool {
        let Some(slot) = self.find_user_task_slot(task_id) else {
            return false;
        };
        if self.retired[slot] {
            return false;
        }

        let saved_rsp = match self.contexts[slot] {
            Some(context) => context.saved_rsp,
            None => return false,
        };
        let is_valid_saved_rsp = self.is_valid_saved_rsp(slot, saved_rsp);

        let Some(context) = self.contexts[slot].as_mut() else {
            return false;
        };
        context.blocked = false;
        if is_valid_saved_rsp {
            context.ready = true;
        }
        true
    }

    pub(super) fn set_current_last_error(&mut self, value: u32) {
        self.last_errors[self.current_task] = value;
    }

    pub(super) fn exit_current_task(&mut self) {
        let slot = self.current_task;
        if slot == 0 {
            panic!("scheduler bootstrap task cannot exit");
        }

        self.mark_slot_ready(
            slot,
            self.contexts[slot].map(|ctx| ctx.saved_rsp).unwrap_or(0),
            false,
        );
        if self.retire_reasons[slot].is_none() {
            self.retire_slot(slot, TaskRetireReason::Exited);
        } else {
            self.retired[slot] = true;
            self.pending_reap = true;
        }
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
            Some(TaskRetireReason::Terminated { requested_by_pid }) => {
                debug::println!(
                    "scheduler: reaped terminated task pid={} slot={} user_mode={} requested_by={:?}",
                    id,
                    slot,
                    context.user_mode,
                    requested_by_pid,
                );
            }
            Some(TaskRetireReason::Exited) => {
                debug::println!(
                    "scheduler: reaped exited task pid={} slot={} user_mode={}",
                    id,
                    slot,
                    context.user_mode,
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

    fn context_returns_to_user(&self, context: TaskContext) -> bool {
        Self::saved_context_returns_to_user(context.saved_rsp)
    }

    fn saved_context_returns_to_user(saved_rsp: usize) -> bool {
        if saved_rsp == 0 {
            return false;
        }

        let saved = unsafe { &*(saved_rsp as *const SavedContext) };
        (saved.cs & 0x3) == 0x3
    }

    fn find_user_task_slot(&self, task_id: u64) -> Option<usize> {
        for slot in 1..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode {
                continue;
            }
            if self.starts[slot].map(|start| start.id) == Some(task_id) {
                return Some(slot);
            }
        }

        None
    }
}
