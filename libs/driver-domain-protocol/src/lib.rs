#![no_std]

//! Fixed, host-initialized memory contracts for isolated driver domains.
//!
//! This crate deliberately describes only byte layouts and validation. Device
//! policy stays in ring3/L0; the kernel may map a known transport only after
//! the header has passed these bounded checks.

#[cfg(all(feature = "std", not(kani)))]
extern crate std;

pub const DVM_DISPLAY_MAGIC: [u8; 8] = *b"RSDVMFB1";
pub const DVM_DISPLAY_VERSION: u32 = 1;
pub const DVM_DISPLAY_HEADER_BYTES: u32 = 4096;
pub const DVM_DISPLAY_RECORD_BYTES: usize = 64;
pub const DVM_DISPLAY_BYTES_PER_PIXEL: u32 = 4;
pub const DVM_DISPLAY_PIXEL_FORMAT_BGRA8888: u32 = 1;
pub const DVM_DISPLAY_FLAG_READY: u32 = 1;
pub const DVM_DISPLAY_KNOWN_FLAGS: u32 = DVM_DISPLAY_FLAG_READY;
/// Generation values are a seqlock: an even non-zero value is a complete
/// frame; an odd value means the RustOS producer is copying a new frame.
pub const DVM_DISPLAY_INITIAL_GENERATION: u64 = 2;

/// Fixed DVM-to-RustOS input frame parameters. The Linux relay cannot choose a
/// variable-length or native input ABI payload.
pub const RUSTOS_INPUT_FRAME_BYTES: usize = 32;
pub const RUSTOS_INPUT_MAGIC: [u8; 4] = *b"RDI1";
pub const RUSTOS_INPUT_VERSION: u8 = 2;
pub const RUSTOS_INPUT_KIND_SESSION_START: u8 = 0;
pub const RUSTOS_INPUT_KIND_KEY: u8 = 1;
pub const RUSTOS_INPUT_KIND_POINTER: u8 = 2;
pub const RUSTOS_INPUT_KIND_SESSION_END: u8 = 3;
pub const LINUX_EVDEV_KEY_MAX: u16 = 0x02ff;
pub const RUSTOS_POINTER_BUTTON_MASK: u8 = 0x1f;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmInputFrameError {
    ZeroEpoch,
    ZeroSequence,
    InvalidKey,
    InvalidPointerButtons,
}

#[cfg(not(kani))]
impl core::fmt::Display for DvmInputFrameError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::ZeroEpoch => "RustOS input relay epoch must be nonzero",
            Self::ZeroSequence => "RustOS input relay sequence must be nonzero",
            Self::InvalidKey => "invalid Linux evdev key frame",
            Self::InvalidPointerButtons => "invalid RustOS pointer buttons",
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

