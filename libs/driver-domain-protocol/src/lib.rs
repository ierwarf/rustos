#![no_std]

//! Fixed, host-initialized memory contracts for isolated driver domains.
//!
//! This crate deliberately describes only byte layouts and validation. Device
//! policy stays in ring3/L0; the kernel may map a known transport only after
//! the header has passed these bounded checks.

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
pub const DVM_NET_MIN_REGION_BYTES: u64 = DVM_NET_HEADER_BYTES as u64
    + 2 * DVM_NET_SLOT_COUNT as u64 * DVM_NET_SLOT_BYTES as u64;
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
            || !self.stride_bytes.is_multiple_of(DVM_DISPLAY_BYTES_PER_PIXEL)
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

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::{
        DVM_DISPLAY_HEADER_BYTES, DVM_NET_APERTURE_BYTES, DVM_NET_FLAG_DVM_READY,
        DVM_NET_MIN_REGION_BYTES,
        DvmDisplayHeader, DvmNetHeader,
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
        assert_eq!(header.rx_ring_offset(), Some(DVM_NET_MIN_REGION_BYTES / 2 + 2048));
        assert!(!header.dvm_ready());
        let ready = DvmNetHeader { flags: header.flags | DVM_NET_FLAG_DVM_READY, ..header };
        assert!(ready.is_valid());
        assert!(ready.dvm_ready());
        assert!(DVM_NET_APERTURE_BYTES >= DVM_NET_MIN_REGION_BYTES);
        assert!(DVM_NET_APERTURE_BYTES.is_power_of_two());
    }
}
