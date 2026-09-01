//! x86_64 exception and interrupt admission for every online CPU.
//!
//! - **Owner:** `kernel-hal` owns CPU-entry decoding; policy and user-fault
//!   disposition belong to registered executive/compat hooks.
//! - **Boundary:** Hardware frames and user-controlled register state enter
//!   ring0 here.
//! - **Lifecycle:** Entry validates and classifies one frame, invokes a
//!   snapshotted hook, then either resumes the same task or retires it.
//! - **Concurrency:** IDT leaves enter tracked IRQ context before touching
//!   shared state; callbacks run without the hook registry lock.
//! - **Failure:** Kernel faults are fatal; user faults use exact-task
//!   retirement and cannot unwind through Rust.
//! - **Forbidden:** No service policy, allocation-heavy recovery, or
//!   untracked lock acquisition in an IDT leaf.
//! - **Evidence:** `exception-retirement` and `msi-vector-ingress`.
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

/// Emit a lock-free, allocation-free exception marker before entering any
/// formatted diagnostics. This remains usable when the exception was caused
/// by corrupted scheduler or lock state and the normal panic path would fault
/// recursively before producing evidence.
#[inline(always)]
fn emergency_exception_marker(index: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in [
        b'\n',
        b'!',
        b'E',
        b'X',
        b':',
        HEX[usize::from(index >> 4)],
        HEX[usize::from(index & 0x0f)],
        b'\n',
    ] {
        unsafe {
            core::arch::asm!(
                "out dx, al",
                in("dx") 0x00e9_u16,
                in("al") byte,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
}

#[unsafe(no_mangle)]
fn rustos_default_handler_aligned(
    stack_frame: InterruptStackFrame,
    index: u8,
    error_code: Option<u64>,
) {
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    let user_mode = is_user_mode(&stack_frame);

    // A successfully handled demand fault is ordinary VM control flow, not an
    // exception diagnostic. Resolve it before emitting emergency markers or
    // dumping the recent trace ring; rejected and illegal faults retain the
    // complete legacy diagnostic/retirement path below.
    if user_mode && index == 14 {
        match crate::hooks::try_handle_current_user_page_fault(
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
    }

    emergency_exception_marker(index);
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
    if user_mode {
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

    let logical_cpu = nucleus_core::util::lockdep::current_cpu_index();
    panic!(
        "Unhandled exception: vector = {}, error code = {:?}, cr2 = {:#x}, rip = {:#x}, cs = {:#x}, rflags = {:#x}, rsp = {:#x}, ss = {:#x}, cpu = {}, apic = {:#x}, task_owner = {:?}, irq_depth = {}, preempt_depth = {}, raw_class = {:?}, last_dispatch = {:?}, last_scheduler_observation = {:?}",
        index,
        error_code,
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        stack_frame.stack_pointer.as_u64(),
        stack_frame.stack_segment.0,
        logical_cpu,
        nucleus_core::util::lockdep::hardware_apic_id(),
        nucleus_core::util::lockdep::current_task_owner(),
        nucleus_core::util::lockdep::irq_context_depth(),
        nucleus_core::util::lockdep::preemption_depth(),
        nucleus_core::util::lockdep::current_lock_class(),
        nucleus_core::util::lockdep::scheduler_dispatch_witness(logical_cpu),
        nucleus_core::util::lockdep::scheduler_observation_witness(logical_cpu),
    );
}

pub extern "x86-interrupt" fn non_maskable_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // NMI can interrupt the owner of every kernel lock. Keep this leaf free of
    // formatted logging, hook-registry access, process-state access,
    // allocation, and tracked locks. Rich diagnostics must be collected from
    // ordinary task/IRQ context after a future lock-free snapshot handoff.
    emergency_exception_marker(2);
}

#[cfg_attr(not(rustos_debug_print_enabled), allow(unused_variables))]
fn log_general_protection_details(error_code: u64, rip: u64, rsp: u64) {
    crate::debug::println!(
        "general protection detail: ec={:#x} rip={:#x} rsp={:#x}",
        error_code,
        rip,
        rsp,
    );
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
        "page fault detail: ec={:#x} present={} write={} user={} rsvd={} ifetch={} pkey={} sstk={} sgx={} rip={:#x} cr2={:#x} rsp={:#x}",
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
        rsp,
    );
}

pub extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    emergency_exception_marker(8);
    let cr2 = Cr2::read().map(|addr| addr.as_u64()).unwrap_or(u64::MAX);
    crate::debug::dump_recent_trace_locations("double-fault");
    let rsp = stack_frame.stack_pointer.as_u64();
    let ring0_top = crate::arch::gdt::privilege_stack_top_for_current_cpu();
    // A kernel stack overflow is the one double fault whose cause is not in
    // the frame. `#PF` deliberately has no IST - it must stay reentrant,
    // and this kernel's page-fault handler blocks and yields, so a per-CPU
    // IST stack would be reused by the next faulting task - which means an
    // exhausted kernel stack cannot deliver its own `#PF` and escalates here
    // instead. Reporting the depth below the published ring0 top, and whether
    // the faulting address sits just under `rsp`, separates that case from an
    // ordinary double fault without consulting any lock or scheduler state.
    let depth_below_ring0_top = ring0_top.wrapping_sub(rsp);
    let push_fault = cr2 != u64::MAX && cr2 < rsp && rsp.wrapping_sub(cr2) <= 4096;
    panic!(
        "Double fault: error code = {:#x}, cr2 = {:#x}, rip = {:#x}, cs = {:#x}, rflags = {:#x}, rsp = {:#x}, ss = {:#x}, ring0_stack_top = {:#x}, depth_below_ring0_top = {:#x}, stack_push_fault = {}",
        error_code,
        cr2,
        stack_frame.instruction_pointer.as_u64(),
        stack_frame.code_segment.0,
        stack_frame.cpu_flags.bits(),
        rsp,
        stack_frame.stack_segment.0,
        ring0_top,
        depth_below_ring0_top,
        push_fault,
    );
}

fn is_user_mode(stack_frame: &InterruptStackFrame) -> bool {
    (stack_frame.code_segment.0 & 0x3) == 0x3
}

pub fn pic_interrupt_handler(
    _stack_frame: InterruptStackFrame,
    index: u8,
    _error_code: Option<u64>,
) {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
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
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::msi::dispatch(index);
}

/// DVM owns input delivery. Legacy keyboard IRQs have no RustOS input policy
/// consumer; acknowledge them so a physical/spurious line cannot wedge PIC.
pub extern "x86-interrupt" fn keyboard_interrupt_eoi_handler(_stack_frame: InterruptStackFrame) {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::pic::send_eoi(KEYBOARD_INTERRUPT_VECTOR);
}

/// See `keyboard_interrupt_eoi_handler`.
pub extern "x86-interrupt" fn mouse_interrupt_eoi_handler(_stack_frame: InterruptStackFrame) {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::pic::send_eoi(MOUSE_INTERRUPT_VECTOR);
}

/// Flush one generation-bound mailbox without acquiring a sender or scheduler
/// lock. The leaf publishes its acknowledgement before the local APIC EOI.
pub extern "x86-interrupt" fn tlb_shootdown_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let _irq_context = nucleus_core::util::lockdep::enter_irq_context();
    crate::arch::tlb_shootdown::on_interrupt();
}

#[cfg(test)]
mod tests {
    #[test]
    fn general_exception_bridge_aligns_every_rust_call_boundary() {
        for raw_rsp in [0x1000_u64, 0x1008, 0x100f, 0x1010, u64::MAX - 0x1f] {
            let aligned = (raw_rsp & !0xf).wrapping_sub(16);
            assert_eq!(aligned & 0xf, 0);
            assert!(aligned <= raw_rsp);
        }
    }

    #[test]
    fn handled_user_page_fault_skips_exception_dump_hot_path() {
        let source = include_str!("handlers.rs");
        let aligned = source
            .split("fn rustos_default_handler_aligned")
            .nth(1)
            .expect("aligned exception handler source");
        let hook = aligned
            .find("try_handle_current_user_page_fault")
            .expect("pager hook must remain installed");
        let resumed = aligned[hook..]
            .find("UserFaultDisposition::Resumed => return")
            .map(|offset| hook + offset)
            .expect("handled demand fault must return");
        let dump = aligned
            .find("dump_recent_trace_locations")
            .expect("unhandled exception diagnostics must remain");
        assert!(hook < resumed && resumed < dump);
    }
}
