use alloc::vec::Vec;
use core::ffi::c_void;

use crate::sync::KernelSpinLock as Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

const DEFAULT_DMA_MASK: u64 = 0xffff_ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DmaAddr(u64);

impl DmaAddr {
    pub(crate) const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DmaMappingKind {
    Streaming,
    Coherent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IommuAddressMode {
    Identity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DmaMapping {
    pub(crate) phys_addr: u64,
    pub(crate) dma_addr: DmaAddr,
    pub(crate) size: usize,
    pub(crate) kind: DmaMappingKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DeviceDmaDomain {
    device_ptr: usize,
    dma_mask: u64,
    coherent_dma_mask: u64,
    mode: IommuAddressMode,
}

static DEVICE_DMA_DOMAINS: Mutex<Vec<DeviceDmaDomain>> = Mutex::new(Vec::new());

pub(crate) fn set_mask(device: *mut c_void, mask: u64) -> i32 {
    irq_safe(|| {
        let mut domains = ensure_device_domain(device as usize);
        if let Some(domain) = domains
            .iter_mut()
            .find(|domain| domain.device_ptr == device as usize)
        {
            domain.dma_mask = mask;
        }
    });
    0
}

pub(crate) fn set_coherent_mask(device: *mut c_void, mask: u64) -> i32 {
    irq_safe(|| {
        let mut domains = ensure_device_domain(device as usize);
        if let Some(domain) = domains
            .iter_mut()
            .find(|domain| domain.device_ptr == device as usize)
        {
            domain.coherent_dma_mask = mask;
        }
    });
    0
}

pub(crate) fn set_mask_and_coherent(device: *mut c_void, mask: u64) -> i32 {
    let status = set_mask(device, mask);
    if status != 0 {
        return status;
    }
    set_coherent_mask(device, mask)
}

pub(crate) fn coherent_mask(device: *mut c_void) -> u64 {
    irq_safe(|| mask_for(device as usize, DmaMappingKind::Coherent))
}

pub(crate) fn mode_for_device(device: *mut c_void) -> IommuAddressMode {
    irq_safe(|| {
        ensure_device_domain(device as usize)
            .iter()
            .find(|domain| domain.device_ptr == device as usize)
            .map(|domain| domain.mode)
            .unwrap_or(IommuAddressMode::Identity)
    })
}

pub(crate) fn map_physical_range(
    device: *mut c_void,
    phys_addr: u64,
    size: usize,
    kind: DmaMappingKind,
) -> Option<DmaMapping> {
    if size == 0 {
        return None;
    }

    let dma_addr = match mode_for_device(device) {
        IommuAddressMode::Identity => DmaAddr::new(phys_addr),
    };

    let mask = irq_safe(|| mask_for(device as usize, kind));
    if exceeds_mask(dma_addr.raw(), size, mask) {
        return None;
    }

    Some(DmaMapping {
        phys_addr,
        dma_addr,
        size,
        kind,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn translate_dma_to_physical(
    device: *mut c_void,
    dma_addr: DmaAddr,
    size: usize,
) -> Option<u64> {
    if size == 0 {
        return None;
    }
    match mode_for_device(device) {
        IommuAddressMode::Identity => {
            let mask = irq_safe(|| mask_for(device as usize, DmaMappingKind::Streaming));
            (!exceeds_mask(dma_addr.raw(), size, mask)).then_some(dma_addr.raw())
        }
    }
}

fn mask_for(device_ptr: usize, kind: DmaMappingKind) -> u64 {
    DEVICE_DMA_DOMAINS
        .lock()
        .iter()
        .find(|domain| domain.device_ptr == device_ptr)
        .map(|domain| match kind {
            DmaMappingKind::Streaming => domain.dma_mask,
            DmaMappingKind::Coherent => {
                if domain.coherent_dma_mask != 0 {
                    domain.coherent_dma_mask
                } else if domain.dma_mask != 0 {
                    domain.dma_mask
                } else {
                    DEFAULT_DMA_MASK
                }
            }
        })
        .unwrap_or(DEFAULT_DMA_MASK)
}

fn ensure_device_domain(
    device_ptr: usize,
) -> crate::sync::KernelSpinGuard<'static, Vec<DeviceDmaDomain>> {
    let mut domains = DEVICE_DMA_DOMAINS.lock();
    if domains.iter().all(|domain| domain.device_ptr != device_ptr) {
        domains.push(DeviceDmaDomain {
            device_ptr,
            dma_mask: DEFAULT_DMA_MASK,
            coherent_dma_mask: DEFAULT_DMA_MASK,
            mode: IommuAddressMode::Identity,
        });
    }
    domains
}

fn exceeds_mask(dma_addr: u64, size: usize, mask: u64) -> bool {
    let end = match dma_addr.checked_add(size.saturating_sub(1) as u64) {
        Some(end) => end,
        None => return true,
    };
    end > mask
}

fn irq_safe<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(test)]
    {
        f()
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coherent_mapping_uses_coherent_mask() {
        let device = 0x1000usize as *mut c_void;
        assert_eq!(set_mask(device, 0xffff), 0);
        assert_eq!(set_coherent_mask(device, 0x1fff), 0);

        let mapping = map_physical_range(device, 0x1000, 0x1000, DmaMappingKind::Coherent)
            .expect("coherent mapping should succeed");
        assert_eq!(mapping.dma_addr.raw(), 0x1000);

        assert!(
            map_physical_range(device, 0x2000, 0x1001, DmaMappingKind::Coherent).is_none(),
            "coherent mapping should respect coherent mask"
        );
    }

    #[test]
    fn streaming_mapping_uses_streaming_mask() {
        let device = 0x2000usize as *mut c_void;
        assert_eq!(set_mask(device, 0x2fff), 0);
        assert_eq!(set_coherent_mask(device, u64::MAX), 0);

        assert!(
            map_physical_range(device, 0x2000, 0x1000, DmaMappingKind::Streaming).is_some(),
            "streaming mapping under dma mask should succeed"
        );
        assert!(
            map_physical_range(device, 0x2000, 0x1001, DmaMappingKind::Streaming).is_none(),
            "streaming mapping should respect dma mask"
        );
    }

    #[test]
    fn identity_backend_round_trips_dma_address() {
        let device = 0x3000usize as *mut c_void;
        let mapping = map_physical_range(device, 0x40_0000, 0x2000, DmaMappingKind::Streaming)
            .expect("identity mapping should succeed");
        assert_eq!(
            translate_dma_to_physical(device, mapping.dma_addr, mapping.size),
            Some(0x40_0000)
        );
    }
}
