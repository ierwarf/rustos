use alloc::boxed::Box;
use alloc::string::String;
use core::{mem, ptr};

use x86_64::registers::model_specific::FsBase;
use x86_64::PhysAddr;
use x86_64::VirtAddr;

use crate::arch::simd::{restore_state, save_state, SimdState};
use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::memory::paging::ProcessAddressSpace;
use crate::user::abi::UserAbi;
use crate::user::linux::{LinuxProcessState, LinuxThreadState};
use crate::user::process;
use crate::user::process_state::{
    SharedUserProcessState, UserProcessState, WindowsThreadRuntimeState,
};

use super::context::{SavedContext, SAVED_CONTEXT_BYTES};
use super::{UserFaultDisposition, UserStackState, UserTaskBootstrap};

pub(super) const MAX_TASK: usize = 32;
// Kernel worker threads run fairly deep Rust call chains during process/module
// bring-up, so smaller stacks can corrupt adjacent task stacks.
const TASK_STACK_SIZE: usize = 256 * 1024;
const TASK_STACK_GUARD_BYTES: usize = 256;
const STACK_CANARY_WORD: u64 = 0x5343_4844_554c_4552;
const TASK_ENTRY_STACK_RESERVE_QWORDS: usize = 3;
const PAGE_FAULT_VECTOR: u8 = 14;
const RFLAGS_RESERVED_BIT_1: u64 = 1 << 1;

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
        reason: &'static str,
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
    console_session: ConsoleSessionHandle,
    process_state_owner: Option<ptr::NonNull<SharedUserProcessState>>,
    user_stack: Option<UserStackState>,
    linux_thread_state: Option<LinuxThreadState>,
    windows_thread_state: Option<WindowsThreadRuntimeState>,
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
    simd_states: [SimdState; MAX_TASK],
    starts: [Option<TaskStart>; MAX_TASK],
    current_task: usize,
    bootstrap_context_captured: bool,
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
            simd_states: [SimdState::new(); MAX_TASK],
            starts: [None; MAX_TASK],
            current_task: 0,
            bootstrap_context_captured: false,
            pending_reap: false,
            stacks: [[0; TASK_STACK_SIZE]; MAX_TASK],
        }
    }

    pub(super) fn reset(&mut self, main_thread_pit_divisor: u16) {
        for slot in 0..MAX_TASK {
            self.clear_slot(slot);
        }

        self.simd_states = [SimdState::new(); MAX_TASK];
        self.retired = [false; MAX_TASK];
        self.retire_reasons = [None; MAX_TASK];
        self.last_errors = [0; MAX_TASK];
        self.current_task = 0;
        self.bootstrap_context_captured = false;
        self.pending_reap = false;
        self.contexts[0] = Some(TaskContext {
            saved_rsp: 0,
            ready: true,
            blocked: false,
            pit_divisor: main_thread_pit_divisor,
            address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
            kernel_stack_top: 0,
            user_mode: false,
            user_abi: None,
            console_session: ConsoleSessionHandle::SYSTEM,
            process_state_owner: None,
            user_stack: None,
            linux_thread_state: None,
            windows_thread_state: None,
        });

        unsafe {
            save_state(&mut self.simd_states[0]);
        }
    }

    pub(super) fn bootstrap_context_ready(&self) -> bool {
        self.contexts[0].is_some()
    }

    pub(super) fn clear_slot(&mut self, slot: usize) {
        if let Some(context) = self.contexts[slot] {
            if let Some(mut owner) = context.process_state_owner {
                unsafe {
                    if owner.as_mut().release() {
                        drop(Box::from_raw(owner.as_ptr()));
                    }
                }
            }
        }

        self.contexts[slot] = None;
        self.retired[slot] = false;
        self.retire_reasons[slot] = None;
        self.last_errors[slot] = 0;
        self.simd_states[slot] = SimdState::new();
        self.starts[slot] = None;
        self.reset_stack_storage(slot);
    }

    fn mark_slot_ready(&mut self, slot: usize, saved_rsp: usize, ready: bool) {
        let Some(context) = self.contexts[slot].as_mut() else {
            return;
        };

        context.saved_rsp = saved_rsp;
        context.ready = ready;
        if slot == 0 && saved_rsp != 0 {
            self.bootstrap_context_captured = true;
        }
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

    fn reset_stack_storage(&mut self, slot: usize) {
        self.stacks[slot].fill(0);
        let canary_words = TASK_STACK_GUARD_BYTES / mem::size_of::<u64>();
        let base = self.stacks[slot].as_mut_ptr() as *mut u64;
        for index in 0..canary_words {
            unsafe {
                ptr::write(base.add(index), STACK_CANARY_WORD);
            }
        }
    }

    fn stack_canary_intact(&self, slot: usize) -> bool {
        let canary_words = TASK_STACK_GUARD_BYTES / mem::size_of::<u64>();
        let base = self.stacks[slot].as_ptr() as *const u64;
        for index in 0..canary_words {
            let value = unsafe { ptr::read(base.add(index)) };
            if value != STACK_CANARY_WORD {
                return false;
            }
        }
        true
    }

    fn scheduler_storage_bounds(&self) -> (usize, usize) {
        let base = self as *const Self as usize;
        (base, base + mem::size_of::<Self>())
    }

    fn scheduler_storage_contains(&self, addr: usize) -> bool {
        let (base, end) = self.scheduler_storage_bounds();
        if addr >= base && addr < end {
            return true;
        }

        let virt_offset = crate::memory::paging::KERNEL_VIRT_OFFSET as usize;
        if base >= virt_offset {
            let low_base = base - virt_offset;
            let low_end = end - virt_offset;
            return addr >= low_base && addr < low_end;
        }

        false
    }

    fn stack_bounds(&self, slot: usize) -> (usize, usize) {
        let base = self.stacks[slot].as_ptr() as usize;
        (base + TASK_STACK_GUARD_BYTES, base + TASK_STACK_SIZE)
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

        if slot >= MAX_TASK {
            return false;
        }

        let (base, top) = self.stack_bounds(slot);
        let Some(frame_end) = saved_rsp.checked_add(SAVED_CONTEXT_BYTES) else {
            return false;
        };

        if slot == 0 {
            let last_byte = frame_end.saturating_sub(1);
            let kernel_base = crate::memory::paging::KERNEL_VIRT_OFFSET as usize;
            return saved_rsp >= kernel_base
                && last_byte >= kernel_base
                && Self::is_canonical_address(saved_rsp as u64)
                && Self::is_canonical_address(last_byte as u64)
                && !self.scheduler_storage_contains(saved_rsp)
                && !self.scheduler_storage_contains(last_byte);
        }

        saved_rsp >= base && frame_end <= top
    }

    fn context_validation_error(
        &self,
        slot: usize,
        context: TaskContext,
        saved_rsp: usize,
    ) -> Option<&'static str> {
        self.validate_saved_context(slot, context.user_mode, saved_rsp)
            .err()
    }

    fn validate_context_slot(&self, slot: usize) -> Result<TaskContext, &'static str> {
        let context = self.contexts[slot].ok_or("task context is missing")?;
        if self.is_bootstrap_context_placeholder(slot, context) {
            return Ok(context);
        }
        self.validate_saved_context(slot, context.user_mode, context.saved_rsp)?;
        Ok(context)
    }

    fn bootstrap_context(&self) -> TaskContext {
        self.contexts[0].expect("scheduler lost the bootstrap task context")
    }

    fn saved_context_ref(saved_rsp: usize) -> Option<&'static SavedContext> {
        if saved_rsp == 0 || (saved_rsp & (mem::align_of::<SavedContext>() - 1)) != 0 {
            return None;
        }

        Some(unsafe { &*(saved_rsp as *const SavedContext) })
    }

    fn is_canonical_address(addr: u64) -> bool {
        let upper = addr >> 48;
        if ((addr >> 47) & 1) == 0 {
            upper == 0
        } else {
            upper == 0xFFFF
        }
    }

    fn validate_saved_context(
        &self,
        slot: usize,
        user_mode_task: bool,
        saved_rsp: usize,
    ) -> Result<(), &'static str> {
        if !self.is_valid_saved_rsp(slot, saved_rsp) {
            return Err("saved context pointer is outside the task stack");
        }
        if slot != 0 && !self.stack_canary_intact(slot) {
            return Err("kernel stack guard was corrupted");
        }

        let saved = Self::saved_context_ref(saved_rsp).ok_or("saved context pointer is invalid")?;
        if (saved.rflags & RFLAGS_RESERVED_BIT_1) == 0 {
            return Err("saved rflags lost the reserved bit");
        }

        let user_cs = crate::arch::gdt::user_code_selector().0 as u64;
        let user_ss = crate::arch::gdt::user_data_selector().0 as u64;
        let kernel_cs = crate::arch::gdt::kernel_code_selector().0 as u64;
        let kernel_ss = crate::arch::gdt::kernel_data_selector().0 as u64;

        if saved.cs == user_cs {
            if !user_mode_task {
                return Err("kernel task cannot return directly to user mode");
            }
            if saved.ss != user_ss {
                return Err("user return frame carries an unexpected stack selector");
            }
            if !Self::is_canonical_address(saved.rip)
                || !Self::is_canonical_address(saved.rsp)
                || saved.rip >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
                || saved.rsp < crate::memory::paging::USER_SPACE_BASE
                || saved.rsp >= crate::memory::paging::USER_SPACE_END_EXCLUSIVE
            {
                return Err("user return frame points outside user space");
            }
            return Ok(());
        }

        if saved.cs != kernel_cs {
            return Err("saved code selector does not match any supported return mode");
        }
        if !Self::is_canonical_address(saved.rip) {
            return Err("kernel return RIP is not canonical");
        }
        if saved.rip >= crate::memory::paging::USER_SPACE_BASE
            && saved.rip < crate::memory::paging::USER_SPACE_END_EXCLUSIVE
        {
            return Err("kernel return RIP points into user space");
        }
        if self.scheduler_storage_contains(saved.rip as usize) {
            return Err("kernel return RIP points into scheduler storage");
        }

        let kernel_interrupt_frame = saved.rsp == 1 && saved.ss == 0;
        let initial_kernel_frame =
            saved.ss == kernel_ss && Self::is_canonical_address(saved.rsp) && saved.rsp != 0;
        if !kernel_interrupt_frame && !initial_kernel_frame {
            return Err("kernel return frame has an invalid stack layout");
        }

        if initial_kernel_frame && slot != 0 {
            let (stack_base, stack_top) = self.stack_bounds(slot);
            let rsp = saved.rsp as usize;
            if rsp < stack_base || rsp > stack_top {
                return Err("kernel return RSP does not belong to the task stack");
            }
        }

        Ok(())
    }

    fn is_bootstrap_context_placeholder(&self, slot: usize, context: TaskContext) -> bool {
        slot == 0
            && !self.bootstrap_context_captured
            && !context.user_mode
            && context.saved_rsp == 0
            && context.kernel_stack_top == 0
            && context.process_state_owner.is_none()
            && self.starts[0].is_none()
    }

    fn context_is_schedulable(&self, slot: usize, context: TaskContext) -> bool {
        self.context_validation_error(slot, context, context.saved_rsp)
            .is_none()
    }

    fn next_ready_task_index(&self, current: usize) -> Option<usize> {
        for offset in 1..=MAX_TASK {
            let idx = (current + offset) % MAX_TASK;
            if let Some(ctx) = self.contexts[idx] {
                if ctx.ready && self.context_is_schedulable(idx, ctx) {
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
                self.reset_stack_storage(slot);
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
                    address_space_root: crate::memory::paging::kernel_root_phys().as_u64(),
                    kernel_stack_top,
                    user_mode: false,
                    user_abi: None,
                    console_session: ConsoleSessionHandle::SYSTEM,
                    process_state_owner: None,
                    user_stack: None,
                    linux_thread_state: None,
                    windows_thread_state: None,
                });
                self.simd_states[slot] = SimdState::new();
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
                self.reset_stack_storage(slot);
                let saved_rsp =
                    self.init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags);
                let exec_path = alloc::string::String::from(bootstrap.exec_path());
                let mut boxed_state = UserProcessState::new(
                    address_space,
                    bootstrap.linux_process_state,
                    bootstrap.linux_memory_map,
                    bootstrap.windows_runtime,
                    bootstrap.logical_admin,
                    exec_path.as_str(),
                );
                if let Some(thread_state) = bootstrap.windows_thread_state {
                    if let Err(error) = process::initialize_windows_thread_identifiers(
                        boxed_state.address_space_mut(),
                        thread_state.teb_address,
                        id,
                        id,
                    ) {
                        panic!("failed to initialize windows thread ids: {:?}", error);
                    }
                }
                let root_phys = boxed_state.address_space().root_phys().as_u64();
                let raw_state =
                    Box::into_raw(Box::new(SharedUserProcessState::new(id, boxed_state)));
                let kernel_stack_top = self.stack_top(slot) as u64;

                self.contexts[slot] = Some(TaskContext {
                    saved_rsp,
                    ready: true,
                    blocked: false,
                    pit_divisor,
                    address_space_root: root_phys,
                    kernel_stack_top,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_state_owner: Some(
                        ptr::NonNull::new(raw_state)
                            .expect("boxed process state pointer must not be null"),
                    ),
                    user_stack: bootstrap.user_stack,
                    linux_thread_state: bootstrap.linux_thread_state,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
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
        if !current.user_mode {
            return None;
        }

        let root_phys = current.address_space_root;
        let Some(mut process_state_owner) = current.process_state_owner else {
            return None;
        };
        let process_id = unsafe { process_state_owner.as_ref().process_id() };
        for slot in 1..MAX_TASK {
            if self.contexts[slot].is_none() {
                unsafe {
                    process_state_owner.as_mut().retain();
                    if let Some(thread_state) = bootstrap.windows_thread_state {
                        if let Err(error) = process::initialize_windows_thread_identifiers(
                            process_state_owner.as_mut().state_mut().address_space_mut(),
                            thread_state.teb_address,
                            process_id,
                            id,
                        ) {
                            panic!("failed to initialize windows thread ids: {:?}", error);
                        }
                    }
                }
                self.reset_stack_storage(slot);
                let kernel_stack_top = self.stack_top(slot) as u64;
                self.contexts[slot] = Some(TaskContext {
                    saved_rsp: self
                        .init_user_task_context(slot, &bootstrap, user_cs, user_ss, rflags),
                    ready: true,
                    blocked: false,
                    pit_divisor,
                    address_space_root: root_phys,
                    kernel_stack_top,
                    user_mode: true,
                    user_abi: Some(bootstrap.abi),
                    console_session: bootstrap.console_session,
                    process_state_owner: Some(process_state_owner),
                    user_stack: bootstrap.user_stack,
                    linux_thread_state: bootstrap.linux_thread_state,
                    windows_thread_state: bootstrap.windows_thread_state.map(|mut state| {
                        state.thread_id = id;
                        state
                    }),
                });
                self.simd_states[slot] = SimdState::new();
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
        bootstrap: &UserTaskBootstrap,
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

    fn retire_slot_due_to_invalid_context(
        &mut self,
        slot: usize,
        saved_rsp: usize,
        reason: &'static str,
    ) {
        if self.contexts[slot].is_none() {
            return;
        }
        self.mark_slot_ready(slot, saved_rsp, false);
        self.retire_slot(
            slot,
            TaskRetireReason::CorruptedContext { saved_rsp, reason },
        );
    }

    fn retire_invalid_ready_tasks(&mut self) {
        for slot in 1..MAX_TASK {
            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if let Err(reason) =
                self.validate_saved_context(slot, context.user_mode, context.saved_rsp)
            {
                self.retire_slot_due_to_invalid_context(slot, context.saved_rsp, reason);
            }
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
        } else if self.contexts[current_slot]
            .and_then(|ctx| self.context_validation_error(current_slot, ctx, current_rsp))
            .is_none()
        {
            self.mark_slot_ready(current_slot, current_rsp, true);
        } else {
            let reason = self.contexts[current_slot]
                .and_then(|ctx| self.context_validation_error(current_slot, ctx, current_rsp))
                .unwrap_or("current task context is missing");
            if current_slot == 0 {
                panic!("scheduler bootstrap context is corrupted: {}", reason);
            }
            self.retire_slot_due_to_invalid_context(current_slot, current_rsp, reason);
        }

        self.retire_invalid_ready_tasks();

        let next_idx = self.next_ready_task_index(self.current_task).unwrap_or(0);

        if let Some(next) = self.contexts[next_idx] {
            match self.context_validation_error(next_idx, next, next.saved_rsp) {
                None => {
                    self.current_task = next_idx;
                    return (next.saved_rsp, next.pit_divisor);
                }
                Some(reason) if next_idx == 0 => {
                    panic!("scheduler bootstrap context is corrupted: {}", reason);
                }
                Some(reason) => {
                    self.retire_slot_due_to_invalid_context(next_idx, next.saved_rsp, reason);
                }
            }
        }

        let bootstrap = self.bootstrap_context();
        self.current_task = 0;
        (bootstrap.saved_rsp, bootstrap.pit_divisor)
    }

    pub(super) fn current_task_start(&self) -> Option<TaskStart> {
        self.starts[self.current_task].filter(|_| {
            self.contexts[self.current_task]
                .map(|ctx| !ctx.user_mode)
                .unwrap_or(false)
        })
    }

    pub(super) fn prepare_current_task_execution(&self) {
        let current =
            self.contexts[self.current_task].expect("scheduler selected a missing task context");
        let placeholder = self.is_bootstrap_context_placeholder(self.current_task, current);
        if !placeholder {
            self.validate_saved_context(self.current_task, current.user_mode, current.saved_rsp)
                .expect("scheduler selected an invalid task context");
        }
        crate::memory::paging::load_address_space_phys(PhysAddr::new(current.address_space_root));
        if current.kernel_stack_top != 0 {
            crate::arch::gdt::set_privilege_stack(current.kernel_stack_top);
            crate::user::syscall::set_kernel_stack_top(current.kernel_stack_top);
        }

        let fs_base = current
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);
        let user_gs_base = current
            .windows_thread_state
            .map(|state| state.teb_address)
            .unwrap_or(0);
        FsBase::write(VirtAddr::new(fs_base));
        crate::user::syscall::prepare_for_context_return(
            !placeholder && self.context_returns_to_user(current),
            user_gs_base,
        );
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

    pub(super) fn save_current_simd_state(&mut self) {
        unsafe {
            save_state(&mut self.simd_states[self.current_task]);
        }
    }

    pub(super) fn restore_current_simd_state(&self) {
        unsafe {
            restore_state(&self.simd_states[self.current_task]);
        }
    }

    pub(super) fn current_user_address_space(&self) -> Option<&ProcessAddressSpace> {
        let context = self.contexts[self.current_task]?;
        if !context.user_mode {
            return None;
        }

        let owner = context.process_state_owner?;
        Some(unsafe { owner.as_ref().state().address_space() })
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
        if !context.user_mode {
            return None;
        }

        let owner = context.process_state_owner?;
        Some(unsafe { owner.as_ref().process_id() })
    }

    pub(super) fn current_console_session(&self) -> ConsoleSessionHandle {
        self.contexts[self.current_task]
            .map(|context| context.console_session)
            .unwrap_or(ConsoleSessionHandle::SYSTEM)
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
        if !context.user_mode {
            return None;
        }

        let abi = context.user_abi?;
        let tid = self.starts[slot].map(|start| start.id)?;
        let mut owner = context.process_state_owner?;
        let process_id = unsafe { owner.as_ref().process_id() };
        let thread_state_ptr = ptr::addr_of_mut!(context.linux_thread_state);
        let process_state = unsafe { owner.as_mut().state_mut() };
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
        if !context.user_mode {
            return None;
        }

        let abi = context.user_abi?;
        let id = self.starts[slot].map(|start| start.id)?;
        let mut owner = context.process_state_owner?;
        let process_state = unsafe { owner.as_mut().state_mut() };
        Some(f(id, abi, process_state))
    }

    pub(super) fn with_current_user_windows_thread_state_mut<R>(
        &mut self,
        f: impl FnOnce(u64, &mut WindowsThreadRuntimeState) -> R,
    ) -> Option<R> {
        let slot = self.current_task;
        let context = self.contexts[slot].as_mut()?;
        if !context.user_mode {
            return None;
        }

        let tid = self.starts[slot].map(|start| start.id)?;
        let thread_state = context.windows_thread_state.as_mut()?;
        Some(f(tid, thread_state))
    }

    pub(super) fn any_user_process_state(
        &mut self,
        mut f: impl FnMut(u64, &UserProcessState) -> bool,
    ) -> bool {
        let mut seen = [None; MAX_TASK];
        let mut seen_count = 0usize;
        for slot in 1..MAX_TASK {
            if self.retired[slot] {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode {
                continue;
            }

            let Some(owner) = context.process_state_owner else {
                continue;
            };
            if seen[..seen_count]
                .iter()
                .any(|seen_owner| *seen_owner == Some(owner))
            {
                continue;
            }

            let process = unsafe { owner.as_ref() };
            if f(process.process_id(), process.state()) {
                return true;
            }

            seen[seen_count] = Some(owner);
            seen_count += 1;
        }

        false
    }

    pub(super) fn exec_current_user_process(
        &mut self,
        address_space: ProcessAddressSpace,
        mut bootstrap: UserTaskBootstrap,
    ) -> bool {
        let slot = self.current_task;
        let Some(current_context) = self.contexts[slot] else {
            return false;
        };
        if !current_context.user_mode {
            return false;
        }

        let Some(mut owner) = current_context.process_state_owner else {
            return false;
        };
        let Some(linux_process_state) = bootstrap.linux_process_state.take() else {
            return false;
        };
        let Some(linux_memory_map) = bootstrap.linux_memory_map.take() else {
            return false;
        };

        let process_id = unsafe { owner.as_ref().process_id() };
        let exec_path = String::from(bootstrap.exec_path());
        let (sibling_slots, sibling_count) = self.collect_process_sibling_slots(slot, owner);
        let new_root = address_space.root_phys().as_u64();
        let new_fs_base = bootstrap
            .linux_thread_state
            .map(|state| state.fs_base)
            .unwrap_or(0);

        {
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            context.address_space_root = new_root;
            context.user_abi = Some(bootstrap.abi);
            context.console_session = bootstrap.console_session;
            context.user_stack = bootstrap.user_stack;
            context.linux_thread_state = bootstrap.linux_thread_state;
            context.blocked = false;
            context.ready = true;
        }

        self.retired[slot] = false;
        self.retire_reasons[slot] = None;
        self.last_errors[slot] = 0;
        self.simd_states[slot] = SimdState::new();
        self.starts[slot] = Some(TaskStart {
            entry: super::noop_task_entry,
            id: process_id,
        });

        crate::memory::paging::load_address_space_phys(PhysAddr::new(new_root));
        unsafe {
            owner.as_mut().state_mut().replace_for_exec(
                address_space,
                linux_process_state,
                linux_memory_map,
                exec_path.as_str(),
            );
        }

        for index in 0..sibling_count {
            self.clear_slot(sibling_slots[index]);
        }

        FsBase::write(VirtAddr::new(new_fs_base));
        true
    }

    fn collect_process_sibling_slots(
        &self,
        current_slot: usize,
        owner: ptr::NonNull<SharedUserProcessState>,
    ) -> ([usize; MAX_TASK], usize) {
        let mut slots = [0usize; MAX_TASK];
        let mut count = 0usize;
        for slot in 1..MAX_TASK {
            if slot == current_slot {
                continue;
            }

            let Some(context) = self.contexts[slot] else {
                continue;
            };
            if !context.user_mode || context.process_state_owner != Some(owner) {
                continue;
            }

            slots[count] = slot;
            count += 1;
        }

        (slots, count)
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
        if !context.user_mode {
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
        let Some(mut owner) = context.process_state_owner else {
            return false;
        };
        let process_state = unsafe { owner.as_mut().state_mut() };
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

        let (saved_rsp, user_mode) = match self.contexts[slot] {
            Some(context) => (context.saved_rsp, context.user_mode),
            None => return false,
        };
        let invalid_reason = self
            .validate_saved_context(slot, user_mode, saved_rsp)
            .err();

        {
            let Some(context) = self.contexts[slot].as_mut() else {
                return false;
            };
            context.blocked = false;
            context.ready = invalid_reason.is_none();
        }

        if let Some(reason) = invalid_reason {
            if slot == 0 {
                panic!("scheduler bootstrap context is corrupted: {}", reason);
            }
            self.retire_slot_due_to_invalid_context(slot, saved_rsp, reason);
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
            Some(TaskRetireReason::CorruptedContext { saved_rsp, reason }) => {
                let (stack_base, stack_top) = self.stack_bounds(slot);
                debug::println!(
                    "scheduler: reaped corrupted task pid={} slot={} user_mode={} saved_rsp={:#x} stack=[{:#x}, {:#x}) reason={}",
                    id,
                    slot,
                    context.user_mode,
                    saved_rsp,
                    stack_base,
                    stack_top,
                    reason,
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
        if self.is_bootstrap_context_placeholder(self.current_task, context) {
            return false;
        }
        Self::saved_context_returns_to_user(context.saved_rsp)
    }

    fn saved_context_returns_to_user(saved_rsp: usize) -> bool {
        let Some(saved) = Self::saved_context_ref(saved_rsp) else {
            return false;
        };
        saved.cs == crate::arch::gdt::user_code_selector().0 as u64
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

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;

    use super::{ConsoleSessionHandle, Scheduler, SharedUserProcessState, TaskContext, MAX_TASK};
    use crate::user::abi::UserAbi;

    fn test_user_context(owner: NonNull<SharedUserProcessState>) -> TaskContext {
        TaskContext {
            saved_rsp: 0,
            ready: true,
            blocked: false,
            pit_divisor: 0,
            address_space_root: 0,
            kernel_stack_top: 0,
            user_mode: true,
            user_abi: Some(UserAbi::Linux),
            console_session: ConsoleSessionHandle::SYSTEM,
            process_state_owner: Some(owner),
            user_stack: None,
            linux_thread_state: None,
            windows_thread_state: None,
        }
    }

    #[test]
    fn collect_process_sibling_slots_returns_matching_user_slots_only() {
        let mut scheduler = Box::<Scheduler>::new_uninit();
        unsafe {
            scheduler.as_mut_ptr().write_bytes(0, 1);
        }
        let mut scheduler = unsafe { scheduler.assume_init() };
        let mut owner_storage = MaybeUninit::<SharedUserProcessState>::uninit();
        let mut other_storage = MaybeUninit::<SharedUserProcessState>::uninit();
        let owner = NonNull::new(owner_storage.as_mut_ptr()).expect("owner pointer");
        let other = NonNull::new(other_storage.as_mut_ptr()).expect("other pointer");

        scheduler.contexts[1] = Some(test_user_context(owner));
        scheduler.contexts[2] = Some(test_user_context(owner));
        scheduler.contexts[3] = Some(test_user_context(other));
        scheduler.contexts[4] = Some(TaskContext {
            user_mode: false,
            process_state_owner: Some(owner),
            ..test_user_context(owner)
        });

        let (slots, count) = scheduler.collect_process_sibling_slots(1, owner);
        assert_eq!(count, 1);
        assert_eq!(slots[0], 2);
        assert!(slots[1..MAX_TASK].iter().all(|slot| *slot == 0));
    }
}
