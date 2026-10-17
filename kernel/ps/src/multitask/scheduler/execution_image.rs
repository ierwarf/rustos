//! Architecture image construction. Production entry-frame writers use actual
//! owned allocation geometry and an IRQ-excluded exclusive frame lease; the
//! directory still encloses admission/exec and MM publication transactions.
use super::*;

// Fixed ABI bytes only; callers must retain the actual exclusive FrameWriter.
pub(super) fn write_user_entry_frame(
    stack_top: usize,
    bootstrap: &UserTaskBootstrap,
    user_cs: u64,
    user_ss: u64,
    rflags: u64,
) -> usize {
    let context_ptr = stack_top - mem::size_of::<SavedContext>();
    let context = context_ptr as *mut SavedContext;
    // SAFETY: constructor or exec retains exclusive frame writer custody across
    // this complete fixed-ABI write and its subsequent publication.
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

// Fixed kernel ABI bytes; caller retains an exclusive physical FrameWriter.
fn write_kernel_entry_frame(
    stack_top: usize,
    cs: u64,
    ss: u64,
    rflags: u64,
    entry_rip: u64,
    arg0: u64,
) -> usize {
    let task_rsp = stack_top - TASK_ENTRY_STACK_RESERVE_QWORDS * mem::size_of::<u64>();
    let context_ptr = task_rsp - mem::size_of::<SavedContext>();
    // SAFETY: the writer owns checked unpublished stack geometry throughout
    // both the architectural trailer and complete SavedContext initialization.
    unsafe {
        let stack_slots = task_rsp as *mut u64;
        ptr::write(stack_slots, task_rsp as u64);
        ptr::write(stack_slots.add(1), ss);
        ptr::write(stack_slots.add(2), 0);
        let context = context_ptr as *mut SavedContext;
        ptr::write_bytes(context as *mut u8, 0, mem::size_of::<SavedContext>());
        (*context).rdi = arg0;
        (*context).rsp = task_rsp as u64;
        (*context).ss = ss;
        (*context).rip = entry_rip;
        (*context).cs = cs;
        (*context).rflags = rflags;
    }
    context_ptr
}
/// Actual reserved producer controls geometry and complete kernel frame bytes.
pub(super) fn reserved_kernel_entry(
    ticket: &stack_resources::admission::Ticket,
    cs: u64,
    ss: u64,
    rflags: u64,
    entry_rip: u64,
    arg0: u64,
) -> usize {
    ticket.with_image(|writer| {
        write_kernel_entry_frame(writer.stack_top(), cs, ss, rflags, entry_rip, arg0)
    })
}

/// Independent user producer owns the actual allocation and exclusive frame lease.
pub(super) fn reserved_user_entry(
    ticket: &stack_resources::admission::Ticket,
    bootstrap: &UserTaskBootstrap,
    user_cs: u64,
    user_ss: u64,
    rflags: u64,
) -> usize {
    ticket.with_image(|writer| {
        write_user_entry_frame(writer.stack_top(), bootstrap, user_cs, user_ss, rflags)
    })
}

#[cfg(test)]
#[path = "execution_image/tests.rs"]
mod tests;
