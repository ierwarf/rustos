// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

/// Fixed DVM-to-RustOS input frame parameters. The Linux relay cannot choose a
/// variable-length or native input ABI payload.
pub const RUSTOS_INPUT_FRAME_BYTES: usize = 32;
pub const RUSTOS_INPUT_MAGIC: [u8; 4] = *b"RDI1";
pub const RUSTOS_INPUT_VERSION: u8 = 3;
pub const RUSTOS_INPUT_KIND_SESSION_START: u8 = 0;
pub const RUSTOS_INPUT_KIND_KEY: u8 = 1;
pub const RUSTOS_INPUT_KIND_POINTER: u8 = 2;
pub const RUSTOS_INPUT_KIND_SESSION_END: u8 = 3;
pub const RUSTOS_INPUT_KIND_POINTER_POSITION: u8 = 4;
pub const LINUX_EVDEV_KEY_MAX: u16 = 0x02ff;
pub const RUSTOS_POINTER_BUTTON_MASK: u8 = 0x1f;
pub const RUSTOS_POINTER_POSITION_MAX_X: u16 = 2559;
pub const RUSTOS_POINTER_POSITION_MAX_Y: u16 = 1439;

/// L0 is the only producer of this fixed input ring. The Linux DVM sends
/// evdev records to L0 over its authenticated control channel; it never maps
/// this aperture. RustOS is the only consumer and acknowledges completed
/// records by advancing `consumer`.
///
/// A 16550 FIFO is not a suitable production data plane for a 125 Hz input
/// stream: it cannot preserve framed records while a policy service is
/// blocked. This ivshmem aperture is bounded,
/// launch-owned, and paired with one fixed MSI-X wake vector.
pub const DVM_INPUT_RING_MAGIC: [u8; 8] = *b"RSDVMIN1";
pub const DVM_INPUT_RING_VERSION: u32 = 2;
pub const DVM_INPUT_RING_HEADER_BYTES: u32 = 4096;
pub const DVM_INPUT_RING_RECORD_BYTES: usize = RUSTOS_INPUT_FRAME_BYTES;
pub const DVM_INPUT_RING_SLOT_COUNT: u32 = 2048;
/// One fixed cache line separates each mutable single-writer cursor.  L0
/// writes only `producer`; RustOS writes only `consumer`.  They must never
/// contend on one cache line under sustained pointer input.
pub const DVM_INPUT_RING_FLAGS_OFFSET: usize = 32;
pub const DVM_INPUT_RING_PRODUCER_OFFSET: usize = 64;
pub const DVM_INPUT_RING_CONSUMER_OFFSET: usize = 128;
/// Monotonic wake generation written only by RustOS after it has registered
/// the inputd waiter and before the authoritative producer recheck. L0 reads
/// it only after publishing a record and rings MSI-X once per new generation.
/// Keeping this beside the consumer cursor preserves single-writer cache-line
/// ownership while closing the stale empty-snapshot lost-wake race.
pub const DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET: usize = 136;
pub const DVM_INPUT_RING_ENCODED_BYTES: usize = 192;
pub const DVM_INPUT_RING_FLAG_READY: u32 = 1;
/// Set only by RustOS after it has validated the aperture and armed the one
/// MSI-X receive vector. L0 waits for this boot-time admission before asking
/// the DVM to emit input; it is not a data-plane poll or a DVM claim.
pub const DVM_INPUT_RING_FLAG_RUSTOS_READY: u32 = 1 << 1;
/// Set only after a real RustOS input client has completed a policy-backed
/// readiness query. Transport readiness alone is insufficient: starting the
/// DVM stream before `inputd` has a live consumer can fill the fixed ring
/// during boot even though MSI-X delivery is healthy.
pub const DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY: u32 = 1 << 2;
pub const DVM_INPUT_RING_KNOWN_FLAGS: u32 = DVM_INPUT_RING_FLAG_READY
    | DVM_INPUT_RING_FLAG_RUSTOS_READY
    | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY;
pub const DVM_INPUT_RING_MIN_REGION_BYTES: u64 = DVM_INPUT_RING_HEADER_BYTES as u64
    + DVM_INPUT_RING_SLOT_COUNT as u64 * DVM_INPUT_RING_RECORD_BYTES as u64;
/// The extra tail keeps the PCI BAR power-of-two sized without admitting
/// caller-selected records or a second ring.
pub const DVM_INPUT_RING_APERTURE_BYTES: u64 = 128 * 1024;

/// Immutable geometry plus the two monotonic, single-writer cursors for the
/// L0-to-RustOS input ring. This is deliberately a fixed-width wire record;
/// encode/decode rather than Rust layout is the ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmInputRingHeader {
    pub region_bytes: u64,
    pub flags: u32,
    pub producer: u64,
    pub consumer: u64,
    pub consumer_wake_generation: u64,
    pub generation: u64,
}

