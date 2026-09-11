//! Architecture-stable saved-context layout and interrupt exclusion leaves.
//!
//! - **Owner:** `kernel-lowlevel` owns the assembly/Rust context ABI.
//! - **Boundary:** Raw CPU frames are represented as typed kernel state only
//!   while their exact stack publication is live.
//! - **Lifecycle:** Entry publishes a continuation, dispatch consumes it, and
//!   the next trap republishes a new frame; consumed storage is not durable
//!   scheduler state.
//! - **Concurrency:** Interrupt enable/disable transitions are explicit and
//!   must compose with scheduler preemption depth.
//! - **Failure:** Layout or selector drift is rejected by build/source
//!   witnesses before runtime.
//! - **Forbidden:** No validation of a consumed frame or assumption that a
//!   saved stack pointer remains immutable while the task runs.
//! - **Evidence:** `exception-retirement`, `scheduler-lifecycle`, and
//!   `syscall-simd-lifecycle`.
use core::mem;
use core::sync::atomic::{AtomicUsize, Ordering};

use x86_64::instructions::interrupts;

use crate::address::higher_half_addr;

const SAVED_GPR_BYTES: usize = 15 * 8;
const SAVED_XMM_BYTES: usize = 16 * 16;
const CONTEXT_PREFIX_BYTES: usize = SAVED_GPR_BYTES + SAVED_XMM_BYTES;
const IRET_FRAME_BYTES: usize = 5 * 8;
pub const SAVED_CONTEXT_BYTES: usize = CONTEXT_PREFIX_BYTES + IRET_FRAME_BYTES;

const _: [(); 0x78] = [(); SAVED_GPR_BYTES];
const _: [(); 0x100] = [(); SAVED_XMM_BYTES];
const _: [(); 0x178] = [(); CONTEXT_PREFIX_BYTES];
const _: [(); 0x28] = [(); IRET_FRAME_BYTES];
const _: [(); 0x1a0] = [(); SAVED_CONTEXT_BYTES];

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SavedContext {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub xmm: [[u8; 16]; 16],
    pub rsp: u64,
    pub ss: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
}

const _: [(); 0x78] = [(); mem::offset_of!(SavedContext, xmm)];
const _: [(); 0x178] = [(); mem::offset_of!(SavedContext, rsp)];
const _: [(); 0x180] = [(); mem::offset_of!(SavedContext, ss)];
const _: [(); 0x188] = [(); mem::offset_of!(SavedContext, rip)];
const _: [(); 0x190] = [(); mem::offset_of!(SavedContext, cs)];
const _: [(); 0x198] = [(); mem::offset_of!(SavedContext, rflags)];
const _: [(); 0x1a0] = [(); mem::size_of::<SavedContext>()];

/// Stable offset of the SIMD image within the scheduler context ABI.
///
/// Syscall entry may use this to make its already-saved user SIMD image the
/// scheduler frame's SIMD image instead of copying it into a second frame.
pub const SAVED_CONTEXT_XMM_OFFSET: usize = mem::offset_of!(SavedContext, xmm);
/// Private software-trap discriminator for a register-carried fast IPC call.
pub const FAST_IPC_CALL_SCHEDULE_TAG: u64 = 0x5255_5354_4f53_4950;
/// Private software-trap discriminator for a deferred fast reply settlement.
pub const FAST_IPC_REPLY_SCHEDULE_TAG: u64 = 0x5255_5354_4f53_5250;

pub type InterruptDispatch = extern "C" fn(*mut SavedContext) -> *mut SavedContext;
/// Scheduler result for a syscall-owned user frame.
///
/// The second machine word tells the assembly return leaf whether the
/// scheduler retained the current task. In that case architectural restore
/// was intentionally skipped and the entry-side `swapgs` must be paired
/// immediately before IRET. A real task switch installs the selected task's
/// GS MSRs itself and therefore must not execute that extra swap.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SavedUserContextDispatch {
    pub context: *mut SavedContext,
    pub swapgs_before_iret: u64,
}

