use x86_64::registers::control::Cr2;
use x86_64::structures::idt::InterruptStackFrame;

const KEYBOARD_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_1_OFFSET + 1;
const MOUSE_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_2_OFFSET + 4;
// RTC scheduling remains wired in the IDT layout even when current platforms do not use it.
#[allow(dead_code)]
const RTC_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_2_OFFSET;

pub fn default_handler(stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    crate::debug::dump_recent_trace_locations("exception");
    if index == 14 {
        log_page_fault_details(
            error_code.unwrap_or(0),
            cr2,
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.stack_pointer.as_u64(),
        );
    } else if index == 13 {
        log_general_protection_details(
            error_code.unwrap_or(0),
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.stack_pointer.as_u64(),
        );
    }
    if is_user_mode(&stack_frame) {
        match crate::multitask::retire_current_user_task_due_to_fault(
            index,
            error_code,
            cr2,
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.stack_pointer.as_u64(),
        ) {
            crate::multitask::UserFaultDisposition::Resumed => return,
            crate::multitask::UserFaultDisposition::Retired => {
                crate::multitask::halt_current_retired_task();
            }
            crate::multitask::UserFaultDisposition::Unhandled => {}
        }

        panic!(
            "user-mode exception was raised without an active user task: vector={}, rip={:#x}, cs={:#x}",
            index,
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.code_segment.0,
        );
    }

    panic!(
        "Unhandled exception: vector = {}, error code = {:?}, cr2 = {:#x}, rip = {:#x}, cs = {:#x}, rflags = {:#x}, rsp = {:#x}, ss = {:#x}",
        index,
        error_code,
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        stack_frame.stack_pointer.as_u64(),
        stack_frame.stack_segment.0,
    );
}

pub extern "x86-interrupt" fn non_maskable_interrupt_handler(stack_frame: InterruptStackFrame) {
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    crate::debug::println!(
        "NMI: cr2={:#x} rip={:#x} cs={:#x} rflags={:#x} rsp={:#x} ss={:#x}",
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        stack_frame.stack_pointer.as_u64(),
        stack_frame.stack_segment.0,
    );
    if let Some(snapshot) = crate::multitask::current_user_snapshot() {
        crate::debug::println!(
            "NMI: current user abi={:?} pid={} tid={} session={:?}",
            snapshot.abi(),
            snapshot.process_id(),
            snapshot.thread_id(),
            snapshot.console_session(),
        );
    } else {
        crate::debug::println!("NMI: no current user task");
    }
    crate::debug::dump_recent_trace_locations("nmi");
}

fn log_general_protection_details(error_code: u64, rip: u64, rsp: u64) {
    crate::debug::println!(
        "general protection detail: ec={:#x} rip={:#x}",
        error_code,
        rip,
    );

    for index in 0..8usize {
        let addr = rsp.saturating_add((index * core::mem::size_of::<u64>()) as u64);
        let value = unsafe { (addr as *const u64).read_volatile() };
        crate::debug::println!(
            "general protection stack[{}]: {:#x} = {:#x}",
            index,
            addr,
            value
        );
    }
}

fn log_page_fault_details(error_code: u64, cr2: u64, rip: u64, rsp: u64) {
    let present = (error_code & 0x1) != 0;
    let write = (error_code & 0x2) != 0;
    let user = (error_code & 0x4) != 0;
    let reserved = (error_code & 0x8) != 0;
    let instruction_fetch = (error_code & 0x10) != 0;
    let protection_key = (error_code & 0x20) != 0;
    let shadow_stack = (error_code & 0x40) != 0;
    let sgx = (error_code & 0x80) != 0;
    crate::debug::println!(
        "page fault detail: ec={:#x} present={} write={} user={} rsvd={} ifetch={} pkey={} sstk={} sgx={} rip={:#x} cr2={:#x}",
        error_code,
        present,
        write,
        user,
        reserved,
        instruction_fetch,
        protection_key,
        shadow_stack,
        sgx,
        rip,
        cr2,
    );

    for index in 0..8usize {
        let addr = rsp.saturating_add((index * core::mem::size_of::<u64>()) as u64);
        let value = unsafe { (addr as *const u64).read_volatile() };
        crate::debug::println!("page fault stack[{}]: {:#x} = {:#x}", index, addr, value);
    }
}

pub extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    crate::debug::dump_recent_trace_locations("double-fault");
    panic!(
        "Double fault: error code = {:#x}, cr2 = {:#x}, rip = {:#x}, cs = {:#x}, rflags = {:#x}, rsp = {:#x}, ss = {:#x}",
        error_code,
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        stack_frame.stack_pointer.as_u64(),
        stack_frame.stack_segment.0,
    );
}

fn is_user_mode(stack_frame: &InterruptStackFrame) -> bool {
    (stack_frame.code_segment.0 & 0x3) == 0x3
}

#[allow(dead_code)]
pub extern "x86-interrupt" fn rtc_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::arch::rtc::on_interrupt();
    crate::arch::pic::send_eoi(RTC_INTERRUPT_VECTOR);
}

pub fn pic_interrupt_handler(
    _stack_frame: InterruptStackFrame,
    index: u8,
    _error_code: Option<u64>,
) {
    let irq = index.saturating_sub(crate::arch::pic::PIC_1_OFFSET);
    let _ = crate::user::syscall::with_kernel_gs_base(|| crate::driver::irq::dispatch_pic_irq(irq));
    crate::arch::pic::send_eoi(index);
}

pub extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::user::syscall::with_kernel_gs_base(|| {
        crate::input::on_keyboard_interrupt();
        let _ = crate::driver::irq::dispatch_pic_irq(1);
    });
    crate::arch::pic::send_eoi(KEYBOARD_INTERRUPT_VECTOR);
}

pub extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::user::syscall::with_kernel_gs_base(|| {
        crate::input::on_mouse_interrupt();
        let _ = crate::driver::irq::dispatch_pic_irq(12);
    });
    crate::arch::pic::send_eoi(MOUSE_INTERRUPT_VECTOR);
}
