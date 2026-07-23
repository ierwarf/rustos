// RING3-MIGRATION-REFERENCE START: storage-DVM block transport substrate.
// Ring0 owns only the fixed address-free queue, immutable transfer-slot
// geometry, and launch-generation revocation. Controller policy, filesystem
// policy, retry, timeout, and recovery remain in storaged/hostd.
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use driver_domain_protocol::{
    DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_COMPLETION_RING_OFFSET, DVM_BLOCK_DATA_OFFSET,
    DVM_BLOCK_FLAG_DVM_READY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_HEADER_BYTES,
    DVM_BLOCK_HEADER_RECORD_BYTES, DVM_BLOCK_KNOWN_FLAGS, DVM_BLOCK_MAGIC, DVM_BLOCK_QUEUE_DEPTH,
    DVM_BLOCK_RECORD_BYTES, DVM_BLOCK_REQUEST_FLAG_FUA, DVM_BLOCK_REQUEST_RING_OFFSET,
    DVM_BLOCK_VERSION, DvmBlockCompletion, DvmBlockCompletionStatus, DvmBlockHeader,
    DvmBlockOperation, DvmBlockRequest,
};

use crate::sync::KernelWaitLock;

const IVSHMEM_VENDOR_ID: u16 = 0x1af4;
const IVSHMEM_DEVICE_ID: u16 = 0x1110;
const IVSHMEM_REGISTERS_BAR: usize = 0;
const IVSHMEM_SHARED_MEMORY_BAR: usize = 2;
const IVSHMEM_DOORBELL_OFFSET: usize = 12;
const BLOCK_DVM_PEER_ID: u16 = 1;
const BLOCK_DVM_REQUEST_VECTOR_INDEX: u16 = 0;
const BLOCK_RING_MSIX_VECTOR_COUNT: u16 = 1;
const MSIX_ENTRY_BYTES: usize = 16;
const MSIX_ENTRY_ADDRESS_LOW_OFFSET: usize = 0;
const MSIX_ENTRY_ADDRESS_HIGH_OFFSET: usize = 4;
const MSIX_ENTRY_DATA_OFFSET: usize = 8;
const MSIX_ENTRY_VECTOR_CONTROL_OFFSET: usize = 12;
const MSIX_ENTRY_VECTOR_MASKED: u32 = 1;
const WAITERS_CAPACITY: usize = 64;
const REGION_BYTES_OFFSET: usize = 16;
const QUEUE_DEPTH_OFFSET: usize = 24;
const DATA_SLOT_BYTES_OFFSET: usize = 28;
const FEATURES_OFFSET: usize = 32;
const GENERATION_OFFSET: usize = 40;
const CAPACITY_SECTORS_OFFSET: usize = 48;
const LOGICAL_BLOCK_SIZE_OFFSET: usize = 56;
const PHYSICAL_BLOCK_SIZE_OFFSET: usize = 60;
const FLAGS_OFFSET: usize = 64;
const RESERVED_OFFSET: usize = 68;
const REQUEST_PRODUCER_OFFSET: usize = 72;
const REQUEST_CONSUMER_OFFSET: usize = 80;
const COMPLETION_PRODUCER_OFFSET: usize = 88;
const COMPLETION_CONSUMER_OFFSET: usize = 96;
const QUEUE_DEPTH: usize = DVM_BLOCK_QUEUE_DEPTH as usize;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static IRQ_PENDING: AtomicBool = AtomicBool::new(false);
static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
static IRQ_VECTOR: AtomicU8 = AtomicU8::new(0);
static FIRST_COMPLETION_REPORTED: AtomicBool = AtomicBool::new(false);
static STATE: KernelWaitLock<Option<DvmBlockState>> = KernelWaitLock::new(None);
static WAITERS: [AtomicU64; WAITERS_CAPACITY] = [const { AtomicU64::new(0) }; WAITERS_CAPACITY];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DvmBlockTicket {
    pub(crate) generation: u64,
    pub(crate) request_id: u64,
    pub(crate) data_slot: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DvmBlockInfo {
    pub(crate) generation: u64,
    pub(crate) capacity_sectors: u64,
    pub(crate) logical_block_size: u32,
    pub(crate) physical_block_size: u32,
    pub(crate) features: u64,
    pub(crate) read_only: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DvmBlockPoll {
    Pending,
    Completed(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DvmBlockError {
    Unavailable,
    Busy,
    Invalid,
    Protocol,
    Revoked,
    DeviceFault,
    Unsupported,
    Cancelled,
}

#[derive(Clone, Copy)]
struct PendingRequest {
    request: DvmBlockRequest,
    completion: Option<DvmBlockCompletion>,
    cancelled: bool,
}

struct DvmBlockState {
    base: *mut u8,
    doorbell: *mut u8,
    geometry: DvmBlockHeader,
    request_producer: u64,
    completion_consumer: u64,
    next_request_id: u64,
    next_operation_id: u64,
    pending: [Option<PendingRequest>; QUEUE_DEPTH],
    revoked: bool,
    ready_observed: bool,
}

// The mapping is kernel-lifetime and every access is serialized by STATE.
unsafe impl Send for DvmBlockState {}

impl DvmBlockState {
    fn new(base: *mut u8, doorbell: *mut u8, header: DvmBlockHeader) -> Self {
        Self {
            base,
            doorbell,
            geometry: header,
            request_producer: header.request_producer,
            completion_consumer: header.completion_consumer,
            next_request_id: 1,
            next_operation_id: 1,
            pending: [None; QUEUE_DEPTH],
            revoked: false,
            ready_observed: header.flags & DVM_BLOCK_FLAG_DVM_READY != 0,
        }
    }

    fn current_header(&mut self) -> Result<DvmBlockHeader, DvmBlockError> {
        if self.revoked {
            return Err(DvmBlockError::Revoked);
        }
        let flags = load_u32(self.base, FLAGS_OFFSET, Ordering::Acquire);
        let expected_fixed_flags = self.geometry.flags & !DVM_BLOCK_FLAG_DVM_READY;
        let fixed_header_matches = load_u64(self.base, 0, Ordering::Acquire)
            == u64::from_le_bytes(DVM_BLOCK_MAGIC)
            && load_u32(self.base, 8, Ordering::Acquire) == DVM_BLOCK_VERSION
            && load_u32(self.base, 12, Ordering::Acquire) == DVM_BLOCK_HEADER_BYTES
            && load_u64(self.base, REGION_BYTES_OFFSET, Ordering::Acquire)
                == self.geometry.region_bytes
            && load_u32(self.base, QUEUE_DEPTH_OFFSET, Ordering::Acquire)
                == self.geometry.queue_depth
            && load_u32(self.base, DATA_SLOT_BYTES_OFFSET, Ordering::Acquire)
                == self.geometry.data_slot_bytes
            && load_u64(self.base, FEATURES_OFFSET, Ordering::Acquire) == self.geometry.features
            && load_u64(self.base, GENERATION_OFFSET, Ordering::Acquire)
                == self.geometry.generation
            && load_u64(self.base, CAPACITY_SECTORS_OFFSET, Ordering::Acquire)
                == self.geometry.capacity_sectors
            && load_u32(self.base, LOGICAL_BLOCK_SIZE_OFFSET, Ordering::Acquire)
                == self.geometry.logical_block_size
            && load_u32(self.base, PHYSICAL_BLOCK_SIZE_OFFSET, Ordering::Acquire)
                == self.geometry.physical_block_size
            && load_u32(self.base, RESERVED_OFFSET, Ordering::Acquire) == 0;
        if !fixed_header_matches
            || flags & !DVM_BLOCK_KNOWN_FLAGS != 0
            || flags & DVM_BLOCK_FLAG_RUSTOS_READY == 0
            || flags & !DVM_BLOCK_FLAG_DVM_READY != expected_fixed_flags
        {
            self.revoke();
            return Err(DvmBlockError::Revoked);
        }
        if flags & DVM_BLOCK_FLAG_DVM_READY == 0 {
            if self.ready_observed {
                self.revoke();
                return Err(DvmBlockError::Revoked);
            }
            return Err(DvmBlockError::Busy);
        }
        if !self.ready_observed {
            report_peer_ready_observation();
        }
        self.ready_observed = true;
        let mut header = self.geometry;
        header.flags = flags;
        Ok(header)
    }

    fn submit(
        &mut self,
        operation: DvmBlockOperation,
        sector: u64,
        data: &[u8],
        data_len: u32,
        fua: bool,
    ) -> Result<DvmBlockTicket, DvmBlockError> {
        let header = self.current_header()?;
        let consumer = load_u64(self.base, REQUEST_CONSUMER_OFFSET, Ordering::Acquire);
        if self.request_producer < consumer
            || self.request_producer.saturating_sub(consumer) >= u64::from(DVM_BLOCK_QUEUE_DEPTH)
        {
            return Err(DvmBlockError::Busy);
        }
        let slot = (self.request_producer % u64::from(DVM_BLOCK_QUEUE_DEPTH)) as usize;
        if self.pending[slot].is_some() {
            return Err(DvmBlockError::Busy);
        }
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(DvmBlockError::Revoked)?;
        let operation_id = if matches!(operation, DvmBlockOperation::Read) {
            0
        } else {
            let operation_id = self.next_operation_id;
            self.next_operation_id = self
                .next_operation_id
                .checked_add(1)
                .ok_or(DvmBlockError::Revoked)?;
            operation_id
        };
        let request = DvmBlockRequest {
            generation: header.generation,
            request_id,
            operation_id,
            operation,
            flags: if fua { DVM_BLOCK_REQUEST_FLAG_FUA } else { 0 },
            data_slot: slot as u32,
            sector,
            data_len,
        };
        if !request.is_valid_for(header)
            || (matches!(operation, DvmBlockOperation::Write) && data.len() != data_len as usize)
            || (!matches!(operation, DvmBlockOperation::Write) && !data.is_empty())
        {
            return Err(DvmBlockError::Invalid);
        }

        if !data.is_empty() {
            let destination = data_slot(self, slot).ok_or(DvmBlockError::Protocol)?;
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len()) };
        }
        self.pending[slot] = Some(PendingRequest {
            request,
            completion: None,
            cancelled: false,
        });
        let record = request.encode();
        let destination =
            request_record(self, self.request_producer).ok_or(DvmBlockError::Protocol)?;
        write_record(destination, &record);
        self.request_producer += 1;
        store_u64(
            self.base,
            REQUEST_PRODUCER_OFFSET,
            self.request_producer,
            Ordering::Release,
        );
        self.signal_request();
        Ok(DvmBlockTicket {
            generation: header.generation,
            request_id,
            data_slot: slot as u32,
        })
    }

    fn signal_request(&self) {
        debug_assert!(!self.doorbell.is_null());
        core::sync::atomic::fence(Ordering::SeqCst);
        let value =
            ivshmem_doorbell_value(BLOCK_DVM_PEER_ID, BLOCK_DVM_REQUEST_VECTOR_INDEX).to_le();
        unsafe {
            self.doorbell
                .add(IVSHMEM_DOORBELL_OFFSET)
                .cast::<u32>()
                .write_volatile(value);
        }
    }

    fn drain_completions(&mut self) -> Result<(), DvmBlockError> {
        let irq_was_pending = IRQ_PENDING.swap(false, Ordering::AcqRel);
        let _ = self.current_header()?;
        let producer = load_u64(self.base, COMPLETION_PRODUCER_OFFSET, Ordering::Acquire);
        if producer < self.completion_consumer
            || producer.saturating_sub(self.completion_consumer) > u64::from(DVM_BLOCK_QUEUE_DEPTH)
        {
            self.revoke();
            return Err(DvmBlockError::Protocol);
        }
        if producer > self.completion_consumer
            && FIRST_COMPLETION_REPORTED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            report_first_completion_observation(
                producer,
                self.completion_consumer,
                irq_was_pending,
            );
        }
        while self.completion_consumer < producer {
            let source =
                completion_record(self, self.completion_consumer).ok_or(DvmBlockError::Protocol)?;
            let record = read_record(source);
            let Some(completion) = DvmBlockCompletion::decode(&record) else {
                self.revoke();
                return Err(DvmBlockError::Protocol);
            };
            let Some(pending) = self.pending.get_mut(completion.data_slot as usize) else {
                self.revoke();
                return Err(DvmBlockError::Protocol);
            };
            let Some(pending) = pending.as_mut() else {
                self.revoke();
                return Err(DvmBlockError::Protocol);
            };
            if pending.completion.is_some()
                || !completion.is_valid_for(self.geometry, pending.request)
            {
                self.revoke();
                return Err(DvmBlockError::Protocol);
            }
            pending.completion = Some(completion);
            let cancelled = pending.cancelled;
            self.completion_consumer += 1;
            store_u64(
                self.base,
                COMPLETION_CONSUMER_OFFSET,
                self.completion_consumer,
                Ordering::Release,
            );
            if cancelled {
                self.pending[completion.data_slot as usize] = None;
            }
        }
        Ok(())
    }

    /// Report a wait event only after the peer becomes ready, a completion is
    /// visible, or the transport has entered a terminal fault.  The initial
    /// `DVM_READY=0` state is an ordinary asynchronous startup phase: treating
    /// it as a fault makes the broker's check-arm-recheck loop spin and lets
    /// early VFS callers exhaust their bounded launch retries before the
    /// storage DVM can publish readiness.
    fn completion_or_fault_pending(&mut self) -> bool {
        if self.revoked {
            return true;
        }
        let readiness_was_observed = self.ready_observed;
        match self.current_header() {
            Ok(_) if !readiness_was_observed => true,
            Ok(_) => {
                let producer = load_u64(self.base, COMPLETION_PRODUCER_OFFSET, Ordering::Acquire);
                producer != self.completion_consumer
            }
            Err(DvmBlockError::Busy) => false,
            Err(_) => true,
        }
    }

    fn poll(
        &mut self,
        ticket: DvmBlockTicket,
        out: &mut [u8],
    ) -> Result<DvmBlockPoll, DvmBlockError> {
        if ticket.generation != self.geometry.generation
            || ticket.data_slot >= DVM_BLOCK_QUEUE_DEPTH
        {
            return Err(DvmBlockError::Revoked);
        }
        self.drain_completions()?;
        let slot = ticket.data_slot as usize;
        let Some(pending) = self.pending[slot] else {
            return Err(DvmBlockError::Revoked);
        };
        if pending.request.request_id != ticket.request_id {
            return Err(DvmBlockError::Revoked);
        }
        if pending.cancelled {
            return Err(DvmBlockError::Cancelled);
        }
        let Some(completion) = pending.completion else {
            return Ok(DvmBlockPoll::Pending);
        };
        let result = match completion.status {
            DvmBlockCompletionStatus::IoError => Err(DvmBlockError::DeviceFault),
            DvmBlockCompletionStatus::Unsupported => Err(DvmBlockError::Unsupported),
            DvmBlockCompletionStatus::Success => {
                if matches!(pending.request.operation, DvmBlockOperation::Read) {
                    let len = pending.request.data_len as usize;
                    if out.len() != len {
                        return Err(DvmBlockError::Invalid);
                    }
                    let source = data_slot(self, slot).ok_or(DvmBlockError::Protocol)?;
                    unsafe { core::ptr::copy_nonoverlapping(source, out.as_mut_ptr(), len) };
                    Ok(DvmBlockPoll::Completed(len))
                } else if out.is_empty() {
                    Ok(DvmBlockPoll::Completed(0))
                } else {
                    Err(DvmBlockError::Invalid)
                }
            }
        };
        result
    }

    fn finish(&mut self, ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
        if ticket.generation != self.geometry.generation
            || ticket.data_slot >= DVM_BLOCK_QUEUE_DEPTH
        {
            return Err(DvmBlockError::Revoked);
        }
        let slot = ticket.data_slot as usize;
        let Some(pending) = self.pending[slot] else {
            return Err(DvmBlockError::Revoked);
        };
        if pending.request.request_id != ticket.request_id || pending.completion.is_none() {
            return Err(DvmBlockError::Invalid);
        }
        self.pending[slot] = None;
        Ok(())
    }

    fn cancel(&mut self, ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
        if ticket.generation != self.geometry.generation
            || ticket.data_slot >= DVM_BLOCK_QUEUE_DEPTH
        {
            return Err(DvmBlockError::Revoked);
        }
        self.drain_completions()?;
        let slot = ticket.data_slot as usize;
        let Some(pending) = self.pending[slot].as_mut() else {
            return Err(DvmBlockError::Revoked);
        };
        if pending.request.request_id != ticket.request_id {
            return Err(DvmBlockError::Revoked);
        }
        pending.cancelled = true;
        if pending.completion.is_some() {
            self.pending[slot] = None;
        }
        Ok(())
    }

    fn revoke(&mut self) {
        if self.revoked {
            return;
        }
        self.revoked = true;
        self.pending = [None; QUEUE_DEPTH];
        fetch_and_u32(
            self.base,
            FLAGS_OFFSET,
            !DVM_BLOCK_FLAG_RUSTOS_READY,
            Ordering::AcqRel,
        );
        IRQ_PENDING.store(true, Ordering::Release);
        wake_waiters();
    }
}

#[cfg(not(test))]
fn report_peer_ready_observation() {
    nucleus_core::debug::write_debugcon_only_line(
        alloc::format!(
            "dvm-block: peer ready observed irq_count={} irq_pending={} vector={:#x}",
            IRQ_COUNT.load(Ordering::Acquire),
            IRQ_PENDING.load(Ordering::Acquire),
            IRQ_VECTOR.load(Ordering::Acquire),
        )
        .as_bytes(),
    );
}

#[cfg(test)]
fn report_peer_ready_observation() {}

#[cfg(not(test))]
fn report_first_completion_observation(producer: u64, consumer: u64, irq_was_pending: bool) {
    nucleus_core::debug::write_debugcon_only_line(
        alloc::format!(
            "dvm-block: first completion observed producer={} consumer={} irq_count={} irq_was_pending={}",
            producer,
            consumer,
            IRQ_COUNT.load(Ordering::Acquire),
            irq_was_pending,
        )
        .as_bytes(),
    );
}

#[cfg(test)]
fn report_first_completion_observation(_producer: u64, _consumer: u64, _irq_was_pending: bool) {}

pub(crate) fn try_install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    let mut guard = STATE.lock();
    if guard.is_some() {
        INSTALLED.store(true, Ordering::Release);
        return true;
    }
    let mut installed: Option<(DvmBlockState, crate::arch::pci::PciDevice)> = None;
    let mut ambiguous = false;
    let mut candidate_count = 0_u32;
    let mut reject_stage = "no-matching-ivshmem";
    let mut rejected_shared_bar = None;
    crate::arch::pci::visit_devices(|device| {
        if device.vendor_id() != IVSHMEM_VENDOR_ID || device.device_id() != IVSHMEM_DEVICE_ID {
            return false;
        }
        candidate_count = candidate_count.saturating_add(1);
        let Some(resource) = device.resource(IVSHMEM_SHARED_MEMORY_BAR) else {
            reject_stage = "shared-bar-missing";
            return false;
        };
        let Some(registers) = device.resource(IVSHMEM_REGISTERS_BAR) else {
            reject_stage = "register-bar-missing";
            return false;
        };
        if resource.is_io || resource.size != DVM_BLOCK_APERTURE_BYTES {
            reject_stage = "shared-bar-shape";
            rejected_shared_bar = Some((
                resource.start,
                resource.size,
                resource.is_io,
                resource.is_64bit,
            ));
            return false;
        }
        if registers.is_io
            || registers.size
                < u64::try_from(IVSHMEM_DOORBELL_OFFSET + core::mem::size_of::<u32>())
                    .unwrap_or(u64::MAX)
        {
            reject_stage = "register-bar-shape";
            return false;
        }
        let Ok(resource_len) = usize::try_from(resource.size) else {
            reject_stage = "shared-bar-host-width";
            return false;
        };
        let mapped = crate::driver::mmio::map(resource.start, resource_len, true).cast::<u8>();
        if mapped.is_null() {
            reject_stage = "shared-bar-map";
            return false;
        }
        let Some(header) = read_header(mapped) else {
            reject_stage = "header";
            crate::driver::mmio::unmap(mapped.cast());
            return false;
        };
        if header.region_bytes != resource.size
            || header.flags & (DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY) != 0
        {
            reject_stage = "header-state";
            crate::driver::mmio::unmap(mapped.cast());
            return false;
        }
        let Ok(registers_len) = usize::try_from(registers.size) else {
            reject_stage = "register-bar-host-width";
            crate::driver::mmio::unmap(mapped.cast());
            return false;
        };
        let doorbell = crate::driver::mmio::map(registers.start, registers_len, false).cast::<u8>();
        if doorbell.is_null() {
            reject_stage = "register-bar-map";
            crate::driver::mmio::unmap(mapped.cast());
            return false;
        }
        if let Some((previous, _)) = installed.take() {
            release_state_mappings(&previous);
            crate::driver::mmio::unmap(mapped.cast());
            crate::driver::mmio::unmap(doorbell.cast());
            ambiguous = true;
            reject_stage = "ambiguous";
            return false;
        }
        if ambiguous {
            crate::driver::mmio::unmap(mapped.cast());
            crate::driver::mmio::unmap(doorbell.cast());
            return false;
        }
        installed = Some((DvmBlockState::new(mapped, doorbell, header), device));
        false
    });
    let Some((mut state, device)) = installed else {
        let diagnostic = if let Some((start, size, is_io, is_64bit)) = rejected_shared_bar {
            alloc::format!(
                "dvm-block: install rejected stage={} ivshmem_candidates={} shared_start={:#x} shared_size={:#x} shared_io={} shared_64={}",
                reject_stage,
                candidate_count,
                start,
                size,
                is_io,
                is_64bit,
            )
        } else {
            alloc::format!(
                "dvm-block: install rejected stage={} ivshmem_candidates={}",
                reject_stage,
                candidate_count,
            )
        };
        nucleus_core::debug::write_debugcon_only_line(diagnostic.as_bytes());
        return false;
    };
    if ambiguous {
        release_state_mappings(&state);
        nucleus_core::debug::write_debugcon_only_line(
            alloc::format!(
                "dvm-block: install rejected stage=ambiguous ivshmem_candidates={}",
                candidate_count,
            )
            .as_bytes(),
        );
        return false;
    }
    if !arm_block_ring_interrupt(device) {
        release_state_mappings(&state);
        nucleus_core::debug::write_debugcon_only_line(
            alloc::format!(
                "dvm-block: install rejected stage=msix-arm ivshmem_candidates={}",
                candidate_count,
            )
            .as_bytes(),
        );
        return false;
    }
    let previous_flags = fetch_or_u32(
        state.base,
        FLAGS_OFFSET,
        DVM_BLOCK_FLAG_RUSTOS_READY,
        Ordering::AcqRel,
    );
    if previous_flags & (DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY) != 0 {
        release_state_mappings(&state);
        return false;
    }
    state.geometry.flags |= DVM_BLOCK_FLAG_RUSTOS_READY;
    state.signal_request();
    nucleus_core::debug::write_debugcon_only_line(
        alloc::format!(
            "dvm-block: transport installed generation={} sectors={} logical={} physical={}",
            state.geometry.generation,
            state.geometry.capacity_sectors,
            state.geometry.logical_block_size,
            state.geometry.physical_block_size,
        )
        .as_bytes(),
    );
    *guard = Some(state);
    INSTALLED.store(true, Ordering::Release);
    true
}

