// Privileged transport primitives retained by the DVM-first topology.
//
// Linux module loading, Linux driver object models, and in-kernel `.ko`
// execution deliberately do not exist here.  The Linux DVM owns hardware
// drivers; RustOS owns only the narrow MMIO/DMA/IRQ substrate required by its
// private shared-memory transports and boot storage.
pub mod dma;
pub mod iommu;
pub mod mmio;
