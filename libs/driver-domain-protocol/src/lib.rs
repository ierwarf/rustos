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

/// Private RustOS compositor to display-DVM render contract. This is not an
/// application graphics ABI: applications cannot provide shaders, GPU virtual
/// addresses, command-buffer bytes, or device handles. The owning compositor
/// emits only this fixed command vocabulary after binding every source token.
pub const DVM_GPU_RENDER_BATCH_MAGIC: [u8; 8] = *b"RSGPU001";
pub const DVM_GPU_RENDER_COMPLETION_MAGIC: [u8; 8] = *b"RSGPUD01";
pub const DVM_GPU_PRIME_COMPLETION_MAGIC: [u8; 8] = *b"RSGPUP01";
pub const DVM_GPU_PRESENT_COMPLETION_MAGIC: [u8; 8] = *b"RSGPUF01";
pub const DVM_GPU_RENDER_VERSION: u32 = 1;
/// Prime v2 authenticates the selected staged-copy or direct-DMA-BUF source
/// mode in bytes 28..32 before the host admits the first submission.
pub const DVM_GPU_PRIME_COMPLETION_VERSION: u32 = 2;
pub const DVM_GPU_RENDER_HEADER_BYTES: usize = 64;
pub const DVM_GPU_RENDER_SOURCE_BYTES: usize = 64;
pub const DVM_GPU_RENDER_COMMAND_BYTES: usize = 64;
pub const DVM_GPU_RENDER_COMPLETION_BYTES: usize = 64;
pub const DVM_GPU_PRIME_COMPLETION_BYTES: usize = 64;
pub const DVM_GPU_PRESENT_COMPLETION_BYTES: usize = 64;
pub const DVM_GPU_RENDER_MAX_BATCH_BYTES: usize = DVM_GPU_RENDER_HEADER_BYTES
    + DVM_GPU_RENDER_SOURCE_BYTES
    + DVM_GPU_RENDER_MAX_COMMANDS as usize * DVM_GPU_RENDER_COMMAND_BYTES;
/// The private compositor binds one immutable per-frame atlas. Individual
/// windows and glyphs are normalized subrectangles, not separately delegated
/// device resources. This keeps the pre-public ABI narrow and fixed.
pub const DVM_GPU_RENDER_MAX_SOURCES: u32 = 1;
pub const DVM_GPU_RENDER_MAX_COMMANDS: u32 = 512;
pub const DVM_GPU_RENDER_MAX_IN_FLIGHT: u32 = 3;
/// A 60 Hz frame must still meet this target to satisfy the commercial
/// performance gate. Missing the target retains the previous front buffer;
/// it does not by itself prove that the GPU context is lost.
pub const DVM_GPU_FRAME_TARGET_US: u32 = 16_667;
/// Hard upper bound for one private compositor submission. Only this bounded
/// timeout (or an explicit device/context error) invalidates the epoch.
pub const DVM_GPU_RENDER_MAX_BUDGET_US: u32 = 50_000;
pub const DVM_GPU_PIPELINE_PRIME_MAX_US: u32 = 500_000;
/// One atlas may be at most 8K BGRA and 256 MiB.
pub const DVM_GPU_RENDER_MAX_DIMENSION: u32 = 8_192;
pub const DVM_GPU_RENDER_MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
pub const DVM_GPU_RENDER_MAX_BATCH_SOURCE_BYTES: u64 = DVM_GPU_RENDER_MAX_SOURCE_BYTES;
pub const DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE: u32 = 1;
pub const DVM_GPU_RENDER_KNOWN_FLAGS: u32 = DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE;
pub const DVM_GPU_SOURCE_FLAG_DEVICE_READ_ONLY: u32 = 1;
/// Textured UI sources use the Wayland-compatible premultiplied-alpha rule.
/// Opaque XRGB caches set alpha to 255 and are therefore premultiplied too.
pub const DVM_GPU_SOURCE_FLAG_PREMULTIPLIED_ALPHA: u32 = 1 << 1;
pub const DVM_GPU_SOURCE_REQUIRED_FLAGS: u32 =
    DVM_GPU_SOURCE_FLAG_DEVICE_READ_ONLY | DVM_GPU_SOURCE_FLAG_PREMULTIPLIED_ALPHA;
pub const DVM_GPU_SOURCE_KNOWN_FLAGS: u32 = DVM_GPU_SOURCE_REQUIRED_FLAGS;
pub const DVM_GPU_PIXEL_FORMAT_BGRA8888: u32 = 1;
pub const DVM_GPU_NO_SOURCE: u32 = u32::MAX;
pub const DVM_GPU_NO_OUTPUT: u32 = u32::MAX;
pub const DVM_GPU_COMMAND_KIND_CLEAR: u32 = 1;
pub const DVM_GPU_COMMAND_KIND_SOLID_QUAD: u32 = 2;
pub const DVM_GPU_COMMAND_KIND_TEXTURED_QUAD: u32 = 3;
pub const DVM_GPU_BLEND_REPLACE: u32 = 1;
pub const DVM_GPU_BLEND_SOURCE_OVER: u32 = 2;
pub const DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT: u32 = 1;
pub const DVM_GPU_COMMAND_KNOWN_FLAGS: u32 = DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT;
pub const DVM_GPU_FIXED_ONE: i32 = 1 << 16;
pub const DVM_GPU_TRANSFORM_LIMIT: i32 = 4 << 16;

/// Fixed private atlas transport embedded in the host-owned display pixel
/// backing. The Linux DVM maps this backing read-only; only the separate
/// control page contains DVM-written completion records.
pub const DVM_GPU_ATLAS_POOL_MAGIC: [u8; 8] = *b"RSGPUA01";
pub const DVM_GPU_ATLAS_SUBMIT_MAGIC: [u8; 8] = *b"RSGPUQ01";
pub const DVM_GPU_ATLAS_COMPLETION_MAGIC: [u8; 8] = *b"RSGPUC01";
pub const DVM_GPU_ATLAS_TRANSPORT_VERSION: u32 = 3;
pub const DVM_GPU_ATLAS_POOL_HEADER_OFFSET: usize = 512;
pub const DVM_GPU_ATLAS_POOL_HEADER_BYTES: usize = 64;
pub const DVM_GPU_ATLAS_SUBMIT_BYTES: usize = 64;
pub const DVM_GPU_ATLAS_DAMAGE_BYTES: usize = 16;
pub const DVM_GPU_ATLAS_MAX_DAMAGE_RECTS: u32 = 64;
pub const DVM_GPU_ATLAS_COMMAND_SLOT_BYTES: u64 = 36 * 1024;
pub const DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES: usize = 256;
pub const DVM_GPU_ATLAS_COMPLETION_POOL_OFFSET: usize = 1024;
/// One DVM-produced, module-validated proof that the fixed GLES pipeline and
/// initial KMS frame completed for the host-selected context epoch.
pub const DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET: usize = 1792;
pub const DVM_GPU_ATLAS_HOST_INVITATION_OFFSET: usize = 2048;
pub const DVM_GPU_ATLAS_DVM_COMPLETION_SEQUENCE_OFFSET: usize = 2080;
pub const DVM_GPU_ATLAS_HOST_COMPLETION_ACK_OFFSET: usize = 2112;
/// Host-owned context descriptor for the current display-DVM lifecycle.
/// A restarted DVM cannot select or reuse an earlier compositor epoch.
pub const DVM_GPU_ATLAS_CONTEXT_ID_OFFSET: usize = 2144;
pub const DVM_GPU_ATLAS_CONTEXT_EPOCH_OFFSET: usize = 2148;
pub const DVM_GPU_ATLAS_PRIME_FENCE_OFFSET: usize = 2152;
pub const DVM_GPU_ATLAS_POOL_FLAG_DVM_READ_ONLY: u32 = 1;
pub const DVM_GPU_ATLAS_POOL_KNOWN_FLAGS: u32 = DVM_GPU_ATLAS_POOL_FLAG_DVM_READ_ONLY;
pub const DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY: u32 = 1;
pub const DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF: u32 = 1 << 1;
pub const DVM_GPU_ATLAS_SUBMIT_KNOWN_FLAGS: u32 =
    DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF;
pub const DVM_GPU_ATLAS_COMPLETION_FLAG_GPU_DONE: u32 = 1;
pub const DVM_GPU_ATLAS_COMPLETION_FLAG_PRESENTED: u32 = 1 << 1;
pub const DVM_GPU_ATLAS_COMPLETION_REQUIRED_FLAGS: u32 =
    DVM_GPU_ATLAS_COMPLETION_FLAG_GPU_DONE | DVM_GPU_ATLAS_COMPLETION_FLAG_PRESENTED;

pub const fn dvm_gpu_atlas_completion_offset(slot: u32) -> Option<usize> {
    if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
        return None;
    }
    DVM_GPU_ATLAS_COMPLETION_POOL_OFFSET
        .checked_add(slot as usize * DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES)
}

pub const fn dvm_gpu_atlas_invitation_offset(slot: u32) -> Option<usize> {
    if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
        return None;
    }
    DVM_GPU_ATLAS_HOST_INVITATION_OFFSET.checked_add(slot as usize * core::mem::size_of::<u64>())
}

pub const fn dvm_gpu_atlas_completion_sequence_offset(slot: u32) -> Option<usize> {
    if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
        return None;
    }
    DVM_GPU_ATLAS_DVM_COMPLETION_SEQUENCE_OFFSET
        .checked_add(slot as usize * core::mem::size_of::<u64>())
}

pub const fn dvm_gpu_atlas_completion_ack_offset(slot: u32) -> Option<usize> {
    if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
        return None;
    }
    DVM_GPU_ATLAS_HOST_COMPLETION_ACK_OFFSET
        .checked_add(slot as usize * core::mem::size_of::<u64>())
}

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

/// One bounded compositor submission. `acquire_value` and `submit_value` are
/// values on the compositor-owned monotonic timeline; the DVM may execute the
/// batch only after the acquire value is complete. `budget_us` is a relative
/// hard timeout because clocks are not assumed to be synchronized across
/// domains. Frame-target accounting is a separate measured property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuRenderBatchHeader {
    pub command_count: u32,
    pub context_id: u32,
    pub context_epoch: u32,
    pub submit_value: u64,
    pub acquire_value: u64,
    pub budget_us: u32,
    pub source_count: u32,
    pub flags: u32,
}

impl DvmGpuRenderBatchHeader {
    pub const fn encoded_len() -> usize {
        DVM_GPU_RENDER_HEADER_BYTES
    }

