// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

/// Production GUI-DVM control messages are fixed-size and separate from the
/// host-provisioned pixel surfaces.  They are deliberately not a command
/// stream: a peer can only publish or release one of these known slots, or
/// advance a focus epoch on an authenticated control channel.
pub const DVM_GUI_SURFACE_MAGIC: [u8; 8] = *b"RSGUI001";
pub const DVM_GUI_SURFACE_VERSION: u32 = 1;
pub const DVM_GUI_SURFACE_SLOT_COUNT: u32 = 3;
pub const DVM_GUI_SURFACE_MESSAGE_BYTES: usize = 64;
pub const DVM_GUI_SURFACE_KIND_PRESENT: u32 = 1;
pub const DVM_GUI_SURFACE_KIND_RELEASE: u32 = 2;
pub const DVM_GUI_SURFACE_KIND_FOCUS: u32 = 3;

/// Fixed, host-created layout for the production GUI-DVM surface pool.  This
/// is intentionally distinct from the retired V2 single-frame aperture:
/// three equally sized pixel slots follow one read-mostly control page.
pub const DVM_GUI_SURFACE_POOL_MAGIC: [u8; 8] = *b"RSGUI002";
pub const DVM_GUI_SURFACE_POOL_VERSION: u32 = 2;
pub const DVM_GUI_SURFACE_SLOT_ALIGNMENT: u32 = 4096;
pub const DVM_GUI_SURFACE_POOL_HEADER_BYTES: u32 = 4096;
pub const DVM_GUI_SURFACE_POOL_RECORD_BYTES: usize = 64;
pub const DVM_GUI_SURFACE_POOL_HOST_RECORD_OFFSET: usize = 64;
pub const DVM_GUI_SURFACE_POOL_DVM_RECORD_OFFSET: usize = 256;
pub const DVM_GUI_SURFACE_POOL_DVM_SEQUENCE_OFFSET: usize = 320;
pub const DVM_GUI_SURFACE_POOL_HOST_ACK_OFFSET: usize = 328;
pub const DVM_GUI_SURFACE_POOL_INVITATION_OFFSET: usize = 336;
pub const DVM_GUI_SURFACE_POOL_READY_ACK_OFFSET: usize = 344;
/// Host-written echo of the exact readiness invitation it accepted. This is a
/// liveness confirmation only; it is not DVM attestation or trusted-UI proof.
pub const DVM_GUI_SURFACE_POOL_READY_CONFIRMATION_OFFSET: usize = 352;
pub const DVM_GUI_SURFACE_POOL_FLAG_READY: u32 = 1;
pub const DVM_GUI_SURFACE_POOL_KNOWN_FLAGS: u32 = DVM_GUI_SURFACE_POOL_FLAG_READY;
pub const DVM_GUI_SURFACE_POOL_PIXEL_FORMAT_BGRA8888: u32 = 1;
pub const DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL: u32 = 4;

/// The only GUI-DVM control operations admitted on the authenticated control
/// channel. Pixel bytes never travel in-band with these messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGuiSurfaceMessageKind {
    Present,
    Release,
    Focus,
}

impl DvmGuiSurfaceMessageKind {
    const fn wire(self) -> u32 {
        match self {
            Self::Present => DVM_GUI_SURFACE_KIND_PRESENT,
            Self::Release => DVM_GUI_SURFACE_KIND_RELEASE,
            Self::Focus => DVM_GUI_SURFACE_KIND_FOCUS,
        }
    }

    const fn decode(value: u32) -> Option<Self> {
        match value {
            DVM_GUI_SURFACE_KIND_PRESENT => Some(Self::Present),
            DVM_GUI_SURFACE_KIND_RELEASE => Some(Self::Release),
            DVM_GUI_SURFACE_KIND_FOCUS => Some(Self::Focus),
            _ => None,
        }
    }
}

/// A fixed GUI-DVM surface message. The host binds the channel to the exact
/// RustOS/GUI-DVM pair; this untrusted payload cannot name a memory address,
/// another domain, or a variable-length buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGuiSurfaceMessage {
    pub kind: DvmGuiSurfaceMessageKind,
    pub slot: u32,
    pub generation: u64,
    pub damage: DvmDisplayDamage,
    pub focus_epoch: u32,
}