fn release_state_mappings(state: &DvmBlockState) {
    crate::driver::mmio::unmap(state.base.cast());
    crate::driver::mmio::unmap(state.doorbell.cast());
}

const fn ivshmem_doorbell_value(peer_id: u16, vector_index: u16) -> u32 {
    ((peer_id as u32) << 16) | vector_index as u32
}

pub(crate) fn submit_read(sector: u64, data_len: u32) -> Result<DvmBlockTicket, DvmBlockError> {
    if !try_install() {
        return Err(DvmBlockError::Unavailable);
    }
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .submit(DvmBlockOperation::Read, sector, &[], data_len, false)
}

pub(crate) fn info() -> Result<DvmBlockInfo, DvmBlockError> {
    if !try_install() {
        return Err(DvmBlockError::Unavailable);
    }
    let mut guard = STATE.lock();
    let state = guard.as_mut().ok_or(DvmBlockError::Unavailable)?;
    let header = state.current_header()?;
    Ok(DvmBlockInfo {
        generation: header.generation,
        capacity_sectors: header.capacity_sectors,
        logical_block_size: header.logical_block_size,
        physical_block_size: header.physical_block_size,
        features: header.features,
        read_only: header.flags & driver_domain_protocol::DVM_BLOCK_FLAG_READ_ONLY != 0,
    })
}

