use core::arch::{asm, global_asm};

#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct FxSaveArea {
    bytes: [u8; 512],
}

impl FxSaveArea {
    pub const fn new() -> Self {
        let mut bytes = [0u8; 512];
        // Default x87 control word and MXCSR state for a fresh task.
        bytes[0] = 0x7f;
        bytes[1] = 0x03;
        bytes[24] = 0x80;
        bytes[25] = 0x1f;
        Self { bytes }
    }
}

#[inline]
pub fn current_rip() -> u64 {
    let rip: u64;
    unsafe {
        asm!(
            "lea {rip}, [rip + 0]",
            rip = out(reg) rip,
            options(nomem, nostack, preserves_flags),
        );
    }
    rip
}

pub unsafe fn enter_higher_half(entry: u64, boot_info_ptr: u64) -> ! {
    unsafe {
        asm!(
            "mov rdi, {boot_info_ptr}",
            "add rsp, {virt_offset}",
            "test rsp, 8",
            "jnz 2f",
            "sub rsp, 8",
            "2:",
            "jmp {entry}",
            boot_info_ptr = in(reg) boot_info_ptr,
            virt_offset = in(reg) crate::paging::KERNEL_VIRT_OFFSET,
            entry = in(reg) entry,
            options(noreturn),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub unsafe fn save_fxstate(area: &mut FxSaveArea) {
    unsafe {
        asm!(
            "fxsave64 [{ptr}]",
            ptr = in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub unsafe fn restore_fxstate(area: &FxSaveArea) {
    unsafe {
        asm!(
            "fxrstor64 [{ptr}]",
            ptr = in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}

global_asm!(
    r#"
    .macro SAVE_CONTEXT
        sub rsp, 0x100
        movdqu [rsp + 0x00], xmm0
        movdqu [rsp + 0x10], xmm1
        movdqu [rsp + 0x20], xmm2
        movdqu [rsp + 0x30], xmm3
        movdqu [rsp + 0x40], xmm4
        movdqu [rsp + 0x50], xmm5
        movdqu [rsp + 0x60], xmm6
        movdqu [rsp + 0x70], xmm7
        movdqu [rsp + 0x80], xmm8
        movdqu [rsp + 0x90], xmm9
        movdqu [rsp + 0xA0], xmm10
        movdqu [rsp + 0xB0], xmm11
        movdqu [rsp + 0xC0], xmm12
        movdqu [rsp + 0xD0], xmm13
        movdqu [rsp + 0xE0], xmm14
        movdqu [rsp + 0xF0], xmm15

        push r15
        push r14
        push r13
        push r12
        push r11
        push r10
        push r9
        push r8
        push rbp
        push rdi
        push rsi
        push rdx
        push rcx
        push rbx
        push rax
    .endm

    .macro RESTORE_CONTEXT_AND_IRET frame_offset
        movdqu xmm0, [rsp + 0x78]
        movdqu xmm1, [rsp + 0x88]
        movdqu xmm2, [rsp + 0x98]
        movdqu xmm3, [rsp + 0xA8]
        movdqu xmm4, [rsp + 0xB8]
        movdqu xmm5, [rsp + 0xC8]
        movdqu xmm6, [rsp + 0xD8]
        movdqu xmm7, [rsp + 0xE8]
        movdqu xmm8, [rsp + 0xF8]
        movdqu xmm9, [rsp + 0x108]
        movdqu xmm10, [rsp + 0x118]
        movdqu xmm11, [rsp + 0x128]
        movdqu xmm12, [rsp + 0x138]
        movdqu xmm13, [rsp + 0x148]
        movdqu xmm14, [rsp + 0x158]
        movdqu xmm15, [rsp + 0x168]

        mov rax, [rsp + 0x00]
        mov rbx, [rsp + 0x08]
        mov rcx, [rsp + 0x10]
        mov rdx, [rsp + 0x18]
        mov rsi, [rsp + 0x20]
        mov rdi, [rsp + 0x28]
        mov rbp, [rsp + 0x30]
        mov r8, [rsp + 0x38]
        mov r9, [rsp + 0x40]
        mov r10, [rsp + 0x48]
        mov r11, [rsp + 0x50]
        mov r12, [rsp + 0x58]
        mov r13, [rsp + 0x60]
        mov r14, [rsp + 0x68]
        mov r15, [rsp + 0x70]

        add rsp, \frame_offset
        iretq
    .endm

    .global timer_interrupt_handler
    .type timer_interrupt_handler, @function
    timer_interrupt_handler:
        test byte ptr [rsp + 8], 0x3
        jnz 1f
        push 0
        push 1
1:
        SAVE_CONTEXT

        mov rax, [rsp + 0x178]
        cmp rax, 1
        je 2f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rsi
        mov [rsp + 0x180], rdi
        mov [rsp + 0x188], rax
        mov [rsp + 0x190], rcx
        mov [rsp + 0x198], rdx
2:

        cld
        mov rdi, rsp
        and rsp, -16
        // SysV x86_64 requires 16-byte alignment before `call`.
        call timer_interrupt_dispatch
        mov rsp, rax

        test byte ptr [rsp + 0x190], 0x3
        jz 3f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rdx
        mov [rsp + 0x180], rsi
        mov [rsp + 0x188], rdi
        mov [rsp + 0x190], rax
        mov [rsp + 0x198], rcx
        RESTORE_CONTEXT_AND_IRET 0x178
3:
        RESTORE_CONTEXT_AND_IRET 0x188

    .size timer_interrupt_handler, . - timer_interrupt_handler

    .global rtc_scheduler_interrupt_handler
    .type rtc_scheduler_interrupt_handler, @function
    rtc_scheduler_interrupt_handler:
        test byte ptr [rsp + 8], 0x3
        jnz 4f
        push 0
        push 1
4:
        SAVE_CONTEXT

        mov rax, [rsp + 0x178]
        cmp rax, 1
        je 5f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rsi
        mov [rsp + 0x180], rdi
        mov [rsp + 0x188], rax
        mov [rsp + 0x190], rcx
        mov [rsp + 0x198], rdx
5:

        cld
        mov rdi, rsp
        and rsp, -16
        // SysV x86_64 requires 16-byte alignment before `call`.
        call rtc_interrupt_dispatch
        mov rsp, rax

        test byte ptr [rsp + 0x190], 0x3
        jz 6f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rdx
        mov [rsp + 0x180], rsi
        mov [rsp + 0x188], rdi
        mov [rsp + 0x190], rax
        mov [rsp + 0x198], rcx
        RESTORE_CONTEXT_AND_IRET 0x178
6:
        RESTORE_CONTEXT_AND_IRET 0x188

    .size rtc_scheduler_interrupt_handler, . - rtc_scheduler_interrupt_handler

    .global software_schedule_interrupt_handler
    .type software_schedule_interrupt_handler, @function
    software_schedule_interrupt_handler:
        test byte ptr [rsp + 8], 0x3
        jnz 7f
        push 0
        push 1
7:
        SAVE_CONTEXT

        mov rax, [rsp + 0x178]
        cmp rax, 1
        je 8f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rsi
        mov [rsp + 0x180], rdi
        mov [rsp + 0x188], rax
        mov [rsp + 0x190], rcx
        mov [rsp + 0x198], rdx
8:

        cld
        mov rdi, rsp
        and rsp, -16
        call software_schedule_interrupt_dispatch
        mov rsp, rax

        test byte ptr [rsp + 0x190], 0x3
        jz 9f
        mov rax, [rsp + 0x178]
        mov rcx, [rsp + 0x180]
        mov rdx, [rsp + 0x188]
        mov rsi, [rsp + 0x190]
        mov rdi, [rsp + 0x198]
        mov [rsp + 0x178], rdx
        mov [rsp + 0x180], rsi
        mov [rsp + 0x188], rdi
        mov [rsp + 0x190], rax
        mov [rsp + 0x198], rcx
        RESTORE_CONTEXT_AND_IRET 0x178
9:
        RESTORE_CONTEXT_AND_IRET 0x188

    .size software_schedule_interrupt_handler, . - software_schedule_interrupt_handler
"#
);

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
pub unsafe fn copy_sse2(src: *const u8, dst: *mut u8, len: usize) {
    use core::arch::x86_64::*;
    use core::ptr;

    if len == 0 || src == dst {
        return;
    }

    let src_addr = src as usize;
    let dst_addr = dst as usize;
    let overlap = match (src_addr.checked_add(len), dst_addr.checked_add(len)) {
        (Some(src_end), Some(dst_end)) => src_addr < dst_end && dst_addr < src_end,
        _ => true,
    };
    if overlap {
        unsafe {
            ptr::copy(src, dst, len);
        }
        return;
    }

    let mut i = 0usize;
    let mut used_stream_store = false;

    unsafe {
        // Align destination to 16 bytes for streaming stores.
        while i < len && ((dst.add(i) as usize) & 0xF) != 0 {
            ptr::write(dst.add(i), ptr::read(src.add(i)));
            i += 1;
        }

        while i + 64 <= len {
            let a = _mm_loadu_si128(src.add(i) as *const __m128i);
            let b = _mm_loadu_si128(src.add(i + 16) as *const __m128i);
            let c = _mm_loadu_si128(src.add(i + 32) as *const __m128i);
            let d = _mm_loadu_si128(src.add(i + 48) as *const __m128i);

            _mm_stream_si128(dst.add(i) as *mut __m128i, a);
            _mm_stream_si128(dst.add(i + 16) as *mut __m128i, b);
            _mm_stream_si128(dst.add(i + 32) as *mut __m128i, c);
            _mm_stream_si128(dst.add(i + 48) as *mut __m128i, d);
            i += 64;
            used_stream_store = true;
        }

        if i < len {
            ptr::copy_nonoverlapping(src.add(i), dst.add(i), len - i);
        }
        if used_stream_store {
            _mm_sfence();
        }
    }
}
