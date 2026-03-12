.section .rodata
.set bad_user_ptr, 0x8000200000

banner:
    .ascii "console ready\r\n"
.set banner_len, . - banner

newline:
    .ascii "\n"
.set newline_len, . - newline

fault_token:
    .ascii "fault"

badwrite_token:
    .ascii "badwrite"

badread_token:
    .ascii "badread"

.section .bss
.align 16
input_buffer:
    .skip 256

.align 16
token_buffer:
    .skip 256

.section .text
.global _start
.type _start, @function
_start:
    lea banner(%rip), %rdi
    mov $banner_len, %rsi
    mov $1, %rax
    syscall

read_loop:
    lea input_buffer(%rip), %rdi
    mov $256, %rsi
    mov $2, %rax
    syscall
    test %rax, %rax
    jle read_loop

    mov %rax, %rdx
    lea input_buffer(%rip), %rsi
    lea token_buffer(%rip), %rdi
    xor %rcx, %rcx

skip_leading_ws:
    test %rdx, %rdx
    jz read_loop
    movzbq (%rsi), %r8
    cmp $' ', %r8b
    je consume_leading_ws
    cmp $9, %r8b
    je consume_leading_ws
    cmp $10, %r8b
    je consume_leading_ws
    cmp $11, %r8b
    je consume_leading_ws
    cmp $12, %r8b
    je consume_leading_ws
    cmp $13, %r8b
    je consume_leading_ws
    jmp collect_token

consume_leading_ws:
    inc %rsi
    dec %rdx
    jmp skip_leading_ws

collect_token:
    test %rdx, %rdx
    jz flush_token
    movzbq (%rsi), %r8
    cmp $' ', %r8b
    je flush_token
    cmp $9, %r8b
    je flush_token
    cmp $10, %r8b
    je flush_token
    cmp $11, %r8b
    je flush_token
    cmp $12, %r8b
    je flush_token
    cmp $13, %r8b
    je flush_token

    cmp $256, %rcx
    jae skip_store
    mov %r8b, (%rdi,%rcx,1)
    inc %rcx

skip_store:
    inc %rsi
    dec %rdx
    jmp collect_token

flush_token:
    test %rcx, %rcx
    jz read_loop

    cmp $5, %rcx
    jne check_badwrite_token_len
    lea token_buffer(%rip), %r8
    lea fault_token(%rip), %r9
    mov $5, %r10

check_fault_token:
    movzbq (%r8), %r11
    movzbq (%r9), %rax
    cmp %al, %r11b
    jne write_token
    inc %r8
    inc %r9
    dec %r10
    jnz check_fault_token

    mov $bad_user_ptr, %r8
    movzbq (%r8), %r9
    jmp read_loop

check_badwrite_token_len:
    cmp $8, %rcx
    jne check_badread_token_len
    lea token_buffer(%rip), %r8
    lea badwrite_token(%rip), %r9
    mov $8, %r10

check_badwrite_token:
    movzbq (%r8), %r11
    movzbq (%r9), %rax
    cmp %al, %r11b
    jne check_badread_token_len
    inc %r8
    inc %r9
    dec %r10
    jnz check_badwrite_token

    mov $bad_user_ptr, %rdi
    mov $4, %rsi
    mov $1, %rax
    syscall
    jmp read_loop

check_badread_token_len:
    cmp $7, %rcx
    jne write_token
    lea token_buffer(%rip), %r8
    lea badread_token(%rip), %r9
    mov $7, %r10

check_badread_token:
    movzbq (%r8), %r11
    movzbq (%r9), %rax
    cmp %al, %r11b
    jne write_token
    inc %r8
    inc %r9
    dec %r10
    jnz check_badread_token

    mov $bad_user_ptr, %rdi
    mov $32, %rsi
    mov $2, %rax
    syscall
    jmp read_loop

write_token:
    lea token_buffer(%rip), %rdi
    mov %rcx, %rsi
    mov $1, %rax
    syscall

    lea newline(%rip), %rdi
    mov $newline_len, %rsi
    mov $1, %rax
    syscall
    jmp read_loop

.size _start, . - _start

.section .note.GNU-stack,"",@progbits
