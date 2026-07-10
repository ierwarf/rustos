    .section .multiboot2.header, "a"
    .balign 8
    .globl rustos_multiboot2_header
rustos_multiboot2_header:
    .long 0xe85250d6
    .long 0
    .long rustos_multiboot2_header_end - rustos_multiboot2_header
    .long -(0xe85250d6 + (rustos_multiboot2_header_end - rustos_multiboot2_header))

    .balign 8
    .short 1
    .short 0
    .long 32
    .long 6
    .long 8
    .long 14
    .long 15
    .long 21
    .long 17

    .balign 8
    .short 5
    .short 1
    .long 20
    .long 0
    .long 0
    .long 32

    .balign 8
    .short 6
    .short 1
    .long 8

    .balign 8
    .short 0
    .short 0
    .long 8
rustos_multiboot2_header_end:

    .section .bss
    .balign 4096
rustos_boot_pml4:
    .skip 4096
rustos_boot_pdpt:
    .skip 4096
rustos_boot_pd0:
    .skip 4096
rustos_boot_pd1:
    .skip 4096
rustos_boot_pd2:
    .skip 4096
rustos_boot_pd3:
    .skip 4096
    .balign 16
rustos_multiboot_stack:
    .skip 65536
rustos_multiboot_stack_top:

    .section .rodata
    .balign 8
rustos_boot_gdt:
    .quad 0
    .quad 0x00af9a000000ffff
    .quad 0x00cf92000000ffff
rustos_boot_gdt_desc:
    .short rustos_boot_gdt_desc - rustos_boot_gdt - 1
    .quad rustos_boot_gdt

    .section .text
    .code32
    .globl _start
_start:
    cli
    mov %eax, %esi
    mov %ebx, %edi

    mov $rustos_boot_pml4, %eax
    mov $rustos_boot_pdpt, %edx
    or $0x3, %edx
    mov %edx, (%eax)

    mov $rustos_boot_pdpt, %eax
    mov $rustos_boot_pd0, %edx
    or $0x3, %edx
    mov %edx, 0(%eax)
    mov $rustos_boot_pd1, %edx
    or $0x3, %edx
    mov %edx, 8(%eax)
    mov $rustos_boot_pd2, %edx
    or $0x3, %edx
    mov %edx, 16(%eax)
    mov $rustos_boot_pd3, %edx
    or $0x3, %edx
    mov %edx, 24(%eax)

    mov $rustos_boot_pd0, %edi
    xor %eax, %eax
    mov $2048, %ecx
1:
    mov %eax, %edx
    or $0x83, %edx
    mov %edx, (%edi)
    movl $0, 4(%edi)
    add $0x200000, %eax
    add $8, %edi
    loop 1b

    mov $rustos_boot_pml4, %eax
    mov %eax, %cr3
    mov %cr4, %eax
    # Rust code may use baseline x86_64 SSE instructions before the higher-level
    # SIMD initializer runs. Do not depend on the firmware or hypervisor having
    # enabled the OS-managed SSE state bits already.
    or $0x620, %eax
    mov %eax, %cr4
    mov $0xc0000080, %ecx
    rdmsr
    or $0x100, %eax
    wrmsr
    mov %cr0, %eax
    or $0x80000001, %eax
    mov %eax, %cr0
    lgdt rustos_boot_gdt_desc
    ljmp $0x08, $rustos_multiboot_long_mode

    .code64
rustos_multiboot_long_mode:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov $rustos_multiboot_stack_top, %rsp
    and $-16, %rsp
    mov %esi, %edi
    mov %ebx, %esi
    call multiboot2_entry64
2:
    hlt
    jmp 2b
