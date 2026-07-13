// RING3-MIGRATION-REFERENCE START: Linux .ko shim module is an explicit ring0
// compatibility substrate. Driver policy belongs in driverd; .ko execution does
// not move to ring3.
pub mod aux;
pub mod base;
pub mod compat;
pub mod compiler;
pub mod device;
pub mod dma;
pub mod export;
pub mod hid;
pub mod input;
pub mod irq;
pub mod mmio;
pub mod pci;
pub mod ps2;
pub mod runtime;
pub mod serio;
pub mod usb;
pub mod workqueue;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinuxCompatExportAbi {
    AlignRustCall,
    PreserveStackTail,
}

pub(crate) mod compat_log {
    pub(crate) fn debugcon_line(bytes: &[u8]) {
        if crate::debug::enabled!(compat, debug) {
            super::compat_log::debugcon_line(bytes);
        }
    }
}

pub fn init_cpu_local_symbols() {
    aux::init_cpu_local_symbols();
    compiler::init_cpu_local_symbols();
    crate::user::syscall::activate_linux_compat_cpu_local();
}

pub(crate) fn export_abi(name: &str) -> LinuxCompatExportAbi {
    if linux_compat_preserves_module_stack(name) {
        LinuxCompatExportAbi::PreserveStackTail
    } else {
        LinuxCompatExportAbi::AlignRustCall
    }
}

fn linux_compat_preserves_module_stack(name: &str) -> bool {
    matches!(
        name,
        "__fentry__"
            | "__x86_return_thunk"
            | "__x86_indirect_thunk_rax"
            | "__x86_indirect_thunk_rcx"
            | "__x86_indirect_thunk_rdx"
            | "__x86_indirect_thunk_r9"
            | "__x86_indirect_thunk_r13"
            | "__x86_indirect_thunk_r15"
            | "usb_control_msg"
            | "usb_interrupt_msg"
            | "snprintf"
            | "scnprintf"
            | "sprintf"
            | "_printk"
            | "_dev_err"
            | "_dev_info"
            | "_dev_warn"
            | "__warn_printk"
            | "netdev_err"
            | "netdev_warn"
            | "netdev_printk"
    )
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    compiler::resolve_symbol(name)
        .or_else(|| base::resolve_symbol(name))
        .or_else(|| runtime::resolve_symbol(name))
        .or_else(|| device::resolve_symbol(name))
        .or_else(|| aux::resolve_symbol(name))
        .or_else(|| export::resolve_symbol(name))
        .or_else(|| dma::resolve_symbol(name))
        .or_else(|| workqueue::resolve_symbol(name))
        .or_else(|| irq::resolve_symbol(name))
        .or_else(|| mmio::resolve_symbol(name))
        .or_else(|| serio::resolve_symbol(name))
        .or_else(|| ps2::resolve_symbol(name))
        .or_else(|| pci::resolve_symbol(name))
        .or_else(|| input::resolve_symbol(name))
        .or_else(|| hid::resolve_symbol(name))
        .or_else(|| usb::resolve_symbol(name))
}
// RING3-MIGRATION-REFERENCE END: Linux .ko shim compatibility substrate exception.
