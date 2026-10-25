# Supervisor/kernel IBT

## Owner and invariant

`kernel-hal::arch::ibt` owns per-CPU supervisor admission. This is IBT only:
no shadow stacks and no user-mode IBT enablement. Per-CPU setup clears
IA32_U_CET before setting CR4.CET, so inherited user controls cannot accidentally
activate for uncertified processes; per-task user CET state remains unsupported.
`KernelBuildConfig::rustflags` appends `-Z cf-protection=branch` and
`-C jump-tables=n` after inherited/config flags. The existing kernel Cargo lane
rebuilds core, alloc, and compiler_builtins with those flags. Host tests and
userspace builds do not acquire these kernel flags.

The BSP calls `init_kernel_ibt` after its final high-address GDT/IDT; every AP
calls it after its private final GDT and the shared final IDT. IDT initialization
establishes CR0.WP first. Admission runs with interrupts disabled, before task
execution/AP online publication. It does not change ASLR translations, alias
sealing, descriptor relocation, or the WP establishment flow.

Each CPU independently checks CPUID maximum basic leaf >= 7 and
CPUID.(7,0).EDX[20]. Unsupported CPUs do not access CET registers. Supported CPUs
require WP, write IA32_S_CET (0x6a2) = ENDBR_EN (bit 2) only, and preserve CR4
while setting CET (bit 23). Readback failure is fatal. Shadow-stack, legacy
compatibility, suppression, and NOTRACK-enable bits are not enabled. There is
no forced CPU feature and no fallback that reports unsupported as active.

Every invocation emits an unconditional lock-free debug-port (0xe9) marker:
`kernel-ibt: cpu=NNNN unsupported (CPUID.IBT=0)` or
`kernel-ibt: cpu=NNNN active (S_CET.ENDBR_EN; positive probe passed)`.
The active marker means register admission plus a successful indirect call to
an ENDBR64/RET target, not completed hardware enforcement qualification.
An IBT-required qualification must reject any unsupported CPU marker.

## Entry and handwritten assembly audit

Intel SDM Vol. 1, section 18.3.3, specifies WAIT_FOR_ENDBRANCH for legacy IDT
interrupt/exception/NMI entry and SYSCALL, including same-CPL events. They are
not exempt just because they are hardware transitions. IRET retains the target
tracker state and is not an ordinary indirect CALL/JMP. RustOS does not use FRED.

- HAL timer, RTC, software-schedule, reschedule, saved-context restore, saved
  syscall-context dispatch, and software/fast-IPC trap callable symbols begin
  with explicit ENDBR64. Local direct-branch and IRET continuation labels do not
  require pads.
- Direct-copy/probe assembly callable symbols and the page-fault recovery entry
  begin with ENDBR64. Exception-table fixups are IRET destinations. The forwarding
  indirect JMP targets the compiler-generated original page-fault IDT wrapper,
  which must also have an ENDBR64.
- The default-handler alignment bridge has a pad. Rust-generated x86-interrupt
  wrappers (including vector 21), NMI/double-fault handlers, and ordinary Rust
  callback/function-pointer targets receive compiler instrumentation.
- The handwritten compat SYSCALL entry starts with ENDBR64 before SWAPGS.
- `asmtools` stack/higher-half indirect transfers target instrumented Rust
  functions. The nucleus Multiboot/AP trampoline executes before CET admission;
  its indirect AP handoff also targets instrumented Rust. Nucleus memory
  primitives are Rust functions with inline assembly, not uninstrumented asm
  function entries. No lowlevel handwritten indirect destinations were found.

The existing general IDT wrapper for #CP/vector 21 includes its hardware error
code and uses the aligned Rust bridge. Supervisor #CP emits `!EX:15`, then panics
with error code/RIP/CS before any recovery hook. It never skips the offending
instruction or disables CET. User #CP retains existing exact-task fault
retirement routing; current terminal fault handling returns Retired/Unhandled,
not Resumed. No user eligibility or per-thread U_CET enablement policy is introduced here.

## Evidence and qualification boundary

HAL unit/static tests cover CPUID gating, assembly pads, BSP/AP descriptor/WP
ordering, and supervisor #CP fail-closed ordering. Xtask tests cover branch-only
flags winning over conflicting inherited/config flags. `cargo xtask check`
checks both boot-cfg kernel dependencies and the ordinary host workspace.
Focused HAL/compat code generation with build-std verifies actual ENDBR64
output (200 generated IDT wrappers, five named Rust handlers, and SYSCALL),
the indirect positive-probe call, and absence of NOTRACK in those assemblies.
Rebuilt compiler_builtins memcpy/memmove/memset/memcmp/bcmp entries also begin
with ENDBR64. This is not a linked-whole-kernel audit.

The available AuthenticAMD host reports IBT=0 (SHSTK is independent), and the
available QEMU 11.1.2 TCG does not implement IBT. Neither an unsupported boot
nor compiling ENDBR instructions proves enforcement. No full image, KVM, or DVM
build/test is part of this lane. An automatic destructive negative probe is not
run: production supervisor #CP is deliberately fatal, not recoverable. On real
IBT hardware, qualification still requires BSP/AP active markers, positive
entry/interrupt/syscall traffic, and a controlled missing-ENDBR negative test
that observes vector 21, CPEC=3, and no resumption. Enforcement remains unverified.

References: [Rust cf-protection](https://doc.rust-lang.org/unstable-book/compiler-flags/cf-protection.html),
[Intel SDM Vol. 1](https://cdrdv2.intel.com/v1/dl/getContent/671436)
(section 18.3.3), [Vol. 2](https://cdrdv2.intel.com/v1/dl/getContent/671110)
(ENDBR64/SYSCALL/INT), and [Vol. 3](https://cdrdv2.intel.com/v1/dl/getContent/671447)
(CR4.CET/WP and Event 21).
