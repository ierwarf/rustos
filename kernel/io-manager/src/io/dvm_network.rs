// RING3-MIGRATION-REFERENCE START: DVM Ethernet transport substrate.
// Ring0 owns only fixed ivshmem mapping and bounded SPSC ring access. Linux
// networking and RustOS socket/TCP policy remain in their respective user
// services; a DVM never supplies a pointer or descriptor chain to RustOS.
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use driver_domain_protocol::{
    DVM_NET_HEADER_BYTES, DVM_NET_RECORD_BYTES, DVM_NET_SLOT_BYTES, DvmNetHeader,
};

use crate::network::PacketError;
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
static STATE: KernelWaitLock<Option<DvmNetworkState>> = KernelWaitLock::new(None);

struct DvmNetworkState {
    base: *mut u8,
    header: DvmNetHeader,
    tx_producer: u32,
    rx_consumer: u32,
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
        installed = Some(DvmNetworkState {
            base: mapped,
            header,
            tx_producer: read_u32(mapped, TX_PRODUCER_OFFSET),
            rx_consumer: read_u32(mapped, RX_CONSUMER_OFFSET),
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

pub(crate) fn available() -> bool {
    if !INSTALLED.load(Ordering::Acquire) && !try_install() {
        return false;
    }
    // The aperture is host-created and header-validated. DVM liveness is not
    // a kernel authority decision: a guest-writable bit could be forged, and
    // the bounded ring already returns Busy/Invalid rather than allocating or
    // following guest-controlled descriptors. L0's authenticated health
    // channel owns reset/revocation policy.
    STATE.lock().is_some()
}

pub(crate) fn transmit(frame: &[u8]) -> Result<usize, PacketError> {
    let mut state = STATE.lock();
    let state = state.as_mut().ok_or(PacketError::NoDevice)?;
    if frame.len() > state.header.mtu as usize {
        return Err(PacketError::TooLarge);
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
        return Err(PacketError::Invalid);
    }
    unsafe {
        for (index, byte) in out.iter_mut().enumerate().take(len) {
            *byte = slot.add(SLOT_LEN_BYTES + index).read_volatile();
        }
    }
    fence(Ordering::Release);
    state.rx_consumer = state.rx_consumer.wrapping_add(1);
    write_u32(state.base, RX_CONSUMER_OFFSET, state.rx_consumer);
    Ok(len)
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
    (end <= state.header.region_bytes).then_some(unsafe { state.base.add(start as usize) })
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
    unsafe { (&*base.add(offset).cast::<AtomicU32>()).store(u32::from_le(value), Ordering::Release) };
}
// RING3-MIGRATION-REFERENCE END: DVM Ethernet transport substrate.
