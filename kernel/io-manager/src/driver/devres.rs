use core::ffi::c_void;

use super::linux::compat::LinuxCompatPciDev;

pub(crate) fn register_pci_disable(_dev: *mut c_void, _pci_dev: *mut LinuxCompatPciDev) {}

pub(crate) fn forget_pci_disable(_dev: *mut c_void, _pci_dev: *mut LinuxCompatPciDev) {}

pub(crate) fn register_irq(_dev: *mut c_void, _irq: u32, _dev_id: *mut c_void) {}

pub(crate) fn forget_irq(_dev: *mut c_void, _irq: u32, _dev_id: *mut c_void) {}

pub(crate) fn register_mmio(_dev: *mut c_void, _addr: *mut c_void) {}

pub(crate) fn forget_mmio(_dev: *mut c_void, _addr: *mut c_void) {}

pub(crate) fn register_dma_coherent(
    _dev: *mut c_void,
    _size: usize,
    _cpu_addr: *mut c_void,
    _dma_handle: u64,
) {
}

pub(crate) fn forget_dma_coherent(_dev: *mut c_void, _cpu_addr: *mut c_void, _dma_handle: u64) {}

pub(crate) fn release_device(_dev: *mut c_void) {}
