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

    # One-page real-mode AP startup image. The BSP copies this complete section
    # to physical 0x8000 only after kernel-mm atomically claims 0x8000..0xa000.
    # Mutable launch data lives in the separate no-execute page at 0x9000.
    .section .rodata.ap_trampoline, "a"
    .balign 4096
    .globl rustos_ap_trampoline_start
rustos_ap_trampoline_start:
    .set RUSTOS_AP_TRAMPOLINE_PHYS, 0x8000
    .set RUSTOS_AP_MAILBOX_PHYS, 0x9000
    .set RUSTOS_AP_MAILBOX_GENERATION, 16
    .set RUSTOS_AP_MAILBOX_STACK_TOP, 24
    .set RUSTOS_AP_MAILBOX_ENTRY, 32
    .set RUSTOS_AP_MAILBOX_CR3, 40
    .set RUSTOS_AP_MAILBOX_LOGICAL_INDEX, 48
    .set RUSTOS_AP_MAILBOX_EXPECTED_APIC_ID, 56
    .set RUSTOS_AP_MAILBOX_MAGIC, 0

    .code16
rustos_ap_real_mode:
    cli
    cld
    mov %cs, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    # INIT leaves CR0.NW and CR0.CD set on supported x86 platforms.  Enter the
    # architecturally defined no-fill state (CD=1, NW=0) and discard any reset
    # cache contents before the AP reads the shared startup mailbox.  Kernel-mm
    # installs the BSP MTRR/PAT baseline and only then clears CD before the AP
    # may publish private readiness.
    mov %cr0, %eax
    and $0xdfffffff, %eax
    mov %eax, %cr0
    wbinvd
    lgdtw rustos_ap_boot_gdt_desc - rustos_ap_trampoline_start
    mov %cr0, %eax
    or $0x1, %eax
    mov %eax, %cr0
    # The first far jump must load a 32-bit protected-mode code descriptor.
    # Loading the long-mode descriptor before IA32_EFER.LMA is active is a
    # boot-fatal #GP which becomes a triple fault because the AP has no IDT yet.
    ljmpl $0x08, $(RUSTOS_AP_TRAMPOLINE_PHYS + rustos_ap_protected_mode - rustos_ap_trampoline_start)

    .code32
rustos_ap_protected_mode:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    mov (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_CR3), %eax
    mov %eax, %cr3
    mov %cr4, %eax
    or $0x620, %eax
    mov %eax, %cr4
    mov $0xc0000080, %ecx
    rdmsr
    or $0x900, %eax
    wrmsr
    mov %cr0, %eax
    or $0x80010001, %eax
    mov %eax, %cr0
    # IA32_EFER.LME and CR0.PG are now active, so this far jump deliberately
    # switches CS from the 32-bit transition descriptor to the 64-bit one.
    ljmpl $0x18, $(RUSTOS_AP_TRAMPOLINE_PHYS + rustos_ap_long_mode - rustos_ap_trampoline_start)

    .code64
rustos_ap_long_mode:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_STACK_TOP), %rsp
    and $-16, %rsp
    xor %rbp, %rbp
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_LOGICAL_INDEX), %rdi
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_GENERATION), %rsi
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_EXPECTED_APIC_ID), %rdx
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_MAGIC), %rcx
    movq (RUSTOS_AP_MAILBOX_PHYS + RUSTOS_AP_MAILBOX_ENTRY), %rax
    call *%rax
rustos_ap_returned:
    cli
    hlt
    jmp rustos_ap_returned

    .balign 8
rustos_ap_boot_gdt:
    .quad 0
    # These descriptors live on the RX trampoline page after publication.
    # Set each accessed bit in the image: otherwise the CPU's first implicit
    # descriptor write faults after paging is enabled and, before IDT install,
    # necessarily escalates to an unobservable AP triple fault.
    .quad 0x00cf9b000000ffff
    .quad 0x00cf93000000ffff
    .quad 0x00af9b000000ffff
rustos_ap_boot_gdt_desc:
    .short rustos_ap_boot_gdt_desc - rustos_ap_boot_gdt - 1
    .long RUSTOS_AP_TRAMPOLINE_PHYS + rustos_ap_boot_gdt - rustos_ap_trampoline_start

    .globl rustos_ap_trampoline_end
rustos_ap_trampoline_end:

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
