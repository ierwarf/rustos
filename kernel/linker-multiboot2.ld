ENTRY(_start)

KERNEL_LOAD_BASE = 0x200000;

PHDRS
{
  text PT_LOAD FILEHDR PHDRS FLAGS(5);
  rodata PT_LOAD FLAGS(4);
  relro PT_LOAD FLAGS(4);
  data PT_LOAD FLAGS(6);
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
    *(.rodata .rodata.*)
    *(.srodata .srodata.*)
    *(.eh_frame_hdr)
    *(.eh_frame)
  } :rodata

  . = ALIGN(4096);
  .data.rel.ro : ALIGN(4096)
  {
    *(.data.rel.ro .data.rel.ro.*)
  } :relro

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

  /DISCARD/ :
  {
    *(.comment)
    *(.note.GNU-stack)
  }
}
