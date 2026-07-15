// DVM-first input transport. Linux owns physical input drivers in the DVM;
// L0 commits only validated fixed frames into the host-owned MSI-X input ring
// and inputd owns event policy above the narrow drain substrate.
pub(crate) mod dvm_frames;
pub(crate) mod dvm_ring;
pub mod event_queue;

pub fn init() {
    dvm_ring::init();
}

/// Bounded DVM transport drain called only by the capability-gated inputd
/// ingest broker.
pub fn service_dvm_input_pending() -> usize {
    dvm_ring::service_pending()
}

/// Publish that a policy-backed input client is live. This is deliberately
/// separate from transport installation so L0 cannot produce into an
/// otherwise healthy ring before `inputd` has a consumer.
pub fn mark_dvm_policy_consumer_ready() -> bool {
    dvm_ring::mark_policy_consumer_ready()
}