const _: [(); 16] = [(); mem::size_of::<SavedUserContextDispatch>()];
const _: [(); 0] = [(); mem::offset_of!(SavedUserContextDispatch, context)];
const _: [(); 8] = [(); mem::offset_of!(SavedUserContextDispatch, swapgs_before_iret)];

pub type SavedUserDispatch = extern "C" fn(*mut SavedContext) -> SavedUserContextDispatch;
pub type ContextSwitchCommit = extern "C" fn();

static TIMER_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static RTC_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static SOFTWARE_SCHEDULE_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static SAVED_USER_CONTEXT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static RESCHEDULE_IPI_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static CONTEXT_SWITCH_COMMIT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    fn timer_interrupt_handler();
    fn rtc_scheduler_interrupt_handler();
    fn software_schedule_interrupt_handler();
    fn reschedule_ipi_interrupt_handler();
    fn software_schedule_trap();
    fn fast_ipc_call_schedule_trap(endpoint: u64, reply: u64, receiver_task_id: u64) -> u64;
    fn fast_ipc_reply_schedule_trap(
        reply: u64,
        caller_task_id: u64,
        context_owner_task_id: u64,
        context_slot: u64,
        context_generation: u64,
    );
    fn schedule_saved_user_context(context: *mut SavedContext) -> !;
}

pub fn timer_interrupt_handler_addr() -> u64 {
    higher_half_addr(timer_interrupt_handler as *const () as usize as u64)
}

pub fn rtc_interrupt_handler_addr() -> u64 {
    higher_half_addr(rtc_scheduler_interrupt_handler as *const () as usize as u64)
}

pub fn software_schedule_interrupt_handler_addr() -> u64 {
    higher_half_addr(software_schedule_interrupt_handler as *const () as usize as u64)
}

pub fn reschedule_ipi_interrupt_handler_addr() -> u64 {
    higher_half_addr(reschedule_ipi_interrupt_handler as *const () as usize as u64)
}

