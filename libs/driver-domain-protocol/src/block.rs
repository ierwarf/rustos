// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

/// Fixed RustOS-to-storage-DVM block transport. The wire vocabulary follows
/// Virtio block operation and durability semantics, but the transport is a
/// RustOS-owned, address-free ivshmem aperture with an explicit launch epoch.
///
/// Request and completion records never contain a pointer or guest-selected
/// DMA address. `data_slot` selects one of the fixed host-owned transfer slots.
/// Each producer publishes record/data and its Release cursor before sending
/// ivshmem MSI-X vector 0. RustOS rings DVM peer 1 for requests; the DVM rings
/// RustOS peer 0 for completions or readiness withdrawal. Doorbell edges are
/// advisory and may coalesce: consumers check the cursor before arming, arm,
/// recheck, and drain until the authoritative ring cursors are equal.
pub const DVM_BLOCK_MAGIC: [u8; 8] = *b"RSDVMBL2";
pub const DVM_BLOCK_VERSION: u32 = 2;
pub const DVM_BLOCK_HEADER_BYTES: u32 = 4096;
pub const DVM_BLOCK_HEADER_RECORD_BYTES: usize = 192;
pub const DVM_BLOCK_EPOCH_SIGNING_BYTES: usize = 72;
pub const DVM_BLOCK_RECORD_BYTES: usize = 64;
pub const DVM_BLOCK_QUEUE_DEPTH: u32 = 64;
pub const DVM_BLOCK_DATA_SLOT_BYTES: u32 = 64 * 1024;
pub const DVM_BLOCK_REQUEST_RING_OFFSET: u64 = DVM_BLOCK_HEADER_BYTES as u64;
pub const DVM_BLOCK_COMPLETION_RING_OFFSET: u64 =
    DVM_BLOCK_REQUEST_RING_OFFSET + DVM_BLOCK_QUEUE_DEPTH as u64 * DVM_BLOCK_RECORD_BYTES as u64;
pub const DVM_BLOCK_DATA_OFFSET: u64 =
    DVM_BLOCK_COMPLETION_RING_OFFSET + DVM_BLOCK_QUEUE_DEPTH as u64 * DVM_BLOCK_RECORD_BYTES as u64;
pub const DVM_BLOCK_USED_BYTES: u64 =
    DVM_BLOCK_DATA_OFFSET + DVM_BLOCK_QUEUE_DEPTH as u64 * DVM_BLOCK_DATA_SLOT_BYTES as u64;
/// PCI BARs, including QEMU ivshmem BAR2, require a power-of-two aperture.
/// The address-free rings and slots occupy `DVM_BLOCK_USED_BYTES`; the
/// remaining tail is reserved and never acquires request/data authority.
pub const DVM_BLOCK_APERTURE_BYTES: u64 = 8 * 1024 * 1024;

pub const DVM_BLOCK_FEATURE_FLUSH: u64 = 1 << 0;
pub const DVM_BLOCK_FEATURE_DISCARD: u64 = 1 << 1;
pub const DVM_BLOCK_FEATURE_WRITE_ZEROES: u64 = 1 << 2;
pub const DVM_BLOCK_FEATURE_FUA: u64 = 1 << 3;
pub const DVM_BLOCK_FEATURE_WRITEBACK: u64 = 1 << 4;
pub const DVM_BLOCK_KNOWN_FEATURES: u64 = DVM_BLOCK_FEATURE_FLUSH
    | DVM_BLOCK_FEATURE_DISCARD
    | DVM_BLOCK_FEATURE_WRITE_ZEROES
    | DVM_BLOCK_FEATURE_FUA
    | DVM_BLOCK_FEATURE_WRITEBACK;
pub const DVM_BLOCK_REQUIRED_FEATURES: u64 = DVM_BLOCK_FEATURE_FLUSH;

pub const DVM_BLOCK_FLAG_RUSTOS_READY: u32 = 1 << 0;
pub const DVM_BLOCK_FLAG_DVM_READY: u32 = 1 << 1;
pub const DVM_BLOCK_FLAG_READ_ONLY: u32 = 1 << 2;
pub const DVM_BLOCK_KNOWN_FLAGS: u32 =
    DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY | DVM_BLOCK_FLAG_READ_ONLY;

