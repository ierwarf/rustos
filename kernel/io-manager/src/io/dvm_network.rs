//! Bounded DVM Ethernet shared-ring transport.
//!
//! - **Owner:** `kernel-io-manager` owns ring mechanics; `netd` owns socket and
//!   packet policy and the Linux DVM owns the device driver.
//! - **Boundary:** Shared headers, cursors, lengths, frames, and control epochs
//!   are untrusted.
//! - **Lifecycle:** Install an admitted aperture/epoch, publish bounded slots,
//!   consume exact sequence, and revoke all stale records on owner loss.
//! - **Concurrency:** Producer/consumer ordering is explicit; IRQ leaves signal
//!   progress and perform no packet parsing.
//! - **Failure:** Arithmetic overflow, malformed frame, cursor corruption,
//!   capacity, and generation mismatch fail without out-of-bounds access.
//! - **Forbidden:** No socket policy, native NIC fallback, guest pointer, or
//!   stale slot replay in ring0.
//! - **Evidence:** `dvm-network-ingress`.
// RING3-MIGRATION-REFERENCE START: DVM Ethernet transport substrate.
// Ring0 owns only fixed ivshmem mapping and bounded SPSC ring access. Linux
// networking and RustOS socket/TCP policy remain in their respective user
// services; a DVM never supplies a pointer or descriptor chain to RustOS.
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use driver_domain_protocol::{
    DVM_NET_HEADER_BYTES, DVM_NET_RECORD_BYTES, DVM_NET_SLOT_BYTES, DvmNetHeader,
    validate_dvm_ethernet_frame,
};

use crate::network::{PacketError, PacketTransportStatus};
use crate::sync::KernelWaitLock;

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;
const TX_PRODUCER_OFFSET: usize = 40;
const TX_CONSUMER_OFFSET: usize = 44;
const RX_PRODUCER_OFFSET: usize = 48;
const RX_CONSUMER_OFFSET: usize = 52;
const SLOT_LEN_BYTES: usize = 4;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);
static STATE: KernelWaitLock<
    Option<DvmNetworkState>,
    { nucleus_core::util::lockdep::LockClass::DvmNetworkStateWait as u8 },
> = KernelWaitLock::new(None);
// The fixed ivshmem header and counters are DVM-writable after installation.
// They can describe bounded frame state, but cannot attest that the L0-vetted
// DVM control session is still alive. netd selects the generation through its
// capability-gated broker after validating the service-owned lifecycle.
static TRANSPORT_LEASE: KernelWaitLock<
    TransportLease,
    { nucleus_core::util::lockdep::LockClass::DvmNetworkLeaseWait as u8 },
> = KernelWaitLock::new(TransportLease::inactive());

struct DvmNetworkState {
    base: *mut u8,
    header: DvmNetHeader,
    tx_producer: u32,
    rx_consumer: u32,
}

#[derive(Clone, Copy)]
struct TransportLease {
    epoch: u32,
}

impl TransportLease {
    const fn inactive() -> Self {
        Self { epoch: 0 }
    }

    fn activate(&mut self, epoch: u32) -> bool {
        if epoch == 0 {
            return false;
        }
        self.epoch = epoch;
        true
    }

    fn revoke_exact(&mut self, epoch: u32) -> bool {
        if epoch == 0 || self.epoch != epoch {
            return false;
        }
        self.epoch = 0;
        true
    }

    const fn is_active(self) -> bool {
        self.epoch != 0
    }
}

// The state is accessed only while STATE is held. The aperture lifetime is the
// kernel lifetime after a successful install.
unsafe impl Send for DvmNetworkState {}

