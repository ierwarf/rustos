use boot_protocol::BootInfo;
use kernel_lowlevel::interrupts::SavedContext;

pub mod arch {
    pub mod acpi {
        pub use crate::arch::acpi::*;
    }

    pub mod asmtools {
        pub use crate::arch::asmtools::*;
    }

    pub mod clock {
        pub use crate::arch::clock::*;
    }

    pub mod gdt {
        pub use crate::arch::gdt::*;
    }

    pub mod msi {
        pub use crate::arch::msi::*;
    }

    pub mod pci {
        pub use crate::arch::pci::*;
    }

    pub mod pic {
        pub use crate::arch::pic::*;
    }

    pub mod pit {
        pub use crate::arch::pit::*;
    }

    pub mod rtc {
        pub use crate::arch::rtc::*;
    }

    pub mod simd {
        pub use crate::arch::simd::*;
    }

    pub mod tlb {
        pub use crate::arch::tlb_shootdown::{
            AddressSpaceMutationGuard, FlushedAddressSpaceMutationGuard, activate_address_space,
            admit_current_cpu_online, assert_address_space_inactive, begin_address_space_mutation,
            begin_address_space_retirement, begin_global_mapping_mutation,
        };
    }

    pub mod timer {
        pub use crate::arch::timer::{arm_next_tick, init_current_cpu};
    }
}

pub mod boot {
    use super::BootInfo;

    pub fn init_gdt() {
        crate::arch::gdt::init();
    }

    pub fn init_gdt_for_cpu(logical_index: usize) {
        crate::arch::gdt::init_for_cpu(logical_index);
    }

    pub fn init_idt() {
        crate::arch::idt::init();
    }

    pub fn init_acpi(boot_info_ptr: *const BootInfo) {
        crate::arch::acpi::init(boot_info_ptr);
        crate::arch::smp::stage_discovered_topology();
    }

    pub fn init_clocksource() -> Option<crate::arch::clock::ClockSourceInfo> {
        crate::arch::clock::init()
    }

    /// # Safety
    ///
    /// See `arch::asmtools::enter_higher_half`; this wrapper preserves the
    /// same entry and boot-info validity requirements.
    pub unsafe fn enter_higher_half(entry: u64, boot_info_ptr: u64) -> ! {
        unsafe { crate::arch::asmtools::enter_higher_half(entry, boot_info_ptr) }
    }

    /// # Safety
    ///
    /// See `arch::asmtools::call_with_stack`; `stack_top` must name a valid
    /// writable kernel stack for `entry`.
    pub unsafe fn call_with_stack(entry: u64, arg0: u64, stack_top: u64) -> ! {
        unsafe { crate::arch::asmtools::call_with_stack(entry, arg0, stack_top) }
    }
}

pub mod cpu {
    pub use crate::arch::acpi::{CpuDescriptor, CpuTopology, MAX_SUPPORTED_CPUS};
    pub use crate::arch::smp::{CpuLifecycleSnapshot, CpuLifecycleState};

    pub fn topology() -> Option<CpuTopology> {
        crate::arch::acpi::cpu_topology()
    }

    pub fn discovered_count() -> usize {
        crate::arch::smp::cpu_count()
    }

    pub fn admitted_online_mask() -> u64 {
        crate::arch::smp::admitted_online_mask()
    }

    pub fn lifecycle_snapshot(logical_index: u8) -> Option<CpuLifecycleSnapshot> {
        crate::arch::smp::snapshot(logical_index)
    }

    pub fn transition_lifecycle(
        logical_index: u8,
        expected_generation: u64,
        next: CpuLifecycleState,
    ) {
        crate::arch::smp::transition(logical_index, expected_generation, next);
    }

    pub fn ap_bootstrap_stack_top(logical_index: u8, expected_generation: u64) -> u64 {
        crate::arch::smp::ap_bootstrap_stack_top(logical_index, expected_generation)
    }

    pub fn ap_bootstrap_stack_bounds(logical_index: u8, expected_generation: u64) -> (u64, u64) {
        crate::arch::smp::ap_bootstrap_stack_bounds(logical_index, expected_generation)
    }

    pub fn local_apic_physical_base() -> Option<u64> {
        crate::arch::msi::physical_base()
    }