    pub fn encode(self) -> [u8; DVM_GPU_RENDER_HEADER_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_RENDER_HEADER_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_RENDER_BATCH_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_RENDER_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_RENDER_HEADER_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&(DVM_GPU_RENDER_COMMAND_BYTES as u32).to_le_bytes());
        bytes[20..24].copy_from_slice(&self.command_count.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.context_id.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.context_epoch.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.submit_value.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.acquire_value.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.budget_us.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.source_count.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.flags.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_RENDER_HEADER_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_RENDER_BATCH_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_RENDER_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_RENDER_HEADER_BYTES as u32
            || read_gpu_u32(bytes, 16)? != DVM_GPU_RENDER_COMMAND_BYTES as u32
            || bytes[60..64].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let header = Self {
            command_count: read_gpu_u32(bytes, 20)?,
            context_id: read_gpu_u32(bytes, 24)?,
            context_epoch: read_gpu_u32(bytes, 28)?,
            submit_value: read_gpu_u64(bytes, 32)?,
            acquire_value: read_gpu_u64(bytes, 40)?,
            budget_us: read_gpu_u32(bytes, 48)?,
            source_count: read_gpu_u32(bytes, 52)?,
            flags: read_gpu_u32(bytes, 56)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        self.command_count != 0
            && self.command_count <= DVM_GPU_RENDER_MAX_COMMANDS
            && self.context_id != 0
            && self.context_epoch != 0
            && self.submit_value != 0
            && self.acquire_value != 0
            && self.acquire_value <= self.submit_value
            && self.budget_us != 0
            && self.budget_us <= DVM_GPU_RENDER_MAX_BUDGET_US
            && self.source_count <= DVM_GPU_RENDER_MAX_SOURCES
            && self.flags == DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE
    }

    pub fn encoded_batch_len(self) -> Option<usize> {
        if !self.is_valid() {
            return None;
        }
        DVM_GPU_RENDER_HEADER_BYTES
            .checked_add(
                usize::try_from(self.source_count)
                    .ok()?
                    .checked_mul(DVM_GPU_RENDER_SOURCE_BYTES)?,
            )?
            .checked_add(
                usize::try_from(self.command_count)
                    .ok()?
                    .checked_mul(DVM_GPU_RENDER_COMMAND_BYTES)?,
            )
    }
}

/// A source is a host-bound capability token, never a guest-selected address.
/// The DVM receives device-read authority only and must signal `release_value`
/// before RustOS reuses the source generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuRenderSource {
    pub token: u64,
    pub generation: u64,
    pub acquire_value: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub pixel_format: u32,
    pub flags: u32,
    pub binding_slot: u32,
    pub content_epoch: u64,
}

impl DvmGpuRenderSource {
    pub fn encode(self) -> [u8; DVM_GPU_RENDER_SOURCE_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_RENDER_SOURCE_BYTES];
        bytes[0..8].copy_from_slice(&self.token.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.generation.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.acquire_value.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.width.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.height.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.stride_bytes.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.pixel_format.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.flags.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.binding_slot.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.content_epoch.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_RENDER_SOURCE_BYTES]) -> Option<Self> {
        if bytes[56..64].iter().any(|byte| *byte != 0) {
            return None;
        }
        let source = Self {
            token: read_gpu_u64(bytes, 0)?,
            generation: read_gpu_u64(bytes, 8)?,
            acquire_value: read_gpu_u64(bytes, 16)?,
            width: read_gpu_u32(bytes, 24)?,
            height: read_gpu_u32(bytes, 28)?,
            stride_bytes: read_gpu_u32(bytes, 32)?,
            pixel_format: read_gpu_u32(bytes, 36)?,
            flags: read_gpu_u32(bytes, 40)?,
            binding_slot: read_gpu_u32(bytes, 44)?,
            content_epoch: read_gpu_u64(bytes, 48)?,
        };
        source.is_valid().then_some(source)
    }

    pub fn is_valid(self) -> bool {
        let source_bytes = u64::from(self.stride_bytes).checked_mul(u64::from(self.height));
        self.token != 0
            && self.generation != 0
            && self.acquire_value != 0
            && self.width != 0
            && self.height != 0
            && self.width <= DVM_GPU_RENDER_MAX_DIMENSION
            && self.height <= DVM_GPU_RENDER_MAX_DIMENSION
            && self.stride_bytes >= self.width.saturating_mul(4)
            && self.stride_bytes.is_multiple_of(4)
            && source_bytes.is_some_and(|bytes| bytes <= DVM_GPU_RENDER_MAX_SOURCE_BYTES)
            && self.pixel_format == DVM_GPU_PIXEL_FORMAT_BGRA8888
            && self.flags == DVM_GPU_SOURCE_REQUIRED_FLAGS
            && self.binding_slot < DVM_GPU_RENDER_MAX_IN_FLIGHT
            && self.content_epoch != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGpuRenderCommandKind {
    Clear,
    SolidQuad,
    TexturedQuad,
}

impl DvmGpuRenderCommandKind {
    const fn wire(self) -> u32 {
        match self {
            Self::Clear => DVM_GPU_COMMAND_KIND_CLEAR,
            Self::SolidQuad => DVM_GPU_COMMAND_KIND_SOLID_QUAD,
            Self::TexturedQuad => DVM_GPU_COMMAND_KIND_TEXTURED_QUAD,
        }
    }

    const fn decode(value: u32) -> Option<Self> {
        match value {
            DVM_GPU_COMMAND_KIND_CLEAR => Some(Self::Clear),
            DVM_GPU_COMMAND_KIND_SOLID_QUAD => Some(Self::SolidQuad),
            DVM_GPU_COMMAND_KIND_TEXTURED_QUAD => Some(Self::TexturedQuad),
            _ => None,
        }
    }
}

/// Fixed compositor primitive. The transform fields are signed 16.16 values
/// consumed by the built-in vertex shader: rotation, X/Y tilt, and perspective.
/// This exercises the GPU 3D pipeline without admitting application shaders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuRenderCommand {
    pub kind: DvmGpuRenderCommandKind,
    pub flags: u32,
    pub source_index: u32,
    pub blend_mode: u32,
    pub destination_x: i32,
    pub destination_y: i32,
    pub destination_width: u32,
    pub destination_height: u32,
    pub source_u: u16,
    pub source_v: u16,
    pub source_width: u16,
    pub source_height: u16,
    pub rgba: u32,
    pub depth: i32,
    pub rotation: i32,
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub perspective: i32,
}

impl DvmGpuRenderCommand {
    pub fn encode(self) -> [u8; DVM_GPU_RENDER_COMMAND_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_RENDER_COMMAND_BYTES];
        bytes[0..4].copy_from_slice(&self.kind.wire().to_le_bytes());
        bytes[4..8].copy_from_slice(&self.flags.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.source_index.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.blend_mode.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.destination_x.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.destination_y.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.destination_width.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.destination_height.to_le_bytes());
        bytes[32..34].copy_from_slice(&self.source_u.to_le_bytes());
        bytes[34..36].copy_from_slice(&self.source_v.to_le_bytes());
        bytes[36..38].copy_from_slice(&self.source_width.to_le_bytes());
        bytes[38..40].copy_from_slice(&self.source_height.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.rgba.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.depth.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.rotation.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.tilt_x.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.tilt_y.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.perspective.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_RENDER_COMMAND_BYTES]) -> Option<Self> {
        let command = Self {
            kind: DvmGpuRenderCommandKind::decode(read_gpu_u32(bytes, 0)?)?,
            flags: read_gpu_u32(bytes, 4)?,
            source_index: read_gpu_u32(bytes, 8)?,
            blend_mode: read_gpu_u32(bytes, 12)?,
            destination_x: read_gpu_i32(bytes, 16)?,
            destination_y: read_gpu_i32(bytes, 20)?,
            destination_width: read_gpu_u32(bytes, 24)?,
            destination_height: read_gpu_u32(bytes, 28)?,
            source_u: read_gpu_u16(bytes, 32)?,
            source_v: read_gpu_u16(bytes, 34)?,
            source_width: read_gpu_u16(bytes, 36)?,
            source_height: read_gpu_u16(bytes, 38)?,
            rgba: read_gpu_u32(bytes, 40)?,
            depth: read_gpu_i32(bytes, 44)?,
            rotation: read_gpu_i32(bytes, 48)?,
            tilt_x: read_gpu_i32(bytes, 52)?,
            tilt_y: read_gpu_i32(bytes, 56)?,
            perspective: read_gpu_i32(bytes, 60)?,
        };
        command
            .is_valid_for(
                DVM_GPU_RENDER_MAX_DIMENSION,
                DVM_GPU_RENDER_MAX_DIMENSION,
                DVM_GPU_RENDER_MAX_SOURCES,
            )
            .then_some(command)
    }

    pub fn is_valid_for(self, output_width: u32, output_height: u32, source_count: u32) -> bool {
        if output_width == 0
            || output_height == 0
            || self.flags & !DVM_GPU_COMMAND_KNOWN_FLAGS != 0
            || !matches!(
                self.blend_mode,
                DVM_GPU_BLEND_REPLACE | DVM_GPU_BLEND_SOURCE_OVER
            )
            || !fixed_transform_is_bounded(self.depth)
            || !fixed_transform_is_bounded(self.rotation)
            || !fixed_transform_is_bounded(self.tilt_x)
            || !fixed_transform_is_bounded(self.tilt_y)
            || !fixed_transform_is_bounded(self.perspective)
        {
            return false;
        }
        match self.kind {
            DvmGpuRenderCommandKind::Clear => {
                self.flags == 0
                    && self.source_index == DVM_GPU_NO_SOURCE
                    && self.blend_mode == DVM_GPU_BLEND_REPLACE
                    && self.destination_x == 0
                    && self.destination_y == 0
                    && self.destination_width == 0
                    && self.destination_height == 0
                    && self.source_u == 0
                    && self.source_v == 0
                    && self.source_width == 0
                    && self.source_height == 0
                    && self.depth == 0
                    && self.rotation == 0
                    && self.tilt_x == 0
                    && self.tilt_y == 0
                    && self.perspective == 0
            }
            DvmGpuRenderCommandKind::SolidQuad | DvmGpuRenderCommandKind::TexturedQuad => {
                if self.flags != DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT
                    || self.destination_x < 0
                    || self.destination_y < 0
                    || self.destination_width == 0
                    || self.destination_height == 0
                    || (self.destination_x as u32)
                        .checked_add(self.destination_width)
                        .is_none_or(|end| end > output_width)
                    || (self.destination_y as u32)
                        .checked_add(self.destination_height)
                        .is_none_or(|end| end > output_height)
                {
                    return false;
                }
                match self.kind {
                    DvmGpuRenderCommandKind::SolidQuad => {
                        self.source_index == DVM_GPU_NO_SOURCE
                            && self.source_u == 0
                            && self.source_v == 0
                            && self.source_width == 0
                            && self.source_height == 0
                    }
                    DvmGpuRenderCommandKind::TexturedQuad => {
                        self.source_index < source_count
                            && self.source_width != 0
                            && self.source_height != 0
                            && u32::from(self.source_u)
                                .checked_add(u32::from(self.source_width))
                                .is_some_and(|end| end <= u32::from(u16::MAX))
                            && u32::from(self.source_v)
                                .checked_add(u32::from(self.source_height))
                                .is_some_and(|end| end <= u32::from(u16::MAX))
                    }
                    DvmGpuRenderCommandKind::Clear => false,
                }
            }
        }
    }
}

