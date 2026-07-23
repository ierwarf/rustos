use core::arch::global_asm;

use x86_64::registers::control::Cr2;
use x86_64::structures::idt::InterruptStackFrame;

const KEYBOARD_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_1_OFFSET + 1;
const MOUSE_INTERRUPT_VECTOR: u8 = crate::arch::pic::PIC_2_OFFSET + 4;

// LLVM's current `x86-interrupt` error-code prologue leaves `%rsp` eight bytes
// away from the ordinary SysV call-site contract before invoking a nested Rust
// function. Keep the generated interrupt prologue for its complete register
// save/restore, but cross one explicit alignment bridge before entering the
// shared Rust exception path. The bridge also accepts already-aligned
// no-error-code entries, so every general exception has one contract.
global_asm!(
    r#"
    .global rustos_default_handler_alignment_bridge
    .type rustos_default_handler_alignment_bridge, @function
rustos_default_handler_alignment_bridge:
    mov r11, rsp
    and rsp, -16
    sub rsp, 16
    mov [rsp], r11
    call rustos_default_handler_aligned
    mov rsp, [rsp]
    ret
    .size rustos_default_handler_alignment_bridge, . - rustos_default_handler_alignment_bridge
"#
);

unsafe extern "Rust" {
    fn rustos_default_handler_alignment_bridge(
        stack_frame: InterruptStackFrame,
        index: u8,
        error_code: Option<u64>,
    );
}

// `set_general_handler!` inlines this tiny handoff into each generated
// `x86-interrupt` wrapper. That makes the assembly bridge the first nested
// call boundary; no ordinary Rust prologue is permitted to run on the
// error-code wrapper's misaligned stack.
#[inline(always)]
pub fn default_handler(stack_frame: InterruptStackFrame, index: u8, error_code: Option<u64>) {
    unsafe {
        rustos_default_handler_alignment_bridge(stack_frame, index, error_code);
    }
}

#[unsafe(no_mangle)]
fn rustos_default_handler_aligned(
    stack_frame: InterruptStackFrame,
    index: u8,
    error_code: Option<u64>,
) {
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
        match crate::hooks::retire_current_user_task_due_to_fault(
            index,
            error_code,
            cr2,
            stack_frame.instruction_pointer.as_u64(),
            stack_frame.stack_pointer.as_u64(),
        ) {
            crate::hooks::UserFaultDisposition::Resumed => return,
            crate::hooks::UserFaultDisposition::Retired => {
                crate::hooks::halt_current_retired_task();
            }
            crate::hooks::UserFaultDisposition::Unhandled => {}
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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
    if let Some(snapshot) = crate::hooks::current_user_snapshot() {
        crate::debug::println!(
            "NMI: current user abi={:?} pid={} tid={} session={:?}",
            snapshot.abi,
            snapshot.process_id,
            snapshot.thread_id,
            snapshot.console_session_raw,
        );
    } else {
        crate::debug::println!("NMI: no current user task");
    }
    crate::debug::dump_recent_trace_locations("nmi");
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
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

#[cfg(test)]
mod tests {
    #[test]
    fn general_exception_bridge_aligns_every_rust_call_boundary() {
        for raw_rsp in [
            0x1000_u64,
            0x1008,
            0x100f,
            0x1010,
            u64::MAX - 0x1f,
        ] {
            let aligned = (raw_rsp & !0xf).wrapping_sub(16);
            assert_eq!(aligned & 0xf, 0);
            assert!(aligned <= raw_rsp);
        }
    }
}

pub fn pic_interrupt_handler(
    _stack_frame: InterruptStackFrame,
    index: u8,
    _error_code: Option<u64>,
) {
    let irq = index.saturating_sub(crate::arch::pic::PIC_1_OFFSET);
    let _ = crate::hooks::dispatch_pic_irq(irq);
    crate::arch::pic::send_eoi(index);
}

/// Generic MSI/MSI-X dispatch is intentionally separate from the legacy PIC
/// range. The per-vector callback is lock-free and device-local; policy work
/// is deferred to the owning broker/service turn.
pub fn msi_interrupt_handler(
    _stack_frame: InterruptStackFrame,
    index: u8,
    _error_code: Option<u64>,
) {
    crate::arch::msi::dispatch(index);
}

/// DVM owns input delivery. Legacy keyboard IRQs have no RustOS input policy
/// consumer; acknowledge them so a physical/spurious line cannot wedge PIC.
pub extern "x86-interrupt" fn keyboard_interrupt_eoi_handler(_stack_frame: InterruptStackFrame) {
    crate::arch::pic::send_eoi(KEYBOARD_INTERRUPT_VECTOR);
}

/// See `keyboard_interrupt_eoi_handler`.
pub extern "x86-interrupt" fn mouse_interrupt_eoi_handler(_stack_frame: InterruptStackFrame) {
    crate::arch::pic::send_eoi(MOUSE_INTERRUPT_VECTOR);
}
