// DVM-first input transport. Linux owns physical input drivers in the DVM;
// ring0 transfers only fixed generation-stamped records from the host-owned
// MSI-X ring. inputd owns framing, sequence, lifecycle, and event policy.
pub(crate) mod dvm_ring;
pub(crate) mod wait_queue;

pub fn init() {
    dvm_ring::init();
}

/// Bounded DVM transport drain called only by the capability-gated inputd
/// ingest broker.
pub fn service_dvm_input_pending(
    dest: &mut [rustos_user_abi::syscall::InputDvmRecordWire],
) -> usize {
    dvm_ring::service_pending(dest)
}

/// Publish that inputd's capability-gated ingestion worker has armed its sole
/// kernel waiter. This is deliberately separate from transport installation
/// so L0 cannot produce into an otherwise healthy ring before the fixed
/// user-space consumer can drain it.
pub fn mark_dvm_policy_consumer_ready() -> bool {
    dvm_ring::mark_policy_consumer_ready()
}