/// Validate one complete private compositor batch after its fixed records have
/// been decoded. This is the cross-record admission gate: individual record
/// validation is insufficient because token uniqueness, binding slots,
/// aggregate residency, acquire fences, and command ordering span records.
pub fn dvm_gpu_render_batch_is_valid(
    header: DvmGpuRenderBatchHeader,
    sources: &[DvmGpuRenderSource],
    commands: &[DvmGpuRenderCommand],
    output_width: u32,
    output_height: u32,
) -> bool {
    if !header.is_valid()
        || output_width == 0
        || output_height == 0
        || output_width > DVM_GPU_RENDER_MAX_DIMENSION
        || output_height > DVM_GPU_RENDER_MAX_DIMENSION
        || sources.len() != header.source_count as usize
        || commands.len() != header.command_count as usize
        || commands
            .first()
            .is_none_or(|command| command.kind != DvmGpuRenderCommandKind::Clear)
    {
        return false;
    }

    let mut aggregate_source_bytes = 0_u64;
    for (index, source) in sources.iter().copied().enumerate() {
        let Some(source_bytes) =
            u64::from(source.stride_bytes).checked_mul(u64::from(source.height))
        else {
            return false;
        };
        let Some(next_aggregate) = aggregate_source_bytes.checked_add(source_bytes) else {
            return false;
        };
        if !source.is_valid()
            || source.acquire_value > header.acquire_value
            || next_aggregate > DVM_GPU_RENDER_MAX_BATCH_SOURCE_BYTES
            || sources[..index]
                .iter()
                .any(|existing| existing.token == source.token)
        {
            return false;
        }
        aggregate_source_bytes = next_aggregate;
    }

    commands
        .iter()
        .copied()
        .enumerate()
        .all(|(index, command)| {
            (index == 0 || command.kind != DvmGpuRenderCommandKind::Clear)
                && command.is_valid_for(output_width, output_height, header.source_count)
        })
        && sources.iter().enumerate().all(|(source_index, _)| {
            commands.iter().any(|command| {
                command.kind == DvmGpuRenderCommandKind::TexturedQuad
                    && command.source_index as usize == source_index
            })
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGpuRenderCompletionStatus {
    Completed,
    Rejected,
    TimedOut,
    ContextLost,
    Revoked,
}

impl DvmGpuRenderCompletionStatus {
    const fn wire(self) -> u32 {
        match self {
            Self::Completed => 1,
            Self::Rejected => 2,
            Self::TimedOut => 3,
            Self::ContextLost => 4,
            Self::Revoked => 5,
        }
    }

    const fn decode(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Completed),
            2 => Some(Self::Rejected),
            3 => Some(Self::TimedOut),
            4 => Some(Self::ContextLost),
            5 => Some(Self::Revoked),
            _ => None,
        }
    }

    const fn invalidates_context(self) -> bool {
        matches!(self, Self::TimedOut | Self::ContextLost | Self::Revoked)
    }
}

/// Terminal DVM response for exactly one admitted submission. Render target
/// indices are DVM-private and cannot name host memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuRenderCompletion {
    pub context_id: u32,
    pub context_epoch: u32,
    pub status: DvmGpuRenderCompletionStatus,
    pub output_index: u32,
    pub submit_value: u64,
    pub completion_value: u64,
    pub render_time_ns: u64,
    pub release_value: u64,
}

impl DvmGpuRenderCompletion {
    pub fn encode(self) -> [u8; DVM_GPU_RENDER_COMPLETION_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_RENDER_COMPLETION_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_RENDER_COMPLETION_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_RENDER_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_RENDER_COMPLETION_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.context_id.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.context_epoch.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.status.wire().to_le_bytes());
        bytes[28..32].copy_from_slice(&self.output_index.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.submit_value.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.completion_value.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.render_time_ns.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.release_value.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_RENDER_COMPLETION_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_RENDER_COMPLETION_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_RENDER_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_RENDER_COMPLETION_BYTES as u32
        {
            return None;
        }
        let completion = Self {
            context_id: read_gpu_u32(bytes, 16)?,
            context_epoch: read_gpu_u32(bytes, 20)?,
            status: DvmGpuRenderCompletionStatus::decode(read_gpu_u32(bytes, 24)?)?,
            output_index: read_gpu_u32(bytes, 28)?,
            submit_value: read_gpu_u64(bytes, 32)?,
            completion_value: read_gpu_u64(bytes, 40)?,
            render_time_ns: read_gpu_u64(bytes, 48)?,
            release_value: read_gpu_u64(bytes, 56)?,
        };
        completion.is_valid().then_some(completion)
    }

    pub fn is_valid(self) -> bool {
        self.context_id != 0
            && self.context_epoch != 0
            && self.submit_value != 0
            && self.completion_value == self.submit_value
            && self.release_value == self.submit_value
            && match self.status {
                DvmGpuRenderCompletionStatus::Completed => {
                    self.output_index < DVM_GPU_RENDER_MAX_IN_FLIGHT && self.render_time_ns != 0
                }
                _ => self.output_index == DVM_GPU_NO_OUTPUT && self.render_time_ns == 0,
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGpuPrimeCompletionStatus {
    Ready,
    TimedOut,
    ContextLost,
    Revoked,
}

impl DvmGpuPrimeCompletionStatus {
    const fn wire(self) -> u32 {
        match self {
            Self::Ready => 1,
            Self::TimedOut => 2,
            Self::ContextLost => 3,
            Self::Revoked => 4,
        }
    }

    const fn decode(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Ready),
            2 => Some(Self::TimedOut),
            3 => Some(Self::ContextLost),
            4 => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Result of the separately fenced built-in shader/pipeline setup phase.
/// Frame admission remains closed until a matching `Ready` record arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuPrimeCompletion {
    pub context_id: u32,
    pub context_epoch: u32,
    pub status: DvmGpuPrimeCompletionStatus,
    /// Exact source transport selected by the DVM for this context. A ready
    /// context must commit to one mode before the host can publish frames.
    pub submit_flags: u32,
    pub fence_value: u64,
    pub duration_ns: u64,
}

impl DvmGpuPrimeCompletion {
    pub fn encode(self) -> [u8; DVM_GPU_PRIME_COMPLETION_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_PRIME_COMPLETION_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_PRIME_COMPLETION_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_PRIME_COMPLETION_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_PRIME_COMPLETION_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.context_id.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.context_epoch.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.status.wire().to_le_bytes());
        bytes[28..32].copy_from_slice(&self.submit_flags.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.fence_value.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.duration_ns.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_PRIME_COMPLETION_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_PRIME_COMPLETION_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_PRIME_COMPLETION_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_PRIME_COMPLETION_BYTES as u32
            || bytes[48..64].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let completion = Self {
            context_id: read_gpu_u32(bytes, 16)?,
            context_epoch: read_gpu_u32(bytes, 20)?,
            status: DvmGpuPrimeCompletionStatus::decode(read_gpu_u32(bytes, 24)?)?,
            submit_flags: read_gpu_u32(bytes, 28)?,
            fence_value: read_gpu_u64(bytes, 32)?,
            duration_ns: read_gpu_u64(bytes, 40)?,
        };
        completion.is_valid().then_some(completion)
    }

    pub fn is_valid(self) -> bool {
        self.context_id != 0
            && self.context_epoch != 0
            && self.fence_value != 0
            && match self.status {
                DvmGpuPrimeCompletionStatus::Ready => {
                    self.duration_ns != 0
                        && self.duration_ns <= u64::from(DVM_GPU_PIPELINE_PRIME_MAX_US) * 1_000
                        && matches!(
                            self.submit_flags,
                            DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY
                                | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
                        )
                }
                _ => self.duration_ns == 0 && self.submit_flags == 0,
            }
    }
}

/// Page-flip completion for a DVM-private render target. It is deliberately a
/// separate fence from GPU completion: the old front output cannot be reused
/// until this record names it as the previous front after a completed flip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuPresentCompletion {
    pub context_id: u32,
    pub context_epoch: u32,
    pub output_index: u32,
    pub submit_value: u64,
    pub present_value: u64,
    pub previous_submit_value: u64,
    pub present_time_ns: u64,
}

impl DvmGpuPresentCompletion {
    pub fn encode(self) -> [u8; DVM_GPU_PRESENT_COMPLETION_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_PRESENT_COMPLETION_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_PRESENT_COMPLETION_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_RENDER_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_PRESENT_COMPLETION_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.context_id.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.context_epoch.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.output_index.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.submit_value.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.present_value.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.previous_submit_value.to_le_bytes());
        bytes[56..64].copy_from_slice(&self.present_time_ns.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_PRESENT_COMPLETION_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_PRESENT_COMPLETION_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_RENDER_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_PRESENT_COMPLETION_BYTES as u32
            || bytes[28..32].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let completion = Self {
            context_id: read_gpu_u32(bytes, 16)?,
            context_epoch: read_gpu_u32(bytes, 20)?,
            output_index: read_gpu_u32(bytes, 24)?,
            submit_value: read_gpu_u64(bytes, 32)?,
            present_value: read_gpu_u64(bytes, 40)?,
            previous_submit_value: read_gpu_u64(bytes, 48)?,
            present_time_ns: read_gpu_u64(bytes, 56)?,
        };
        completion.is_valid().then_some(completion)
    }

    pub fn is_valid(self) -> bool {
        self.context_id != 0
            && self.context_epoch != 0
            && self.output_index < DVM_GPU_RENDER_MAX_IN_FLIGHT
            && self.submit_value != 0
            && self.present_value == self.submit_value
            && self.previous_submit_value < self.submit_value
            && self.present_time_ns != 0
    }
}

/// Geometry of the immutable atlas and command slots inside the read-only DVM
/// pixel backing. Existing direct-scanout slots remain before `command_offset`;
/// the private compositor therefore does not alias source pixels with a
/// DVM-private render target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuAtlasPoolHeader {
    pub region_bytes: u64,
    pub command_offset: u64,
    pub atlas_offset: u64,
    pub atlas_slot_bytes: u64,
    pub atlas_width: u32,
    pub atlas_height: u32,
    pub atlas_stride_bytes: u32,
    pub flags: u32,
}

impl DvmGpuAtlasPoolHeader {
    pub fn new(
        region_bytes: u64,
        gui_pool: DvmGuiSurfacePoolHeader,
        atlas_width: u32,
        atlas_height: u32,
    ) -> Option<Self> {
        if !gui_pool.is_valid()
            || gui_pool.region_bytes != region_bytes
            || atlas_width == 0
            || atlas_height == 0
            || atlas_width > DVM_GPU_RENDER_MAX_DIMENSION
            || atlas_height > DVM_GPU_RENDER_MAX_DIMENSION
        {
            return None;
        }
        let gui_end = gui_pool
            .slot_offset(DVM_GUI_SURFACE_SLOT_COUNT - 1)?
            .checked_add(gui_pool.slot_bytes)?;
        let command_offset = align_up_gpu_u64(gui_end, DVM_GUI_SURFACE_SLOT_ALIGNMENT.into())?;
        let command_end = command_offset.checked_add(
            DVM_GPU_ATLAS_COMMAND_SLOT_BYTES
                .checked_mul(u64::from(DVM_GPU_RENDER_MAX_IN_FLIGHT))?,
        )?;
        let atlas_offset = align_up_gpu_u64(command_end, DVM_GUI_SURFACE_SLOT_ALIGNMENT.into())?;
        let height_gcd = gcd_u32(atlas_height, DVM_GUI_SURFACE_SLOT_ALIGNMENT);
        let stride_alignment = DVM_GUI_SURFACE_SLOT_ALIGNMENT / height_gcd;
        let packed_stride = atlas_width.checked_mul(4)?;
        let atlas_stride_bytes = align_up_u32(packed_stride, stride_alignment);
        let atlas_slot_bytes =
            u64::from(atlas_stride_bytes).checked_mul(u64::from(atlas_height))?;
        let header = Self {
            region_bytes,
            command_offset,
            atlas_offset,
            atlas_slot_bytes,
            atlas_width,
            atlas_height,
            atlas_stride_bytes,
            flags: DVM_GPU_ATLAS_POOL_FLAG_DVM_READ_ONLY,
        };
        header.is_valid().then_some(header)
    }