pub(crate) fn submit_write(
    sector: u64,
    data: &[u8],
    fua: bool,
) -> Result<DvmBlockTicket, DvmBlockError> {
    if !try_install() {
        return Err(DvmBlockError::Unavailable);
    }
    let data_len = u32::try_from(data.len()).map_err(|_| DvmBlockError::Invalid)?;
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .submit(DvmBlockOperation::Write, sector, data, data_len, fua)
}

pub(crate) fn submit_flush() -> Result<DvmBlockTicket, DvmBlockError> {
    if !try_install() {
        return Err(DvmBlockError::Unavailable);
    }
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .submit(DvmBlockOperation::Flush, 0, &[], 0, false)
}

pub(crate) fn poll(ticket: DvmBlockTicket, out: &mut [u8]) -> Result<DvmBlockPoll, DvmBlockError> {
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .poll(ticket, out)
}

pub(crate) fn cancel(ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .cancel(ticket)
}

pub(crate) fn finish(ticket: DvmBlockTicket) -> Result<(), DvmBlockError> {
    STATE
        .lock()
        .as_mut()
        .ok_or(DvmBlockError::Unavailable)?
        .finish(ticket)
}

pub(crate) fn completion_or_fault_pending() -> bool {
    if IRQ_PENDING.load(Ordering::Acquire) {
        return true;
    }
    let mut guard = STATE.lock();
    let Some(state) = guard.as_mut() else {
        return false;
    };
    state.completion_or_fault_pending()
}

