// Linux module loading, hardware-driver DMA allocation, and in-kernel `.ko`
// execution deliberately do not exist here. Linux DVMs own physical devices;
// RustOS retains only MMIO mapping for bounded shared-memory transports.
pub mod mmio;