pub(crate) fn try_install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    // The first boot-time probe can run before a firmware/PCI path has made
    // every ivshmem function visible. Serialize both boot and demand probes so
    // the first netd packet-status request can safely retry without creating a
    // second mapping or racing ring ownership.
    let mut state_guard = STATE.lock();
    if state_guard.is_some() {
        INSTALLED.store(true, Ordering::Release);
        return true;
    }
    let mut installed = None;
    let mut candidates = 0_u32;
    let mut last_rejection = "none";
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != IVSHMEM_VENDOR_ID || device.device_id() != IVSHMEM_DEVICE_ID {
            return false;
        }
        candidates = candidates.saturating_add(1);
        let Some(resource) = device.resource(IVSHMEM_SHARED_MEMORY_BAR) else {
            last_rejection = "missing-bar";
            return false;
        };
        if resource.is_io || resource.size < u64::from(DVM_NET_HEADER_BYTES) {
            last_rejection = "invalid-bar";
            return false;
        }
        let Ok(resource_len) = usize::try_from(resource.size) else {
            last_rejection = "bar-too-large";
            return false;
        };
        // ivshmem exposes shared RAM rather than device control registers.
        // Match the display aperture's write-combining mapping; the explicit
        // acquire/release ring counters provide the cross-domain ordering.
        let mapped = crate::driver::mmio::map(resource.start, resource_len, true).cast::<u8>();
        if mapped.is_null() {
            last_rejection = "map-failed";
            return false;
        }
        let Some(header) = read_header(mapped) else {
            crate::driver::mmio::unmap(mapped.cast());
            last_rejection = "invalid-header";
            return false;
        };
        if header.region_bytes > resource.size {
            crate::driver::mmio::unmap(mapped.cast());
            last_rejection = "header-outside-bar";
            return false;
        }
        let tx_producer = read_u32(mapped, TX_PRODUCER_OFFSET);
        let initial_rx_consumer = read_u32(mapped, RX_CONSUMER_OFFSET);
        let initial_rx_producer = read_u32(mapped, RX_PRODUCER_OFFSET);
        let rx_consumer =
            if initial_rx_producer.wrapping_sub(initial_rx_consumer) <= header.slot_count {
                // The host may map the aperture after the DVM is already running.
                // A bounded backlog from before this kernel control lease is not
                // delivered into netd. A forged producer cannot advance the
                // RustOS-owned consumer because this distance was validated first.
                write_u32(mapped, RX_CONSUMER_OFFSET, initial_rx_producer);
                initial_rx_producer
            } else {
                // Preserve the last kernel cursor so every receive fails Invalid
                // until the DVM restores a valid fixed-ring relation.
                initial_rx_consumer
            };
        installed = Some(DvmNetworkState {
            base: mapped,
            header,
            // A ring mapped after the DVM has started must not replay frames
            // which predate the current transport generation lease. Do not
            // rewind the DVM-visible TX producer; begin at its observed value.
            tx_producer,
            // RX is RustOS-owned state.  It may move only after the producer
            // distance is bounded; a forged producer leaves the queue fail
            // closed until the DVM restores a valid fixed-ring state.
            rx_consumer,
        });
        true
    });
    let Some(state) = installed else {
        if !UNAVAILABLE_LOGGED.swap(true, Ordering::AcqRel) {
            crate::debug::println!(
                "dvm-network: shared transport unavailable candidates={} last_rejection={}",
                candidates,
                last_rejection
            );
        }
        return false;
    };
    let slots = state.header.slot_count;
    let mtu = state.header.mtu;
    let region = state.header.region_bytes;
    *state_guard = Some(state);
    INSTALLED.store(true, Ordering::Release);
    UNAVAILABLE_LOGGED.store(false, Ordering::Release);
    crate::debug::println!(
        "dvm-network: shared transport installed slots={} mtu={} region={}",
        slots,
        mtu,
        region
    );
    true
}

pub(crate) fn transport_status() -> PacketTransportStatus {
    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        return PacketTransportStatus::Unavailable;
    }
    // The aperture is host-created and header-validated, but mapping it does
    // not make a DVM live.  A guest-writable ready bit could be forged.  Hold
    // the lease across the state check so SESSION_END cannot race a reported
    // available transport with a later packet submission.
    let lease = TRANSPORT_LEASE.lock();
    if STATE.lock().is_none() {
        return PacketTransportStatus::Unavailable;
    }
    if lease.is_active() {
        PacketTransportStatus::Active
    } else {
        PacketTransportStatus::AwaitingAuthenticatedControl
    }
}

