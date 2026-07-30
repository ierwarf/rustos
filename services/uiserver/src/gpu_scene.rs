//! Private scene-to-GPU contract compiler.
//!
//! This module intentionally stops before the application-visible graphics
//! ABI and before the RustOS-to-DVM submit transport. It gives `uiserver` one
//! bounded, validated representation for desktop composition so the next ABI
//! slice cannot smuggle raw shaders, GPU addresses, or unbounded command data
//! into the display DVM.

use std::vec::Vec;

use driver_domain_protocol::{
    dvm_gpu_render_batch_is_valid, DvmGpuPresentCompletion, DvmGpuPrimeCompletion,
    DvmGpuRenderBatchHeader, DvmGpuRenderCommand, DvmGpuRenderCommandKind, DvmGpuRenderCompletion,
    DvmGpuRenderCompletionStatus, DvmGpuRenderSource, DvmGpuTimeline, DvmGpuTimelineError,
    DVM_GPU_BLEND_REPLACE, DVM_GPU_BLEND_SOURCE_OVER, DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
    DVM_GPU_FIXED_ONE, DVM_GPU_NO_SOURCE, DVM_GPU_PIXEL_FORMAT_BGRA8888,
    DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE, DVM_GPU_RENDER_MAX_BUDGET_US,
    DVM_GPU_RENDER_MAX_COMMANDS, DVM_GPU_RENDER_MAX_IN_FLIGHT, DVM_GPU_RENDER_MAX_SOURCES,
    DVM_GPU_SOURCE_REQUIRED_FLAGS,
};

use crate::canvas::Rect;

const DEFAULT_FRAME_TIMEOUT_US: u32 = DVM_GPU_RENDER_MAX_BUDGET_US;
const BOOT_CONTEXT_ID: u32 = 1;
const BOOT_CONTEXT_EPOCH: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GpuLayerTransform {
    pub(crate) depth: i32,
    pub(crate) rotation: i32,
    pub(crate) tilt_x: i32,
    pub(crate) tilt_y: i32,
    pub(crate) perspective: i32,
}