fn crc32(bytes: &[u8]) -> u32 {
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

/// A fixed two-ring Ethernet transport. RustOS is the producer of `tx` and
/// consumer of `rx`; the Linux DVM has the inverse ownership. No peer may
/// select an address, descriptor chain, or allocation outside this aperture.
pub const DVM_NET_MAGIC: [u8; 8] = *b"RSDVMNT1";
pub const DVM_NET_VERSION: u32 = 1;
pub const DVM_NET_HEADER_BYTES: u32 = 4096;
pub const DVM_NET_RECORD_BYTES: usize = 64;
pub const DVM_NET_SLOT_COUNT: u32 = 64;
pub const DVM_NET_SLOT_BYTES: u32 = 2048;
pub const DVM_NET_MTU: u32 = 1514;
/// Set only by the host when it has created a valid, fixed-layout aperture.
pub const DVM_NET_FLAG_READY: u32 = 1;
/// Set by the Linux relay only after it has opened both the aperture and its
/// DVM-owned NIC. RustOS must not regard the transport as usable before this.
pub const DVM_NET_FLAG_DVM_READY: u32 = 1 << 1;
pub const DVM_NET_KNOWN_FLAGS: u32 = DVM_NET_FLAG_READY | DVM_NET_FLAG_DVM_READY;
pub const DVM_NET_MIN_REGION_BYTES: u64 =
    DVM_NET_HEADER_BYTES as u64 + 2 * DVM_NET_SLOT_COUNT as u64 * DVM_NET_SLOT_BYTES as u64;
/// PCI BARs must be powers of two. This leaves the tail reserved while keeping
/// the two fixed rings bounded to `DVM_NET_MIN_REGION_BYTES`.
pub const DVM_NET_APERTURE_BYTES: u64 = 512 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmNetHeader {
    pub region_bytes: u64,
    pub slot_count: u32,
    pub slot_bytes: u32,
    pub mtu: u32,
    pub flags: u32,
    pub generation: u64,
}

impl DvmNetHeader {
    pub const fn new(region_bytes: u64, generation: u64) -> Self {
        Self {
            region_bytes,
            slot_count: DVM_NET_SLOT_COUNT,
            slot_bytes: DVM_NET_SLOT_BYTES,
            mtu: DVM_NET_MTU,
            flags: DVM_NET_FLAG_READY,
            generation,
        }
    }

    pub const fn encoded_len() -> usize {
        DVM_NET_RECORD_BYTES
    }

    pub const fn ring_bytes(self) -> u64 {
        self.slot_count as u64 * self.slot_bytes as u64
    }

    pub const fn tx_ring_offset() -> u64 {
        DVM_NET_HEADER_BYTES as u64
    }

    pub fn rx_ring_offset(self) -> Option<u64> {
        Self::tx_ring_offset().checked_add(self.ring_bytes())
    }

    pub fn encode(self) -> [u8; DVM_NET_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_NET_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_NET_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_NET_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_NET_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.slot_count.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.slot_bytes.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.mtu.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.flags.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_NET_RECORD_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_NET_MAGIC
            || read_u32(bytes, 8)? != DVM_NET_VERSION
            || read_u32(bytes, 12)? != DVM_NET_HEADER_BYTES
        {
            return None;
        }
        let header = Self {
            region_bytes: read_u64(bytes, 16)?,
            slot_count: read_u32(bytes, 24)?,
            slot_bytes: read_u32(bytes, 28)?,
            mtu: read_u32(bytes, 32)?,
            flags: read_u32(bytes, 36)?,
            generation: read_u64(bytes, 56)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        if self.flags & DVM_NET_FLAG_READY == 0
            || self.flags & !DVM_NET_KNOWN_FLAGS != 0
            || self.generation == 0
            || self.slot_count != DVM_NET_SLOT_COUNT
            || self.slot_bytes != DVM_NET_SLOT_BYTES
            || self.mtu != DVM_NET_MTU
            || self.slot_bytes <= self.mtu
        {
            return false;
        }
        self.region_bytes >= DVM_NET_MIN_REGION_BYTES
    }

    pub const fn dvm_ready(self) -> bool {
        self.flags & DVM_NET_FLAG_DVM_READY != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmDisplayHeader {
    pub region_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub frame_bytes: u64,
    pub generation: u64,
    pub flags: u32,
}

impl DvmDisplayHeader {
    pub const fn new(region_bytes: u64, width: u32, height: u32, generation: u64) -> Self {
        let stride_bytes = width.saturating_mul(DVM_DISPLAY_BYTES_PER_PIXEL);
        let frame_bytes = (stride_bytes as u64).saturating_mul(height as u64);
        Self {
            region_bytes,
            width,
            height,
            stride_bytes,
            frame_bytes,
            generation,
            flags: DVM_DISPLAY_FLAG_READY,
        }
    }

    pub const fn encoded_len() -> usize {
        DVM_DISPLAY_RECORD_BYTES
    }

    pub fn encode(self) -> [u8; DVM_DISPLAY_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_DISPLAY_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_DISPLAY_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_DISPLAY_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_DISPLAY_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.width.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.height.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.stride_bytes.to_le_bytes());
        bytes[36..40].copy_from_slice(&DVM_DISPLAY_BYTES_PER_PIXEL.to_le_bytes());
        bytes[40..44].copy_from_slice(&DVM_DISPLAY_PIXEL_FORMAT_BGRA8888.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.flags.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.frame_bytes.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.generation.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_DISPLAY_RECORD_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_DISPLAY_MAGIC
            || read_u32(bytes, 8)? != DVM_DISPLAY_VERSION
            || read_u32(bytes, 12)? != DVM_DISPLAY_HEADER_BYTES
            || read_u32(bytes, 36)? != DVM_DISPLAY_BYTES_PER_PIXEL
            || read_u32(bytes, 40)? != DVM_DISPLAY_PIXEL_FORMAT_BGRA8888
        {
            return None;
        }
        let header = Self {
            region_bytes: read_u64(bytes, 16)?,
            width: read_u32(bytes, 24)?,
            height: read_u32(bytes, 28)?,
            stride_bytes: read_u32(bytes, 32)?,
            flags: read_u32(bytes, 44)?,
            frame_bytes: read_u64(bytes, 48)?,
            generation: read_u64(bytes, 56)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        if self.flags != DVM_DISPLAY_FLAG_READY
            || self.width == 0
            || self.height == 0
            || self.generation == 0
            || self.stride_bytes < self.width.saturating_mul(DVM_DISPLAY_BYTES_PER_PIXEL)
            || !self
                .stride_bytes
                .is_multiple_of(DVM_DISPLAY_BYTES_PER_PIXEL)
        {
            return false;
        }
        let Some(frame_bytes) = (self.stride_bytes as u64).checked_mul(self.height as u64) else {
            return false;
        };
        if frame_bytes != self.frame_bytes {
            return false;
        }
        let Some(required) = u64::from(DVM_DISPLAY_HEADER_BYTES).checked_add(frame_bytes) else {
            return false;
        };
        self.region_bytes >= required
    }
}

fn read_u32(bytes: &[u8; DVM_DISPLAY_RECORD_BYTES], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

fn read_u64(bytes: &[u8; DVM_DISPLAY_RECORD_BYTES], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}

#[cfg(kani)]
mod verification {
    use super::{
        LINUX_EVDEV_KEY_MAX, RUSTOS_INPUT_FRAME_BYTES, RUSTOS_INPUT_KIND_KEY,
        RUSTOS_INPUT_KIND_POINTER, RUSTOS_INPUT_MAGIC, RUSTOS_INPUT_VERSION,
        RUSTOS_POINTER_BUTTON_MASK, RustosInputFrame,
    };

    fn assert_header(bytes: &[u8; RUSTOS_INPUT_FRAME_BYTES], kind: u8, epoch: u32, sequence: u32) {
        assert!(bytes[0] == RUSTOS_INPUT_MAGIC[0]);
        assert!(bytes[1] == RUSTOS_INPUT_MAGIC[1]);
        assert!(bytes[2] == RUSTOS_INPUT_MAGIC[2]);
        assert!(bytes[3] == RUSTOS_INPUT_MAGIC[3]);
        assert!(bytes[4] == RUSTOS_INPUT_VERSION);
        assert!(bytes[5] == kind);
        let epoch_bytes = epoch.to_be_bytes();
        assert!(bytes[8] == epoch_bytes[0]);
        assert!(bytes[9] == epoch_bytes[1]);
        assert!(bytes[10] == epoch_bytes[2]);
        assert!(bytes[11] == epoch_bytes[3]);
        let sequence_bytes = sequence.to_be_bytes();
        assert!(bytes[12] == sequence_bytes[0]);
        assert!(bytes[13] == sequence_bytes[1]);
        assert!(bytes[14] == sequence_bytes[2]);
        assert!(bytes[15] == sequence_bytes[3]);
    }

    #[kani::proof]
    fn key_frame_has_exact_provenance_bounds_and_checksum() {
        let epoch: u32 = kani::any();
        let sequence: u32 = kani::any();
        let code: u16 = kani::any();
        let value: u8 = kani::any();
        let result = RustosInputFrame::linux_evdev_key(epoch, sequence, code, value);

        if epoch == 0 || sequence == 0 || code == 0 || code > LINUX_EVDEV_KEY_MAX || value > 2 {
            assert!(result.is_err());
        } else if let Ok(frame) = result {
            let bytes = frame.as_bytes();
            assert_header(bytes, RUSTOS_INPUT_KIND_KEY, epoch, sequence);
            let code_bytes = code.to_be_bytes();
            assert!(bytes[16] == code_bytes[0]);
            assert!(bytes[17] == code_bytes[1]);
            assert!(bytes[18] == value);
            if epoch == 7 && sequence == 3 && code == 30 && value == 1 {
                assert!(bytes[28] == 0xc1);
                assert!(bytes[29] == 0xd7);
                assert!(bytes[30] == 0x7c);
                assert!(bytes[31] == 0x0a);
            }
        } else {
            assert!(false);
        }
    }

    #[kani::proof]
    fn pointer_frame_rejects_unknown_button_bits_and_preserves_wire_fields() {
        let epoch: u32 = kani::any();
        let sequence: u32 = kani::any();
        let dx: i16 = kani::any();
        let dy: i16 = kani::any();
        let wheel_vertical: i16 = kani::any();
        let wheel_horizontal: i16 = kani::any();
        let buttons: u8 = kani::any();
        let result = RustosInputFrame::linux_evdev_pointer(
            epoch,
            sequence,
            dx,
            dy,
            wheel_vertical,
            wheel_horizontal,
            buttons,
        );

        if epoch == 0 || sequence == 0 || buttons & !RUSTOS_POINTER_BUTTON_MASK != 0 {
            assert!(result.is_err());
        } else if let Ok(frame) = result {
            let bytes = frame.as_bytes();
            assert_header(bytes, RUSTOS_INPUT_KIND_POINTER, epoch, sequence);
            let dx_bytes = dx.to_be_bytes();
            assert!(bytes[16] == dx_bytes[0]);
            assert!(bytes[17] == dx_bytes[1]);
            let dy_bytes = dy.to_be_bytes();
            assert!(bytes[18] == dy_bytes[0]);
            assert!(bytes[19] == dy_bytes[1]);
            let wheel_vertical_bytes = wheel_vertical.to_be_bytes();
            assert!(bytes[20] == wheel_vertical_bytes[0]);
            assert!(bytes[21] == wheel_vertical_bytes[1]);
            let wheel_horizontal_bytes = wheel_horizontal.to_be_bytes();
            assert!(bytes[22] == wheel_horizontal_bytes[0]);
            assert!(bytes[23] == wheel_horizontal_bytes[1]);
            assert!(bytes[24] == buttons);
            if epoch == 9
                && sequence == 4
                && dx == -4
                && dy == 2
                && wheel_vertical == 1
                && wheel_horizontal == 0
                && buttons == 3
            {
                assert!(bytes[28] == 0xb7);
                assert!(bytes[29] == 0xd3);
                assert!(bytes[30] == 0x37);
                assert!(bytes[31] == 0x18);
            }
        } else {
            assert!(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DVM_DISPLAY_HEADER_BYTES, DVM_NET_APERTURE_BYTES, DVM_NET_FLAG_DVM_READY,
        DVM_NET_MIN_REGION_BYTES, DvmDisplayHeader, DvmNetHeader,
    };

    #[test]
    fn round_trip_is_fixed_width_and_validated() {
        let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
        assert!(header.is_valid());
        assert_eq!(header.encode().len(), DvmDisplayHeader::encoded_len());
        assert_eq!(DvmDisplayHeader::decode(&header.encode()), Some(header));
    }

    #[test]
    fn rejects_unready_or_truncated_regions() {
        let mut header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
        header.flags = 0;
        assert!(!header.is_valid());

        let mut encoded = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1).encode();
        encoded[12..16].copy_from_slice(&(DVM_DISPLAY_HEADER_BYTES - 1).to_le_bytes());
        assert!(DvmDisplayHeader::decode(&encoded).is_none());
    }

    #[test]
    fn net_contract_has_two_bounded_fixed_rings() {
        let header = DvmNetHeader::new(DVM_NET_MIN_REGION_BYTES, 1);
        assert!(header.is_valid());
        assert_eq!(DvmNetHeader::decode(&header.encode()), Some(header));
        assert_eq!(
            header.rx_ring_offset(),
            Some(DVM_NET_MIN_REGION_BYTES / 2 + 2048)
        );
        assert!(!header.dvm_ready());
        let ready = DvmNetHeader {
            flags: header.flags | DVM_NET_FLAG_DVM_READY,
            ..header
        };
        assert!(ready.is_valid());
        assert!(ready.dvm_ready());
        assert!(DVM_NET_APERTURE_BYTES >= DVM_NET_MIN_REGION_BYTES);
        assert!(DVM_NET_APERTURE_BYTES.is_power_of_two());
    }
}