pub const DVM_BLOCK_REQUEST_FLAG_FUA: u32 = 1 << 0;
pub const DVM_BLOCK_REQUEST_FLAG_UNMAP: u32 = 1 << 1;
pub const DVM_BLOCK_REQUEST_KNOWN_FLAGS: u32 =
    DVM_BLOCK_REQUEST_FLAG_FUA | DVM_BLOCK_REQUEST_FLAG_UNMAP;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmBlockHeader {
    pub region_bytes: u64,
    pub queue_depth: u32,
    pub data_slot_bytes: u32,
    pub features: u64,
    pub generation: u64,
    pub capacity_sectors: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub flags: u32,
    pub request_producer: u64,
    pub request_consumer: u64,
    pub completion_producer: u64,
    pub completion_consumer: u64,
    /// Ed25519 signature over `epoch_signing_bytes()`, produced by L0.
    ///
    /// The DVM maps this record writable for ring progress, so the signature
    /// is the authority boundary for immutable geometry and generation.
    pub epoch_signature: [u8; 64],
}

impl DvmBlockHeader {
    pub const fn new(
        generation: u64,
        capacity_sectors: u64,
        logical_block_size: u32,
        physical_block_size: u32,
        features: u64,
    ) -> Self {
        Self {
            region_bytes: DVM_BLOCK_APERTURE_BYTES,
            queue_depth: DVM_BLOCK_QUEUE_DEPTH,
            data_slot_bytes: DVM_BLOCK_DATA_SLOT_BYTES,
            features,
            generation,
            capacity_sectors,
            logical_block_size,
            physical_block_size,
            flags: 0,
            request_producer: 0,
            request_consumer: 0,
            completion_producer: 0,
            completion_consumer: 0,
            epoch_signature: [0; 64],
        }
    }

    pub const fn with_epoch_signature(mut self, epoch_signature: [u8; 64]) -> Self {
        self.epoch_signature = epoch_signature;
        self
    }

    pub fn epoch_signing_bytes(self) -> [u8; DVM_BLOCK_EPOCH_SIGNING_BYTES] {
        let mut bytes = [0_u8; DVM_BLOCK_EPOCH_SIGNING_BYTES];
        bytes[0..8].copy_from_slice(&DVM_BLOCK_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_BLOCK_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_BLOCK_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.queue_depth.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.data_slot_bytes.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.features.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.generation.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.logical_block_size.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.physical_block_size.to_le_bytes());
        bytes[64..68].copy_from_slice(&(self.flags & DVM_BLOCK_FLAG_READ_ONLY).to_le_bytes());
        bytes
    }

    pub fn is_valid(self) -> bool {
        self.region_bytes == DVM_BLOCK_APERTURE_BYTES
            && self.queue_depth == DVM_BLOCK_QUEUE_DEPTH
            && self.data_slot_bytes == DVM_BLOCK_DATA_SLOT_BYTES
            && self.features & !DVM_BLOCK_KNOWN_FEATURES == 0
            && self.features & DVM_BLOCK_REQUIRED_FEATURES == DVM_BLOCK_REQUIRED_FEATURES
            && self.generation != 0
            && self.capacity_sectors != 0
            // Capacity is counted in 512-byte sectors; every consumer derives
            // a byte capacity from it, so a product that overflows u64 is
            // never a real device.
            && self.capacity_sectors.checked_mul(512).is_some()
            && valid_block_size(self.logical_block_size)
            && valid_block_size(self.physical_block_size)
            && self.physical_block_size >= self.logical_block_size
            && self
                .physical_block_size
                .is_multiple_of(self.logical_block_size)
            && self.flags & !DVM_BLOCK_KNOWN_FLAGS == 0
            && (self.flags & DVM_BLOCK_FLAG_DVM_READY == 0
                || self.flags & DVM_BLOCK_FLAG_RUSTOS_READY != 0)
            && bounded_block_cursor_pair(self.request_producer, self.request_consumer)
            && bounded_block_cursor_pair(self.completion_producer, self.completion_consumer)
            && self.epoch_signature.iter().any(|byte| *byte != 0)
    }

