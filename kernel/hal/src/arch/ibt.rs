//! Per-CPU supervisor IBT admission, after final GDT/IDT installation and CR0.WP.
//!
//! - **Owner:** HAL owns CPU capability admission and supervisor CET state.
//! - **Boundary:** CPUID.IBT must admit every CET MSR/control-register access.
//! - **Lifecycle:** Final descriptors and WP precede enablement; every AP runs
//!   this before online publication. Unsupported CPUs never touch CET MSRs.
//! - **Concurrency:** Local IRQs stay disabled; setup is per-CPU, allocation-free
//!   and does not publish user-mode CET state or hold service/MM locks.
//! - **Failure:** Supervisor #CP is fatal; unsupported hardware reports absence,
//!   never a successful enforcement/selftest claim.
//! - **Forbidden:** No shadow stacks, U_CET enablement, legacy bitmap, NOTRACK bypass,
//!   or synthetic capability advertisement.
//! - **Evidence:** CPUID/entry-order/landing-pad tests and the per-CPU positive
//!   probe. Missing-ENDBR negative qualification requires capable hardware.

#[cfg(rustos_boot_image)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IbtStatus {
    Unsupported,
    Active,
}

#[cfg(any(rustos_boot_image, test))]
const fn supports_ibt(max_basic_leaf: u32, leaf7_edx: u32) -> bool {
    max_basic_leaf >= 7 && leaf7_edx & (1 << 20) != 0
}

#[cfg(rustos_boot_image)]
const IA32_S_CET: u32 = 0x6a2;
#[cfg(rustos_boot_image)]
const IA32_U_CET: u32 = 0x6a0;
#[cfg(rustos_boot_image)]
const ENDBR_EN: u64 = 1 << 2;
#[cfg(rustos_boot_image)]
const CR4_CET: u64 = 1 << 23;

/// Admit only this CPU. Must run with final descriptors and interrupts disabled,
/// before publishing it online. An unsupported CPU never executes a CET MSR access.
#[cfg(rustos_boot_image)]
pub fn enable_on_this_cpu() -> IbtStatus {
    use core::arch::{
        asm,
        x86_64::{__cpuid, __cpuid_count},
    };
    use x86_64::registers::{
        control::{Cr0, Cr0Flags},
        model_specific::Msr,
    };

    assert!(!x86_64::instructions::interrupts::are_enabled());
    let max_leaf = __cpuid(0).eax;
    let edx = if max_leaf >= 7 {
        __cpuid_count(7, 0).edx
    } else {
        0
    };
    let status = if supports_ibt(max_leaf, edx) {
        assert!(
            Cr0::read().contains(Cr0Flags::WRITE_PROTECT),
            "IBT requires CR0.WP"
        );
        // SAFETY: CPUID admits these registers, WP is set, and the installed
        // #CP path is fatal. Set only ENDBR_EN: SHSTK and all bypasses stay off.
        unsafe {
            let mut cr4: u64;
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
            // Establish a known compatibility default before CR4.CET can
            // activate any inherited user controls. User eligibility and
            // per-thread tracker preservation are not implemented yet.
            Msr::new(IA32_U_CET).write(0);
            Msr::new(IA32_S_CET).write(ENDBR_EN);
            cr4 |= CR4_CET;
            asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags));
            assert_eq!(Msr::new(IA32_S_CET).read(), ENDBR_EN);
            assert_eq!(Msr::new(IA32_U_CET).read(), 0);
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
            assert_ne!(cr4 & CR4_CET, 0);
            // Keep the branch genuinely indirect, even under LTO. This proves
            // the landing pad works, NOT that a missing pad would fault.
            asm!("call {target}", target = in(reg) rustos_ibt_positive_target as *const (), clobber_abi("C"));
        }
        IbtStatus::Active
    } else {
        IbtStatus::Unsupported
    };
    report_status(status);
    status
}

#[cfg(rustos_boot_image)]
core::arch::global_asm!(
    r#"
    .text
    .global rustos_ibt_positive_target
    .type rustos_ibt_positive_target, @function
rustos_ibt_positive_target:
    endbr64
    ret
    .size rustos_ibt_positive_target, . - rustos_ibt_positive_target
"#
);

#[cfg(rustos_boot_image)]
unsafe extern "C" {
    fn rustos_ibt_positive_target();
}

