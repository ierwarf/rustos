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
pub mod netdev;
pub mod pci;
pub mod ps2;
pub mod runtime;
pub mod serio;
pub mod skbuff;
pub mod usb;
pub mod virtio;
pub mod virtio_drm;
pub mod workqueue;

pub fn init_cpu_local_symbols() {
    aux::init_cpu_local_symbols();
    compiler::init_cpu_local_symbols();
    crate::user::syscall::activate_linux_compat_cpu_local();
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
        .or_else(|| virtio::resolve_symbol(name))
        .or_else(|| netdev::resolve_symbol(name))
        .or_else(|| skbuff::resolve_symbol(name))
        .or_else(|| serio::resolve_symbol(name))
        .or_else(|| ps2::resolve_symbol(name))
        .or_else(|| pci::resolve_symbol(name))
        .or_else(|| input::resolve_symbol(name))
        .or_else(|| hid::resolve_symbol(name))
        .or_else(|| usb::resolve_symbol(name))
        .or_else(|| virtio_drm::resolve_symbol(name))
}