    pub fn configure_local_apic_mmio(physical_base: u64, virtual_base: u64) -> bool {
        crate::arch::msi::configure_mmio(physical_base, virtual_base)
    }

    pub fn init_local_apic() -> bool {
        crate::arch::msi::init()
    }

    pub fn start_application_processor(
        apic_id: u32,
        startup_vector: u8,
    ) -> Result<(), crate::arch::msi::StartupIpiError> {
        crate::arch::msi::start_application_processor(apic_id, startup_vector)
    }

    pub fn init_simd() {
        crate::arch::simd::init();
    }

    pub fn simd_mode_name() -> &'static str {
        crate::arch::simd::mode_name()
    }

    pub fn current_rip() -> u64 {
        crate::arch::asmtools::current_rip()
    }
}

pub mod interrupts {
    use kernel_lowlevel::interrupts::SavedContext;

    use crate::hooks::{
        HeartbeatHooks, InterruptHooks, TaskHooks,
        register_heartbeat_hooks as install_heartbeat_hooks,
        register_interrupt_hooks as install_interrupt_hooks,
        register_task_hooks as install_task_hooks,
    };

    pub fn disable_interrupts() {
        x86_64::instructions::interrupts::disable();
    }

    pub fn init_pic() {
        crate::arch::pic::init();
        assert!(
            crate::arch::msi::init(),
            "local APIC initialization requires one admitted uncacheable MMIO mapping"
        );
        kernel_lowlevel::interrupts::register_timer_interrupt_dispatch(
            super::timer_interrupt_default_dispatch,
        );
        kernel_lowlevel::interrupts::register_software_schedule_interrupt_dispatch(
            super::software_schedule_interrupt_default_dispatch,
        );
        kernel_lowlevel::interrupts::register_reschedule_ipi_interrupt_dispatch(
            super::reschedule_ipi_interrupt_default_dispatch,
        );
    }

    pub fn register_task_hooks(hooks: TaskHooks) {
        install_task_hooks(hooks);
    }

    pub fn register_interrupt_hooks(hooks: InterruptHooks) {
        install_interrupt_hooks(hooks);
    }

    pub fn register_heartbeat_hooks(hooks: HeartbeatHooks) {
        install_heartbeat_hooks(hooks);
    }

    /// # Safety
    ///
    /// `context` must be a valid saved kernel context created by the matching
    /// low-level context-save path, and restoring it must be the next control
    /// transfer on this CPU.
    pub unsafe fn restore_kernel_saved_context(context: *mut SavedContext) -> ! {
        unsafe extern "C" {
            #[link_name = "restore_kernel_saved_context"]
            fn restore_kernel_saved_context_raw(context: *mut SavedContext) -> !;
        }

        unsafe { restore_kernel_saved_context_raw(context) }
    }
}

pub mod time {
    pub fn init_rtc() {
        kernel_lowlevel::interrupts::register_rtc_interrupt_dispatch(
            super::rtc_interrupt_default_dispatch,
        );
        crate::arch::rtc::init();
    }
}

pub use crate::hooks::{
    CurrentUserSnapshot, HeartbeatHooks, HeartbeatSnapshot, InputEventQueueDebugSnapshot,
    InterruptHooks, TaskHooks, UserFaultDisposition,
};
pub use boot::{
    call_with_stack, enter_higher_half, init_acpi, init_clocksource, init_gdt, init_idt,
};
pub use cpu::{current_rip, init_simd, simd_mode_name};
pub use interrupts::{
    disable_interrupts, init_pic, register_heartbeat_hooks, register_interrupt_hooks,
    register_task_hooks, restore_kernel_saved_context,
};
pub use time::init_rtc;

extern "C" fn timer_interrupt_default_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_1_OFFSET);
    context_ptr
}

extern "C" fn rtc_interrupt_default_dispatch(context_ptr: *mut SavedContext) -> *mut SavedContext {
    crate::arch::rtc::on_interrupt();
    crate::arch::pic::send_eoi(crate::arch::pic::PIC_2_OFFSET);
    context_ptr
}

extern "C" fn software_schedule_interrupt_default_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    context_ptr
}

extern "C" fn reschedule_ipi_interrupt_default_dispatch(
    context_ptr: *mut SavedContext,
) -> *mut SavedContext {
    crate::arch::msi::local_apic_eoi();
    context_ptr
}