impl GpuLayerTransform {
    pub(crate) const fn flat() -> Self {
        Self {
            depth: 0,
            rotation: 0,
            tilt_x: 0,
            tilt_y: 0,
            perspective: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GpuTextureCapability {
    pub(crate) token: u64,
    pub(crate) generation: u64,
    pub(crate) content_epoch: u64,
    /// Physical member of the immutable three-atlas pool. This is not a
    /// command source index; one batch always refers to descriptor index zero.
    pub(crate) binding_slot: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride_bytes: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GpuTextureRegion {
    pub(crate) atlas: GpuTextureCapability,
    pub(crate) source_rect: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GpuLayerKind {
    Solid { rgba: u32 },
    Texture { region: GpuTextureRegion },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GpuSceneLayer {
    pub(crate) destination: Rect,
    pub(crate) opacity: u8,
    pub(crate) source_over: bool,
    pub(crate) transform: GpuLayerTransform,
    pub(crate) kind: GpuLayerKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GpuSceneBatch {
    pub(crate) header: DvmGpuRenderBatchHeader,
    pub(crate) sources: Vec<DvmGpuRenderSource>,
    pub(crate) commands: Vec<DvmGpuRenderCommand>,
    output_width: u32,
    output_height: u32,
}

impl GpuSceneBatch {
    pub(crate) fn encode(&self) -> Result<Vec<u8>, GpuSceneError> {
        let expected = self
            .header
            .encoded_batch_len()
            .ok_or(GpuSceneError::InvalidContract)?;
        if !dvm_gpu_render_batch_is_valid(
            self.header,
            &self.sources,
            &self.commands,
            self.output_width,
            self.output_height,
        ) {
            return Err(GpuSceneError::InvalidContract);
        }
        let mut bytes = Vec::with_capacity(expected);
        bytes.extend_from_slice(&self.header.encode());
        for source in &self.sources {
            bytes.extend_from_slice(&source.encode());
        }
        for command in &self.commands {
            bytes.extend_from_slice(&command.encode());
        }
        if bytes.len() != expected {
            return Err(GpuSceneError::InvalidContract);
        }
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GpuSceneError {
    SceneNotReady,
    InvalidContext,
    InvalidOutput,
    InvalidLayer,
    InvalidContract,
    TokenRebound,
    AtlasFull,
    SourceLimit,
    CommandLimit,
    Timeline(DvmGpuTimelineError),
}

impl From<DvmGpuTimelineError> for GpuSceneError {
    fn from(value: DvmGpuTimelineError) -> Self {
        Self::Timeline(value)
    }
}

/// Deterministic shelf packer for one immutable atlas generation. It copies
/// independently rasterized source surfaces into isolated regions; it never
/// paints their final destination positions or performs z-order composition.
/// One transparent texel around each region prevents nearest-sample bleed.
pub(crate) struct GpuAtlasPacker<'a> {
    pixels: &'a mut [u32],
    width: usize,
    height: usize,
    stride_pixels: usize,
    next_x: usize,
    next_y: usize,
    row_height: usize,
}

impl<'a> GpuAtlasPacker<'a> {
    const PADDING: usize = 1;

    pub(crate) fn new(
        pixels: &'a mut [u32],
        width: usize,
        height: usize,
        stride_pixels: usize,
    ) -> Result<Self, GpuSceneError> {
        let required = height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(stride_pixels))
            .and_then(|prefix| prefix.checked_add(width));
        if width == 0
            || height == 0
            || stride_pixels < width
            || required.is_none_or(|required| required > pixels.len())
        {
            return Err(GpuSceneError::InvalidLayer);
        }
        Ok(Self {
            pixels,
            width,
            height,
            stride_pixels,
            next_x: 0,
            next_y: 0,
            row_height: 0,
        })
    }

    pub(crate) fn place_bgra(
        &mut self,
        source: &[u32],
        width: usize,
        height: usize,
        stride_pixels: usize,
    ) -> Result<Rect, GpuSceneError> {
        self.place(source, width, height, stride_pixels, false)
    }

    /// Place an opaque XRGB cache into the premultiplied-alpha atlas.
    pub(crate) fn place_xrgb(
        &mut self,
        source: &[u32],
        width: usize,
        height: usize,
        stride_pixels: usize,
    ) -> Result<Rect, GpuSceneError> {
        self.place(source, width, height, stride_pixels, true)
    }

    pub(crate) fn write_xrgb_at(
        &mut self,
        destination: Rect,
        source: &[u32],
        stride_pixels: usize,
    ) -> Result<(), GpuSceneError> {
        let source_required = destination
            .height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(stride_pixels))
            .and_then(|prefix| prefix.checked_add(destination.width));
        if destination.is_empty()
            || stride_pixels < destination.width
            || source_required.is_none_or(|required| required > source.len())
            || destination.x == 0
            || destination.y == 0
            || destination
                .x
                .checked_add(destination.width)
                .is_none_or(|end| end >= self.width)
            || destination
                .y
                .checked_add(destination.height)
                .is_none_or(|end| end >= self.height)
        {
            return Err(GpuSceneError::InvalidLayer);
        }
        let outer_x = destination.x - Self::PADDING;
        let outer_y = destination.y - Self::PADDING;
        let outer_width = destination.width + Self::PADDING * 2;
        let outer_height = destination.height + Self::PADDING * 2;
        for row in outer_y..outer_y + outer_height {
            let start = row
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(outer_x))
                .ok_or(GpuSceneError::InvalidLayer)?;
            let end = start
                .checked_add(outer_width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            self.pixels
                .get_mut(start..end)
                .ok_or(GpuSceneError::InvalidLayer)?
                .fill(0);
        }
        for row in 0..destination.height {
            let source_start = row
                .checked_mul(stride_pixels)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let source_end = source_start
                .checked_add(destination.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_start = (destination.y + row)
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(destination.x))
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_end = destination_start
                .checked_add(destination.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let source_row = source
                .get(source_start..source_end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_row = self
                .pixels
                .get_mut(destination_start..destination_end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            copy_xrgb_opaque_row(destination_row, source_row);
        }
        Ok(())
    }

    fn place(
        &mut self,
        source: &[u32],
        width: usize,
        height: usize,
        stride_pixels: usize,
        force_opaque: bool,
    ) -> Result<Rect, GpuSceneError> {
        let source_required = height
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(stride_pixels))
            .and_then(|prefix| prefix.checked_add(width));
        if width == 0
            || height == 0
            || stride_pixels < width
            || source_required.is_none_or(|required| required > source.len())
        {
            return Err(GpuSceneError::InvalidLayer);
        }
        let padding = Self::PADDING;
        let outer_width = width
            .checked_add(padding * 2)
            .ok_or(GpuSceneError::AtlasFull)?;
        let outer_height = height
            .checked_add(padding * 2)
            .ok_or(GpuSceneError::AtlasFull)?;
        if outer_width > self.width || outer_height > self.height {
            return Err(GpuSceneError::AtlasFull);
        }

        let mut outer_x = self.next_x;
        let mut outer_y = self.next_y;
        let mut row_height = self.row_height;
        if outer_x
            .checked_add(outer_width)
            .is_none_or(|end| end > self.width)
        {
            outer_x = 0;
            outer_y = outer_y
                .checked_add(row_height)
                .ok_or(GpuSceneError::AtlasFull)?;
            row_height = 0;
        }
        if outer_y
            .checked_add(outer_height)
            .is_none_or(|end| end > self.height)
        {
            return Err(GpuSceneError::AtlasFull);
        }

        for row in outer_y..outer_y + outer_height {
            let start = row
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(outer_x))
                .ok_or(GpuSceneError::AtlasFull)?;
            let end = start
                .checked_add(outer_width)
                .ok_or(GpuSceneError::AtlasFull)?;
            self.pixels
                .get_mut(start..end)
                .ok_or(GpuSceneError::InvalidLayer)?
                .fill(0);
        }

        let destination = Rect {
            x: outer_x + padding,
            y: outer_y + padding,
            width,
            height,
        };
        for row in 0..height {
            let source_start = row
                .checked_mul(stride_pixels)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let source_end = source_start
                .checked_add(width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_start = (destination.y + row)
                .checked_mul(self.stride_pixels)
                .and_then(|offset| offset.checked_add(destination.x))
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_end = destination_start
                .checked_add(width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let source_row = source
                .get(source_start..source_end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_row = self
                .pixels
                .get_mut(destination_start..destination_end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            if force_opaque {
                copy_xrgb_opaque_row(destination_row, source_row);
            } else {
                destination_row.copy_from_slice(source_row);
            }
        }

        self.next_x = outer_x + outer_width;
        self.next_y = outer_y;
        self.row_height = row_height.max(outer_height);
        Ok(destination)
    }
}

/// Convert one XRGB row to the compositor's opaque BGRA atlas format.
///
/// Desktop and window topology rebuilds copy several million pixels before
/// the first interactive frame. Keeping the conversion as a per-pixel
/// iterator made that one-time rebuild exceed the 50 ms UI-loop contract on
/// modest hosts. SSE2 is part of the x86-64 baseline, and this private helper
/// preserves the exact `source | 0xff00_0000` semantics for every pixel.
#[inline]
fn copy_xrgb_opaque_row(destination: &mut [u32], source: &[u32]) {
    debug_assert_eq!(destination.len(), source.len());
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{
            __m128i, _mm_loadu_si128, _mm_or_si128, _mm_set1_epi32, _mm_storeu_si128,
        };

        let opaque = unsafe { _mm_set1_epi32(0xff00_0000_u32 as i32) };
        let mut index = 0;
        while index + 4 <= destination.len() {
            // SAFETY: both four-pixel ranges are bounded by the loop
            // condition, and the unaligned load/store intrinsics accept the
            // row alignment provided by arbitrary atlas rectangles.
            unsafe {
                let pixels = _mm_loadu_si128(source.as_ptr().add(index).cast::<__m128i>());
                _mm_storeu_si128(
                    destination.as_mut_ptr().add(index).cast::<__m128i>(),
                    _mm_or_si128(pixels, opaque),
                );
            }
            index += 4;
        }
        for (destination, source) in destination[index..].iter_mut().zip(&source[index..]) {
            *destination = *source | 0xff00_0000;
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    for (destination, source) in destination.iter_mut().zip(source) {
        *destination = *source | 0xff00_0000;
    }
}

pub(crate) struct GpuSceneCompiler {
    context_id: u32,
    context_epoch: u32,
    next_submit: u64,
    timeline: DvmGpuTimeline,
}

impl GpuSceneCompiler {
    pub(crate) fn new(context_id: u32, context_epoch: u32) -> Result<Self, GpuSceneError> {
        let timeline =
            DvmGpuTimeline::new(context_id, context_epoch).ok_or(GpuSceneError::InvalidContext)?;
        Ok(Self {
            context_id,
            context_epoch,
            next_submit: 1,
            timeline,
        })
    }

    pub(crate) fn compile(
        &mut self,
        output_width: u32,
        output_height: u32,
        acquire_value: u64,
        clear_rgba: u32,
        layers: &[GpuSceneLayer],
    ) -> Result<GpuSceneBatch, GpuSceneError> {
        if output_width == 0 || output_height == 0 || acquire_value == 0 || self.next_submit == 0 {
            return Err(GpuSceneError::InvalidOutput);
        }
        let command_count = layers
            .len()
            .checked_add(1)
            .ok_or(GpuSceneError::CommandLimit)?;
        if command_count > DVM_GPU_RENDER_MAX_COMMANDS as usize {
            return Err(GpuSceneError::CommandLimit);
        }

        let mut sources = Vec::with_capacity(layers.len().min(DVM_GPU_RENDER_MAX_SOURCES as usize));
        let mut commands = Vec::with_capacity(command_count);
        commands.push(clear_command(clear_rgba));
        for layer in layers {
            let command = match layer.kind {
                GpuLayerKind::Solid { rgba } => layer_command(
                    *layer,
                    DvmGpuRenderCommandKind::SolidQuad,
                    DVM_GPU_NO_SOURCE,
                    rgba,
                    layer.source_over,
                    None,
                )?,
                GpuLayerKind::Texture { region } => {
                    let source_index = bind_source(&mut sources, region.atlas, acquire_value)?;
                    let source_rect = normalized_source_rect(region)?;
                    layer_command(
                        *layer,
                        DvmGpuRenderCommandKind::TexturedQuad,
                        source_index,
                        opacity_color(layer.opacity),
                        layer.source_over,
                        Some(source_rect),
                    )?
                }
            };
            if !command.is_valid_for(output_width, output_height, sources.len() as u32) {
                return Err(GpuSceneError::InvalidLayer);
            }
            commands.push(command);
        }

        let header = DvmGpuRenderBatchHeader {
            command_count: commands.len() as u32,
            context_id: self.context_id,
            context_epoch: self.context_epoch,
            submit_value: self.next_submit,
            acquire_value,
            budget_us: DEFAULT_FRAME_TIMEOUT_US,
            source_count: sources.len() as u32,
            flags: DVM_GPU_RENDER_FLAG_PRESENT_ON_COMPLETE,
        };
        if !dvm_gpu_render_batch_is_valid(header, &sources, &commands, output_width, output_height)
        {
            return Err(GpuSceneError::InvalidContract);
        }
        let following_submit = self
            .next_submit
            .checked_add(1)
            .ok_or(GpuSceneError::Timeline(DvmGpuTimelineError::TimelineOrder))?;
        self.timeline.admit(header)?;
        self.next_submit = following_submit;
        Ok(GpuSceneBatch {
            header,
            sources,
            commands,
            output_width,
            output_height,
        })
    }

    pub(crate) fn begin_prime(&mut self, fence_value: u64) -> Result<(), GpuSceneError> {
        self.timeline.begin_prime(fence_value)?;
        Ok(())
    }

    pub(crate) fn complete_prime(
        &mut self,
        completion: DvmGpuPrimeCompletion,
    ) -> Result<(), GpuSceneError> {
        self.timeline.complete_prime(completion)?;
        Ok(())
    }

    pub(crate) fn signal_acquire(&mut self, value: u64) -> Result<(), GpuSceneError> {
        self.timeline.signal_acquire(value)?;
        Ok(())
    }

    pub(crate) fn complete(
        &mut self,
        completion: DvmGpuRenderCompletion,
    ) -> Result<(), GpuSceneError> {
        self.timeline.complete(completion)?;
        Ok(())
    }

    pub(crate) fn present(
        &mut self,
        completion: DvmGpuPresentCompletion,
    ) -> Result<(), GpuSceneError> {
        self.timeline.present(completion)?;
        Ok(())
    }

    pub(crate) fn revoke(&mut self) {
        self.timeline.revoke();
    }

    pub(crate) fn reset(&mut self, new_epoch: u32) -> Result<(), GpuSceneError> {
        self.timeline.reset(new_epoch)?;
        self.context_epoch = new_epoch;
        self.next_submit = 1;
        Ok(())
    }
}

fn bind_source(
    sources: &mut Vec<DvmGpuRenderSource>,
    source: GpuTextureCapability,
    acquire_value: u64,
) -> Result<u32, GpuSceneError> {
    if let Some((index, existing)) = sources
        .iter()
        .enumerate()
        .find(|(_, existing)| existing.token == source.token)
    {
        let same_capability = existing.generation == source.generation
            && existing.content_epoch == source.content_epoch
            && existing.width == source.width
            && existing.height == source.height
            && existing.stride_bytes == source.stride_bytes
            && existing.binding_slot == source.binding_slot;
        return if same_capability {
            u32::try_from(index).map_err(|_| GpuSceneError::SourceLimit)
        } else {
            Err(GpuSceneError::TokenRebound)
        };
    }
    if sources.len() >= DVM_GPU_RENDER_MAX_SOURCES as usize {
        return Err(GpuSceneError::SourceLimit);
    }
    if source.binding_slot >= DVM_GPU_RENDER_MAX_IN_FLIGHT {
        return Err(GpuSceneError::InvalidLayer);
    }
    let source_index = u32::try_from(sources.len()).map_err(|_| GpuSceneError::SourceLimit)?;
    let bound = DvmGpuRenderSource {
        token: source.token,
        generation: source.generation,
        acquire_value,
        width: source.width,
        height: source.height,
        stride_bytes: source.stride_bytes,
        pixel_format: DVM_GPU_PIXEL_FORMAT_BGRA8888,
        flags: DVM_GPU_SOURCE_REQUIRED_FLAGS,
        binding_slot: source.binding_slot,
        content_epoch: source.content_epoch,
    };
    if !bound.is_valid() {
        return Err(GpuSceneError::InvalidLayer);
    }
    sources.push(bound);
    Ok(source_index)
}

fn clear_command(rgba: u32) -> DvmGpuRenderCommand {
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
        rgba,
        depth: 0,
        rotation: 0,
        tilt_x: 0,
        tilt_y: 0,
        perspective: 0,
    }
}

fn layer_command(
    layer: GpuSceneLayer,
    kind: DvmGpuRenderCommandKind,
    source_index: u32,
    rgba: u32,
    source_over: bool,
    source_rect: Option<(u16, u16, u16, u16)>,
) -> Result<DvmGpuRenderCommand, GpuSceneError> {
    let destination_x =
        i32::try_from(layer.destination.x).map_err(|_| GpuSceneError::InvalidLayer)?;
    let destination_y =
        i32::try_from(layer.destination.y).map_err(|_| GpuSceneError::InvalidLayer)?;
    let destination_width =
        u32::try_from(layer.destination.width).map_err(|_| GpuSceneError::InvalidLayer)?;
    let destination_height =
        u32::try_from(layer.destination.height).map_err(|_| GpuSceneError::InvalidLayer)?;
    let textured = kind == DvmGpuRenderCommandKind::TexturedQuad;
    let (source_u, source_v, source_width, source_height) = match (textured, source_rect) {
        (true, Some(source_rect)) => source_rect,
        (false, None) => (0, 0, 0, 0),
        _ => return Err(GpuSceneError::InvalidLayer),
    };
    Ok(DvmGpuRenderCommand {
        kind,
        flags: DVM_GPU_COMMAND_FLAG_CLIP_OUTPUT,
        source_index,
        blend_mode: if source_over || layer.opacity != u8::MAX {
            DVM_GPU_BLEND_SOURCE_OVER
        } else {
            DVM_GPU_BLEND_REPLACE
        },
        destination_x,
        destination_y,
        destination_width,
        destination_height,
        source_u,
        source_v,
        source_width,
        source_height,
        rgba,
        depth: layer.transform.depth,
        rotation: layer.transform.rotation,
        tilt_x: layer.transform.tilt_x,
        tilt_y: layer.transform.tilt_y,
        perspective: layer.transform.perspective,
    })
}

fn normalized_source_rect(region: GpuTextureRegion) -> Result<(u16, u16, u16, u16), GpuSceneError> {
    let atlas_width =
        usize::try_from(region.atlas.width).map_err(|_| GpuSceneError::InvalidLayer)?;
    let atlas_height =
        usize::try_from(region.atlas.height).map_err(|_| GpuSceneError::InvalidLayer)?;
    let rect = region.source_rect;
    let end_x = rect
        .x
        .checked_add(rect.width)
        .ok_or(GpuSceneError::InvalidLayer)?;
    let end_y = rect
        .y
        .checked_add(rect.height)
        .ok_or(GpuSceneError::InvalidLayer)?;
    if rect.width == 0 || rect.height == 0 || end_x > atlas_width || end_y > atlas_height {
        return Err(GpuSceneError::InvalidLayer);
    }
    let (source_u, source_width) = normalized_axis(rect.x, rect.width, atlas_width)?;
    let (source_v, source_height) = normalized_axis(rect.y, rect.height, atlas_height)?;
    Ok((source_u, source_v, source_width, source_height))
}

fn normalized_axis(start: usize, length: usize, total: usize) -> Result<(u16, u16), GpuSceneError> {
    if total == 0 || length == 0 {
        return Err(GpuSceneError::InvalidLayer);
    }
    let end = start
        .checked_add(length)
        .filter(|end| *end <= total)
        .ok_or(GpuSceneError::InvalidLayer)?;
    let scale = u64::from(u16::MAX);
    let total = u64::try_from(total).map_err(|_| GpuSceneError::InvalidLayer)?;
    let start = u64::try_from(start)
        .map_err(|_| GpuSceneError::InvalidLayer)?
        .checked_mul(scale)
        .ok_or(GpuSceneError::InvalidLayer)?
        / total;
    let end = u64::try_from(end)
        .map_err(|_| GpuSceneError::InvalidLayer)?
        .checked_mul(scale)
        .and_then(|value| value.checked_add(total - 1))
        .ok_or(GpuSceneError::InvalidLayer)?
        / total;
    let width = end.checked_sub(start).ok_or(GpuSceneError::InvalidLayer)?;
    if width == 0 || end > scale {
        return Err(GpuSceneError::InvalidLayer);
    }
    Ok((
        u16::try_from(start).map_err(|_| GpuSceneError::InvalidLayer)?,
        u16::try_from(width).map_err(|_| GpuSceneError::InvalidLayer)?,
    ))
}

fn opacity_color(opacity: u8) -> u32 {
    (u32::from(opacity) << 24) | 0x00ff_ffff
}

pub(crate) fn validate_boot_contract(output_width: u32, output_height: u32) -> bool {
    let mut compiler = match GpuSceneCompiler::new(BOOT_CONTEXT_ID, BOOT_CONTEXT_EPOCH) {
        Ok(compiler) => compiler,
        Err(_) => return false,
    };
    let layer = GpuSceneLayer {
        destination: Rect {
            x: 0,
            y: 0,
            width: usize::try_from(output_width.min(64)).unwrap_or_default(),
            height: usize::try_from(output_height.min(64)).unwrap_or_default(),
        },
        opacity: u8::MAX,
        source_over: false,
        transform: GpuLayerTransform {
            depth: DVM_GPU_FIXED_ONE / 8,
            rotation: DVM_GPU_FIXED_ONE / 32,
            tilt_x: DVM_GPU_FIXED_ONE / 64,
            tilt_y: 0,
            perspective: DVM_GPU_FIXED_ONE / 64,
        },
        kind: GpuLayerKind::Solid { rgba: 0xff20_20e0 },
    };
    if compiler.begin_prime(1).is_err()
        || compiler
            .complete_prime(DvmGpuPrimeCompletion {
                context_id: BOOT_CONTEXT_ID,
                context_epoch: BOOT_CONTEXT_EPOCH,
                status: driver_domain_protocol::DvmGpuPrimeCompletionStatus::Ready,
                submit_flags: driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
                fence_value: 1,
                duration_ns: 1,
            })
            .is_err()
        || compiler.signal_acquire(1).is_err()
    {
        return false;
    }
    let batch = match compiler.compile(output_width, output_height, 1, 0xff18_1010, &[layer]) {
        Ok(batch) => batch,
        Err(_) => return false,
    };
    let encoded = match batch.encode() {
        Ok(encoded) => encoded,
        Err(_) => return false,
    };
    if encoded.len() != batch.header.encoded_batch_len().unwrap_or_default() {
        return false;
    }
    let completion = DvmGpuRenderCompletion {
        context_id: BOOT_CONTEXT_ID,
        context_epoch: BOOT_CONTEXT_EPOCH,
        status: DvmGpuRenderCompletionStatus::Completed,
        output_index: 0,
        submit_value: 1,
        completion_value: 1,
        render_time_ns: 1,
        release_value: 1,
    };
    let present = DvmGpuPresentCompletion {
        context_id: BOOT_CONTEXT_ID,
        context_epoch: BOOT_CONTEXT_EPOCH,
        output_index: 0,
        submit_value: 1,
        present_value: 1,
        previous_submit_value: 0,
        present_time_ns: 1,
    };
    if compiler.complete(completion).is_err() || compiler.present(present).is_err() {
        return false;
    }
    compiler.revoke();
    compiler.reset(2).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready_compiler() -> GpuSceneCompiler {
        let mut compiler = GpuSceneCompiler::new(9, 4).unwrap();
        compiler.begin_prime(1).unwrap();
        compiler
            .complete_prime(DvmGpuPrimeCompletion {
                context_id: 9,
                context_epoch: 4,
                status: driver_domain_protocol::DvmGpuPrimeCompletionStatus::Ready,
                submit_flags: driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY,
                fence_value: 1,
                duration_ns: 1,
            })
            .unwrap();
        compiler.signal_acquire(1).unwrap();
        compiler
    }

    fn texture(token: u64, generation: u64) -> GpuTextureCapability {
        GpuTextureCapability {
            token,
            generation,
            content_epoch: generation,
            binding_slot: 2,
            width: 320,
            height: 200,
            stride_bytes: 1280,
        }
    }

    fn layer(source: GpuTextureCapability, x: usize) -> GpuSceneLayer {
        GpuSceneLayer {
            destination: Rect {
                x,
                y: 20,
                width: 320,
                height: 200,
            },
            opacity: 220,
            source_over: true,
            transform: GpuLayerTransform::flat(),
            kind: GpuLayerKind::Texture {
                region: GpuTextureRegion {
                    atlas: source,
                    source_rect: Rect {
                        x: 0,
                        y: 0,
                        width: 320,
                        height: 200,
                    },
                },
            },
        }
    }

    #[test]
    fn scene_compiler_deduplicates_capabilities_and_preserves_z_order() {
        let mut compiler = ready_compiler();
        let source = texture(0x100, 2);
        let batch = compiler
            .compile(
                1280,
                720,
                1,
                0xff00_0000,
                &[layer(source, 10), layer(source, 400)],
            )
            .unwrap();
        assert_eq!(batch.sources.len(), 1);
        assert_eq!(batch.commands.len(), 3);
        assert_eq!(batch.commands[1].destination_x, 10);
        assert_eq!(batch.commands[2].destination_x, 400);
        assert_eq!(batch.commands[1].source_index, 0);
        assert_eq!(batch.commands[2].source_index, 0);
        assert_eq!(batch.sources[0].binding_slot, 2);
        assert_eq!(batch.commands[1].source_width, u16::MAX);
        assert_eq!(batch.commands[1].source_height, u16::MAX);
        assert_eq!(
            batch.encode().unwrap().len(),
            batch.header.encoded_batch_len().unwrap()
        );
    }

    #[test]
    fn scene_compiler_normalizes_atlas_subrect_and_rejects_escape() {
        let mut compiler = ready_compiler();
        let atlas = texture(0x200, 3);
        let mut textured = layer(atlas, 10);
        let GpuLayerKind::Texture { ref mut region } = textured.kind else {
            unreachable!();
        };
        region.source_rect = Rect {
            x: 160,
            y: 100,
            width: 80,
            height: 50,
        };
        let batch = compiler.compile(1280, 720, 1, 0, &[textured]).unwrap();
        assert_eq!(batch.commands[1].source_u, 32_767);
        assert_eq!(batch.commands[1].source_v, 32_767);
        assert_eq!(batch.commands[1].source_width, 16_385);
        assert_eq!(batch.commands[1].source_height, 16_385);

        let mut outside = layer(atlas, 10);
        let GpuLayerKind::Texture { ref mut region } = outside.kind else {
            unreachable!();
        };
        region.source_rect.x = 319;
        assert_eq!(
            compiler.compile(1280, 720, 1, 0, &[outside]),
            Err(GpuSceneError::InvalidLayer)
        );
    }

    #[test]
    fn atlas_packer_isolated_regions_and_fails_closed_when_full() {
        let mut pixels = vec![0xffff_ffff; 8 * 6];
        let first = [1_u32, 2, 3, 4];
        let second = [5_u32, 6, 7, 8];
        {
            let mut packer = GpuAtlasPacker::new(&mut pixels, 8, 6, 8).unwrap();
            let first_rect = packer.place_bgra(&first, 2, 2, 2).unwrap();
            let second_rect = packer.place_bgra(&second, 2, 2, 2).unwrap();
            assert_eq!(
                first_rect,
                Rect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2
                }
            );
            assert_eq!(
                second_rect,
                Rect {
                    x: 5,
                    y: 1,
                    width: 2,
                    height: 2
                }
            );
            assert_eq!(
                packer.place_bgra(&first, 2, 2, 2),
                Err(GpuSceneError::AtlasFull)
            );
        }
        assert_eq!(pixels[9], 1);
        assert_eq!(pixels[13], 5);
        assert_eq!(pixels[0], 0);
        assert_eq!(pixels[4], 0);
    }

    #[test]
    fn xrgb_row_conversion_preserves_color_and_forces_opaque_alpha() {
        let source = [
            0x0000_0000,
            0x0012_3456,
            0x7f65_4321,
            0xffab_cdef,
            0x0001_0203,
        ];
        let mut destination = [0_u32; 5];
        copy_xrgb_opaque_row(&mut destination, &source);
        assert_eq!(
            destination,
            [
                0xff00_0000,
                0xff12_3456,
                0xff65_4321,
                0xffab_cdef,
                0xff01_0203,
            ]
        );
    }

    #[test]
    fn scene_compiler_rejects_rebound_token_and_out_of_bounds_layer() {
        let mut compiler = ready_compiler();
        assert_eq!(
            compiler.compile(
                1280,
                720,
                1,
                0,
                &[layer(texture(7, 2), 0), layer(texture(7, 4), 400)]
            ),
            Err(GpuSceneError::TokenRebound)
        );
        assert_eq!(
            compiler.compile(1280, 720, 1, 0, &[layer(texture(8, 2), 1100)]),
            Err(GpuSceneError::InvalidLayer)
        );
    }

    #[test]
    fn boot_contract_exercises_3d_transform_and_epoch_reset() {
        assert!(validate_boot_contract(1600, 900));
        assert!(!validate_boot_contract(0, 900));
    }
}