impl DvmInputRingHeader {
    pub const fn new(region_bytes: u64, generation: u64) -> Self {
        Self {
            region_bytes,
            flags: DVM_INPUT_RING_FLAG_READY,
            producer: 0,
            consumer: 0,
            consumer_wake_generation: 0,
            generation,
        }
    }

    pub const fn encoded_len() -> usize {
        DVM_INPUT_RING_ENCODED_BYTES
    }

    pub const fn records_offset() -> u64 {
        DVM_INPUT_RING_HEADER_BYTES as u64
    }

    pub fn record_offset(sequence: u64) -> u64 {
        Self::records_offset()
            + (sequence % DVM_INPUT_RING_SLOT_COUNT as u64) * DVM_INPUT_RING_RECORD_BYTES as u64
    }

    pub const fn is_valid(self) -> bool {
        self.region_bytes >= DVM_INPUT_RING_MIN_REGION_BYTES
            && self.region_bytes <= DVM_INPUT_RING_APERTURE_BYTES
            && self.flags & !DVM_INPUT_RING_KNOWN_FLAGS == 0
            && self.flags & DVM_INPUT_RING_FLAG_READY != 0
            && self.generation != 0
            && self.producer >= self.consumer
            && self.producer.saturating_sub(self.consumer) <= DVM_INPUT_RING_SLOT_COUNT as u64
    }

    pub fn encode(self) -> [u8; Self::encoded_len()] {
        let mut bytes = [0_u8; Self::encoded_len()];
        bytes[0..8].copy_from_slice(&DVM_INPUT_RING_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_INPUT_RING_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_INPUT_RING_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&DVM_INPUT_RING_SLOT_COUNT.to_le_bytes());
        bytes[28..32].copy_from_slice(&(DVM_INPUT_RING_RECORD_BYTES as u32).to_le_bytes());
        bytes[DVM_INPUT_RING_FLAGS_OFFSET..DVM_INPUT_RING_FLAGS_OFFSET + 4]
            .copy_from_slice(&self.flags.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.generation.to_le_bytes());
        bytes[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + 8]
            .copy_from_slice(&self.producer.to_le_bytes());
        bytes[DVM_INPUT_RING_CONSUMER_OFFSET..DVM_INPUT_RING_CONSUMER_OFFSET + 8]
            .copy_from_slice(&self.consumer.to_le_bytes());
        bytes[DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
            ..DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + 8]
            .copy_from_slice(&self.consumer_wake_generation.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::encoded_len()
            || bytes[0..8] != DVM_INPUT_RING_MAGIC
            || u32::from_le_bytes(bytes[8..12].try_into().ok()?) != DVM_INPUT_RING_VERSION
            || u32::from_le_bytes(bytes[12..16].try_into().ok()?) != DVM_INPUT_RING_HEADER_BYTES
            || u32::from_le_bytes(bytes[24..28].try_into().ok()?) != DVM_INPUT_RING_SLOT_COUNT
            || u32::from_le_bytes(bytes[28..32].try_into().ok()?)
                != DVM_INPUT_RING_RECORD_BYTES as u32
            || bytes[36..56].iter().any(|byte| *byte != 0)
            || bytes[64 + 8..DVM_INPUT_RING_CONSUMER_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
            || bytes
                [DVM_INPUT_RING_CONSUMER_OFFSET + 8..DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET]
                .iter()
                .any(|byte| *byte != 0)
            || bytes[DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + 8..]
                .iter()
                .any(|byte| *byte != 0)
        {
            return None;
        }
        let header = Self {
            region_bytes: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            flags: u32::from_le_bytes(
                bytes[DVM_INPUT_RING_FLAGS_OFFSET..DVM_INPUT_RING_FLAGS_OFFSET + 4]
                    .try_into()
                    .ok()?,
            ),
            producer: u64::from_le_bytes(
                bytes[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + 8]
                    .try_into()
                    .ok()?,
            ),
            consumer: u64::from_le_bytes(
                bytes[DVM_INPUT_RING_CONSUMER_OFFSET..DVM_INPUT_RING_CONSUMER_OFFSET + 8]
                    .try_into()
                    .ok()?,
            ),
            consumer_wake_generation: u64::from_le_bytes(
                bytes[DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
                    ..DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET + 8]
                    .try_into()
                    .ok()?,
            ),
            generation: u64::from_le_bytes(bytes[56..64].try_into().ok()?),
        };
        header.is_valid().then_some(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmInputFrameError {
    ZeroEpoch,
    ZeroSequence,
    InvalidKey,
    InvalidPointerButtons,
    InvalidPointerPosition,
}

#[cfg(not(kani))]
impl core::fmt::Display for DvmInputFrameError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::ZeroEpoch => "RustOS input relay epoch must be nonzero",
            Self::ZeroSequence => "RustOS input relay sequence must be nonzero",
            Self::InvalidKey => "invalid Linux evdev key frame",
            Self::InvalidPointerButtons => "invalid RustOS pointer buttons",
            Self::InvalidPointerPosition => "invalid RustOS absolute pointer position",
        })
    }
}

#[cfg(all(feature = "std", not(kani)))]
impl std::error::Error for DvmInputFrameError {}

/// A fixed, checksummed L0-to-RustOS input frame. Construction lives in the
/// shared no_std protocol crate so the host transport cannot drift from the
/// bounded wire contract exercised by the proof harnesses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustosInputFrame {
    bytes: [u8; RUSTOS_INPUT_FRAME_BYTES],
}