impl DvmGuiSurfaceMessage {
    pub const fn present(slot: u32, generation: u64, damage: DvmDisplayDamage) -> Self {
        Self {
            kind: DvmGuiSurfaceMessageKind::Present,
            slot,
            generation,
            damage,
            focus_epoch: 0,
        }
    }

    pub const fn release(slot: u32, generation: u64) -> Self {
        Self {
            kind: DvmGuiSurfaceMessageKind::Release,
            slot,
            generation,
            damage: DvmDisplayDamage::full(),
            focus_epoch: 0,
        }
    }

    pub const fn focus(focus_epoch: u32) -> Self {
        Self {
            kind: DvmGuiSurfaceMessageKind::Focus,
            slot: 0,
            generation: 0,
            damage: DvmDisplayDamage::full(),
            focus_epoch,
        }
    }

    pub const fn encoded_len() -> usize {
        DVM_GUI_SURFACE_MESSAGE_BYTES
    }

    pub fn encode(self) -> [u8; DVM_GUI_SURFACE_MESSAGE_BYTES] {
        let mut bytes = [0_u8; DVM_GUI_SURFACE_MESSAGE_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GUI_SURFACE_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GUI_SURFACE_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.kind.wire().to_le_bytes());
        bytes[16..20].copy_from_slice(&self.slot.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_le_bytes());
        bytes[32..56].copy_from_slice(&self.damage.encode());
        bytes[56..60].copy_from_slice(&self.focus_epoch.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GUI_SURFACE_MAGIC
            || read_gui_u32(bytes, 8)? != DVM_GUI_SURFACE_VERSION
            || bytes[20..24].iter().any(|byte| *byte != 0)
            || bytes[60..64].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let damage_bytes: [u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES] = bytes[32..56].try_into().ok()?;
        Some(Self {
            kind: DvmGuiSurfaceMessageKind::decode(read_gui_u32(bytes, 12)?)?,
            slot: read_gui_u32(bytes, 16)?,
            generation: read_gui_u64(bytes, 24)?,
            damage: DvmDisplayDamage::decode(&damage_bytes),
            focus_epoch: read_gui_u32(bytes, 56)?,
        })
    }

    /// Validate fields after the host has authenticated the peer and selected
    /// its three surface capabilities. This is intentionally independent of a
    /// guest-provided length, pointer, or domain identifier.
    pub fn is_valid_for(self, display: DvmDisplayHeader) -> bool {
        self.is_valid_for_dimensions(display.width, display.height)
    }

    /// Validate this fixed record against a host-selected surface geometry.
    /// A returned record never chooses an address, slot count, or peer.
    pub fn is_valid_for_dimensions(self, width: u32, height: u32) -> bool {
        let display = DvmDisplayHeader::new(
            u64::from(DVM_DISPLAY_HEADER_BYTES).saturating_add(
                u64::from(width)
                    .saturating_mul(u64::from(height))
                    .saturating_mul(4),
            ),
            width,
            height,
            2,
        );
        match self.kind {
            DvmGuiSurfaceMessageKind::Present => {
                self.slot < DVM_GUI_SURFACE_SLOT_COUNT
                    && self.generation != 0
                    && self.focus_epoch == 0
                    && self.damage.is_valid_for(display)
            }
            DvmGuiSurfaceMessageKind::Release => {
                self.slot < DVM_GUI_SURFACE_SLOT_COUNT
                    && self.generation != 0
                    && self.focus_epoch == 0
                    && self.damage == DvmDisplayDamage::full()
            }
            DvmGuiSurfaceMessageKind::Focus => {
                self.slot == 0
                    && self.generation == 0
                    && self.focus_epoch != 0
                    && self.damage == DvmDisplayDamage::full()
            }
        }
    }
}

/// Host-created geometry and control-page bounds for the three-slot GUI-DVM
/// pool.  Pixel slots start at byte 4096; the only control records are three
/// host-to-DVM `PRESENT` records and one DVM-to-host `RELEASE`/`FOCUS` record.
/// The sequence/ack pair prevents a coalesced MSI-X notification from causing
/// a later return record to overwrite an earlier unconsumed return record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGuiSurfacePoolHeader {
    pub region_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub slot_bytes: u64,
    pub flags: u32,
}

impl DvmGuiSurfacePoolHeader {
    pub const fn encoded_len() -> usize {
        DVM_GUI_SURFACE_POOL_RECORD_BYTES
    }

