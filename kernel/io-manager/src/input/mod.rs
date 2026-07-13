// DVM-first input transport.  Linux owns physical input drivers in the DVM;
// RustOS accepts only validated RDI2 frames and leaves event policy to inputd.
pub(crate) mod dvm_serial;
pub mod event_queue;

pub fn init() {
    dvm_serial::init();
}

/// Housekeeping intentionally has no native-input fallback.  The DVM UART is
/// drained only by inputd's authenticated ingest broker below.
pub fn service_pending() -> usize {
    0
}

/// Bounded DVM transport drain called only by the capability-gated inputd
/// ingest broker.
pub fn service_dvm_input_pending() -> usize {
    dvm_serial::service_pending()
}