    pub const fn encoded_len() -> usize {
        DVM_GPU_ATLAS_POOL_HEADER_BYTES
    }

    pub fn encode(self) -> [u8; DVM_GPU_ATLAS_POOL_HEADER_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_ATLAS_POOL_HEADER_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_ATLAS_POOL_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_ATLAS_TRANSPORT_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_ATLAS_POOL_HEADER_BYTES as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.region_bytes.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.command_offset.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.atlas_offset.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.atlas_slot_bytes.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.atlas_width.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.atlas_height.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.atlas_stride_bytes.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.flags.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_ATLAS_POOL_HEADER_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_ATLAS_POOL_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_ATLAS_TRANSPORT_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_ATLAS_POOL_HEADER_BYTES as u32
        {
            return None;
        }
        let header = Self {
            region_bytes: read_gpu_u64(bytes, 16)?,
            command_offset: read_gpu_u64(bytes, 24)?,
            atlas_offset: read_gpu_u64(bytes, 32)?,
            atlas_slot_bytes: read_gpu_u64(bytes, 40)?,
            atlas_width: read_gpu_u32(bytes, 48)?,
            atlas_height: read_gpu_u32(bytes, 52)?,
            atlas_stride_bytes: read_gpu_u32(bytes, 56)?,
            flags: read_gpu_u32(bytes, 60)?,
        };
        header.is_valid().then_some(header)
    }

    pub fn is_valid(self) -> bool {
        let Some(command_bytes) =
            DVM_GPU_ATLAS_COMMAND_SLOT_BYTES.checked_mul(u64::from(DVM_GPU_RENDER_MAX_IN_FLIGHT))
        else {
            return false;
        };
        let Some(command_end) = self.command_offset.checked_add(command_bytes) else {
            return false;
        };
        let Some(atlas_bytes) = self
            .atlas_slot_bytes
            .checked_mul(u64::from(DVM_GPU_RENDER_MAX_IN_FLIGHT))
        else {
            return false;
        };
        let Some(atlas_end) = self.atlas_offset.checked_add(atlas_bytes) else {
            return false;
        };
        self.region_bytes != 0
            && self.command_offset >= u64::from(DVM_GUI_SURFACE_POOL_HEADER_BYTES)
            && self
                .command_offset
                .is_multiple_of(u64::from(DVM_GUI_SURFACE_SLOT_ALIGNMENT))
            && DVM_GPU_ATLAS_COMMAND_SLOT_BYTES
                >= (DVM_GPU_ATLAS_SUBMIT_BYTES
                    + DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize * DVM_GPU_ATLAS_DAMAGE_BYTES
                    + DVM_GPU_RENDER_MAX_BATCH_BYTES) as u64
            && command_end <= self.atlas_offset
            && self
                .atlas_offset
                .is_multiple_of(u64::from(DVM_GUI_SURFACE_SLOT_ALIGNMENT))
            && self.atlas_width != 0
            && self.atlas_height != 0
            && self.atlas_width <= DVM_GPU_RENDER_MAX_DIMENSION
            && self.atlas_height <= DVM_GPU_RENDER_MAX_DIMENSION
            && self.atlas_stride_bytes >= self.atlas_width.saturating_mul(4)
            && self.atlas_stride_bytes.is_multiple_of(4)
            && self.atlas_slot_bytes
                == u64::from(self.atlas_stride_bytes).saturating_mul(u64::from(self.atlas_height))
            && self.atlas_slot_bytes <= DVM_GPU_RENDER_MAX_SOURCE_BYTES
            && atlas_end <= self.region_bytes
            && self.flags == DVM_GPU_ATLAS_POOL_FLAG_DVM_READ_ONLY
    }

    pub fn command_slot_offset(self, slot: u32) -> Option<u64> {
        if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
            return None;
        }
        self.command_offset
            .checked_add(DVM_GPU_ATLAS_COMMAND_SLOT_BYTES.checked_mul(u64::from(slot))?)
    }

    pub fn atlas_slot_offset(self, slot: u32) -> Option<u64> {
        if slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
            return None;
        }
        self.atlas_offset
            .checked_add(self.atlas_slot_bytes.checked_mul(u64::from(slot))?)
    }
}

/// One changed rectangle in the retained atlas texture. The initial submit
/// must cover the complete atlas; later records are deltas from the preceding
/// monotonically ordered submission. Rectangles are bounded and non-empty.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuAtlasDamage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DvmGpuAtlasDamage {
    pub fn encode(self) -> [u8; DVM_GPU_ATLAS_DAMAGE_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_ATLAS_DAMAGE_BYTES];
        bytes[0..4].copy_from_slice(&self.x.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.y.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.width.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.height.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_ATLAS_DAMAGE_BYTES]) -> Option<Self> {
        Some(Self {
            x: read_gpu_u32(bytes, 0)?,
            y: read_gpu_u32(bytes, 4)?,
            width: read_gpu_u32(bytes, 8)?,
            height: read_gpu_u32(bytes, 12)?,
        })
    }

    pub fn is_valid_for(self, atlas_width: u32, atlas_height: u32) -> bool {
        self.width != 0
            && self.height != 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|end| end <= atlas_width)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|end| end <= atlas_height)
    }

    pub fn is_full_atlas(self, atlas_width: u32, atlas_height: u32) -> bool {
        self.x == 0 && self.y == 0 && self.width == atlas_width && self.height == atlas_height
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.x < other.x.saturating_add(other.width)
            && other.x < self.x.saturating_add(self.width)
            && self.y < other.y.saturating_add(other.height)
            && other.y < self.y.saturating_add(self.height)
    }
}

pub fn dvm_gpu_atlas_damage_is_valid(
    damage: &[DvmGpuAtlasDamage],
    atlas_width: u32,
    atlas_height: u32,
    initial: bool,
) -> bool {
    if damage.len() > DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize {
        return false;
    }
    if damage.is_empty() {
        return !initial;
    }
    if initial && (damage.len() != 1 || !damage[0].is_full_atlas(atlas_width, atlas_height)) {
        return false;
    }
    damage.iter().copied().enumerate().all(|(index, rect)| {
        rect.is_valid_for(atlas_width, atlas_height)
            && !damage[..index]
                .iter()
                .copied()
                .any(|existing| existing.overlaps(rect))
    })
}

/// Immutable publication record stored immediately before bounded damage
/// records and one encoded render batch in the DVM-read-only command slot. A
/// control-page invitation is only a wake hint; these records are the
/// authoritative snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuAtlasSubmit {
    pub slot: u32,
    pub batch_bytes: u32,
    pub generation: u64,
    pub sequence: u64,
    pub context_epoch: u32,
    pub flags: u32,
    pub content_epoch: u64,
    pub damage_count: u32,
}