    pub fn new(region_bytes: u64, width: u32, height: u32) -> Self {
        // Every slot starts on a page boundary so the DVM can export the
        // immutable snapshot as one DMA-BUF without granting access to an
        // adjacent slot. Since slot_bytes = stride * height, align the stride
        // by PAGE_SIZE / gcd(height, PAGE_SIZE).
        let height_gcd = gcd_u32(height.max(1), DVM_GUI_SURFACE_SLOT_ALIGNMENT);
        let stride_alignment = DVM_GUI_SURFACE_SLOT_ALIGNMENT / height_gcd;
        let packed_stride = width.saturating_mul(DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL);
        let stride_bytes = align_up_u32(packed_stride, stride_alignment);
        let slot_bytes = u64::from(stride_bytes).saturating_mul(u64::from(height));
        Self {
            region_bytes,
            width,
            height,
            stride_bytes,
            slot_bytes,
            flags: DVM_GUI_SURFACE_POOL_FLAG_READY,
        }
    }

    pub fn encode(self) -> [u8; DVM_GUI_SURFACE_POOL_RECORD_BYTES] {
        let mut bytes = [0_u8; DVM_GUI_SURFACE_POOL_RECORD_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GUI_SURFACE_POOL_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GUI_SURFACE_POOL_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&DVM_GUI_SURFACE_POOL_HEADER_BYTES.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.width.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.height.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.stride_bytes.to_le_bytes());
        bytes[36..40].copy_from_slice(&DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL.to_le_bytes());
        bytes[40..44].copy_from_slice(&DVM_GUI_SURFACE_POOL_PIXEL_FORMAT_BGRA8888.to_le_bytes());
        bytes[44..48].copy_from_slice(&DVM_GUI_SURFACE_SLOT_COUNT.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.slot_bytes.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.flags.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GUI_SURFACE_POOL_RECORD_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GUI_SURFACE_POOL_MAGIC
            || read_gui_u32(bytes, 8)? != DVM_GUI_SURFACE_POOL_VERSION
            || read_gui_u32(bytes, 12)? != DVM_GUI_SURFACE_POOL_HEADER_BYTES
            || read_gui_u32(bytes, 36)? != DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL
            || read_gui_u32(bytes, 40)? != DVM_GUI_SURFACE_POOL_PIXEL_FORMAT_BGRA8888
            || read_gui_u32(bytes, 44)? != DVM_GUI_SURFACE_SLOT_COUNT
            || bytes[60..64].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let header = Self {
            region_bytes: read_gui_u64(bytes, 16)?,
            width: read_gui_u32(bytes, 24)?,
            height: read_gui_u32(bytes, 28)?,
            stride_bytes: read_gui_u32(bytes, 32)?,
            slot_bytes: read_gui_u64(bytes, 48)?,
            flags: read_gui_u32(bytes, 56)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        if self.width == 0
            || self.height == 0
            || self.width > u16::MAX.into()
            || self.height > u16::MAX.into()
            || self.flags & DVM_GUI_SURFACE_POOL_FLAG_READY == 0
            || self.flags & !DVM_GUI_SURFACE_POOL_KNOWN_FLAGS != 0
            || self.stride_bytes
                < self
                    .width
                    .saturating_mul(DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL)
            || !self
                .stride_bytes
                .is_multiple_of(DVM_GUI_SURFACE_POOL_BYTES_PER_PIXEL)
            || self.slot_bytes
                != u64::from(self.stride_bytes).saturating_mul(u64::from(self.height))
            || !self
                .slot_bytes
                .is_multiple_of(u64::from(DVM_GUI_SURFACE_SLOT_ALIGNMENT))
        {
            return false;
        }
        let Some(pixel_bytes) = self
            .slot_bytes
            .checked_mul(u64::from(DVM_GUI_SURFACE_SLOT_COUNT))
        else {
            return false;
        };
        let Some(required) = u64::from(DVM_GUI_SURFACE_POOL_HEADER_BYTES).checked_add(pixel_bytes)
        else {
            return false;
        };
        required <= self.region_bytes
    }

    pub fn slot_offset(self, slot: u32) -> Option<u64> {
        if slot >= DVM_GUI_SURFACE_SLOT_COUNT {
            return None;
        }
        u64::from(DVM_GUI_SURFACE_POOL_HEADER_BYTES)
            .checked_add(self.slot_bytes.checked_mul(u64::from(slot))?)
    }
}

pub(crate) fn read_gui_u32(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

pub(crate) fn read_gui_u64(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}