pub(crate) fn arm_waiter(task_id: u64) -> bool {
    if task_id == 0 {
        return false;
    }
    for slot in &WAITERS {
        let existing = slot.load(Ordering::Acquire);
        if existing == task_id {
            return true;
        }
        if existing != 0 && !crate::multitask::is_user_task_alive(existing) {
            let _ = slot.compare_exchange(existing, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
    WAITERS.iter().any(|slot| {
        slot.compare_exchange(0, task_id, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    })
}

pub(crate) fn disarm_waiter(task_id: u64) {
    for slot in &WAITERS {
        let _ = slot.compare_exchange(task_id, 0, Ordering::AcqRel, Ordering::Acquire);
    }
}

fn wake_waiters() {
    for slot in &WAITERS {
        let task_id = slot.swap(0, Ordering::AcqRel);
        if task_id != 0 {
            let _ = crate::multitask::wake_task(task_id);
        }
    }
}

fn block_ring_interrupt(_vector: u8) {
    IRQ_COUNT.fetch_add(1, Ordering::Relaxed);
    IRQ_PENDING.store(true, Ordering::Release);
    wake_waiters();
}

fn request_record(state: &DvmBlockState, sequence: u64) -> Option<*mut u8> {
    ring_record(state, DVM_BLOCK_REQUEST_RING_OFFSET, sequence)
}

fn completion_record(state: &DvmBlockState, sequence: u64) -> Option<*mut u8> {
    ring_record(state, DVM_BLOCK_COMPLETION_RING_OFFSET, sequence)
}

fn ring_record(state: &DvmBlockState, offset: u64, sequence: u64) -> Option<*mut u8> {
    let index = sequence % u64::from(DVM_BLOCK_QUEUE_DEPTH);
    let start = offset.checked_add(index.checked_mul(DVM_BLOCK_RECORD_BYTES as u64)?)?;
    let end = start.checked_add(DVM_BLOCK_RECORD_BYTES as u64)?;
    if end > state.geometry.region_bytes {
        return None;
    }
    Some(unsafe { state.base.add(start as usize) })
}

fn data_slot(state: &DvmBlockState, slot: usize) -> Option<*mut u8> {
    if slot >= QUEUE_DEPTH {
        return None;
    }
    let start = DVM_BLOCK_DATA_OFFSET
        .checked_add((slot as u64).checked_mul(u64::from(state.geometry.data_slot_bytes))?)?;
    let end = start.checked_add(u64::from(state.geometry.data_slot_bytes))?;
    if end > state.geometry.region_bytes {
        return None;
    }
    Some(unsafe { state.base.add(start as usize) })
}

fn read_header(base: *mut u8) -> Option<DvmBlockHeader> {
    let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile(base.add(offset)) };
    }
    // Both rings are single-producer/single-consumer. Reading the consumer
    // first and producer second yields a valid snapshot while either peer is
    // advancing its own cursor. All four fields are naturally aligned and
    // single-copy atomic by the wire contract.
    bytes[REQUEST_CONSUMER_OFFSET..REQUEST_CONSUMER_OFFSET + 8]
        .copy_from_slice(&load_u64(base, REQUEST_CONSUMER_OFFSET, Ordering::Acquire).to_le_bytes());
    bytes[COMPLETION_CONSUMER_OFFSET..COMPLETION_CONSUMER_OFFSET + 8].copy_from_slice(
        &load_u64(base, COMPLETION_CONSUMER_OFFSET, Ordering::Acquire).to_le_bytes(),
    );
    bytes[REQUEST_PRODUCER_OFFSET..REQUEST_PRODUCER_OFFSET + 8]
        .copy_from_slice(&load_u64(base, REQUEST_PRODUCER_OFFSET, Ordering::Acquire).to_le_bytes());
    bytes[COMPLETION_PRODUCER_OFFSET..COMPLETION_PRODUCER_OFFSET + 8].copy_from_slice(
        &load_u64(base, COMPLETION_PRODUCER_OFFSET, Ordering::Acquire).to_le_bytes(),
    );
    DvmBlockHeader::decode(&bytes)
}

fn read_record(base: *mut u8) -> [u8; DVM_BLOCK_RECORD_BYTES] {
    let mut bytes = [0_u8; DVM_BLOCK_RECORD_BYTES];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile(base.add(offset)) };
    }
    bytes
}

