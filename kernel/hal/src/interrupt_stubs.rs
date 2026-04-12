use core::arch::global_asm;

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

    .global software_schedule_trap
    .type software_schedule_trap, @function
    software_schedule_trap:
        int 0x30
        ret
    .size software_schedule_trap, . - software_schedule_trap
"#
);