pub(crate) fn transmit(frame: &[u8]) -> Result<usize, PacketError> {
    // Lock ordering is always control lease, then ring state.  Revocation
    // waits for an already-started bounded packet operation, then makes every
    // later operation fail as NoDevice rather than using an untrusted aperture.
    let lease = TRANSPORT_LEASE.lock();
    if !lease.is_active() {
        return Err(PacketError::NoDevice);
    }
    let mut state = STATE.lock();
    let state = state.as_mut().ok_or(PacketError::NoDevice)?;
    if frame.len() > state.header.mtu as usize {
        return Err(PacketError::TooLarge);
    }
    if validate_dvm_ethernet_frame(frame).is_err() {
        return Err(PacketError::Invalid);
    }
    let consumer = read_u32(state.base, TX_CONSUMER_OFFSET);
    let used = state.tx_producer.wrapping_sub(consumer);
    if used > state.header.slot_count {
        return Err(PacketError::Invalid);
    }
    if used == state.header.slot_count {
        return Err(PacketError::Busy);
    }
    let slot = tx_slot(state, state.tx_producer).ok_or(PacketError::Invalid)?;
    write_u32(slot, 0, frame.len() as u32);
    unsafe {
        for (index, byte) in frame.iter().enumerate() {
            slot.add(SLOT_LEN_BYTES + index).write_volatile(*byte);
        }
    }
    fence(Ordering::Release);
    state.tx_producer = state.tx_producer.wrapping_add(1);
    write_u32(state.base, TX_PRODUCER_OFFSET, state.tx_producer);
    Ok(frame.len())
}

pub(crate) fn receive(out: &mut [u8]) -> Result<usize, PacketError> {
    let lease = TRANSPORT_LEASE.lock();
    if !lease.is_active() {
        return Err(PacketError::NoDevice);
    }
    let mut state = STATE.lock();
    let state = state.as_mut().ok_or(PacketError::NoDevice)?;
    let producer = read_u32(state.base, RX_PRODUCER_OFFSET);
    let available = producer.wrapping_sub(state.rx_consumer);
    if available > state.header.slot_count {
        return Err(PacketError::Invalid);
    }
    if available == 0 {
        return Err(PacketError::WouldBlock);
    }
    fence(Ordering::Acquire);
    let slot = rx_slot(state, state.rx_consumer).ok_or(PacketError::Invalid)?;
    let len = read_u32(slot, 0) as usize;
    if len == 0 || len > state.header.mtu as usize || len > out.len() {
        advance_rx_consumer(state);
        return Err(PacketError::Invalid);
    }
    unsafe {
        for (index, byte) in out.iter_mut().enumerate().take(len) {
            *byte = slot.add(SLOT_LEN_BYTES + index).read_volatile();
        }
    }
    if validate_dvm_ethernet_frame(&out[..len]).is_err() {
        advance_rx_consumer(state);
        return Err(PacketError::Invalid);
    }
    advance_rx_consumer(state);
    Ok(len)
}

fn advance_rx_consumer(state: &mut DvmNetworkState) {
    fence(Ordering::Release);
    state.rx_consumer = state.rx_consumer.wrapping_add(1);
    write_u32(state.base, RX_CONSUMER_OFFSET, state.rx_consumer);
}

/// Enforce a netd-selected transport generation. The kernel validates only
/// non-zero generation and exact-revoke semantics; input-session ownership and
/// recovery policy remain entirely in inputd/netd.
pub(crate) fn grant_transport_lease(epoch: u32) -> bool {
    if epoch == 0 {
        return false;
    }

    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        // Remember a live service-selected generation even when PCI probing
        // is temporarily early. A later install starts with an empty receive
        // view and can then use this lease; no DVM header value can create it.
        return TRANSPORT_LEASE.lock().activate(epoch);
    }

    // Readers hold TRANSPORT_LEASE before STATE, so reset the receive cursor
    // before publishing the new lease. This drops only a bounded, validated
    // backlog from a retired session. A forged producer never advances a
    // RustOS-owned cursor during this reset.
    let (activated, discarded) = {
        let mut lease = TRANSPORT_LEASE.lock();
        let mut state = STATE.lock();
        let Some(state) = state.as_mut() else {
            return false;
        };
        let discarded = state.discard_retired_receive_backlog();
        (lease.activate(epoch), discarded)
    };
    if activated {
        // Do not call the logging path while either transport lock is held.
        crate::debug::println!(
            "dvm-network: transport generation lease active discarded_rx={}",
            discarded
        );
    }
    activated
}

