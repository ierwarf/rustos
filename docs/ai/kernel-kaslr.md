# Kernel KASLR contract

## Owners and address domains

- The BSP entry (`kernel/src/main.rs`) admits BootInfo entropy and initializes
  the existing `boot-random` CSPRNG exactly once, before MM chooses an image
  address. The executive must not reseed it. Missing entropy is boot-fatal;
  there is no timer, PID, or deterministic production fallback.
- `kernel-mm::memory::kernel_vm` owns the image mappings, ELF rebasing, and
  one-way fixed-alias seal. `kernel-lowlevel::address` publishes the image
  translation once, before any AP can start.
- Physical load remains at 2 MiB. PML4 slot 256 remains the fixed 512 GiB
  physical direct map; slot 257 remains MMIO. Only slot 258 contains the
  randomized executable kernel image. `higher_half_addr` still means direct
  map, not image relocation. `kernel_image_addr` converts bootstrap image
  pointers; `lower_half_addr`/kernel virtual-to-physical conversion recognize
  the actual image range before applying direct-map subtraction.

## GRUB boot artifact

GRUB Multiboot2 rejects `SHT_REL`/`SHT_RELA` section headers even when the kernel
owns relocation. Xtask therefore changes only the linked, allocated read-only
RELA section's **type** to opaque `SHT_PROGBITS` in the boot copy before detached
signing. Loaded bytes, addresses, symbols, program headers and `PT_DYNAMIC`
remain unchanged; the Cargo/debug ELF retains its original section types.
The bootstrap uses its linker-bounded table and MM uses `DT_RELA`, not section
type discovery. Invalid/writable/executable relocation payloads fail the build.

## Choice and relocation

The image is bounded to 256 MiB by 128 image leaf tables, with two page
directories covering a possible GiB boundary crossing. A private ChaCha20 child
stream from `boot-random::Random` uniformly selects a 2 MiB-aligned image start
in `[0xffff810000000000, 0xffff818000000000 - image_size]`.
There are `floor((512 GiB - image_size) / 2 MiB) + 1` choices (over 261,000,
approximately 18 bits). Physical placement is not randomized by this mechanism.

All kernel crates are position-independent. The static PIE has no interpreter
and is linked at virtual/physical 2 MiB, not virtual zero. BootInfo's
`nucleus_image.load_bias` continues to name the physical ELF header; the ELF
load bias is that address **minus 2 MiB**. The BSP assembly admits only
symbol-free `R_X86_64_RELATIVE` entries and first applies them with ELF bias
zero before Rust can access GOT slots or vtables. Only BSP bootstrap constants
are absolute linker symbols. Once both aliases are mapped, MM reapplies RELA
entries with `B = chosen_image_start - 2 MiB`, writing `B + A` to the physical
relocation targets. Unsupported relocations, ET_EXEC, absent relocation data,
and an oversized image fail closed.

PT_LOAD permissions are mapped with 4 KiB image leaves and W xor X; supervisor
access only. Before the one-way jump, low identity/direct-map text is RX and
relocation-bearing non-text data remains writable/NX. No AP or userspace root
exists then. On high entry, GDT/TSS/IST and IDT are constructed at final image
addresses, CR0.WP is enabled, and MM makes **every** fixed kernel-image alias
NX with final data write permissions. The seal audits every segment page and
flushes the BSP TLB before proceeding. Identity mappings remain for physical
boot records and the separately copied AP trampoline; they do not retain
executable kernel image text. AP mailbox CR3 is physical, while AP entry and
stacks, IRQ callbacks, syscall entry and CPU-local pointers use the final image
or ordinary allocated mappings. User roots inherit the image slot alongside
the other supervisor mappings.

## Debug witness and validation boundary

With the existing debug-only `RUSTOS_LIFECYCLE_TRACE=true` and logging enabled,
high-entry seal emits exactly this debugcon shape:

```text
aslr: kernel base=0x... pc=0x...
```

`pc` is the executing instruction address sampled at the sealing function,
not a reported random choice. The seal refuses to run outside the image, and
the witness follows the all-page fixed-alias NX audit and TLB flush.
Release-shaped builds have lifecycle tracing disabled and compile this witness
out, even when ordinary logging is enabled. Do not expose it through a user
ABI or enable it in shipped release configurations.

Host tests cover slot/window bounds, image-vs-direct-map translation,
fixed-physical static-PIE bias, and position-independent build configuration.
Kernel-only build plus ELF inspection verifies ET_DYN, PT_LOAD W xor X,
PT_DYNAMIC/no interpreter, and symbol-free RELATIVE relocations. Repeated
signed-image boots, IRQ/syscall service execution, 1/2/4/8-vCPU AP startup, and
release witness absence are integration acceptance, not established by host
unit tests. The pre-descriptor bootstrap runs with IRQs disabled and without the final
exception/NMI tables; deferring construction until high entry extends this
existing interval through entropy/paging initialization. High entry installs
GDT/TSS/IDT immediately, before logging, allocation or executive work. The
bootstrap GDT remains mapped and readable until replacement completes. No NMI
coverage is claimed during this interval; ordinary KVM boot acceptance cannot
prove NMI-injection safety.

## Architecture references

- System V generic ELF relocation semantics:
  <https://refspecs.linuxfoundation.org/elf/gabi4+/ch4.reloc.html>.
- AMD64 psABI section 4.4.1 / table 4.10 (`R_X86_64_RELATIVE = B + A`):
  <https://refspecs.linuxfoundation.org/elf/x86_64-abi-0.99.pdf>.
- Linux x86 boot protocol (`relocatable_kernel`, `kernel_alignment`,
  `KASLR_FLAG`) is a comparison baseline, not RustOS's Multiboot2 ABI:
  <https://docs.kernel.org/arch/x86/boot.html>.
