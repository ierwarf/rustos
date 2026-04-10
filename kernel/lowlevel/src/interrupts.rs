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

pub type InterruptDispatch = extern "C" fn(*mut SavedContext) -> *mut SavedContext;

static TIMER_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static RTC_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);
static SOFTWARE_SCHEDULE_INTERRUPT_DISPATCH: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    fn timer_interrupt_handler();
    fn rtc_scheduler_interrupt_handler();
    fn software_schedule_interrupt_handler();
    fn software_schedule_trap();
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

pub fn register_timer_interrupt_dispatch(callback: InterruptDispatch) {
    TIMER_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_rtc_interrupt_dispatch(callback: InterruptDispatch) {
    RTC_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn register_software_schedule_interrupt_dispatch(callback: InterruptDispatch) {
    SOFTWARE_SCHEDULE_INTERRUPT_DISPATCH.store(callback as usize, Ordering::Release);
}

pub fn trigger_software_schedule() {
    interrupts::without_interrupts(|| unsafe {
        software_schedule_trap();
    });
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

fn dispatch(slot: &AtomicUsize, context_ptr: *mut SavedContext) -> *mut SavedContext {
    let callback_addr = slot.load(Ordering::Acquire);
    if callback_addr == 0 {
        return context_ptr;
    }

    let callback = unsafe { mem::transmute::<usize, InterruptDispatch>(callback_addr) };
    callback(context_ptr)
}
