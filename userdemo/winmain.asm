BITS 64
DEFAULT REL

%define STD_INPUT_HANDLE  0xfffffff6
%define STD_OUTPUT_HANDLE 0xfffffff5

section .text
global start

start:
    sub rsp, 40
    mov ecx, STD_OUTPUT_HANDLE
    call [rel iat_GetStdHandle]
    add rsp, 40
    mov [rel stdout_handle], rax

    sub rsp, 40
    mov ecx, STD_INPUT_HANDLE
    call [rel iat_GetStdHandle]
    add rsp, 40
    mov [rel stdin_handle], rax

    sub rsp, 40
    mov rcx, [rel stdout_handle]
    lea rdx, [rel banner]
    mov r8d, banner_len
    lea r9, [rel bytes_io]
    mov qword [rsp + 32], 0
    call [rel iat_WriteFile]
    add rsp, 40

read_loop:
    sub rsp, 40
    mov rcx, [rel stdin_handle]
    lea rdx, [rel input_buffer]
    mov r8d, 255
    lea r9, [rel bytes_io]
    mov qword [rsp + 32], 0
    call [rel iat_ReadFile]
    add rsp, 40
    test eax, eax
    jz exit_process

    mov ecx, [rel bytes_io]
    test ecx, ecx
    jz read_loop

    lea rsi, [rel input_buffer]
    lea rdi, [rel token_buffer]
    xor edx, edx

skip_leading_ws:
    test ecx, ecx
    jz read_loop
    mov al, [rsi]
    cmp al, ' '
    je consume_leading_ws
    cmp al, 9
    je consume_leading_ws
    cmp al, 10
    je consume_leading_ws
    cmp al, 11
    je consume_leading_ws
    cmp al, 12
    je consume_leading_ws
    cmp al, 13
    je consume_leading_ws
    jmp collect_token

consume_leading_ws:
    inc rsi
    dec ecx
    jmp skip_leading_ws

collect_token:
    test ecx, ecx
    jz flush_token
    mov al, [rsi]
    cmp al, ' '
    je flush_token
    cmp al, 9
    je flush_token
    cmp al, 10
    je flush_token
    cmp al, 11
    je flush_token
    cmp al, 12
    je flush_token
    cmp al, 13
    je flush_token

    cmp edx, 255
    jae skip_store
    mov [rdi + rdx], al
    inc edx

skip_store:
    inc rsi
    dec ecx
    jmp collect_token

flush_token:
    test edx, edx
    jz read_loop
    mov [rel token_len], edx

    sub rsp, 40
    mov rcx, [rel stdout_handle]
    lea rdx, [rel token_buffer]
    mov r8d, [rel token_len]
    lea r9, [rel bytes_io]
    mov qword [rsp + 32], 0
    call [rel iat_WriteFile]
    add rsp, 40

    sub rsp, 40
    mov rcx, [rel stdout_handle]
    lea rdx, [rel newline]
    mov r8d, newline_len
    lea r9, [rel bytes_io]
    mov qword [rsp + 32], 0
    call [rel iat_WriteFile]
    add rsp, 40
    jmp read_loop

exit_process:
    sub rsp, 40
    xor ecx, ecx
    call [rel iat_ExitProcess]
    int3

section .rdata
banner:
    db "console ready from win32", 13, 10
banner_len equ $ - banner

newline:
    db 10
newline_len equ $ - newline

section .data
stdin_handle dq 0
stdout_handle dq 0
bytes_io dd 0
token_len dd 0

section .bss
input_buffer resb 256
token_buffer resb 256

section .idata$2 data readable writeable
align 8
import_descriptor:
    dd ilt wrt ..imagebase
    dd 0
    dd 0
    dd dll_name wrt ..imagebase
    dd iat wrt ..imagebase
    dd 0, 0, 0, 0, 0

section .idata$4 data readable writeable
align 8
ilt:
    dd hint_GetStdHandle wrt ..imagebase, 0
    dd hint_ReadFile wrt ..imagebase, 0
    dd hint_WriteFile wrt ..imagebase, 0
    dd hint_ExitProcess wrt ..imagebase, 0
    dq 0

section .idata$5 data readable writeable
align 8
iat:
iat_GetStdHandle:
    dd hint_GetStdHandle wrt ..imagebase, 0
iat_ReadFile:
    dd hint_ReadFile wrt ..imagebase, 0
iat_WriteFile:
    dd hint_WriteFile wrt ..imagebase, 0
iat_ExitProcess:
    dd hint_ExitProcess wrt ..imagebase, 0
    dq 0

section .idata$6 data readable writeable
hint_GetStdHandle:
    dw 0
    db "GetStdHandle", 0
    align 2, db 0

hint_ReadFile:
    dw 0
    db "ReadFile", 0
    align 2, db 0

hint_WriteFile:
    dw 0
    db "WriteFile", 0
    align 2, db 0

hint_ExitProcess:
    dw 0
    db "ExitProcess", 0
    align 2, db 0

section .idata$7 data readable writeable
dll_name:
    db "KERNEL32.dll", 0
