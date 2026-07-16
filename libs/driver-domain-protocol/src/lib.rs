#![no_std]

//! Fixed, host-initialized memory contracts for isolated driver domains.
//!
//! This crate deliberately describes only byte layouts and validation. Device
//! policy stays in ring3/L0; the kernel may map a known transport only after
//! the header has passed these bounded checks.

#[cfg(all(feature = "std", not(kani)))]
extern crate std;

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

fn gcd_u32(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

fn align_up_u32(value: u32, alignment: u32) -> u32 {
    debug_assert!(alignment != 0);
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded / alignment * alignment)
        .unwrap_or(u32::MAX)
}

fn read_gui_u32(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

fn read_gui_u64(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}

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
pub const RUSTOS_POINTER_POSITION_MAX_X: u16 = 1599;
pub const RUSTOS_POINTER_POSITION_MAX_Y: u16 = 899;

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
pub const DVM_INPUT_RING_VERSION: u32 = 1;
pub const DVM_INPUT_RING_HEADER_BYTES: u32 = 4096;
pub const DVM_INPUT_RING_RECORD_BYTES: usize = RUSTOS_INPUT_FRAME_BYTES;
pub const DVM_INPUT_RING_SLOT_COUNT: u32 = 2048;
/// One fixed cache line separates each mutable single-writer cursor.  L0
/// writes only `producer`; RustOS writes only `consumer`.  They must never
/// contend on one cache line under sustained pointer input.
pub const DVM_INPUT_RING_FLAGS_OFFSET: usize = 32;
pub const DVM_INPUT_RING_PRODUCER_OFFSET: usize = 64;
pub const DVM_INPUT_RING_CONSUMER_OFFSET: usize = 128;
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
    pub generation: u64,
}