impl DvmGpuAtlasSubmit {
    pub fn encode(self) -> [u8; DVM_GPU_ATLAS_SUBMIT_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_ATLAS_SUBMIT_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_ATLAS_SUBMIT_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_ATLAS_TRANSPORT_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_ATLAS_SUBMIT_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.slot.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.batch_bytes.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[40..44].copy_from_slice(&self.context_epoch.to_le_bytes());
        bytes[44..48].copy_from_slice(&self.flags.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.content_epoch.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.damage_count.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_ATLAS_SUBMIT_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_ATLAS_SUBMIT_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_ATLAS_TRANSPORT_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_ATLAS_SUBMIT_BYTES as u32
            || bytes[60..64].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let submit = Self {
            slot: read_gpu_u32(bytes, 16)?,
            batch_bytes: read_gpu_u32(bytes, 20)?,
            generation: read_gpu_u64(bytes, 24)?,
            sequence: read_gpu_u64(bytes, 32)?,
            context_epoch: read_gpu_u32(bytes, 40)?,
            flags: read_gpu_u32(bytes, 44)?,
            content_epoch: read_gpu_u64(bytes, 48)?,
            damage_count: read_gpu_u32(bytes, 56)?,
        };
        submit.is_valid().then_some(submit)
    }

    pub fn is_valid(self) -> bool {
        self.slot < DVM_GPU_RENDER_MAX_IN_FLIGHT
            && self.batch_bytes as usize >= DVM_GPU_RENDER_HEADER_BYTES
            && self.batch_bytes as usize <= DVM_GPU_RENDER_MAX_BATCH_BYTES
            && u64::from(self.batch_bytes)
                .checked_add(DVM_GPU_ATLAS_SUBMIT_BYTES as u64)
                .and_then(|bytes| {
                    u64::from(self.damage_count)
                        .checked_mul(DVM_GPU_ATLAS_DAMAGE_BYTES as u64)
                        .and_then(|damage_bytes| bytes.checked_add(damage_bytes))
                })
                .is_some_and(|bytes| bytes <= DVM_GPU_ATLAS_COMMAND_SLOT_BYTES)
            && self.generation != 0
            && self.sequence != 0
            && self.context_epoch != 0
            && matches!(
                self.flags,
                DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY | DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
            )
            && self.content_epoch != 0
            && self.damage_count <= DVM_GPU_ATLAS_MAX_DAMAGE_RECTS
    }

    pub fn matches_batch(
        self,
        header: DvmGpuRenderBatchHeader,
        source: DvmGpuRenderSource,
    ) -> bool {
        self.is_valid()
            && header.source_count == 1
            && header.context_epoch == self.context_epoch
            && header.encoded_batch_len() == Some(self.batch_bytes as usize)
            && source.is_valid()
            && source.binding_slot == self.slot
            && source.generation == self.generation
            && source.content_epoch == self.content_epoch
            && source.acquire_value == header.acquire_value
    }
}

/// One DVM-written, fixed completion snapshot. Source release and KMS present
/// are carried together in the initial transport so a slot is never reclaimed
/// merely because the command bytes were accepted. The outer `sequence` is
/// acknowledged separately in the shared control page before replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuAtlasCompletion {
    pub slot: u32,
    pub flags: u32,
    pub generation: u64,
    pub sequence: u64,
    pub render: DvmGpuRenderCompletion,
    pub present: DvmGpuPresentCompletion,
}