// Unconditional, lock-free debug-port evidence, including in release images.
#[cfg(rustos_boot_image)]
fn report_status(status: IbtStatus) {
    fn emit(byte: u8) {
        // SAFETY: boot-image HAL runs at CPL0; the diagnostic byte-port has
        // no memory ownership or interrupt-state side effects.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0xe9_u16, in("al") byte, options(nomem, nostack, preserves_flags))
        };
    }
    for &byte in b"kernel-ibt: cpu=" {
        emit(byte);
    }
    let cpu = nucleus_core::util::lockdep::current_cpu_index();
    for shift in (0..4).rev() {
        emit(b"0123456789abcdef"[(cpu >> (shift * 4)) & 15]);
    }
    let message: &[u8] = match status {
        IbtStatus::Unsupported => b" unsupported (CPUID.IBT=0)\n",
        IbtStatus::Active => b" active (S_CET.ENDBR_EN; positive probe passed)\n",
    };
    for &byte in message {
        emit(byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembly_entry_symbols_have_endbr64() {
        for (source, symbols) in [
            (
                include_str!("../interrupt_stubs.rs"),
                &[
                    "timer_interrupt_handler",
                    "rtc_scheduler_interrupt_handler",
                    "software_schedule_interrupt_handler",
                    "reschedule_ipi_interrupt_handler",
                    "restore_kernel_saved_context",
                    "schedule_saved_user_context",
                    "software_schedule_trap",
                    "fast_ipc_call_schedule_trap",
                    "fast_ipc_reply_schedule_trap",
                ][..],
            ),
            (
                include_str!("user_access.rs"),
                &[
                    "rustos_user_copy_from",
                    "rustos_user_copy_to",
                    "rustos_user_probe_write",
                    "rustos_page_fault_entry",
                ][..],
            ),
            (
                include_str!("idt/handlers.rs"),
                &["rustos_default_handler_alignment_bridge"][..],
            ),
            (
                include_str!("../../../compat/src/user/syscall/mod.rs"),
                &["syscall_entry"][..],
            ),
        ] {
            for symbol in symbols {
                let label = alloc::format!("{symbol}:");
                let body = source.split_once(&label).expect("assembly entry symbol").1;
                assert!(
                    body.trim_start().starts_with("endbr64"),
                    "{symbol} lacks landing pad"
                );
            }
        }
    }

    #[test]
    fn bsp_and_ap_admit_ibt_after_final_descriptors_and_wp() {
        for source in [
            include_str!("../../../src/main.rs"),
            include_str!("../../../executive/src/boot.rs"),
        ] {
            let gdt = source
                .find("hal_api::init_gdt")
                .or_else(|| source.find("hal_api::boot::init_gdt_for_cpu"))
                .unwrap();
            let idt = source[gdt..].find("hal_api::init_idt();").unwrap() + gdt;
            let ibt = source.find("hal_api::boot::init_kernel_ibt();").unwrap();
            assert!(gdt < idt && idt < ibt);
        }
        let idt = include_str!("idt/mod.rs");
        assert!(idt.contains("ensure_write_protect_on_this_cpu();"));
        assert!(idt.contains("set_general_handler!(&mut idt, default_handler, 0..=31)"));
    }

    #[test]
    fn supervisor_cp_is_fatal_before_any_fault_hook() {
        let source = include_str!("idt/handlers.rs");
        let body = source
            .split_once("fn rustos_default_handler_aligned(")
            .unwrap()
            .1;
        let cp = body
            .find("if index == 21 && !is_user_mode(&stack_frame)")
            .unwrap();
        let fatal = body.find("control-protection fault (#CP)").unwrap();
        let hook = body.find("crate::hooks::").unwrap();
        assert!(cp < fatal && fatal < hook);
    }

    #[test]
    fn user_controls_stay_disabled_before_global_cet_enablement() {
        let source = include_str!("ibt.rs");
        let body = source
            .split_once("pub fn enable_on_this_cpu()")
            .unwrap()
            .1
            .split_once("// Unconditional")
            .unwrap()
            .0;
        assert!(
            body.find("Msr::new(IA32_U_CET).write(0)").unwrap()
                < body.find("cr4 |= CR4_CET").unwrap()
        );
        assert!(body.contains("assert_eq!(Msr::new(IA32_U_CET).read(), 0)"));
    }

    #[test]
    fn supervisor_ibt_requires_its_own_cpuid_bit() {
        assert!(!supports_ibt(6, 1 << 20));
        assert!(!supports_ibt(7, 0)); // SHSTK is in ECX, not EDX.IBT.
        assert!(!supports_ibt(7, u32::MAX & !(1 << 20)));
        assert!(supports_ibt(7, 1 << 20));
        assert!(supports_ibt(0x20, u32::MAX));
    }
}
