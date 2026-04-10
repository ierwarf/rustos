#[derive(Clone, Copy, Debug)]
pub(crate) struct UsbHostControllerInfo {
    pub(crate) address: crate::arch::pci::PciDevice,
    pub(crate) kind: crate::arch::pci::UsbHostControllerKind,
    pub(crate) bar0: Option<crate::arch::pci::PciResource>,
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code))]
    pub(crate) irq_line: u8,
}

// Text names are retained for diagnostics and future generated USB inventories.
#[allow(dead_code)]
pub(crate) fn controller_kind_name(kind: crate::arch::pci::UsbHostControllerKind) -> &'static str {
    match kind {
        crate::arch::pci::UsbHostControllerKind::Uhci => "uhci",
        crate::arch::pci::UsbHostControllerKind::Ohci => "ohci",
        crate::arch::pci::UsbHostControllerKind::Ehci => "ehci",
        crate::arch::pci::UsbHostControllerKind::Xhci => "xhci",
        crate::arch::pci::UsbHostControllerKind::Unknown(_) => "unknown",
    }
}
