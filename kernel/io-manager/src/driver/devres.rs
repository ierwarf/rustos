use alloc::vec::Vec;
use core::ffi::c_void;

use crate::sync::KernelSpinLock as Mutex;
use x86_64::instructions::interrupts;

use crate::driver::linux::compat::LinuxCompatPciDev;

enum ManagedResource {
    Mmio {
        addr: usize,
    },
    Irq {
        irq: u32,
        dev_id: usize,
    },
    DmaCoherent {
        size: usize,
        cpu_addr: usize,
        dma_handle: u64,
    },
    PciDisable {
        pdev: usize,
    },
}

struct DeviceDevres {
    device_ptr: usize,
    resources: Vec<ManagedResource>,
}

static DEVICE_DEVRES: Mutex<Vec<DeviceDevres>> = Mutex::new(Vec::new());

pub(crate) fn register_mmio(device: *mut c_void, addr: *mut c_void) {
    if device.is_null() || addr.is_null() {
        return;
    }

    irq_safe(|| {
        with_resources_mut(device as usize, |resources| {
            resources.push(ManagedResource::Mmio {
                addr: addr as usize,
            });
        });
    });
}

pub(crate) fn forget_mmio(device: *mut c_void, addr: *mut c_void) {
    if device.is_null() || addr.is_null() {
        return;
    }

    irq_safe(|| {
        let _ = remove_matching_resource(
            device as usize,
            |resource| matches!(resource, ManagedResource::Mmio { addr: mapped } if *mapped == addr as usize),
        );
    });
}

pub(crate) fn register_irq(device: *mut c_void, irq: u32, dev_id: *mut c_void) {
    if device.is_null() {
        return;
    }

    irq_safe(|| {
        with_resources_mut(device as usize, |resources| {
            resources.push(ManagedResource::Irq {
                irq,
                dev_id: dev_id as usize,
            });
        });
    });
}

pub(crate) fn forget_irq(device: *mut c_void, irq: u32, dev_id: *mut c_void) {
    if device.is_null() {
        return;
    }

    irq_safe(|| {
        let _ = remove_matching_resource(device as usize, |resource| {
            matches!(
                resource,
                ManagedResource::Irq {
                    irq: mapped_irq,
                    dev_id: mapped_dev_id,
                } if *mapped_irq == irq && *mapped_dev_id == dev_id as usize
            )
        });
    });
}

pub(crate) fn register_dma_coherent(
    device: *mut c_void,
    size: usize,
    cpu_addr: *mut c_void,
    dma_handle: u64,
) {
    if device.is_null() || cpu_addr.is_null() {
        return;
    }

    irq_safe(|| {
        with_resources_mut(device as usize, |resources| {
            resources.push(ManagedResource::DmaCoherent {
                size,
                cpu_addr: cpu_addr as usize,
                dma_handle,
            });
        });
    });
}

pub(crate) fn forget_dma_coherent(device: *mut c_void, cpu_addr: *mut c_void, dma_handle: u64) {
    if device.is_null() || cpu_addr.is_null() {
        return;
    }

    irq_safe(|| {
        let _ = remove_matching_resource(device as usize, |resource| {
            matches!(
                resource,
                ManagedResource::DmaCoherent {
                    cpu_addr: mapped_cpu_addr,
                    dma_handle: mapped_dma_handle,
                    ..
                } if *mapped_cpu_addr == cpu_addr as usize && *mapped_dma_handle == dma_handle
            )
        });
    });
}

pub(crate) fn register_pci_disable(device: *mut c_void, pdev: *mut LinuxCompatPciDev) {
    if device.is_null() || pdev.is_null() {
        return;
    }

    irq_safe(|| {
        with_resources_mut(device as usize, |resources| {
            if resources.iter().any(|resource| {
                matches!(resource, ManagedResource::PciDisable { pdev: existing } if *existing == pdev as usize)
            }) {
                return;
            }
            resources.push(ManagedResource::PciDisable {
                pdev: pdev as usize,
            });
        });
    });
}

pub(crate) fn forget_pci_disable(device: *mut c_void, pdev: *mut LinuxCompatPciDev) {
    if device.is_null() || pdev.is_null() {
        return;
    }

    irq_safe(|| {
        let _ = remove_matching_resource(
            device as usize,
            |resource| matches!(resource, ManagedResource::PciDisable { pdev: existing } if *existing == pdev as usize),
        );
    });
}

pub(crate) fn release_device(device: *mut c_void) {
    if device.is_null() {
        return;
    }

    let resources = irq_safe(|| take_resources(device as usize));
    for resource in resources.into_iter().rev() {
        match resource {
            ManagedResource::Mmio { addr } => {
                crate::driver::mmio::unmap(addr as *mut c_void);
            }
            ManagedResource::Irq { irq, dev_id } => {
                let _ = crate::driver::irq::free_irq(irq, dev_id as *mut c_void);
            }
            ManagedResource::DmaCoherent {
                size,
                cpu_addr,
                dma_handle,
            } => {
                let _ = size;
                crate::driver::dma::free_coherent(device, cpu_addr as *mut c_void, dma_handle);
            }
            ManagedResource::PciDisable { pdev } => {
                crate::driver::pci::disable_device(pdev as *mut LinuxCompatPciDev);
            }
        }
    }
}

fn take_resources(device_ptr: usize) -> Vec<ManagedResource> {
    let mut devices = DEVICE_DEVRES.lock();
    let Some(index) = devices
        .iter()
        .position(|entry| entry.device_ptr == device_ptr)
    else {
        return Vec::new();
    };
    devices.swap_remove(index).resources
}

fn with_resources_mut<T>(device_ptr: usize, f: impl FnOnce(&mut Vec<ManagedResource>) -> T) -> T {
    let mut devices = DEVICE_DEVRES.lock();
    let index = if let Some(index) = devices
        .iter()
        .position(|entry| entry.device_ptr == device_ptr)
    {
        index
    } else {
        devices.push(DeviceDevres {
            device_ptr,
            resources: Vec::new(),
        });
        devices.len() - 1
    };
    f(&mut devices[index].resources)
}

fn remove_matching_resource(
    device_ptr: usize,
    predicate: impl Fn(&ManagedResource) -> bool,
) -> bool {
    let mut devices = DEVICE_DEVRES.lock();
    let Some(device_index) = devices
        .iter()
        .position(|entry| entry.device_ptr == device_ptr)
    else {
        return false;
    };
    let resources = &mut devices[device_index].resources;
    let Some(resource_index) = resources.iter().position(predicate) else {
        return false;
    };
    resources.swap_remove(resource_index);
    if resources.is_empty() {
        devices.swap_remove(device_index);
    }
    true
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    interrupts::without_interrupts(f)
}
