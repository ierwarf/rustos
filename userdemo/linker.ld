ENTRY(_start)

PHDRS
{
  text PT_LOAD FLAGS(5);
  data PT_LOAD FLAGS(6);
}

SECTIONS
{
  . = 0x0000008000400000;

  .text ALIGN(0x1000) :
  {
    *(.text*)
    *(.rodata*)
  } :text

  .data ALIGN(0x1000) :
  {
    *(.data*)
  } :data

  .bss ALIGN(0x1000) (NOLOAD) :
  {
    *(.bss*)
    *(COMMON)
  } :data

  /DISCARD/ :
  {
    *(.comment*)
    *(.eh_frame*)
    *(.note*)
  }
}