/// Revoke exactly the netd-selected transport generation. An old cleanup
/// cannot tear down a newer lease.
pub(crate) fn revoke_transport_lease(epoch: u32) -> bool {
    let revoked = {
        let mut lease = TRANSPORT_LEASE.lock();
        lease.revoke_exact(epoch)
    };
    if revoked {
        crate::debug::println!("dvm-network: transport generation lease revoked");
    }
    revoked
}

/// Clear any generation when netd starts or loses its service-owned state.
/// This is capability-gated by the net broker and carries no device policy.
pub(crate) fn reset_transport_lease() {
    TRANSPORT_LEASE.lock().epoch = 0;
}

impl DvmNetworkState {
    fn discard_retired_receive_backlog(&mut self) -> bool {
        let producer = read_u32(self.base, RX_PRODUCER_OFFSET);
        let available = producer.wrapping_sub(self.rx_consumer);
        if available > self.header.slot_count {
            return false;
        }
        self.rx_consumer = producer;
        write_u32(self.base, RX_CONSUMER_OFFSET, self.rx_consumer);
        true
    }
}

fn tx_slot(state: &DvmNetworkState, sequence: u32) -> Option<*mut u8> {
    slot_at(state, DvmNetHeader::tx_ring_offset(), sequence)
}

fn rx_slot(state: &DvmNetworkState, sequence: u32) -> Option<*mut u8> {
    slot_at(state, state.header.rx_ring_offset()?, sequence)
}

fn slot_at(state: &DvmNetworkState, offset: u64, sequence: u32) -> Option<*mut u8> {
    let index = u64::from(sequence % state.header.slot_count);
    let start = offset.checked_add(index.checked_mul(u64::from(DVM_NET_SLOT_BYTES))?)?;
    let end = start.checked_add(u64::from(DVM_NET_SLOT_BYTES))?;
    if end > state.header.region_bytes {
        return None;
    }
    let start = usize::try_from(start).ok()?;
    Some(unsafe { state.base.add(start) })
}

fn read_header(mapped: *const u8) -> Option<DvmNetHeader> {
    let mut bytes = [0_u8; DVM_NET_RECORD_BYTES];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { mapped.add(index).read_volatile() };
    }
    DvmNetHeader::decode(&bytes)
}

fn read_u32(base: *const u8, offset: usize) -> u32 {
    // The runner owns an aligned page and all counter offsets are 4-byte
    // aligned. The aperture is ordinary shared RAM, not MMIO, so an atomic
    // load provides the producer/consumer acquire edge without torn bytes.
    unsafe { (&*base.add(offset).cast::<AtomicU32>()).load(Ordering::Acquire) }.to_le()
}

fn write_u32(base: *mut u8, offset: usize, value: u32) {
    // See `read_u32`: fixed aligned offsets are part of the transport ABI.
    unsafe {
        (&*base.add(offset).cast::<AtomicU32>()).store(u32::from_le(value), Ordering::Release)
    };
}

#[cfg(test)]
mod tests {
    use super::TransportLease;

    #[test]
    fn control_lease_requires_nonzero_epoch_and_exact_revocation() {
        let mut lease = TransportLease::inactive();
        assert!(!lease.is_active());
        assert!(!lease.activate(0));
        assert!(lease.activate(7));
        assert!(lease.is_active());
        assert!(!lease.revoke_exact(6));
        assert!(lease.is_active());
        assert!(lease.revoke_exact(7));
        assert!(!lease.is_active());
    }

    #[test]
    fn stale_cleanup_cannot_revoke_replaced_control_lease() {
        let mut lease = TransportLease::inactive();
        assert!(lease.activate(7));
        assert!(lease.activate(11));
        assert!(!lease.revoke_exact(7));
        assert!(lease.is_active());
        assert!(lease.revoke_exact(11));
    }
}
// RING3-MIGRATION-REFERENCE END: DVM Ethernet transport substrate.
