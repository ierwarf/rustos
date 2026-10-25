# IBT hardening boundaries

IBT (indirect-branch tracking) is not shadow stack. Code generation, hardware
capability and runtime enforcement are separate acceptance claims.

## RustOS build coverage

- Kernel target crates, core/alloc/compiler-builtins use branch-only CET code
  generation. Native Rust userspace uses `-Z cf-protection=branch` and
  `-C jump-tables=n`; dynamic Rust packages rebuild std/panic-abort rather than
  relying on the uninstrumented prebuilt sysroot. Static-PIE services retain
  their no-interpreter build and early allocation/GOT-free relocation path.
- Repository-built C/MinGW code uses `-fcf-protection=branch` and
  `-fno-jump-tables`. Tool-source inputs invalidate cached C artifacts when
  these flags change. This does not qualify external CRTs or DLLs.
- Handwritten assembly is not instrumented by rustc. Native service `_start`
  and the exported relocation leaf have explicit ENDBR64 landing pads. Kernel
  assembly targets and CPU enforcement belong to kernel HAL, not loaderd.
- Do not force GNU IBT property notes to hide uninstrumented inputs. ENDBR
  presence alone does not prove that CET is enabled or that every dependency
  or dynamically loaded object is eligible.

## Native userspace enforcement boundary

Instrumentation must **not** enable `IA32_U_CET` globally for every Linux/PE
compatibility process. Native dynamic executables can still reach externally
supplied libc, the ELF interpreter, assembly or DSOs whose complete indirect
branch target closure is not certified. Loaderd owns eligibility policy; ring0
would own the associated per-thread CET architectural state, including tracker
preservation across interrupts, scheduling, exec and signal returns. Supervisor setup clears U_CET before CR4.CET enablement rather than inheriting
unknown firmware controls. Until that complete contract and hardware
qualification exist, native userspace is
**instrumented, not user-mode-IBT-enforced**. It is not acceptable to report
whole-OS enforced IBT from a compiler flag or supervisor-only CET.

## Linux DVM

The appliance pins resolved `CONFIG_X86_KERNEL_IBT=y` and its compiler/CET
prerequisites. Kbuild/objtool own instrumentation/sealing for the matching
kernel and signed modules. Ordinary module-signature validation remains
required. Do not add blanket flags to Buildroot userspace: upstream Linux
supports kernel IBT and userspace shadow stack, not userspace IBT. Musl shadow
stack support, threads, unwind and signal handling need separate qualification.

## Qualification

Inspect final artifacts and assembly entry points, then on IBT-capable hardware
require guest CPUID support, supervisor CET state on each online CPU, successful
valid indirect calls and intentional missing-ENDBR #CP rejection. Verify user
and kernel fault isolation independently if user enforcement is introduced.

The development host probed on 2026-10-24 returned CPUID.7.0 EDX=0x10000010:
IBT bit 20 is clear, although SHSTK is present. Installed QEMU 11.1.2 TCG does
not advertise CET IBT. Builds and ordinary KVM boots on this host qualify
instrumentation/unsupported-hardware behavior only, never IBT enforcement.

## Primary references

- [Rust cf-protection](https://doc.rust-lang.org/unstable-book/compiler-flags/cf-protection.html): branch vs return/full; rebuild standard modules.
- [Linux CET support](https://docs.kernel.org/arch/x86/shstk.html): kernel IBT vs userspace shadow stack.
- [Linux 6.12.94 x86 flags](https://raw.githubusercontent.com/gregkh/linux/v6.12.94/arch/x86/Makefile): strict kernel IBT disables jump tables/NOTRACK generation.
- [Linux 6.12.94 Kconfig](https://raw.githubusercontent.com/gregkh/linux/v6.12.94/arch/x86/Kconfig): compiler, objtool and linker dependencies.
- [Intel SDM](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html): independent CPUID IBT/SHSTK capabilities, CR0.WP/CR4.CET ordering, S_CET/U_CET and tracker state.
- [QEMU CPU models](https://www.qemu.org/docs/master/system/i386/cpu.html): KVM can filter host features; model names do not prove guest support.
