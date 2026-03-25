pub(crate) mod aux;
pub(crate) mod base;
pub(crate) mod compat;
pub(crate) mod compiler;
pub(crate) mod device;
pub(crate) mod dma;
pub(crate) mod export;
pub(crate) mod hid;
pub(crate) mod input;
pub(crate) mod irq;
pub(crate) mod mmio;
pub(crate) mod pci;
pub(crate) mod ps2;
pub(crate) mod runtime;
pub(crate) mod serio;
pub(crate) mod usb;
pub(crate) mod workqueue;

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