    pub fn encode(self) -> [u8; DVM_BLOCK_HEADER_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_BLOCK_HEADER_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_BLOCK_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_BLOCK_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_BLOCK_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.queue_depth.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.data_slot_bytes.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.features.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.generation.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.capacity_sectors.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.logical_block_size.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.physical_block_size.to_le_bytes());
        bytes[64..68].copy_from_slice(&self.flags.to_le_bytes());
        bytes[72..80].copy_from_slice(&self.request_producer.to_le_bytes());
        bytes[80..88].copy_from_slice(&self.request_consumer.to_le_bytes());
        bytes[88..96].copy_from_slice(&self.completion_producer.to_le_bytes());
        bytes[96..104].copy_from_slice(&self.completion_consumer.to_le_bytes());
        bytes[104..168].copy_from_slice(&self.epoch_signature);
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_BLOCK_HEADER_RECORD_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_BLOCK_MAGIC
            || block_read_u32(bytes, 8)? != DVM_BLOCK_VERSION
            || block_read_u32(bytes, 12)? != DVM_BLOCK_HEADER_BYTES
            || bytes[68..72].iter().any(|byte| *byte != 0)
            || bytes[168..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let header = Self {
            region_bytes: block_read_u64(bytes, 16)?,
            queue_depth: block_read_u32(bytes, 24)?,
            data_slot_bytes: block_read_u32(bytes, 28)?,
            features: block_read_u64(bytes, 32)?,
            generation: block_read_u64(bytes, 40)?,
            capacity_sectors: block_read_u64(bytes, 48)?,
            logical_block_size: block_read_u32(bytes, 56)?,
            physical_block_size: block_read_u32(bytes, 60)?,
            flags: block_read_u32(bytes, 64)?,
            request_producer: block_read_u64(bytes, 72)?,
            request_consumer: block_read_u64(bytes, 80)?,
            completion_producer: block_read_u64(bytes, 88)?,
            completion_consumer: block_read_u64(bytes, 96)?,
            epoch_signature: bytes[104..168].try_into().ok()?,
        };
        header.is_valid().then_some(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DvmBlockOperation {
    Read = 0,
    Write = 1,
    Flush = 4,
    Discard = 11,
    WriteZeroes = 13,
}

impl DvmBlockOperation {
    pub(crate) const fn wire(self) -> u32 {
        self as u32
    }

    pub(crate) fn decode(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            4 => Some(Self::Flush),
            11 => Some(Self::Discard),
            13 => Some(Self::WriteZeroes),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmBlockRequest {
    pub generation: u64,
    pub request_id: u64,
    pub operation_id: u64,
    pub operation: DvmBlockOperation,
    pub flags: u32,
    pub data_slot: u32,
    pub sector: u64,
    pub data_len: u32,
}

impl DvmBlockRequest {
    pub fn is_valid_for(self, header: DvmBlockHeader) -> bool {
        if !header.is_valid()
            || header.flags & DVM_BLOCK_FLAG_DVM_READY == 0
            || self.generation != header.generation
            || self.request_id == 0
            || self.flags & !DVM_BLOCK_REQUEST_KNOWN_FLAGS != 0
        {
            return false;
        }

        let byte_range_valid = || {
            let logical_sectors = header.logical_block_size / 512;
            self.data_len != 0
                && self.data_len <= header.data_slot_bytes
                && self.data_len.is_multiple_of(header.logical_block_size)
                && self.data_slot < header.queue_depth
                && self.sector.is_multiple_of(u64::from(logical_sectors))
                && self
                    .sector
                    .checked_add(u64::from(self.data_len / 512))
                    .is_some_and(|end| end <= header.capacity_sectors)
        };

        match self.operation {
            DvmBlockOperation::Read => {
                self.operation_id == 0 && self.flags == 0 && byte_range_valid()
            }
            DvmBlockOperation::Write => {
                header.flags & DVM_BLOCK_FLAG_READ_ONLY == 0
                    && self.operation_id != 0
                    && self.flags & DVM_BLOCK_REQUEST_FLAG_UNMAP == 0
                    && (self.flags & DVM_BLOCK_REQUEST_FLAG_FUA == 0
                        || header.features & DVM_BLOCK_FEATURE_FUA != 0)
                    && byte_range_valid()
            }
            DvmBlockOperation::Flush => {
                self.operation_id != 0
                    && self.flags == 0
                    && self.data_slot < header.queue_depth
                    && self.sector == 0
                    && self.data_len == 0
                    && header.features & DVM_BLOCK_FEATURE_FLUSH != 0
            }
            DvmBlockOperation::Discard => {
                header.flags & DVM_BLOCK_FLAG_READ_ONLY == 0
                    && self.operation_id != 0
                    && self.flags == 0
                    && self.data_slot < header.queue_depth
                    && header.features & DVM_BLOCK_FEATURE_DISCARD != 0
                    && byte_range_valid()
            }
            DvmBlockOperation::WriteZeroes => {
                header.flags & DVM_BLOCK_FLAG_READ_ONLY == 0
                    && self.operation_id != 0
                    && self.flags & DVM_BLOCK_REQUEST_FLAG_FUA == 0
                    && self.data_slot < header.queue_depth
                    && header.features & DVM_BLOCK_FEATURE_WRITE_ZEROES != 0
                    && byte_range_valid()
            }
        }
    }

    pub fn encode(self) -> [u8; DVM_BLOCK_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_BLOCK_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.request_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.operation_id.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.operation.wire().to_le_bytes());
        bytes[28..32].copy_from_slice(&self.flags.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.data_slot.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.sector.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.data_len.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_BLOCK_RECORD_BYTES]) -> Option<Self> {
        if bytes[36..40].iter().any(|byte| *byte != 0) || bytes[52..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        Some(Self {
            generation: block_read_u64(bytes, 0)?,
            request_id: block_read_u64(bytes, 8)?,
            operation_id: block_read_u64(bytes, 16)?,
            operation: DvmBlockOperation::decode(block_read_u32(bytes, 24)?)?,
            flags: block_read_u32(bytes, 28)?,
            data_slot: block_read_u32(bytes, 32)?,
            sector: block_read_u64(bytes, 40)?,
            data_len: block_read_u32(bytes, 48)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DvmBlockCompletionStatus {
    Success = 0,
    IoError = 1,
    Unsupported = 2,
}

impl DvmBlockCompletionStatus {
    const fn wire(self) -> u32 {
        self as u32
    }

    fn decode(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Success),
            1 => Some(Self::IoError),
            2 => Some(Self::Unsupported),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmBlockCompletion {
    pub generation: u64,
    pub request_id: u64,
    pub operation_id: u64,
    pub status: DvmBlockCompletionStatus,
    pub data_slot: u32,
    pub completed_bytes: u32,
    pub durable_through_operation_id: u64,
}

impl DvmBlockCompletion {
    pub fn is_valid_for(self, header: DvmBlockHeader, request: DvmBlockRequest) -> bool {
        if !header.is_valid()
            || self.generation != header.generation
            || self.generation != request.generation
            || self.request_id != request.request_id
            || self.operation_id != request.operation_id
            || self.data_slot != request.data_slot
        {
            return false;
        }
        if self.status != DvmBlockCompletionStatus::Success {
            return self.completed_bytes == 0 && self.durable_through_operation_id == 0;
        }

        let completed_shape = match request.operation {
            DvmBlockOperation::Read | DvmBlockOperation::Write => {
                self.completed_bytes == request.data_len
            }
            DvmBlockOperation::Flush
            | DvmBlockOperation::Discard
            | DvmBlockOperation::WriteZeroes => self.completed_bytes == 0,
        };
        if !completed_shape {
            return false;
        }

        match request.operation {
            DvmBlockOperation::Read => self.durable_through_operation_id == 0,
            DvmBlockOperation::Write
                if request.flags & DVM_BLOCK_REQUEST_FLAG_FUA != 0
                    || header.features & DVM_BLOCK_FEATURE_WRITEBACK == 0 =>
            {
                self.durable_through_operation_id == request.operation_id
            }
            DvmBlockOperation::Flush => self.durable_through_operation_id == request.operation_id,
            DvmBlockOperation::Write
            | DvmBlockOperation::Discard
            | DvmBlockOperation::WriteZeroes => {
                self.durable_through_operation_id <= request.operation_id
            }
        }
    }

    pub fn encode(self) -> [u8; DVM_BLOCK_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_BLOCK_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&self.generation.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.request_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.operation_id.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.status.wire().to_le_bytes());
        bytes[28..32].copy_from_slice(&self.data_slot.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.completed_bytes.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.durable_through_operation_id.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_BLOCK_RECORD_BYTES]) -> Option<Self> {
        if bytes[36..40].iter().any(|byte| *byte != 0) || bytes[48..].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        Some(Self {
            generation: block_read_u64(bytes, 0)?,
            request_id: block_read_u64(bytes, 8)?,
            operation_id: block_read_u64(bytes, 16)?,
            status: DvmBlockCompletionStatus::decode(block_read_u32(bytes, 24)?)?,
            data_slot: block_read_u32(bytes, 28)?,
            completed_bytes: block_read_u32(bytes, 32)?,
            durable_through_operation_id: block_read_u64(bytes, 40)?,
        })
    }
}

pub(crate) fn valid_block_size(value: u32) -> bool {
    (512..=4096).contains(&value) && value.is_power_of_two()
}

pub(crate) fn bounded_block_cursor_pair(producer: u64, consumer: u64) -> bool {
    producer >= consumer && producer.saturating_sub(consumer) <= u64::from(DVM_BLOCK_QUEUE_DEPTH)
}

pub(crate) fn block_read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

pub(crate) fn block_read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}