impl RustosInputFrame {
    pub fn session_start(epoch: u32) -> Result<Self, DvmInputFrameError> {
        if epoch == 0 {
            return Err(DvmInputFrameError::ZeroEpoch);
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_SESSION_START, epoch, 0);
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn session_end(epoch: u32, sequence: u32) -> Result<Self, DvmInputFrameError> {
        if epoch == 0 {
            return Err(DvmInputFrameError::ZeroEpoch);
        }
        if sequence == 0 {
            return Err(DvmInputFrameError::ZeroSequence);
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_SESSION_END, epoch, sequence);
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn linux_evdev_key(
        epoch: u32,
        sequence: u32,
        code: u16,
        value: u8,
    ) -> Result<Self, DvmInputFrameError> {
        if epoch == 0 {
            return Err(DvmInputFrameError::ZeroEpoch);
        }
        if sequence == 0 {
            return Err(DvmInputFrameError::ZeroSequence);
        }
        if code == 0 || code > LINUX_EVDEV_KEY_MAX || value > 2 {
            return Err(DvmInputFrameError::InvalidKey);
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_KEY, epoch, sequence);
        frame.bytes[16..18].copy_from_slice(&code.to_be_bytes());
        frame.bytes[18] = value;
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn linux_evdev_pointer(
        epoch: u32,
        sequence: u32,
        dx: i16,
        dy: i16,
        wheel_vertical: i16,
        wheel_horizontal: i16,
        buttons: u8,
    ) -> Result<Self, DvmInputFrameError> {
        if epoch == 0 {
            return Err(DvmInputFrameError::ZeroEpoch);
        }
        if sequence == 0 {
            return Err(DvmInputFrameError::ZeroSequence);
        }
        if buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
            return Err(DvmInputFrameError::InvalidPointerButtons);
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_POINTER, epoch, sequence);
        frame.bytes[16..18].copy_from_slice(&dx.to_be_bytes());
        frame.bytes[18..20].copy_from_slice(&dy.to_be_bytes());
        frame.bytes[20..22].copy_from_slice(&wheel_vertical.to_be_bytes());
        frame.bytes[22..24].copy_from_slice(&wheel_horizontal.to_be_bytes());
        frame.bytes[24] = buttons;
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn linux_evdev_pointer_position(
        epoch: u32,
        sequence: u32,
        x: u16,
        y: u16,
        wheel_vertical: i16,
        wheel_horizontal: i16,
        buttons: u8,
    ) -> Result<Self, DvmInputFrameError> {
        if epoch == 0 {
            return Err(DvmInputFrameError::ZeroEpoch);
        }
        if sequence == 0 {
            return Err(DvmInputFrameError::ZeroSequence);
        }
        if x > RUSTOS_POINTER_POSITION_MAX_X || y > RUSTOS_POINTER_POSITION_MAX_Y {
            return Err(DvmInputFrameError::InvalidPointerPosition);
        }
        if buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
            return Err(DvmInputFrameError::InvalidPointerButtons);
        }
        let mut frame = Self::new(RUSTOS_INPUT_KIND_POINTER_POSITION, epoch, sequence);
        frame.bytes[16..18].copy_from_slice(&x.to_be_bytes());
        frame.bytes[18..20].copy_from_slice(&y.to_be_bytes());
        frame.bytes[20..22].copy_from_slice(&wheel_vertical.to_be_bytes());
        frame.bytes[22..24].copy_from_slice(&wheel_horizontal.to_be_bytes());
        frame.bytes[24] = buttons;
        frame.finish_checksum();
        Ok(frame)
    }

    pub fn as_bytes(&self) -> &[u8; RUSTOS_INPUT_FRAME_BYTES] {
        &self.bytes
    }

    fn new(kind: u8, epoch: u32, sequence: u32) -> Self {
        let mut bytes = [0_u8; RUSTOS_INPUT_FRAME_BYTES];
        bytes[..4].copy_from_slice(&RUSTOS_INPUT_MAGIC);
        bytes[4] = RUSTOS_INPUT_VERSION;
        bytes[5] = kind;
        bytes[8..12].copy_from_slice(&epoch.to_be_bytes());
        bytes[12..16].copy_from_slice(&sequence.to_be_bytes());
        Self { bytes }
    }

    fn finish_checksum(&mut self) {
        let checksum = crc32(&self.bytes[..28]).to_be_bytes();
        self.bytes[28..32].copy_from_slice(&checksum);
    }
}

pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg() & 0xedb8_8320;
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
}