impl DvmGpuAtlasCompletion {
    pub fn encode(self) -> [u8; DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES] {
        let mut bytes = [0_u8; DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES];
        bytes[0..8].copy_from_slice(&DVM_GPU_ATLAS_COMPLETION_MAGIC);
        bytes[8..12].copy_from_slice(&DVM_GPU_ATLAS_TRANSPORT_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES as u32).to_le_bytes());
        bytes[16..20].copy_from_slice(&self.slot.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.flags.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.generation.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.sequence.to_le_bytes());
        bytes[64..128].copy_from_slice(&self.render.encode());
        bytes[128..192].copy_from_slice(&self.present.encode());
        bytes
    }

    pub fn decode(bytes: &[u8; DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES]) -> Option<Self> {
        if bytes[0..8] != DVM_GPU_ATLAS_COMPLETION_MAGIC
            || read_gpu_u32(bytes, 8)? != DVM_GPU_ATLAS_TRANSPORT_VERSION
            || read_gpu_u32(bytes, 12)? != DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES as u32
            || bytes[40..64].iter().any(|byte| *byte != 0)
            || bytes[192..256].iter().any(|byte| *byte != 0)
        {
            return None;
        }
        let render_bytes: [u8; DVM_GPU_RENDER_COMPLETION_BYTES] = bytes[64..128].try_into().ok()?;
        let present_bytes: [u8; DVM_GPU_PRESENT_COMPLETION_BYTES] =
            bytes[128..192].try_into().ok()?;
        let completion = Self {
            slot: read_gpu_u32(bytes, 16)?,
            flags: read_gpu_u32(bytes, 20)?,
            generation: read_gpu_u64(bytes, 24)?,
            sequence: read_gpu_u64(bytes, 32)?,
            render: DvmGpuRenderCompletion::decode(&render_bytes)?,
            present: DvmGpuPresentCompletion::decode(&present_bytes)?,
        };
        completion.is_valid().then_some(completion)
    }

    pub fn is_valid(self) -> bool {
        self.slot < DVM_GPU_RENDER_MAX_IN_FLIGHT
            && self.flags == DVM_GPU_ATLAS_COMPLETION_REQUIRED_FLAGS
            && self.generation != 0
            && self.sequence != 0
            && self.render.status == DvmGpuRenderCompletionStatus::Completed
            && self.render.context_id == self.present.context_id
            && self.render.context_epoch == self.present.context_epoch
            && self.render.output_index == self.present.output_index
            && self.render.submit_value == self.present.submit_value
            && self.render.completion_value == self.present.present_value
            && self.render.release_value == self.present.present_value
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGpuTimelineError {
    ContextMismatch,
    ContextInactive,
    PipelineNotReady,
    PrimeState,
    AcquireNotReady,
    DeadlineExceeded,
    OutputBusy,
    TimelineOrder,
    QueueFull,
    InvalidContract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DvmGpuPipelineState {
    Unprimed,
    Priming,
    Ready,
    Inactive,
}

/// Small reference state machine used by the RustOS compositor scheduler.
/// Actual device scheduling remains in the DVM driver, but queue admission,
/// epoch invalidation, and accepted completion order are RustOS-owned policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DvmGpuTimeline {
    context_id: u32,
    context_epoch: u32,
    pipeline_state: DvmGpuPipelineState,
    prime_fence_value: u64,
    acquire_completed_value: u64,
    submitted_value: u64,
    completed_value: u64,
    released_value: u64,
    presented_value: u64,
    pending_submit: [u64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
    pending_budget_us: [u32; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
    output_submit: [u64; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
    front_output: u32,
    front_submit: u64,
    in_flight: u32,
}

impl DvmGpuTimeline {
    pub const fn new(context_id: u32, context_epoch: u32) -> Option<Self> {
        if context_id == 0 || context_epoch == 0 {
            return None;
        }
        Some(Self {
            context_id,
            context_epoch,
            pipeline_state: DvmGpuPipelineState::Unprimed,
            prime_fence_value: 0,
            acquire_completed_value: 0,
            submitted_value: 0,
            completed_value: 0,
            released_value: 0,
            presented_value: 0,
            pending_submit: [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
            pending_budget_us: [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
            output_submit: [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize],
            front_output: DVM_GPU_NO_OUTPUT,
            front_submit: 0,
            in_flight: 0,
        })
    }

    pub fn begin_prime(&mut self, fence_value: u64) -> Result<(), DvmGpuTimelineError> {
        if self.pipeline_state == DvmGpuPipelineState::Inactive {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if self.pipeline_state != DvmGpuPipelineState::Unprimed || fence_value == 0 {
            return Err(DvmGpuTimelineError::PrimeState);
        }
        self.prime_fence_value = fence_value;
        self.pipeline_state = DvmGpuPipelineState::Priming;
        Ok(())
    }

    pub fn complete_prime(
        &mut self,
        completion: DvmGpuPrimeCompletion,
    ) -> Result<(), DvmGpuTimelineError> {
        if !completion.is_valid() {
            return Err(DvmGpuTimelineError::InvalidContract);
        }
        if completion.context_id != self.context_id
            || completion.context_epoch != self.context_epoch
        {
            return Err(DvmGpuTimelineError::ContextMismatch);
        }
        if self.pipeline_state != DvmGpuPipelineState::Priming
            || completion.fence_value != self.prime_fence_value
        {
            return Err(DvmGpuTimelineError::PrimeState);
        }
        if completion.status == DvmGpuPrimeCompletionStatus::Ready {
            self.pipeline_state = DvmGpuPipelineState::Ready;
        } else {
            self.invalidate();
        }
        Ok(())
    }

    pub fn timeout_prime(&mut self) -> Result<(), DvmGpuTimelineError> {
        if self.pipeline_state != DvmGpuPipelineState::Priming {
            return Err(DvmGpuTimelineError::PrimeState);
        }
        self.invalidate();
        Ok(())
    }

    /// Advance the RustOS-owned aggregate acquire timeline. A submitted batch
    /// may wait on this value, but cannot execute merely because it supplied a
    /// numerically plausible fence value.
    pub fn signal_acquire(&mut self, value: u64) -> Result<(), DvmGpuTimelineError> {
        if self.pipeline_state == DvmGpuPipelineState::Inactive {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if value == 0 || value < self.acquire_completed_value {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        self.acquire_completed_value = value;
        Ok(())
    }

    pub fn admit(&mut self, header: DvmGpuRenderBatchHeader) -> Result<(), DvmGpuTimelineError> {
        if !header.is_valid() {
            return Err(DvmGpuTimelineError::InvalidContract);
        }
        if header.context_id != self.context_id || header.context_epoch != self.context_epoch {
            return Err(DvmGpuTimelineError::ContextMismatch);
        }
        if self.pipeline_state == DvmGpuPipelineState::Inactive {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if self.pipeline_state != DvmGpuPipelineState::Ready {
            return Err(DvmGpuTimelineError::PipelineNotReady);
        }
        if header.acquire_value > self.acquire_completed_value {
            return Err(DvmGpuTimelineError::AcquireNotReady);
        }
        if self.in_flight >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
            return Err(DvmGpuTimelineError::QueueFull);
        }
        if self.submitted_value == u64::MAX
            || header.submit_value != self.submitted_value + 1
            || header.acquire_value > header.submit_value
        {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        let Some(slot) = self.pending_submit.iter().position(|value| *value == 0) else {
            return Err(DvmGpuTimelineError::QueueFull);
        };
        self.pending_submit[slot] = header.submit_value;
        self.pending_budget_us[slot] = header.budget_us;
        self.submitted_value = header.submit_value;
        self.in_flight += 1;
        Ok(())
    }

    pub fn complete(
        &mut self,
        completion: DvmGpuRenderCompletion,
    ) -> Result<(), DvmGpuTimelineError> {
        if !completion.is_valid() {
            return Err(DvmGpuTimelineError::InvalidContract);
        }
        if completion.context_id != self.context_id
            || completion.context_epoch != self.context_epoch
        {
            return Err(DvmGpuTimelineError::ContextMismatch);
        }
        if self.pipeline_state == DvmGpuPipelineState::Inactive || self.in_flight == 0 {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if self.completed_value == u64::MAX
            || completion.submit_value != self.completed_value + 1
            || completion.submit_value > self.submitted_value
        {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        let Some(slot) = self
            .pending_submit
            .iter()
            .position(|value| *value == completion.submit_value)
        else {
            return Err(DvmGpuTimelineError::TimelineOrder);
        };
        let budget_ns = u64::from(self.pending_budget_us[slot]) * 1_000;
        if completion.status == DvmGpuRenderCompletionStatus::Completed
            && completion.render_time_ns > budget_ns
        {
            self.invalidate();
            return Err(DvmGpuTimelineError::DeadlineExceeded);
        }
        if completion.status == DvmGpuRenderCompletionStatus::Completed
            && self.output_submit[completion.output_index as usize] != 0
        {
            self.invalidate();
            return Err(DvmGpuTimelineError::OutputBusy);
        }
        self.pending_submit[slot] = 0;
        self.pending_budget_us[slot] = 0;
        self.completed_value = completion.submit_value;
        self.released_value = completion.release_value;
        self.in_flight -= 1;
        if completion.status.invalidates_context() {
            self.invalidate();
        } else if completion.status == DvmGpuRenderCompletionStatus::Completed {
            self.output_submit[completion.output_index as usize] = completion.submit_value;
        }
        Ok(())
    }

    pub fn timeout_submission(&mut self, submit_value: u64) -> Result<(), DvmGpuTimelineError> {
        if self.pipeline_state == DvmGpuPipelineState::Inactive {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if submit_value == 0 || !self.pending_submit.contains(&submit_value) {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        self.invalidate();
        Ok(())
    }

    pub fn present(
        &mut self,
        completion: DvmGpuPresentCompletion,
    ) -> Result<(), DvmGpuTimelineError> {
        if !completion.is_valid() {
            return Err(DvmGpuTimelineError::InvalidContract);
        }
        if completion.context_id != self.context_id
            || completion.context_epoch != self.context_epoch
        {
            return Err(DvmGpuTimelineError::ContextMismatch);
        }
        if self.pipeline_state != DvmGpuPipelineState::Ready {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        let output = completion.output_index as usize;
        if self.output_submit[output] != completion.submit_value {
            return Err(DvmGpuTimelineError::OutputBusy);
        }
        let oldest_ready = self
            .output_submit
            .iter()
            .copied()
            .filter(|value| *value != 0 && *value != self.front_submit)
            .min();
        if oldest_ready != Some(completion.submit_value)
            || completion.submit_value <= self.front_submit
            || completion.previous_submit_value != self.front_submit
        {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        if self.front_output != DVM_GPU_NO_OUTPUT {
            self.output_submit[self.front_output as usize] = 0;
        }
        self.front_output = completion.output_index;
        self.front_submit = completion.submit_value;
        self.presented_value = completion.present_value;
        Ok(())
    }

    pub fn revoke(&mut self) {
        self.invalidate();
    }

    pub fn reset(&mut self, new_epoch: u32) -> Result<(), DvmGpuTimelineError> {
        if self.pipeline_state != DvmGpuPipelineState::Inactive {
            return Err(DvmGpuTimelineError::ContextInactive);
        }
        if new_epoch == 0 || new_epoch <= self.context_epoch {
            return Err(DvmGpuTimelineError::TimelineOrder);
        }
        self.context_epoch = new_epoch;
        self.pipeline_state = DvmGpuPipelineState::Unprimed;
        self.prime_fence_value = 0;
        self.acquire_completed_value = 0;
        self.submitted_value = 0;
        self.completed_value = 0;
        self.released_value = 0;
        self.presented_value = 0;
        self.pending_submit = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.pending_budget_us = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.output_submit = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.front_output = DVM_GPU_NO_OUTPUT;
        self.front_submit = 0;
        self.in_flight = 0;
        Ok(())
    }

    fn invalidate(&mut self) {
        self.pipeline_state = DvmGpuPipelineState::Inactive;
        self.pending_submit = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.pending_budget_us = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.output_submit = [0; DVM_GPU_RENDER_MAX_IN_FLIGHT as usize];
        self.front_output = DVM_GPU_NO_OUTPUT;
        self.front_submit = 0;
        self.in_flight = 0;
    }

    pub const fn completed_value(self) -> u64 {
        self.completed_value
    }

    pub const fn in_flight(self) -> u32 {
        self.in_flight
    }

    pub fn is_active(self) -> bool {
        self.pipeline_state != DvmGpuPipelineState::Inactive
    }

    pub const fn pipeline_state(self) -> DvmGpuPipelineState {
        self.pipeline_state
    }

    pub const fn released_value(self) -> u64 {
        self.released_value
    }

    pub const fn presented_value(self) -> u64 {
        self.presented_value
    }
}

fn fixed_transform_is_bounded(value: i32) -> bool {
    value != i32::MIN && value.abs() <= DVM_GPU_TRANSFORM_LIMIT
}

fn read_gpu_u16<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_gpu_u32<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_gpu_i32<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn read_gpu_u64<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
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

fn align_up_gpu_u64(value: u64, alignment: u64) -> Option<u64> {
    if alignment == 0 {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded / alignment * alignment)
}

fn read_gui_u32(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

fn read_gui_u64(bytes: &[u8; DVM_GUI_SURFACE_MESSAGE_BYTES], offset: usize) -> Option<u64> {
    let chunk = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(chunk.try_into().ok()?))
}

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
    const fn wire(self) -> u32 {
        self as u32
    }

    fn decode(value: u32) -> Option<Self> {
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

fn valid_block_size(value: u32) -> bool {
    (512..=4096).contains(&value) && value.is_power_of_two()
}

fn bounded_block_cursor_pair(producer: u64, consumer: u64) -> bool {
    producer >= consumer && producer.saturating_sub(consumer) <= u64::from(DVM_BLOCK_QUEUE_DEPTH)
}

fn block_read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let chunk = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(chunk.try_into().ok()?))
}

fn block_read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
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
        kani::cover!(result.is_ok());
        kani::cover!(result.is_err());

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
        kani::cover!(result.is_ok());
        kani::cover!(result.is_err());

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
        DVM_GPU_BLEND_REPLACE, DVM_GPU_BLEND_SOURCE_OVER, DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
        DVM_GPU_FIXED_ONE, DVM_GPU_NO_OUTPUT, DVM_GPU_NO_SOURCE, DVM_GPU_PIXEL_FORMAT_BGRA8888,
        DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE, DVM_GPU_SOURCE_REQUIRED_FLAGS,
        DVM_INPUT_RING_APERTURE_BYTES, DVM_INPUT_RING_CONSUMER_OFFSET,
        DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET, DVM_INPUT_RING_FLAG_POLICY_CONSUMER_READY,
        DVM_INPUT_RING_FLAG_RUSTOS_READY, DVM_INPUT_RING_PRODUCER_OFFSET, DVM_NET_APERTURE_BYTES,
        DVM_NET_FLAG_DVM_READY, DVM_NET_MIN_REGION_BYTES, DvmDisplayDamage, DvmDisplayHeader,
        DvmGpuPipelineState, DvmGpuPresentCompletion, DvmGpuPrimeCompletion,
        DvmGpuPrimeCompletionStatus, DvmGpuRenderBatchHeader, DvmGpuRenderCommand,
        DvmGpuRenderCommandKind, DvmGpuRenderCompletion, DvmGpuRenderCompletionStatus,
        DvmGpuRenderSource, DvmGpuTimeline, DvmGpuTimelineError, DvmGuiSurfaceMessage,
        DvmInputRingHeader, DvmNetHeader, ETHERTYPE_ARP, ETHERTYPE_IPV4, EthernetFrameError,
        RUSTOS_INPUT_KIND_KEY, RUSTOS_INPUT_KIND_POINTER_POSITION, RUSTOS_INPUT_VERSION,
        RustosInputFrame, dvm_gpu_render_batch_is_valid, validate_dvm_ethernet_frame,
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

    fn gpu_batch(submit_value: u64) -> DvmGpuRenderBatchHeader {
        DvmGpuRenderBatchHeader {
            command_count: 3,
            context_id: 7,
            context_epoch: 11,
            submit_value,
            acquire_value: 1,
            budget_us: 16_000,
            source_count: 1,
            flags: DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE,
        }
    }

    fn prime_timeline(timeline: &mut DvmGpuTimeline, epoch: u32) {
        timeline.begin_prime(9).unwrap();
        let completion = DvmGpuPrimeCompletion {
            context_id: 7,
            context_epoch: epoch,
            status: DvmGpuPrimeCompletionStatus::Ready,
            submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
            fence_value: 9,
            duration_ns: 400_000,
        };
        assert_eq!(
            DvmGpuPrimeCompletion::decode(&completion.encode()),
            Some(completion)
        );
        let direct = DvmGpuPrimeCompletion {
            submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF,
            ..completion
        };
        assert_eq!(
            DvmGpuPrimeCompletion::decode(&direct.encode()),
            Some(direct)
        );
        let ambiguous = DvmGpuPrimeCompletion {
            submit_flags: super::DVM_GPU_ATLAS_SUBMIT_KNOWN_FLAGS,
            ..completion
        };
        assert!(!ambiguous.is_valid());
        let mut legacy = completion.encode();
        legacy[8..12].copy_from_slice(&super::DVM_GPU_RENDER_VERSION.to_le_bytes());
        assert_eq!(DvmGpuPrimeCompletion::decode(&legacy), None);
        timeline.complete_prime(completion).unwrap();
        timeline.signal_acquire(1).unwrap();
    }

    fn gpu_completion(
        submit_value: u64,
        status: DvmGpuRenderCompletionStatus,
    ) -> DvmGpuRenderCompletion {
        let completed = status == DvmGpuRenderCompletionStatus::Completed;
        DvmGpuRenderCompletion {
            context_id: 7,
            context_epoch: 11,
            status,
            output_index: if completed { 0 } else { DVM_GPU_NO_OUTPUT },
            submit_value,
            completion_value: submit_value,
            render_time_ns: if completed { 900_000 } else { 0 },
            release_value: submit_value,
        }
    }

    fn gpu_source(token: u64, binding_slot: u32) -> DvmGpuRenderSource {
        DvmGpuRenderSource {
            token,
            generation: 2,
            acquire_value: 1,
            width: 64,
            height: 64,
            stride_bytes: 256,
            pixel_format: DVM_GPU_PIXEL_FORMAT_BGRA8888,
            flags: DVM_GPU_SOURCE_REQUIRED_FLAGS,
            binding_slot,
            content_epoch: 4,
        }
    }

    fn gpu_clear() -> DvmGpuRenderCommand {
        DvmGpuRenderCommand {
            kind: DvmGpuRenderCommandKind::Clear,
            flags: 0,
            source_index: DVM_GPU_NO_SOURCE,
            blend_mode: DVM_GPU_BLEND_REPLACE,
            destination_x: 0,
            destination_y: 0,
            destination_width: 0,
            destination_height: 0,
            source_u: 0,
            source_v: 0,
            source_width: 0,
            source_height: 0,
            rgba: 0xff10_1010,
            depth: 0,
            rotation: 0,
            tilt_x: 0,
            tilt_y: 0,
            perspective: 0,
        }
    }

    fn gpu_textured(source_index: u32) -> DvmGpuRenderCommand {
        DvmGpuRenderCommand {
            kind: DvmGpuRenderCommandKind::TexturedQuad,
            flags: DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
            source_index,
            blend_mode: DVM_GPU_BLEND_SOURCE_OVER,
            destination_x: 8,
            destination_y: 8,
            destination_width: 48,
            destination_height: 48,
            source_u: 0,
            source_v: 0,
            source_width: u16::MAX,
            source_height: u16::MAX,
            rgba: u32::MAX,
            depth: 0,
            rotation: 0,
            tilt_x: 0,
            tilt_y: 0,
            perspective: 0,
        }
    }

    #[test]
    fn gpu_render_contract_is_fixed_bounded_and_address_free() {
        let header = gpu_batch(1);
        assert_eq!(
            DvmGpuRenderBatchHeader::decode(&header.encode()),
            Some(header)
        );
        assert_eq!(header.encoded_batch_len(), Some(64 + 64 + 3 * 64));

        let source = DvmGpuRenderSource {
            token: 0x100,
            generation: 2,
            acquire_value: 1,
            width: 1600,
            height: 900,
            stride_bytes: 6400,
            pixel_format: DVM_GPU_PIXEL_FORMAT_BGRA8888,
            flags: DVM_GPU_SOURCE_REQUIRED_FLAGS,
            binding_slot: 0,
            content_epoch: 4,
        };
        assert_eq!(DvmGpuRenderSource::decode(&source.encode()), Some(source));
        let mut writable = source;
        writable.flags = 0;
        assert!(!writable.is_valid());

        let mut oversized = header;
        oversized.command_count = super::DVM_GPU_RENDER_MAX_COMMANDS + 1;
        assert!(!oversized.is_valid());
        let mut late = header;
        late.acquire_value = 2;
        assert!(!late.is_valid());

        let mut bounded_jitter = header;
        bounded_jitter.budget_us = super::DVM_GPU_RENDER_MAX_BUDGET_US;
        assert!(bounded_jitter.is_valid());
        assert!(bounded_jitter.budget_us > super::DVM_GPU_FRAME_TARGET_US);
        bounded_jitter.budget_us += 1;
        assert!(!bounded_jitter.is_valid());
    }

    #[test]
    fn gpu_batch_admission_binds_one_atlas_to_a_physical_pool_slot() {
        let mut header = gpu_batch(1);
        header.command_count = 2;
        let source = gpu_source(0x100, 2);
        let commands = [gpu_clear(), gpu_textured(0)];
        assert!(dvm_gpu_render_batch_is_valid(
            header,
            &[source],
            &commands,
            128,
            128
        ));

        let rebound = gpu_source(0x101, 1);
        header.source_count = 2;
        header.command_count = 3;
        assert!(!dvm_gpu_render_batch_is_valid(
            header,
            &[source, rebound],
            &[gpu_clear(), gpu_textured(0), gpu_textured(1)],
            128,
            128
        ));

        let mut outside_pool = source;
        outside_pool.binding_slot = super::DVM_GPU_RENDER_MAX_IN_FLIGHT;
        assert!(!outside_pool.is_valid());

        let mut late = source;
        late.acquire_value = 2;
        header.source_count = 1;
        header.command_count = 2;
        assert!(!dvm_gpu_render_batch_is_valid(
            header,
            &[late],
            &commands,
            128,
            128
        ));

        let mut oversized = source;
        oversized.width = super::DVM_GPU_RENDER_MAX_DIMENSION;
        oversized.height = super::DVM_GPU_RENDER_MAX_DIMENSION;
        oversized.stride_bytes = oversized.width * 4 + 4;
        assert!(!oversized.is_valid());
    }

    #[test]
    fn gpu_render_commands_reject_unknown_or_out_of_bounds_work() {
        let clear = gpu_clear();
        assert!(clear.is_valid_for(1600, 900, 1));
        assert_eq!(DvmGpuRenderCommand::decode(&clear.encode()), Some(clear));

        let mut textured = gpu_textured(0);
        textured.destination_x = 100;
        textured.destination_y = 80;
        textured.destination_width = 800;
        textured.destination_height = 600;
        textured.depth = DVM_GPU_FIXED_ONE / 4;
        textured.rotation = DVM_GPU_FIXED_ONE / 12;
        textured.tilt_x = DVM_GPU_FIXED_ONE / 8;
        textured.perspective = DVM_GPU_FIXED_ONE / 16;
        assert!(textured.is_valid_for(1600, 900, 1));
        assert_eq!(
            DvmGpuRenderCommand::decode(&textured.encode()),
            Some(textured)
        );

        let mut outside = textured;
        outside.destination_width = 1600;
        assert!(!outside.is_valid_for(1600, 900, 1));
        let mut foreign_source = textured;
        foreign_source.source_index = 1;
        assert!(!foreign_source.is_valid_for(1600, 900, 1));
        let mut transform_overflow = textured;
        transform_overflow.perspective = super::DVM_GPU_TRANSFORM_LIMIT + 1;
        assert!(!transform_overflow.is_valid_for(1600, 900, 1));
    }

    #[test]
    fn gpu_atlas_transport_separates_immutable_sources_from_completions() {
        let region_bytes = 128_u64 * 1024 * 1024;
        let gui_pool = super::DvmGuiSurfacePoolHeader::new(region_bytes, 1600, 900);
        assert!(gui_pool.is_valid());
        let atlas = super::DvmGpuAtlasPoolHeader::new(region_bytes, gui_pool, 2048, 2048)
            .expect("atlas layout fits fixed backing");
        assert_eq!(
            super::DvmGpuAtlasPoolHeader::decode(&atlas.encode()),
            Some(atlas)
        );
        let gui_end = gui_pool.slot_offset(2).unwrap() + gui_pool.slot_bytes;
        assert!(atlas.command_offset >= gui_end);
        assert!(atlas.atlas_slot_offset(2).unwrap() + atlas.atlas_slot_bytes <= region_bytes);
        assert_eq!(atlas.command_slot_offset(3), None);
        assert_eq!(atlas.atlas_slot_offset(3), None);
        assert!(
            super::DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET
                >= super::DVM_GPU_ATLAS_COMPLETION_POOL_OFFSET
                    + super::DVM_GPU_ATLAS_COMPLETION_SLOT_BYTES
                        * super::DVM_GPU_RENDER_MAX_IN_FLIGHT as usize
        );
        const {
            assert!(
                super::DVM_GPU_ATLAS_PRIME_COMPLETION_OFFSET
                    + super::DVM_GPU_PRIME_COMPLETION_BYTES
                    <= super::DVM_GPU_ATLAS_HOST_INVITATION_OFFSET
            );
        }
        assert!(
            super::DVM_GPU_ATLAS_PRIME_FENCE_OFFSET + core::mem::size_of::<u64>()
                <= super::DVM_GUI_SURFACE_POOL_HEADER_BYTES as usize
        );

        let header = gpu_batch(1);
        let source = gpu_source(0x100, 2);
        let full_damage = super::DvmGpuAtlasDamage {
            x: 0,
            y: 0,
            width: atlas.atlas_width,
            height: atlas.atlas_height,
        };
        assert!(super::dvm_gpu_atlas_damage_is_valid(
            &[full_damage],
            atlas.atlas_width,
            atlas.atlas_height,
            true,
        ));
        assert!(!super::dvm_gpu_atlas_damage_is_valid(
            &[
                super::DvmGpuAtlasDamage {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
                super::DvmGpuAtlasDamage {
                    x: 32,
                    y: 32,
                    width: 64,
                    height: 64,
                },
            ],
            atlas.atlas_width,
            atlas.atlas_height,
            false,
        ));
        let submit = super::DvmGpuAtlasSubmit {
            slot: 2,
            batch_bytes: header.encoded_batch_len().unwrap() as u32,
            generation: source.generation,
            sequence: 1,
            context_epoch: header.context_epoch,
            flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
            content_epoch: source.content_epoch,
            damage_count: 1,
        };
        assert_eq!(
            super::DvmGpuAtlasSubmit::decode(&submit.encode()),
            Some(submit)
        );
        assert!(submit.matches_batch(header, source));
        let mut ambiguous = submit;
        ambiguous.flags = super::DVM_GPU_ATLAS_SUBMIT_KNOWN_FLAGS;
        assert!(!ambiguous.is_valid());

        let render = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
        let present = DvmGpuPresentCompletion {
            context_id: 7,
            context_epoch: 11,
            output_index: 0,
            submit_value: 1,
            present_value: 1,
            previous_submit_value: 0,
            present_time_ns: 1,
        };
        let completion = super::DvmGpuAtlasCompletion {
            slot: 2,
            flags: super::DVM_GPU_ATLAS_COMPLETION_REQUIRED_FLAGS,
            generation: source.generation,
            sequence: 1,
            render,
            present,
        };
        assert_eq!(
            super::DvmGpuAtlasCompletion::decode(&completion.encode()),
            Some(completion)
        );
        let mut accepted_only = completion;
        accepted_only.flags = super::DVM_GPU_ATLAS_COMPLETION_FLAG_GPU_DONE;
        assert!(!accepted_only.is_valid());
    }

    #[test]
    fn gpu_timeline_is_monotonic_bounded_and_reset_by_epoch() {
        let mut timeline = DvmGpuTimeline::new(7, 11).expect("valid context");
        assert_eq!(
            timeline.admit(gpu_batch(1)),
            Err(DvmGpuTimelineError::PipelineNotReady)
        );
        prime_timeline(&mut timeline, 11);
        timeline.admit(gpu_batch(1)).unwrap();
        timeline.admit(gpu_batch(2)).unwrap();
        timeline.admit(gpu_batch(3)).unwrap();
        assert_eq!(timeline.in_flight(), 3);
        assert_eq!(
            timeline.admit(gpu_batch(4)),
            Err(DvmGpuTimelineError::QueueFull)
        );

        let completion = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
        assert_eq!(
            DvmGpuRenderCompletion::decode(&completion.encode()),
            Some(completion)
        );
        timeline.complete(completion).unwrap();
        assert_eq!(timeline.completed_value(), 1);
        assert_eq!(timeline.in_flight(), 2);

        timeline
            .complete(gpu_completion(2, DvmGpuRenderCompletionStatus::TimedOut))
            .unwrap();
        assert!(!timeline.is_active());
        assert_eq!(
            timeline.admit(gpu_batch(4)),
            Err(DvmGpuTimelineError::ContextInactive)
        );
        timeline.reset(12).unwrap();
        assert!(timeline.is_active());
        assert_eq!(timeline.pipeline_state(), DvmGpuPipelineState::Unprimed);
        let mut fresh = gpu_batch(1);
        fresh.context_epoch = 12;
        assert_eq!(
            timeline.admit(fresh),
            Err(DvmGpuTimelineError::PipelineNotReady)
        );
        prime_timeline(&mut timeline, 12);
        timeline.admit(fresh).unwrap();

        let mut stale = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
        stale.context_epoch = 11;
        assert_eq!(
            timeline.complete(stale),
            Err(DvmGpuTimelineError::ContextMismatch)
        );
    }

    #[test]
    fn gpu_timeline_requires_prime_and_acquire_and_retires_outputs_in_fence_order() {
        let mut timeline = DvmGpuTimeline::new(7, 11).unwrap();
        assert_eq!(
            timeline.reset(12),
            Err(DvmGpuTimelineError::ContextInactive)
        );
        timeline.begin_prime(4).unwrap();
        let too_slow_prime = DvmGpuPrimeCompletion {
            context_id: 7,
            context_epoch: 11,
            status: DvmGpuPrimeCompletionStatus::Ready,
            submit_flags: super::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
            fence_value: 4,
            duration_ns: u64::from(super::DVM_GPU_PIPELINE_PRIME_MAX_US) * 1_000 + 1,
        };
        assert!(!too_slow_prime.is_valid());
        assert_eq!(
            timeline.complete_prime(too_slow_prime),
            Err(DvmGpuTimelineError::InvalidContract)
        );
        timeline.timeout_prime().unwrap();
        timeline.reset(12).unwrap();
        prime_timeline(&mut timeline, 12);

        let mut first = gpu_batch(1);
        first.context_epoch = 12;
        let mut second = gpu_batch(2);
        second.context_epoch = 12;
        timeline.admit(first).unwrap();
        timeline.admit(second).unwrap();

        let mut first_done = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
        first_done.context_epoch = 12;
        let mut second_done = gpu_completion(2, DvmGpuRenderCompletionStatus::Completed);
        second_done.context_epoch = 12;
        second_done.output_index = 1;
        timeline.complete(first_done).unwrap();
        timeline.complete(second_done).unwrap();

        let first_present = DvmGpuPresentCompletion {
            context_id: 7,
            context_epoch: 12,
            output_index: 0,
            submit_value: 1,
            present_value: 1,
            previous_submit_value: 0,
            present_time_ns: 1,
        };
        let second_present = DvmGpuPresentCompletion {
            context_id: 7,
            context_epoch: 12,
            output_index: 1,
            submit_value: 2,
            present_value: 2,
            previous_submit_value: 1,
            present_time_ns: 2,
        };
        assert_eq!(
            timeline.present(second_present),
            Err(DvmGpuTimelineError::TimelineOrder)
        );
        assert_eq!(
            DvmGpuPresentCompletion::decode(&first_present.encode()),
            Some(first_present)
        );
        timeline.present(first_present).unwrap();
        timeline.present(second_present).unwrap();
        assert_eq!(timeline.presented_value(), 2);

        let mut third = gpu_batch(3);
        third.context_epoch = 12;
        timeline.admit(third).unwrap();
        let mut output_reuse = gpu_completion(3, DvmGpuRenderCompletionStatus::Completed);
        output_reuse.context_epoch = 12;
        output_reuse.output_index = 1;
        assert_eq!(
            timeline.complete(output_reuse),
            Err(DvmGpuTimelineError::OutputBusy)
        );
        assert!(!timeline.is_active());

        let mut deadline = DvmGpuTimeline::new(7, 11).unwrap();
        prime_timeline(&mut deadline, 11);
        deadline.admit(gpu_batch(1)).unwrap();
        let mut late = gpu_completion(1, DvmGpuRenderCompletionStatus::Completed);
        late.render_time_ns = 16_000_001;
        assert_eq!(
            deadline.complete(late),
            Err(DvmGpuTimelineError::DeadlineExceeded)
        );
        assert!(!deadline.is_active());
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
        const {
            assert!(DVM_NET_APERTURE_BYTES >= DVM_NET_MIN_REGION_BYTES);
        }
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
        assert_eq!(
            DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET % core::mem::align_of::<u64>(),
            0
        );
        const {
            assert!(DVM_INPUT_RING_PRODUCER_OFFSET + 8 <= DVM_INPUT_RING_CONSUMER_OFFSET);
            assert!(DVM_INPUT_RING_CONSUMER_OFFSET - DVM_INPUT_RING_PRODUCER_OFFSET >= 64);
            assert!(
                DVM_INPUT_RING_CONSUMER_OFFSET + 8
                    <= DVM_INPUT_RING_CONSUMER_WAKE_GENERATION_OFFSET
            );
        }

        let armed = DvmInputRingHeader {
            consumer_wake_generation: 17,
            ..header
        };
        assert_eq!(DvmInputRingHeader::decode(&armed.encode()), Some(armed));

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
    fn input_frame_requires_nonzero_provenance_bounds_and_stable_checksum() {
        assert!(RustosInputFrame::linux_evdev_key(0, 3, 30, 1).is_err());
        assert!(RustosInputFrame::linux_evdev_key(7, 0, 30, 1).is_err());
        assert!(RustosInputFrame::linux_evdev_key(7, 3, 0, 1).is_err());
        assert!(RustosInputFrame::linux_evdev_key(7, 3, 30, 3).is_err());

        let frame =
            RustosInputFrame::linux_evdev_key(7, 3, 30, 1).expect("bounded key frame admitted");
        let bytes = frame.as_bytes();
        assert_eq!(bytes[4], RUSTOS_INPUT_VERSION);
        assert_eq!(bytes[5], RUSTOS_INPUT_KIND_KEY);
        assert_eq!(&bytes[8..12], &7_u32.to_be_bytes());
        assert_eq!(&bytes[12..16], &3_u32.to_be_bytes());
        assert_eq!(&bytes[16..18], &30_u16.to_be_bytes());
        assert_eq!(bytes[18], 1);
        assert_eq!(&bytes[28..32], &[0x40, 0xf2, 0x19, 0x2d]);
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

#[cfg(test)]
mod block_transport_tests {
    use super::{
        DVM_BLOCK_APERTURE_BYTES, DVM_BLOCK_FEATURE_DISCARD, DVM_BLOCK_FEATURE_FLUSH,
        DVM_BLOCK_FEATURE_FUA, DVM_BLOCK_FEATURE_WRITE_ZEROES, DVM_BLOCK_FEATURE_WRITEBACK,
        DVM_BLOCK_FLAG_DVM_READY, DVM_BLOCK_FLAG_RUSTOS_READY, DVM_BLOCK_REQUEST_FLAG_FUA,
        DVM_BLOCK_USED_BYTES, DvmBlockCompletion, DvmBlockCompletionStatus, DvmBlockHeader,
        DvmBlockOperation, DvmBlockRequest,
    };

    fn ready_header() -> DvmBlockHeader {
        let mut header = DvmBlockHeader::new(
            7,
            1024 * 1024,
            4096,
            4096,
            DVM_BLOCK_FEATURE_FLUSH
                | DVM_BLOCK_FEATURE_DISCARD
                | DVM_BLOCK_FEATURE_WRITE_ZEROES
                | DVM_BLOCK_FEATURE_FUA
                | DVM_BLOCK_FEATURE_WRITEBACK,
        )
        .with_epoch_signature([0x5a; 64]);
        header.flags |= DVM_BLOCK_FLAG_RUSTOS_READY | DVM_BLOCK_FLAG_DVM_READY;
        header
    }

    #[test]
    fn block_header_is_fixed_bounded_and_reserved_zero() {
        let header = ready_header();
        assert!(DVM_BLOCK_APERTURE_BYTES.is_power_of_two());
        const {
            assert!(DVM_BLOCK_USED_BYTES <= DVM_BLOCK_APERTURE_BYTES);
        }
        assert!(header.is_valid());
        assert_eq!(DvmBlockHeader::decode(&header.encode()), Some(header));

        let mut unknown_feature = header;
        unknown_feature.features |= 1 << 63;
        assert!(!unknown_feature.is_valid());

        let mut overrun = header;
        overrun.request_producer = 65;
        assert!(!overrun.is_valid());

        let mut reserved = header.encode();
        reserved[168] = 1;
        assert!(DvmBlockHeader::decode(&reserved).is_none());

        let mut unsigned = header;
        unsigned.epoch_signature = [0; 64];
        assert!(!unsigned.is_valid());
    }

    #[test]
    fn block_requests_are_address_free_epoch_bound_and_range_checked() {
        let header = ready_header();
        let request = DvmBlockRequest {
            generation: header.generation,
            request_id: 1,
            operation_id: 9,
            operation: DvmBlockOperation::Write,
            flags: DVM_BLOCK_REQUEST_FLAG_FUA,
            data_slot: 3,
            sector: 8,
            data_len: 4096,
        };
        assert!(request.is_valid_for(header));
        assert_eq!(DvmBlockRequest::decode(&request.encode()), Some(request));

        let mut stale = request;
        stale.generation -= 1;
        assert!(!stale.is_valid_for(header));

        let mut misaligned = request;
        misaligned.sector = 1;
        assert!(!misaligned.is_valid_for(header));

        let mut outside = request;
        outside.sector = header.capacity_sectors;
        assert!(!outside.is_valid_for(header));

        let flush = DvmBlockRequest {
            generation: header.generation,
            request_id: 2,
            operation_id: 10,
            operation: DvmBlockOperation::Flush,
            flags: 0,
            data_slot: 7,
            sector: 0,
            data_len: 0,
        };
        assert!(flush.is_valid_for(header));
    }

    #[test]
    fn block_completion_binds_request_and_explicit_durability() {
        let header = ready_header();
        let write = DvmBlockRequest {
            generation: header.generation,
            request_id: 3,
            operation_id: 11,
            operation: DvmBlockOperation::Write,
            flags: DVM_BLOCK_REQUEST_FLAG_FUA,
            data_slot: 4,
            sector: 16,
            data_len: 4096,
        };
        let completion = DvmBlockCompletion {
            generation: header.generation,
            request_id: write.request_id,
            operation_id: write.operation_id,
            status: DvmBlockCompletionStatus::Success,
            data_slot: write.data_slot,
            completed_bytes: write.data_len,
            durable_through_operation_id: write.operation_id,
        };
        assert!(completion.is_valid_for(header, write));
        assert_eq!(
            DvmBlockCompletion::decode(&completion.encode()),
            Some(completion)
        );

        let mut fabricated_stability = completion;
        fabricated_stability.durable_through_operation_id = 0;
        assert!(!fabricated_stability.is_valid_for(header, write));

        let mut foreign = completion;
        foreign.request_id += 1;
        assert!(!foreign.is_valid_for(header, write));
    }
}
