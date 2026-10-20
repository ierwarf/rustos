/* HECS assembly island. No compiler-generated code between seed and check.
 * SysV entry/exit is respected; SYSCALL intentionally clobbers RAX/RCX/R11.
 * operation: 0=asynchronous spin, 24=yield, 35=blocking nanosleep, 186=gettid.
 * Return zero or first mismatch: 1=syscall, 2=RSP, 16+GPR, 64+XMM, 96=MXCSR.
 * SSE2 baseline only: this probe does NOT claim YMM/XSAVE coverage.
 */
.text
.p2align 4
.globl hecs_register_probe
.type hecs_register_probe,@function
hecs_register_probe:
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15
    subq $472, %rsp
    movq %rsi, 392(%rsp)
    movq %rsp, 384(%rsp)
    stmxcsr 400(%rsp)
    movl %edi, %eax
    andl $3, %eax
    shll $13, %eax
    orl $0x1f80, %eax
    movl %eax, 404(%rsp)
    ldmxcsr 404(%rsp)
    movq $100000, 416(%rsp)
    movq $0, 424(%rsp)
    movq $1, 432(%rsp)

.macro HECS_PATTERN index
    movabsq $(0x13579bdf02468000 + \index), %rax
    xorq %rdi, %rax
    movq %rax, (8*\index)(%rsp)
.endm
.irp i,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14
    HECS_PATTERN \i
.endr
.macro HECS_VECTOR index
    movabsq $(0xfedcba9876540000 + \index), %rax
    xorq %rdi, %rax
    movq %rax, (128+16*\index)(%rsp)
    notq %rax
    movq %rax, (136+16*\index)(%rsp)
    movdqu (128+16*\index)(%rsp), %xmm\index
.endm
.irp i,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    HECS_VECTOR \i
.endr
    cmpq $35, 392(%rsp)
    jne .Lhecs_seed_gpr
    leaq 424(%rsp), %rax
    movq %rax, 48(%rsp)
    movq $0, 40(%rsp)
.Lhecs_seed_gpr:
    movq 0(%rsp), %rbx
    movq 8(%rsp), %rbp
    movq 16(%rsp), %r12
    movq 24(%rsp), %r13
    movq 32(%rsp), %r14
    movq 40(%rsp), %rsi
    movq 48(%rsp), %rdi
    movq 56(%rsp), %rdx
    movq 64(%rsp), %r8
    movq 72(%rsp), %r9
    movq 80(%rsp), %r10
    movq 88(%rsp), %r15
    movq 96(%rsp), %rcx
    movq 104(%rsp), %r11
    movq 112(%rsp), %rax
    cmpq $0, 392(%rsp)
    je .Lhecs_spin
    movq 392(%rsp), %rax
    syscall
    cmpq $186, 392(%rsp)
    je .Lhecs_tid
    testq %rax, %rax
    jne .Lhecs_syscall_fail
    jmp .Lhecs_check
.Lhecs_tid:
    testq %rax, %rax
    jle .Lhecs_syscall_fail
    jmp .Lhecs_check
.Lhecs_spin:
    subq $1, 416(%rsp)
    jnz .Lhecs_spin
    cmpq $0, 416(%rsp)
    jne .Lhecs_syscall_fail
    cmpq 112(%rsp), %rax
    jne .Lhecs_fail_14
    cmpq 104(%rsp), %r11
    jne .Lhecs_fail_13
    cmpq 96(%rsp), %rcx
    jne .Lhecs_fail_12
.Lhecs_check:
    cmpq %rsp, 384(%rsp)
    jne .Lhecs_rsp_fail
.macro HECS_CHECK_GPR reg, index
    cmpq (8*\index)(%rsp), %\reg
    jne .Lhecs_fail_\index
.endm
    HECS_CHECK_GPR rbx, 0
    HECS_CHECK_GPR rbp, 1
    HECS_CHECK_GPR r12, 2
    HECS_CHECK_GPR r13, 3
    HECS_CHECK_GPR r14, 4
    HECS_CHECK_GPR rsi, 5
    HECS_CHECK_GPR rdi, 6
    HECS_CHECK_GPR rdx, 7
    HECS_CHECK_GPR r8, 8
    HECS_CHECK_GPR r9, 9
    HECS_CHECK_GPR r10, 10
    HECS_CHECK_GPR r15, 11
.macro HECS_CHECK_XMM index
    pcmpeqb (128+16*\index)(%rsp), %xmm\index
    pmovmskb %xmm\index, %eax
    cmpl $65535, %eax
    jne .Lhecs_xmm_fail_\index
.endm
.irp i,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    HECS_CHECK_XMM \i
.endr
    stmxcsr 408(%rsp)
    movl 408(%rsp), %eax
    cmpl 404(%rsp), %eax
    jne .Lhecs_mxcsr_fail
    xorl %eax, %eax
    jmp .Lhecs_done
.macro HECS_GPR_FAIL index
.Lhecs_fail_\index:
    movl $(16+\index), %eax
    jmp .Lhecs_done
.endm
.irp i,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14
    HECS_GPR_FAIL \i
.endr
.macro HECS_XMM_FAIL index
.Lhecs_xmm_fail_\index:
    movl $(64+\index), %eax
    jmp .Lhecs_done
.endm
.irp i,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
    HECS_XMM_FAIL \i
.endr
.Lhecs_syscall_fail:
    movl $1, %eax
    jmp .Lhecs_done
.Lhecs_rsp_fail:
    movl $2, %eax
    jmp .Lhecs_done
.Lhecs_mxcsr_fail:
    movl $96, %eax
.Lhecs_done:
    ldmxcsr 400(%rsp)
    addq $472, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    ret
.size hecs_register_probe, .-hecs_register_probe
.section .note.GNU-stack,"",@progbits
