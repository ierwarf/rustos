// SPDX-License-Identifier: MIT
//! Moved verbatim from the former single-file crate root; see `lib.rs`.

#[allow(unused_imports)]
use crate::*;

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
/// The first `DVM_GPU_COLD_SUBMIT_COUNT` submissions of a context epoch
/// (`submit_value <= count`; submit values are strictly sequential from 1)
/// compile host shaders and fault in host GPU state, and on a loaded host
/// the DVM vCPU can also be descheduled for tens of milliseconds. Their
/// effective render budget is raised to `DVM_GPU_COLD_RENDER_BUDGET_US`.
/// Both the Linux relay and `DvmGpuTimeline` derive it from the same
/// `dvm_gpu_effective_budget_us`, so a cold frame is never accepted by one
/// side and rejected by the other. Steady-state frames keep the 50 ms bound.
pub const DVM_GPU_COLD_SUBMIT_COUNT: u64 = 2;
pub const DVM_GPU_COLD_RENDER_BUDGET_US: u32 = 500_000;
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

/// Effective render budget for a submission: the requested `budget_us`, raised
/// to the cold-start allowance for the first submissions of an epoch.
pub const fn dvm_gpu_effective_budget_us(budget_us: u32, submit_value: u64) -> u32 {
    if submit_value != 0
        && submit_value <= DVM_GPU_COLD_SUBMIT_COUNT
        && budget_us < DVM_GPU_COLD_RENDER_BUDGET_US
    {
        DVM_GPU_COLD_RENDER_BUDGET_US
    } else {
        budget_us
    }
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
        self.pending_budget_us[slot] =
            dvm_gpu_effective_budget_us(header.budget_us, header.submit_value);
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

pub(crate) fn fixed_transform_is_bounded(value: i32) -> bool {
    value != i32::MIN && value.abs() <= DVM_GPU_TRANSFORM_LIMIT
}

pub(crate) fn read_gpu_u16<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

pub(crate) fn read_gpu_u32<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(crate) fn read_gpu_i32<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

pub(crate) fn read_gpu_u64<const N: usize>(bytes: &[u8; N], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
    ))
}

pub(crate) fn gcd_u32(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

pub(crate) fn align_up_u32(value: u32, alignment: u32) -> u32 {
    debug_assert!(alignment != 0);
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded / alignment * alignment)
        .unwrap_or(u32::MAX)
}

pub(crate) fn align_up_gpu_u64(value: u64, alignment: u64) -> Option<u64> {
    if alignment == 0 {
        return None;
    }
    value
        .checked_add(alignment - 1)
        .map(|rounded| rounded / alignment * alignment)
}