fn write_record(base: *mut u8, bytes: &[u8; DVM_BLOCK_RECORD_BYTES]) {
    for (offset, byte) in bytes.iter().copied().enumerate() {
        unsafe { core::ptr::write_volatile(base.add(offset), byte) };
    }
}

fn arm_block_ring_interrupt(device: crate::arch::pci::PciDevice) -> bool {
    let Some(capability) = device.msix_capability() else {
        return false;
    };
    if capability.table_entries() != BLOCK_RING_MSIX_VECTOR_COUNT {
        return false;
    }
    let Some(table_resource) = capability.table_resource(device) else {
        return false;
    };
    let Ok(table_len) = usize::try_from(table_resource.size) else {
        return false;
    };
    let Ok(table_offset) = usize::try_from(capability.table_offset()) else {
        return false;
    };
    if table_offset
        .checked_add(MSIX_ENTRY_BYTES)
        .is_none_or(|end| end > table_len)
    {
        return false;
    }
    capability.set_function_masked(device, true);
    capability.set_enabled(device, false);
    let Some(mut vector_lease) = crate::arch::msi::MsiVectorLease::allocate() else {
        return false;
    };
    if !vector_lease.register_handler(block_ring_interrupt) {
        return false;
    }
    let Some(message) = vector_lease.message() else {
        return false;
    };
    let table = crate::driver::mmio::map(table_resource.start, table_len, false).cast::<u8>();
    if table.is_null() {
        return false;
    }
    unsafe {
        program_msix_entry(table.add(table_offset), message);
        core::sync::atomic::fence(Ordering::SeqCst);
        table
            .add(table_offset + MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(0);
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    capability.set_enabled(device, true);
    capability.set_function_masked(device, false);
    crate::driver::mmio::unmap(table.cast());
    IRQ_VECTOR.store(vector_lease.commit(), Ordering::Release);
    true
}

unsafe fn program_msix_entry(entry: *mut u8, message: crate::arch::msi::MsiMessage) {
    unsafe {
        entry
            .add(MSIX_ENTRY_VECTOR_CONTROL_OFFSET)
            .cast::<u32>()
            .write_volatile(MSIX_ENTRY_VECTOR_MASKED.to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_LOW_OFFSET)
            .cast::<u32>()
            .write_volatile((message.address as u32).to_le());
        entry
            .add(MSIX_ENTRY_ADDRESS_HIGH_OFFSET)
            .cast::<u32>()
            .write_volatile(((message.address >> 32) as u32).to_le());
        entry
            .add(MSIX_ENTRY_DATA_OFFSET)
            .cast::<u32>()
            .write_volatile(message.data.to_le());
    }
}

fn load_u32(base: *mut u8, offset: usize, ordering: Ordering) -> u32 {
    let address = unsafe { base.add(offset) };
    debug_assert_eq!(address.align_offset(core::mem::align_of::<AtomicU32>()), 0);
    // SAFETY: The ABI fixes every u32 field at a naturally aligned offset in a
    // page-aligned, kernel-lifetime shared-memory aperture. The peer accesses
    // these fields atomically under the same wire contract.
    unsafe { AtomicU32::from_ptr(address.cast::<u32>()).load(ordering) }
}

fn fetch_and_u32(base: *mut u8, offset: usize, value: u32, ordering: Ordering) -> u32 {
    let address = unsafe { base.add(offset) };
    debug_assert_eq!(address.align_offset(core::mem::align_of::<AtomicU32>()), 0);
    // SAFETY: See load_u32. Readiness has one RustOS writer and the peer may
    // only update its own DVM-ready bit through an atomic read/modify/write.
    unsafe { AtomicU32::from_ptr(address.cast::<u32>()).fetch_and(value, ordering) }
}

fn fetch_or_u32(base: *mut u8, offset: usize, value: u32, ordering: Ordering) -> u32 {
    let address = unsafe { base.add(offset) };
    debug_assert_eq!(address.align_offset(core::mem::align_of::<AtomicU32>()), 0);
    // SAFETY: The shared header contract naturally aligns the flags field and
    // every participant updates readiness through atomic read-modify-write.
    unsafe { AtomicU32::from_ptr(address.cast::<u32>()).fetch_or(value, ordering) }
}

fn load_u64(base: *mut u8, offset: usize, ordering: Ordering) -> u64 {
    let address = unsafe { base.add(offset) };
    debug_assert_eq!(address.align_offset(core::mem::align_of::<AtomicU64>()), 0);
    // SAFETY: See load_u32. RustOS supports this transport only on little-
    // endian x86_64, so the native atomic representation is the wire value.
    unsafe { AtomicU64::from_ptr(address.cast::<u64>()).load(ordering) }
}

fn store_u64(base: *mut u8, offset: usize, value: u64, ordering: Ordering) {
    let address = unsafe { base.add(offset) };
    debug_assert_eq!(address.align_offset(core::mem::align_of::<AtomicU64>()), 0);
    // SAFETY: See load_u64. Each cursor has exactly one writer.
    unsafe { AtomicU64::from_ptr(address.cast::<u64>()).store(value, ordering) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use driver_domain_protocol::{
        DVM_BLOCK_FEATURE_FLUSH, DVM_BLOCK_FEATURE_FUA, DVM_BLOCK_FLAG_DVM_READY,
    };

    fn aperture_bytes(aperture: &mut [u64]) -> &mut [u8] {
        // SAFETY: The u64 backing guarantees the production ABI's alignment;
        // the byte slice spans the exact same initialized allocation.
        unsafe {
            core::slice::from_raw_parts_mut(
                aperture.as_mut_ptr().cast::<u8>(),
                core::mem::size_of_val(aperture),
            )
        }
    }

    fn test_state() -> (alloc::vec::Vec<u64>, alloc::vec::Vec<u32>, DvmBlockState) {
        let mut aperture = vec![0_u64; DVM_BLOCK_APERTURE_BYTES as usize / 8];
        let mut registers =
            vec![0_u32; (IVSHMEM_DOORBELL_OFFSET + core::mem::size_of::<u32>()) / 4];
        let mut header = DvmBlockHeader::new(
            7,
            1024 * 1024,
            4096,
            4096,
            DVM_BLOCK_FEATURE_FLUSH | DVM_BLOCK_FEATURE_FUA,
        );
        header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
        aperture_bytes(&mut aperture)[..DVM_BLOCK_HEADER_RECORD_BYTES]
            .copy_from_slice(&header.encode());
        let state = DvmBlockState::new(
            aperture.as_mut_ptr().cast::<u8>(),
            registers.as_mut_ptr().cast::<u8>(),
            header,
        );
        (aperture, registers, state)
    }

    #[test]
    fn request_and_completion_bind_exact_slot_epoch_and_durability() {
        let (mut aperture, registers, mut state) = test_state();
        let data = vec![0x5a_u8; 4096];
        let ticket = state
            .submit(DvmBlockOperation::Write, 8, &data, 4096, true)
            .unwrap();
        let request_offset = DVM_BLOCK_REQUEST_RING_OFFSET as usize;
        let request: [u8; DVM_BLOCK_RECORD_BYTES] = aperture_bytes(&mut aperture)
            [request_offset..request_offset + DVM_BLOCK_RECORD_BYTES]
            .try_into()
            .unwrap();
        let request = DvmBlockRequest::decode(&request).unwrap();
        assert_eq!(request.request_id, ticket.request_id);
        assert_eq!(
            registers[IVSHMEM_DOORBELL_OFFSET / 4],
            ivshmem_doorbell_value(BLOCK_DVM_PEER_ID, BLOCK_DVM_REQUEST_VECTOR_INDEX)
        );
        assert_eq!(
            &aperture_bytes(&mut aperture)
                [DVM_BLOCK_DATA_OFFSET as usize..DVM_BLOCK_DATA_OFFSET as usize + data.len()],
            data.as_slice()
        );

        let completion = DvmBlockCompletion {
            generation: request.generation,
            request_id: request.request_id,
            operation_id: request.operation_id,
            status: DvmBlockCompletionStatus::Success,
            data_slot: request.data_slot,
            completed_bytes: request.data_len,
            durable_through_operation_id: request.operation_id,
        };
        let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
        aperture_bytes(&mut aperture)
            [completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
            .copy_from_slice(&completion.encode());
        let base = aperture.as_mut_ptr().cast::<u8>();
        store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
        store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
        assert_eq!(
            state.poll(ticket, &mut []).unwrap(),
            DvmBlockPoll::Completed(0)
        );
    }

    #[test]
    fn stale_completion_revokes_the_transport() {
        let (mut aperture, _registers, mut state) = test_state();
        let ticket = state
            .submit(DvmBlockOperation::Read, 8, &[], 4096, false)
            .unwrap();
        let completion = DvmBlockCompletion {
            generation: ticket.generation - 1,
            request_id: ticket.request_id,
            operation_id: 0,
            status: DvmBlockCompletionStatus::Success,
            data_slot: ticket.data_slot,
            completed_bytes: 4096,
            durable_through_operation_id: 0,
        };
        let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
        aperture_bytes(&mut aperture)
            [completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
            .copy_from_slice(&completion.encode());
        let base = aperture.as_mut_ptr().cast::<u8>();
        store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
        store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
        assert_eq!(
            state.poll(ticket, &mut [0_u8; 4096]),
            Err(DvmBlockError::Protocol)
        );
        assert!(state.revoked);
    }

    #[test]
    fn cancellation_keeps_the_slot_owned_until_the_exact_completion() {
        let (mut aperture, _registers, mut state) = test_state();
        let ticket = state
            .submit(DvmBlockOperation::Read, 8, &[], 4096, false)
            .unwrap();
        state.cancel(ticket).unwrap();
        assert!(state.pending[ticket.data_slot as usize].is_some());
        assert_eq!(
            state.poll(ticket, &mut [0_u8; 4096]),
            Err(DvmBlockError::Cancelled)
        );

        let completion = DvmBlockCompletion {
            generation: ticket.generation,
            request_id: ticket.request_id,
            operation_id: 0,
            status: DvmBlockCompletionStatus::Success,
            data_slot: ticket.data_slot,
            completed_bytes: 4096,
            durable_through_operation_id: 0,
        };
        let completion_offset = DVM_BLOCK_COMPLETION_RING_OFFSET as usize;
        aperture_bytes(&mut aperture)
            [completion_offset..completion_offset + DVM_BLOCK_RECORD_BYTES]
            .copy_from_slice(&completion.encode());
        let base = aperture.as_mut_ptr().cast::<u8>();
        store_u64(base, REQUEST_CONSUMER_OFFSET, 1, Ordering::Release);
        store_u64(base, COMPLETION_PRODUCER_OFFSET, 1, Ordering::Release);
        assert_eq!(
            state.poll(ticket, &mut [0_u8; 4096]),
            Err(DvmBlockError::Revoked)
        );
        assert!(state.pending[ticket.data_slot as usize].is_none());
        assert!(!state.revoked);
    }

    #[test]
    fn readiness_may_arrive_once_but_cannot_be_withdrawn() {
        let (mut aperture, _registers, mut state) = test_state();
        let base = aperture.as_mut_ptr().cast::<u8>();
        let flags = load_u32(base, FLAGS_OFFSET, Ordering::Acquire);
        unsafe {
            AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
                .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
        }
        state.ready_observed = false;
        assert_eq!(state.current_header(), Err(DvmBlockError::Busy));
        assert!(!state.revoked);

        unsafe {
            AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
                .store(flags, Ordering::Release);
        }
        assert!(state.current_header().is_ok());
        unsafe {
            AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
                .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
        }
        assert_eq!(state.current_header(), Err(DvmBlockError::Revoked));
        assert!(state.revoked);
    }

    #[test]
    fn startup_not_ready_is_sleepable_not_a_fault_event() {
        let (mut aperture, _registers, mut state) = test_state();
        let base = aperture.as_mut_ptr().cast::<u8>();
        let flags = load_u32(base, FLAGS_OFFSET, Ordering::Acquire);
        unsafe {
            AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
                .store(flags & !DVM_BLOCK_FLAG_DVM_READY, Ordering::Release);
        }
        state.ready_observed = false;

        assert!(!state.completion_or_fault_pending());
        assert!(!state.revoked);

        unsafe {
            AtomicU32::from_ptr(base.add(FLAGS_OFFSET).cast::<u32>())
                .store(flags, Ordering::Release);
        }
        assert!(state.completion_or_fault_pending());
        assert!(!state.completion_or_fault_pending());
        assert!(state.ready_observed);
    }
}
// RING3-MIGRATION-REFERENCE END: storage-DVM block transport substrate.