impl DvmInputRingHeader {
    pub const fn new(region_bytes: u64, generation: u64) -> Self {
        Self {
            region_bytes,
            flags: DVM_INPUT_RING_FLAG_READY,
            producer: 0,
            consumer: 0,
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
            || bytes[DVM_INPUT_RING_CONSUMER_OFFSET + 8..]
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

const ETHERNET_HEADER_BYTES: usize = 14;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const IPV4_MIN_HEADER_BYTES: usize = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EthernetFrameError {
    Length,
    UnsupportedEtherType,
    InvalidArp,
    InvalidIpv4,
    InvalidIpv4Checksum,
    FragmentedIpv4,
}

/// Validate the complete Ethernet payload accepted by the enabled RustOS
/// network topology.  The netd stack is IPv4-only, so admitting an unknown
/// EtherType or fragmented datagram would create an unmodelled payload path.
/// Ethernet padding is allowed after the IPv4 total length.
pub fn validate_dvm_ethernet_frame(frame: &[u8]) -> Result<(), EthernetFrameError> {
    if !(ETHERNET_HEADER_BYTES..=DVM_NET_MTU as usize).contains(&frame.len()) {
        return Err(EthernetFrameError::Length);
    }
    let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    match ethertype {
        ETHERTYPE_ARP => validate_arp_payload(&frame[ETHERNET_HEADER_BYTES..]),
        ETHERTYPE_IPV4 => validate_ipv4_payload(&frame[ETHERNET_HEADER_BYTES..]),
        _ => Err(EthernetFrameError::UnsupportedEtherType),
    }
}

fn validate_arp_payload(payload: &[u8]) -> Result<(), EthernetFrameError> {
    if payload.len() < 28
        || payload[0..2] != 1_u16.to_be_bytes()
        || payload[2..4] != ETHERTYPE_IPV4.to_be_bytes()
        || payload[4] != 6
        || payload[5] != 4
        || !matches!(u16::from_be_bytes([payload[6], payload[7]]), 1 | 2)
    {
        return Err(EthernetFrameError::InvalidArp);
    }
    Ok(())
}

fn validate_ipv4_payload(payload: &[u8]) -> Result<(), EthernetFrameError> {
    if payload.len() < IPV4_MIN_HEADER_BYTES || payload[0] >> 4 != 4 {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let header_len = usize::from(payload[0] & 0x0f) * 4;
    if header_len < IPV4_MIN_HEADER_BYTES || header_len > payload.len() {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let total_len = usize::from(u16::from_be_bytes([payload[2], payload[3]]));
    if total_len < header_len || total_len > payload.len() {
        return Err(EthernetFrameError::InvalidIpv4);
    }
    let fragment = u16::from_be_bytes([payload[6], payload[7]]);
    if fragment & 0x3fff != 0 {
        return Err(EthernetFrameError::FragmentedIpv4);
    }
    let mut sum = 0_u32;
    for chunk in payload[..header_len].chunks_exact(2) {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
        sum = (sum & 0xffff) + (sum >> 16);
    }
    if sum != 0xffff {
        return Err(EthernetFrameError::InvalidIpv4Checksum);
    }
    Ok(())
}

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

fn read_damage_u32(bytes: &[u8; DVM_DISPLAY_DAMAGE_RECORD_BYTES], offset: usize) -> u32 {
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
                assert!(bytes[28] == 0x40);
                assert!(bytes[29] == 0xf2);
                assert!(bytes[30] == 0x19);
                assert!(bytes[31] == 0x2d);
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
                assert!(bytes[28] == 0x36);
                assert!(bytes[29] == 0xf6);
                assert!(bytes[30] == 0x52);
                assert!(bytes[31] == 0x3f);
            }
        } else {
            assert!(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DVM_DISPLAY_FLAG_HOST_ARMED, DVM_DISPLAY_FLAG_READY, DVM_DISPLAY_HEADER_BYTES,
        DVM_DISPLAY_INVITATION_GENERATION_OFFSET, DVM_DISPLAY_READY_ACK_GENERATION_OFFSET,
        DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_CONSUMER_OFFSET,
        DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY, DVM_INPUT_RING_FLAG_RUSTOS_READY,
        DVM_INPUT_RING_PRODUCER_OFFSET, DVM_NET_APERTURE_BYTES, DVM_NET_FLAG_DVM_READY,
        DVM_NET_MIN_REGION_BYTES, DvmDisplayDamage, DvmDisplayHeader, DvmGuiSurfaceMessage,
        DvmInputRingHeader, DvmNetHeader, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrameError,
        RUSTOS_INPUT_KIND_POINTER_POSITION, RUSTOS_INPUT_VERSION, RustosInputFrame,
        validate_dvm_ethernet_frame,
    };

    #[test]
    fn round_trip_is_fixed_width_and_validated() {
        let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
        assert!(header.is_valid());
        assert_eq!(header.encode().len(), DvmDisplayHeader::encoded_len());
        assert_eq!(DvmDisplayHeader::decode(&header.encode()), Some(header));
    }

    #[test]
    fn display_damage_is_explicit_and_bounds_checked() {
        let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 2);
        let full = DvmDisplayDamage::full();
        assert_eq!(DvmDisplayDamage::decode(&full.encode()), full);
        assert!(full.is_valid_for(header));
        assert!(DvmDisplayDamage::rect(1599, 899, 1, 1).is_valid_for(header));
        assert!(!DvmDisplayDamage::rect(1599, 899, 2, 1).is_valid_for(header));
        assert!(!DvmDisplayDamage::rect(0, 0, 0, 1).is_valid_for(header));
    }

    #[test]
    fn gui_surface_control_is_fixed_and_capability_bounded() {
        let header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 2);
        let present = DvmGuiSurfaceMessage::present(2, 7, DvmDisplayDamage::rect(1599, 899, 1, 1));
        assert_eq!(present.encode().len(), DvmGuiSurfaceMessage::encoded_len());
        assert_eq!(
            DvmGuiSurfaceMessage::decode(&present.encode()),
            Some(present)
        );
        assert!(present.is_valid_for(header));
        assert!(DvmGuiSurfaceMessage::release(0, 7).is_valid_for(header));
        assert!(DvmGuiSurfaceMessage::focus(1).is_valid_for(header));
        assert!(
            !DvmGuiSurfaceMessage::present(3, 7, DvmDisplayDamage::full()).is_valid_for(header)
        );
        assert!(
            !DvmGuiSurfaceMessage::present(0, 0, DvmDisplayDamage::full()).is_valid_for(header)
        );
    }

    #[test]
    fn gui_surface_pool_is_fixed_three_slot_geometry() {
        let header = super::DvmGuiSurfacePoolHeader::new(32 * 1024 * 1024, 1600, 900);
        assert!(header.is_valid());
        assert_eq!(
            super::DvmGuiSurfacePoolHeader::decode(&header.encode()),
            Some(header)
        );
        assert_eq!(header.slot_offset(0), Some(4096));
        assert_eq!(header.slot_offset(2), Some(4096 + header.slot_bytes * 2));
        assert_eq!(header.slot_offset(3), None);
        assert_eq!(header.slot_bytes % 4096, 0);
        assert_eq!(header.slot_offset(1).unwrap() % 4096, 0);
        assert!(
            header.slot_offset(1).unwrap() >= header.slot_offset(0).unwrap() + header.slot_bytes
        );

        let mut encoded = header.encode();
        encoded[44..48].copy_from_slice(&2_u32.to_le_bytes());
        assert!(super::DvmGuiSurfacePoolHeader::decode(&encoded).is_none());
    }

    #[test]
    fn rejects_unready_or_truncated_regions() {
        let mut header = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1);
        header.flags = 0;
        assert!(!header.is_valid());

        header.flags = DVM_DISPLAY_FLAG_READY | DVM_DISPLAY_FLAG_HOST_ARMED;
        assert!(header.is_valid());
        header.flags |= 1 << 31;
        assert!(!header.is_valid());

        let mut encoded = DvmDisplayHeader::new(8 * 1024 * 1024, 1600, 900, 1).encode();
        encoded[12..16].copy_from_slice(&(DVM_DISPLAY_HEADER_BYTES - 1).to_le_bytes());
        assert!(DvmDisplayHeader::decode(&encoded).is_none());
    }

    #[test]
    fn display_readiness_generations_stay_inside_the_fixed_header() {
        assert!(
            DVM_DISPLAY_INVITATION_GENERATION_OFFSET + core::mem::size_of::<u64>()
                <= DVM_DISPLAY_HEADER_BYTES as usize
        );
        assert!(
            DVM_DISPLAY_READY_ACK_GENERATION_OFFSET + core::mem::size_of::<u64>()
                <= DVM_DISPLAY_HEADER_BYTES as usize
        );
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

    fn ipv4_test_frame(fragment: u16) -> [u8; 34] {
        let mut frame = [0_u8; 34];
        frame[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let ip = &mut frame[14..];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&20_u16.to_be_bytes());
        ip[6..8].copy_from_slice(&fragment.to_be_bytes());
        ip[8] = 64;
        ip[9] = 1;
        ip[12..16].copy_from_slice(&[10, 0, 0, 1]);
        ip[16..20].copy_from_slice(&[10, 0, 0, 2]);
        let mut sum = 0_u32;
        for chunk in ip.chunks_exact(2) {
            sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
            sum = (sum & 0xffff) + (sum >> 16);
        }
        ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        frame
    }

    #[test]
    fn dvm_ethernet_payload_rejects_bad_checksum_and_fragments() {
        let frame = ipv4_test_frame(0);
        assert_eq!(validate_dvm_ethernet_frame(&frame), Ok(()));

        let mut corrupt = frame;
        corrupt[22] ^= 1;
        assert_eq!(
            validate_dvm_ethernet_frame(&corrupt),
            Err(EthernetFrameError::InvalidIpv4Checksum)
        );

        assert_eq!(
            validate_dvm_ethernet_frame(&ipv4_test_frame(0x2000)),
            Err(EthernetFrameError::FragmentedIpv4)
        );
    }

    #[test]
    fn dvm_ethernet_payload_accepts_only_bounded_ipv4_or_arp() {
        let mut arp = [0_u8; 42];
        arp[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
        arp[14..16].copy_from_slice(&1_u16.to_be_bytes());
        arp[16..18].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        arp[18] = 6;
        arp[19] = 4;
        arp[20..22].copy_from_slice(&1_u16.to_be_bytes());
        assert_eq!(validate_dvm_ethernet_frame(&arp), Ok(()));

        arp[12..14].copy_from_slice(&0x86dd_u16.to_be_bytes());
        assert_eq!(
            validate_dvm_ethernet_frame(&arp),
            Err(EthernetFrameError::UnsupportedEtherType)
        );
        assert_eq!(
            validate_dvm_ethernet_frame(&[0_u8; 13]),
            Err(EthernetFrameError::Length)
        );
    }

    #[test]
    fn input_ring_has_separate_cursor_cache_lines_and_rejects_tampering() {
        let header = DvmInputRingHeader::new(DVM_INPUT_RING_APERTURE_BYTES, 9);
        assert!(header.is_valid());
        assert_eq!(DvmInputRingHeader::decode(&header.encode()), Some(header));
        assert_eq!(
            DVM_INPUT_RING_PRODUCER_OFFSET % core::mem::align_of::<u64>(),
            0
        );
        assert_eq!(
            DVM_INPUT_RING_CONSUMER_OFFSET % core::mem::align_of::<u64>(),
            0
        );
        assert!(DVM_INPUT_RING_PRODUCER_OFFSET + 8 <= DVM_INPUT_RING_CONSUMER_OFFSET);
        assert!(DVM_INPUT_RING_CONSUMER_OFFSET - DVM_INPUT_RING_PRODUCER_OFFSET >= 64);

        let mut reserved = header.encode();
        reserved[72] = 1;
        assert!(DvmInputRingHeader::decode(&reserved).is_none());

        let mut overrun = header.encode();
        overrun[DVM_INPUT_RING_PRODUCER_OFFSET..DVM_INPUT_RING_PRODUCER_OFFSET + 8]
            .copy_from_slice(&2049_u64.to_le_bytes());
        assert!(DvmInputRingHeader::decode(&overrun).is_none());

        let admitted = DvmInputRingHeader {
            flags: header.flags
                | DVM_INPUT_RING_FLAG_RUSTOS_READY
                | DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
            ..header
        };
        assert_eq!(
            DvmInputRingHeader::decode(&admitted.encode()),
            Some(admitted)
        );
    }

    #[test]
    fn absolute_pointer_frame_is_bounded_and_keeps_position_semantics() {
        let frame = RustosInputFrame::linux_evdev_pointer_position(7, 1, 800, 450, 0, 0, 0)
            .expect("bounded absolute position");
        let bytes = frame.as_bytes();
        assert_eq!(bytes[4], RUSTOS_INPUT_VERSION);
        assert_eq!(bytes[5], RUSTOS_INPUT_KIND_POINTER_POSITION);
        assert_eq!(u16::from_be_bytes(bytes[16..18].try_into().unwrap()), 800);
        assert_eq!(u16::from_be_bytes(bytes[18..20].try_into().unwrap()), 450);
        assert!(RustosInputFrame::linux_evdev_pointer_position(7, 2, 1600, 450, 0, 0, 0).is_err());
        assert!(RustosInputFrame::linux_evdev_pointer_position(7, 2, 800, 900, 0, 0, 0).is_err());
    }
}
