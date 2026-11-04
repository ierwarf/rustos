// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

pub const DVM_DISPLAY_MAGIC: [u8; 8] = *b"RSDVMFB1";
pub const DVM_DISPLAY_VERSION: u32 = 2;
pub const DVM_DISPLAY_HEADER_BYTES: u32 = 4096;
pub const DVM_DISPLAY_RECORD_BYTES: usize = 64;
pub const DVM_DISPLAY_GENERATION_OFFSET: usize = 56;
/// The damage record lives in the otherwise reserved, host-initialized header
/// page. It is bracketed by the display generation seqlock rather than being a
/// DVM-selected command stream.
pub const DVM_DISPLAY_DAMAGE_OFFSET: usize = 64;
pub const DVM_DISPLAY_DAMAGE_RECORD_BYTES: usize = 24;
/// Host-written invitation generation for the fixed GUI-DVM readiness
/// handshake. It lives in the validated header page, not inside pixel memory.
pub const DVM_DISPLAY_INVITATION_GENERATION_OFFSET: usize = 96;
/// DVM-module-written acknowledgement of exactly one host invitation. RustOS
/// accepts the readiness MSI-X vector only after this value matches its current
/// expected invitation generation; a late ready doorbell cannot resurrect a
/// newer relay instance.
pub const DVM_DISPLAY_READY_ACK_GENERATION_OFFSET: usize = 104;
pub const DVM_DISPLAY_DAMAGE_FULL: u32 = 1;
pub const DVM_DISPLAY_BYTES_PER_PIXEL: u32 = 4;
pub const DVM_DISPLAY_PIXEL_FORMAT_BGRA8888: u32 = 1;
/// The host has installed its MSI-X receive entry and may now ask the DVM
/// relay to prove that KMS is live. This is host-written state, never a DVM
/// claim or an authentication result.
pub const DVM_DISPLAY_FLAG_HOST_ARMED: u32 = 1 << 1;
/// The host observed the fixed DVM readiness doorbell through its own MSI-X
/// entry. The DVM may use this only as an availability acknowledgement; it
/// does not attest the DVM, scanout, or input path.
pub const DVM_DISPLAY_FLAG_PEER_READY: u32 = 1 << 2;
pub const DVM_DISPLAY_FLAG_READY: u32 = 1;
pub const DVM_DISPLAY_KNOWN_FLAGS: u32 =
    DVM_DISPLAY_FLAG_READY | DVM_DISPLAY_FLAG_HOST_ARMED | DVM_DISPLAY_FLAG_PEER_READY;
/// Generation values are a seqlock: an even non-zero value is a complete
/// frame; an odd value means the RustOS producer is copying a new frame.
pub const DVM_DISPLAY_INITIAL_GENERATION: u64 = 2;

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

/// Exact damage metadata for one stable DVM display generation. A full frame
/// is explicit; a partial rectangle must stay inside the fixed display bounds.
/// This lets the Linux DRM relay avoid copying a full 1600x900 aperture for a
/// cursor-sized update into an uncached dumb buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmDisplayDamage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub flags: u32,
}

impl DvmDisplayDamage {
    pub const fn full() -> Self {
        Self {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            flags: DVM_DISPLAY_DAMAGE_FULL,
        }
    }

    pub const fn rect(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
            flags: 0,
        }
    }

    pub fn encode(self) -> [u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES];
        bytes[0..4].copy_from_slice(&self.x.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.y.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.width.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.height.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.flags.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES]) -> Self {
        Self {
            x: read_damage_u32(bytes, 0),
            y: read_damage_u32(bytes, 4),
            width: read_damage_u32(bytes, 8),
            height: read_damage_u32(bytes, 12),
            flags: read_damage_u32(bytes, 16),
        }
    }

    pub fn is_valid_for(self, display: DvmDisplayHeader) -> bool {
        self.is_valid_for_dimensions(display.width, display.height)
    }

    pub fn is_valid_for_dimensions(self, width: u32, height: u32) -> bool {
        if self.flags == DVM_DISPLAY_DAMAGE_FULL {
            return self.x == 0 && self.y == 0 && self.width == 0 && self.height == 0;
        }
        if self.flags != 0 || self.width == 0 || self.height == 0 {
            return false;
        }
        self.x
            .checked_add(self.width)
            .is_some_and(|end| end <= width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|end| end <= height)
    }
}

pub(crate) fn read_damage_u32(bytes: &[u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
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
        if self.flags & DVM_DISPLAY_FLAG_READY == 0
            || self.flags & !DVM_DISPLAY_KNOWN_FLAGS != 0
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

pub(crate) fn read_u32(bytes: &[u8; DVM_DISPLAY_RECORD_BYTES], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

pub(crate) fn read_u64(bytes: &[u8; DVM_DISPLAY_RECORD_BYTES], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}