pub fn register_timer_interrupt_dispatch(callback: InterruptDispatch) {
    TIMER_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_rtc_interrupt_dispatch(callback: InterruptDispatch) {
    RTC_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_software_schedule_interrupt_dispatch(callback: InterruptDispatch) {
    SOFTWARE_SCHEDULE_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_saved_user_context_dispatch(callback: SavedUserDispatch) {
    // ORDERING: Release publishes the typed scheduler callback before syscall
    // entry can transfer its initialized user frame into this dispatch path.
    SAVED_USER_CONTEXT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_reschedule_ipi_interrupt_dispatch(callback: InterruptDispatch) {
    // ORDERING: Release publishes the complete scheduler callback before any
    // CPU can receive the private reschedule vector.
    RESCHEDULE_IPI_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_context_switch_commit(callback: ContextSwitchCommit) {
    // ORDERING: Release publishes the stack-handoff commit callback before
    // scheduler interrupts may expose an outgoing stack transition.
    CONTEXT_SWITCH_COMMIT.store(callback as usize, Ordering::Release);
}

pub fn trigger_software_schedule() {
    interrupts::without_interrupts(|| unsafe {
        software_schedule_trap();
    });
}

/// Enter the software scheduler while preserving an IF-enabled kernel
/// continuation. This is the only safe direct-reschedule path from a live
/// syscall body; callers must have enabled interrupts before entering it.
pub fn trigger_software_schedule_interruptible() {
    assert!(
        interrupts::are_enabled(),
        "interruptible software schedule requires IF=1"
    );
    unsafe {
        software_schedule_trap();
    }
}

/// Commit and dispatch one register-carried fast IPC call at the software
/// scheduling boundary. The returned word is decoded by the scheduler owner.
pub fn trigger_fast_ipc_call_schedule(endpoint: u64, reply: u64, receiver_task_id: u64) -> u64 {
    // SAFETY: the assembly leaf only places the typed arguments and private
    // discriminator in caller-saved registers before entering the installed
    // software scheduling vector, then returns the handler-stamped result.
    unsafe { fast_ipc_call_schedule_trap(endpoint, reply, receiver_task_id) }
}

/// Settle one reply's scheduling-context custody at the same software trap
/// that dispatches the already-blocked server.
pub fn trigger_fast_ipc_reply_schedule(
    reply: u64,
    caller_task_id: u64,
    context_owner_task_id: u64,
    context_slot: u64,
    context_generation: u64,
) {
    // SAFETY: the assembly leaf preserves the five typed words in the SysV
    // caller-saved register image consumed only by the private schedule vector.
    unsafe {
        fast_ipc_reply_schedule_trap(
            reply,
            caller_task_id,
            context_owner_task_id,
            context_slot,
            context_generation,
        )
    }
}

/// Dispatch a complete, normalized user context and return through IRET.
///
/// # Safety
///
/// `context` must point to an initialized [`SavedContext`] on the current
/// task's kernel stack. Its user selectors and return state must already have
/// passed the syscall-return contract. This transfer never returns to the
/// caller; scheduler ownership of the outgoing stack is committed by the
/// matching HAL assembly boundary.
pub unsafe fn dispatch_saved_user_context(context: *mut SavedContext) -> ! {
    // SAFETY: the caller contract proves `context` is the current task's
    // initialized scheduler-shaped syscall frame; the assembly leaf consumes
    // it without returning or manufacturing another owner.
    unsafe { schedule_saved_user_context(context) }
}

#[unsafe(no_mangle)]
extern "C" fn timer_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    dispatch(&TIMER_INTERRUPT_DISPATCH, context_ptr)
}

#[unsafe(no_mangle)]
extern "C" fn rtc_interrupt_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    dispatch(&RTC_INTERRUPT_DISPATCH, context_ptr)
}

#[unsafe(no_mangle)]
extern "C" fn software_schedule_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    dispatch(&SOFTWARE_SCHEDULE_INTERRUPT_DISPATCH, context_ptr)
}

#[unsafe(no_mangle)]
extern "C" fn saved_user_context_dispatch(
    context_ptr: *mut SavedContext,
) -> SavedUserContextDispatch {
    // ORDERING: Acquire pairs with callback registration so this path observes
    // the complete HAL/PS scheduler dispatch installation before invoking it.
    let callback_addr = SAVED_USER_CONTEXT_DISPATCH.load(Ordering::Acquire);
    if callback_addr == 0 {
        return SavedUserContextDispatch {
            context: context_ptr,
            swapgs_before_iret: 1,
        };
    }

    // SAFETY: only `register_saved_user_context_dispatch` publishes this
    // address, and it accepts the exact callback type.
    let callback = unsafe { mem::transmute::<usize, SavedUserDispatch>(callback_addr) };
    callback(context_ptr)
}

#[unsafe(no_mangle)]
extern "C" fn reschedule_ipi_interrupt_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    dispatch(&RESCHEDULE_IPI_INTERRUPT_DISPATCH, context_ptr)
}

/// Called by the interrupt stubs only after `rsp` names the selected incoming
/// frame. Until this boundary, scheduler ownership intentionally retains the
/// outgoing task stack as a second CPU-local transition owner.
#[unsafe(no_mangle)]
extern "C" fn scheduler_context_switch_commit_dispatch() {
    // ORDERING: Acquire observes the registered exact callback before the
    // assembly boundary delegates outgoing-owner release.
    let callback_addr = CONTEXT_SWITCH_COMMIT.load(Ordering::Acquire);
    if callback_addr == 0 {
        return;
    }
    // SAFETY: only `register_context_switch_commit` can publish this address,
    // and it accepts the exact `extern "C" fn()` type.
    let callback = unsafe { mem::transmute::<usize, ContextSwitchCommit>(callback_addr) };
    callback();
}

fn dispatch(slot: &AtomicUsize, context_ptr: *mut SavedContext) -> *mut SavedContext {
    let callback_addr = slot.load(Ordering::Acquire);
    if callback_addr == 0 {
        return context_ptr;
    }

    let callback = unsafe { mem::transmute::<usize, InterruptDispatch>(callback_addr) };
    callback(context_ptr)
}
