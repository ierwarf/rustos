// DVM-first input transport.  Linux owns physical input drivers in the DVM;
// RustOS accepts only validated RDI2 frames and leaves event policy to inputd.
pub(crate) mod dvm_serial;
pub mod event_queue;

pub fn init() {
    dvm_serial::init();
}

/// Bounded DVM transport drain called only by the capability-gated inputd
/// ingest broker.
pub fn service_dvm_input_pending() -> usize {
    dvm_serial::service_pending()
}
