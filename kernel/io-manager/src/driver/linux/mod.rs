// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// pub mod aux;
// pub mod base;
// pub mod compat;
// pub mod compiler;
// pub mod device;
// pub mod dma;
// pub mod export;
// pub mod hid;
// pub mod input;
// pub mod irq;
// pub mod mmio;
// pub mod netdev;
// pub mod pci;
// pub mod ps2;
// pub mod runtime;
// pub mod serio;
// pub mod skbuff;
// pub mod usb;
// pub mod virtio;
// pub mod virtio_drm;
// pub mod workqueue;
//
// #[derive(Clone, Copy, Debug, Eq, PartialEq)]
// pub(crate) enum LinuxCompatExportAbi {
//     AlignRustCall,
//     PreserveStackTail,
// }
//
// #[derive(Clone, Copy, Debug)]
// pub(crate) struct LinuxCompatSymbol {
//     pub(crate) addr: usize,
//     pub(crate) abi: LinuxCompatExportAbi,
// }
//
// impl LinuxCompatSymbol {
//     pub(crate) const fn align_rust_call(addr: usize) -> Self {
//         Self {
//             addr,
//             abi: LinuxCompatExportAbi::AlignRustCall,
//         }
//     }
//
//     pub(crate) const fn preserve_stack_tail(addr: usize) -> Self {
//         Self {
//             addr,
//             abi: LinuxCompatExportAbi::PreserveStackTail,
//         }
//     }
// }
//
// macro_rules! linux_compat_symbols {
//     ($name:expr, { $($symbol:literal => $addr:expr $(, $abi:ident)?;)* }) => {{
//         match $name {
//             $(
//                 $symbol => Some(super::linux_compat_symbols!(@symbol $addr $(, $abi)?)),
//             )*
//             _ => None,
//         }
//     }};
//     (@symbol $addr:expr, preserve_stack_tail) => {
//         super::LinuxCompatSymbol::preserve_stack_tail($addr as *const () as usize)
//     };
//     (@symbol $addr:expr, align_rust_call) => {
//         super::LinuxCompatSymbol::align_rust_call($addr as *const () as usize)
//     };
//     (@symbol $addr:expr) => {
//         super::LinuxCompatSymbol::align_rust_call($addr as *const () as usize)
//     };
// }
//
// pub(crate) use linux_compat_symbols;
//
// pub fn init_cpu_local_symbols() {
//     aux::init_cpu_local_symbols();
//     compiler::init_cpu_local_symbols();
//     crate::user::syscall::activate_linux_compat_cpu_local();
// }
//
// pub(crate) fn export_abi(name: &str) -> LinuxCompatExportAbi {
//     virtio::symbol_abi(name)
//         .or_else(|| netdev::symbol_abi(name))
//         .or_else(|| virtio_drm::symbol_abi(name))
//         .unwrap_or_else(|| {
//             if linux_compat_preserves_module_stack(name) {
//                 LinuxCompatExportAbi::PreserveStackTail
//             } else {
//                 LinuxCompatExportAbi::AlignRustCall
//             }
//         })
// }
//
// fn linux_compat_preserves_module_stack(name: &str) -> bool {
//     matches!(
//         name,
//         "__fentry__"
//             | "__x86_return_thunk"
//             | "__x86_indirect_thunk_rax"
//             | "__x86_indirect_thunk_rcx"
//             | "__x86_indirect_thunk_rdx"
//             | "__x86_indirect_thunk_r9"
//             | "__x86_indirect_thunk_r13"
//             | "__x86_indirect_thunk_r15"
//             | "usb_control_msg"
//             | "usb_interrupt_msg"
//             | "snprintf"
//             | "scnprintf"
//             | "sprintf"
//             | "_printk"
//             | "_dev_err"
//             | "_dev_info"
//             | "_dev_warn"
//             | "__warn_printk"
//             | "netdev_err"
//             | "netdev_warn"
//             | "netdev_printk"
//     )
// }
//
// pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
//     compiler::resolve_symbol(name)
//         .or_else(|| base::resolve_symbol(name))
//         .or_else(|| runtime::resolve_symbol(name))
//         .or_else(|| device::resolve_symbol(name))
//         .or_else(|| aux::resolve_symbol(name))
//         .or_else(|| export::resolve_symbol(name))
//         .or_else(|| dma::resolve_symbol(name))
//         .or_else(|| workqueue::resolve_symbol(name))
//         .or_else(|| irq::resolve_symbol(name))
//         .or_else(|| mmio::resolve_symbol(name))
//         .or_else(|| virtio::resolve_symbol(name))
//         .or_else(|| netdev::resolve_symbol(name))
//         .or_else(|| skbuff::resolve_symbol(name))
//         .or_else(|| serio::resolve_symbol(name))
//         .or_else(|| ps2::resolve_symbol(name))
//         .or_else(|| pci::resolve_symbol(name))
//         .or_else(|| input::resolve_symbol(name))
//         .or_else(|| hid::resolve_symbol(name))
//         .or_else(|| usb::resolve_symbol(name))
//         .or_else(|| virtio_drm::resolve_symbol(name))
// }
