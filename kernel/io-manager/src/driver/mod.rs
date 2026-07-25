// RustOS links its minimal DVM front-end substrate into the kernel image.
// Linux DVMs own physical devices; this module exposes only the MMIO mapping
// primitive used by fixed, bounded shared-memory transports.
pub mod mmio;
