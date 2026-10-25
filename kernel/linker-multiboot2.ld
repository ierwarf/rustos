ENTRY(_start)

KERNEL_LOAD_BASE = 0x200000;

PHDRS
{
  text PT_LOAD FILEHDR PHDRS FLAGS(5);
  rodata PT_LOAD FLAGS(4);
  relro PT_LOAD FLAGS(4);
  data PT_LOAD FLAGS(6);
  dynamic PT_DYNAMIC FLAGS(6);
}

SECTIONS
{
  . = KERNEL_LOAD_BASE + SIZEOF_HEADERS;

  .text : ALIGN(8)
  {
    KEEP(*(.multiboot2.header))
    KEEP(*(.text._start))
    *(.text .text.*)
  } :text

  . = ALIGN(4096);
  .rodata : ALIGN(4096)
  {
    *(.note.gnu.build-id)
    KEEP(*(.rodata.ap_trampoline))
    *(.rodata .rodata.*)
    *(.srodata .srodata.*)
    *(.eh_frame_hdr)
    *(.eh_frame)
  } :rodata

  .dynsym : { *(.dynsym) } :rodata
  .dynstr : { *(.dynstr) } :rodata
  .gnu.hash : { *(.gnu.hash) } :rodata
  .hash : { *(.hash) } :rodata
  .rela.dyn : ALIGN(8)
  {
    HIDDEN(rustos_rela_start = .);
    *(.rela.dyn)
    HIDDEN(rustos_rela_end = .);
  } :rodata

  . = ALIGN(4096);
  .data.rel.ro : ALIGN(4096)
  {
    *(.data.rel.ro .data.rel.ro.*)
  } :relro

  .dynamic : ALIGN(8) { *(.dynamic) } :relro :dynamic

  .got : ALIGN(8)
  {
    *(.got .got.*)
  } :relro

  .data : ALIGN(4096)
  {
    *(.data .data.*)
    *(.sdata .sdata.*)
  } :data

  . = ALIGN(4096);
  .bss : ALIGN(4096)
  {
    *(.bss .bss.*)
    *(.sbss .sbss.*)
    *(COMMON)
  } :data

  . = ALIGN(4096);
  _end = .;

  /* Only the 32-bit BSP bootstrap uses absolute physical addresses. */
  HIDDEN(rustos_boot_pml4_phys = ABSOLUTE(rustos_boot_pml4));
  HIDDEN(rustos_boot_pdpt_phys = ABSOLUTE(rustos_boot_pdpt));
  HIDDEN(rustos_boot_pd0_phys = ABSOLUTE(rustos_boot_pd0));
  HIDDEN(rustos_boot_pd1_phys = ABSOLUTE(rustos_boot_pd1));
  HIDDEN(rustos_boot_pd2_phys = ABSOLUTE(rustos_boot_pd2));
  HIDDEN(rustos_boot_pd3_phys = ABSOLUTE(rustos_boot_pd3));
  HIDDEN(rustos_boot_gdt_phys = ABSOLUTE(rustos_boot_gdt));
  HIDDEN(rustos_boot_gdt_desc_phys = ABSOLUTE(rustos_boot_gdt_desc));
  HIDDEN(rustos_multiboot_long_mode_phys = ABSOLUTE(rustos_multiboot_long_mode));

  ASSERT((rustos_ap_trampoline_end - rustos_ap_trampoline_start) <= 4096,
         "AP trampoline exceeds its executable page")

  /DISCARD/ :
  {
    *(.comment)
    *(.note.GNU-stack)
  }
}
