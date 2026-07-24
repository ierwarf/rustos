use core::mem::size_of;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use std::vec::Vec;

use driver_domain_protocol::{
    DvmGpuAtlasCompletion, DvmGpuAtlasDamage, DvmGpuPrimeCompletion, DvmGpuPrimeCompletionStatus,
};
use rustos_user_abi::device::{DISPLAY_INFO_FLAG_DVM_SCANOUT, DISPLAY_INFO_FLAG_GPU_COMPOSITOR};

use crate::app::{AppState, CursorMotion};
use crate::canvas::Rect;
use crate::gpu_scene::{
    GpuLayerKind, GpuSceneCompiler, GpuSceneError, GpuSceneLayer, GpuTextureCapability,
};
use crate::render::{
    build_gpu_atlas_scene, gpu_cursor_layer, gpu_scene_reuse_signature, update_gpu_cursor_texture,
    update_wayland_gpu_textures, GpuAtlasScene, WaylandAtlasBinding,
};
use crate::sys::{
    debug_line, diag_line, display_create_gpu_atlas_surface, display_get_info,
    display_gpu_get_info, display_gpu_query_completion, display_gpu_submit, map_surface,
    require_background_thread_class, DisplayGpuInfo, DisplayInfo, DisplaySurfaceCreate,
    SurfaceMapping, EAGAIN, EINVAL, ENODEV, ESTALE,
};

const GPU_READY_RETRY: Duration = Duration::from_millis(50);
const GPU_READY_TIMEOUT: Duration = Duration::from_secs(20);
const GPU_FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
const GPU_COMPLETION_TIMEOUT: Duration = Duration::from_millis(50);
const GPU_COMPLETION_POLL: Duration = Duration::from_micros(100);
// QEMU has no trustworthy display-vblank clock to export to uiserver yet.
// Pace the private compositor just above 60 Hz instead of submitting at the
// input-source rate. A cumulative deadline avoids drift, and missed slots are
// skipped rather than burst, preserving one bounded frame of backpressure.
const GPU_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const ETIMEDOUT: i32 = 110;
const EIO: i32 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GpuProviderAdmission {
    SoftwareFallback,
    WaitForDvmGpu,
    Ready,
    Invalid,
}

fn gpu_provider_admission(display: DisplayInfo) -> GpuProviderAdmission {
    match (
        display.flags & DISPLAY_INFO_FLAG_DVM_SCANOUT != 0,
        display.flags & DISPLAY_INFO_FLAG_GPU_COMPOSITOR != 0,
    ) {
        (false, false) => GpuProviderAdmission::SoftwareFallback,
        (true, false) => GpuProviderAdmission::WaitForDvmGpu,
        (true, true) => GpuProviderAdmission::Ready,
        (false, true) => GpuProviderAdmission::Invalid,
    }
}

fn same_display_contract(expected: DisplayInfo, current: DisplayInfo) -> bool {
    expected.width == current.width
        && expected.height == current.height
        && expected.stride_bytes == current.stride_bytes
        && expected.bytes_per_pixel == current.bytes_per_pixel
        && expected.pixel_format == current.pixel_format
        && expected.generation == current.generation
}

struct GpuAtlasSlot {
    surface: DisplaySurfaceCreate,
    _surface_fd: OwnedFd,
    mapping: SurfaceMapping,
    /// Last complete atlas snapshot retained by this specific backing slot.
    /// Zero means the slot has never carried a submitted snapshot.
    content_epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingGpuFrame {
    slot_index: usize,
    surface_handle: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AtlasDamageEpoch {
    epoch: u64,
    damage: Vec<DvmGpuAtlasDamage>,
}

pub(crate) struct GpuCompositor {
    display_fd: RawFd,
    info: DisplayGpuInfo,
    slots: Vec<GpuAtlasSlot>,
    compiler: GpuSceneCompiler,
    atlas: Vec<u32>,
    layers: Vec<GpuSceneLayer>,
    cursor_source_rect: Option<Rect>,
    cursor_motion: CursorMotion,
    retained_scene_signature: u64,
    wayland_bindings: Vec<WaylandAtlasBinding>,
    next_slot: usize,
    next_content_epoch: u64,
    damage_history: VecDeque<AtlasDamageEpoch>,
    pending: Vec<PendingGpuFrame>,
    active: bool,
    activation_deadline: Instant,
    next_submission_at: Instant,
    scene_wait_logged: bool,
}

pub(crate) struct GpuInitialization {
    display: DisplayInfo,
    compositor: GpuCompositor,
}

pub(crate) type GpuInitializationResult = Result<Option<GpuInitialization>, i32>;

pub(crate) enum GpuCompositorRuntime {
    SoftwareFallback,
    Waiting {
        deadline: Instant,
        next_probe: Instant,
        initialization: Option<Receiver<GpuInitializationResult>>,
    },
    Active(GpuCompositor),
}

impl GpuCompositorRuntime {
    pub(crate) fn new(display_fd: RawFd, display: DisplayInfo) -> Result<Self, i32> {
        let now = Instant::now();
        match gpu_provider_admission(display) {
            GpuProviderAdmission::SoftwareFallback => Ok(Self::SoftwareFallback),
            GpuProviderAdmission::WaitForDvmGpu | GpuProviderAdmission::Ready => {
                debug_line("uiserver: waiting for mandatory DVM GPU compositor");
                debug_line("uiserver: GPU atlas initialization dispatched during boot");
                Ok(Self::Waiting {
                    deadline: now + GPU_READY_TIMEOUT,
                    next_probe: now + GPU_READY_RETRY,
                    initialization: Some(start_gpu_initialization(display_fd, display)?),
                })
            }
            GpuProviderAdmission::Invalid => Err(EINVAL),
        }
    }

    pub(crate) fn poll(
        &mut self,
        display_fd: RawFd,
        display: &mut DisplayInfo,
    ) -> Result<bool, i32> {
        match self {
            Self::SoftwareFallback => Ok(false),
            Self::Active(compositor) => match compositor.poll_completions() {
                Ok(()) => Ok(false),
                Err(ENODEV) => {
                    debug_line("uiserver: DVM GPU compositor offline; bounded recovery started");
                    *self = Self::waiting(Instant::now());
                    Ok(false)
                }
                Err(err) => Err(err),
            },
            Self::Waiting {
                deadline,
                next_probe,
                initialization,
            } => {
                let now = Instant::now();
                if now >= *deadline {
                    debug_line("uiserver: mandatory DVM GPU compositor readiness timed out");
                    return Err(ETIMEDOUT);
                }
                if let Some(receiver) = initialization.as_ref() {
                    match receiver.try_recv() {
                        Ok(Ok(Some(initialized))) => {
                            if !same_display_contract(*display, initialized.display) {
                                return Err(ESTALE);
                            }
                            *display = initialized.display;
                            *self = Self::Active(initialized.compositor);
                            debug_line(
                                "uiserver: GPU atlas initialization completed off UI thread",
                            );
                            return Ok(true);
                        }
                        Ok(Ok(None)) => {
                            *initialization = None;
                            *next_probe = now + GPU_READY_RETRY;
                            return Ok(false);
                        }
                        Ok(Err(err)) => return Err(err),
                        Err(TryRecvError::Empty) => return Ok(false),
                        Err(TryRecvError::Disconnected) => return Err(EIO),
                    }
                }
                if now < *next_probe {
                    return Ok(false);
                }
                *next_probe = now + GPU_READY_RETRY;
                let current_display = display_get_info(display_fd)?;
                if !same_display_contract(*display, current_display) {
                    return Err(ESTALE);
                }
                match gpu_provider_admission(current_display) {
                    GpuProviderAdmission::Ready => {
                        debug_line("uiserver: GPU atlas initialization dispatched off UI thread");
                        *initialization =
                            Some(start_gpu_initialization(display_fd, current_display)?);
                    }
                    GpuProviderAdmission::WaitForDvmGpu => {}
                    GpuProviderAdmission::SoftwareFallback | GpuProviderAdmission::Invalid => {
                        return Err(ESTALE);
                    }
                }
                Ok(false)
            }
        }
    }

    pub(crate) fn present(&mut self, state: &mut AppState, scene_dirty: bool) -> Result<bool, i32> {
        match self {
            Self::Active(compositor) => match compositor.present(state, scene_dirty) {
                Ok(presented) => Ok(presented),
                Err(ENODEV) => {
                    debug_line("uiserver: DVM GPU compositor lost during submit; recovering");
                    *self = Self::waiting(Instant::now());
                    Ok(false)
                }
                Err(err) => Err(err),
            },
            Self::SoftwareFallback | Self::Waiting { .. } => Ok(false),
        }
    }

    fn waiting(now: Instant) -> Self {
        Self::Waiting {
            deadline: now + GPU_READY_TIMEOUT,
            next_probe: now,
            initialization: None,
        }
    }
}

fn start_gpu_initialization(
    display_fd: RawFd,
    expected_display: DisplayInfo,
) -> Result<Receiver<GpuInitializationResult>, i32> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("uiserver-gpu-init".into())
        .spawn(move || {
            debug_line("uiserver: GPU initialization worker started");
            require_background_thread_class();
            debug_line("uiserver: GPU initialization worker entered background class");
            let deadline = Instant::now() + GPU_READY_TIMEOUT;
            let result = loop {
                match GpuCompositor::try_initialize(display_fd, expected_display) {
                    Ok(None) if Instant::now() < deadline => thread::sleep(GPU_READY_RETRY),
                    Ok(None) => break Err(ETIMEDOUT),
                    terminal => break terminal,
                }
            };
            let _ = sender.send(result);
        })
        .map_err(|_| EAGAIN)?;
    Ok(receiver)
}

impl GpuCompositor {
    fn try_initialize(display_fd: RawFd, expected_display: DisplayInfo) -> GpuInitializationResult {
        let current_display = display_get_info(display_fd)?;
        if !same_display_contract(expected_display, current_display) {
            return Err(ESTALE);
        }
        let info = match gpu_provider_admission(current_display) {
            GpuProviderAdmission::Ready => match display_gpu_get_info(display_fd) {
                Ok(info) => info,
                Err(EAGAIN | ENODEV) => return Ok(None),
                Err(err) => return Err(err),
            },
            GpuProviderAdmission::WaitForDvmGpu => return Ok(None),
            GpuProviderAdmission::SoftwareFallback | GpuProviderAdmission::Invalid => {
                return Err(ESTALE);
            }
        };
        if info.generation != current_display.generation {
            return Err(ESTALE);
        }
        debug_line(&format!(
            "uiserver: display_get_info gpu-ready width={} height={} stride={} bpp={} fmt={} flags={:#x} gen={}",
            current_display.width,
            current_display.height,
            current_display.stride_bytes,
            current_display.bytes_per_pixel,
            current_display.pixel_format,
            current_display.flags,
            current_display.generation,
        ));
        let logical_bytes = usize::try_from(info.atlas_stride_bytes)
            .ok()
            .and_then(|stride| stride.checked_mul(info.atlas_height as usize))
            .ok_or(EINVAL)?;
        if logical_bytes == 0 || !logical_bytes.is_multiple_of(size_of::<u32>()) {
            return Err(EINVAL);
        }
        let mut slots = Vec::with_capacity(info.slot_count as usize);
        for _ in 0..info.slot_count {
            let surface = display_create_gpu_atlas_surface(display_fd, info)?;
            let raw_fd = i32::try_from(surface.handle).map_err(|_| EINVAL)?;
            let surface_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let mapping_len = usize::try_from(surface.mapping_len).map_err(|_| EINVAL)?;
            let mapping = map_surface(surface_fd.as_raw_fd(), mapping_len)?;
            slots.push(GpuAtlasSlot {
                surface,
                _surface_fd: surface_fd,
                mapping,
                content_epoch: 0,
            });
        }
        slots.sort_by_key(|slot| slot.surface.reserved);
        if slots.len() != info.slot_count as usize
            || slots
                .iter()
                .enumerate()
                .any(|(index, slot)| slot.surface.reserved as usize != index)
        {
            return Err(EINVAL);
        }
        let mut compiler =
            GpuSceneCompiler::new(info.context_id, info.context_epoch).map_err(gpu_scene_errno)?;
        compiler
            .begin_prime(info.prime_fence_value)
            .map_err(gpu_scene_errno)?;
        // GPU_GET_INFO carries the DVM-produced, kernel-module-validated
        // completion for the fixed GLES pipeline and initial KMS frame. The
        // local scheduler advances only from that measured transport record.
        compiler
            .complete_prime(DvmGpuPrimeCompletion {
                context_id: info.context_id,
                context_epoch: info.context_epoch,
                status: DvmGpuPrimeCompletionStatus::Ready,
                submit_flags: match info.flags {
                    rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_STAGED_COPY => {
                        driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY
                    }
                    rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF => {
                        driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
                    }
                    _ => return Err(EINVAL),
                },
                fence_value: info.prime_fence_value,
                duration_ns: info.prime_duration_ns,
            })
            .map_err(gpu_scene_errno)?;
        Ok(Some(GpuInitialization {
            display: current_display,
            compositor: Self {
                display_fd,
                info,
                slots,
                compiler,
                atlas: vec![0_u32; logical_bytes / size_of::<u32>()],
                layers: Vec::new(),
                cursor_source_rect: None,
                cursor_motion: CursorMotion::stationary(),
                retained_scene_signature: 0,
                wayland_bindings: Vec::new(),
                next_slot: 0,
                next_content_epoch: 1,
                damage_history: VecDeque::with_capacity(info.slot_count as usize + 1),
                pending: Vec::with_capacity(info.slot_count as usize),
                active: false,
                activation_deadline: Instant::now() + GPU_FIRST_FRAME_TIMEOUT,
                next_submission_at: Instant::now(),
                scene_wait_logged: false,
            },
        }))
    }

    pub(crate) fn present(&mut self, state: &mut AppState, scene_dirty: bool) -> Result<bool, i32> {
        while self.retire_oldest(false)? {}
        let submission_started = Instant::now();
        if self.active && submission_started < self.next_submission_at {
            return Err(EAGAIN);
        }
        let Some(slot_index) = self.select_submission_slot() else {
            // Steady-state completion waits never own the UI thread. The
            // retained update is retried after the next input/Wayland turn.
            return Err(EAGAIN);
        };
        let mut damage = Vec::new();
        let current_scene_signature = gpu_scene_reuse_signature(state);
        let mut full_rebuild = !self.active
            || (scene_dirty && current_scene_signature != self.retained_scene_signature);
        if scene_dirty && !full_rebuild {
            match update_wayland_gpu_textures(
                state,
                self.atlas.as_mut_slice(),
                self.info.atlas_width as usize,
                self.info.atlas_height as usize,
                self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                self.wayland_bindings.as_mut_slice(),
            )
            .map_err(gpu_scene_errno)?
            {
                Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                None => full_rebuild = true,
            }
        }
        if full_rebuild {
            let mut next_atlas = vec![0_u32; self.atlas.len()];
            let capability = self.capability_for_slot(slot_index, self.next_content_epoch)?;
            let GpuAtlasScene {
                layers,
                cursor_source_rect,
                wayland_bindings,
            } = match build_gpu_atlas_scene(
                state,
                next_atlas.as_mut_slice(),
                self.info.atlas_width as usize,
                self.info.atlas_height as usize,
                self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                capability,
            ) {
                Ok(scene) => scene,
                Err(GpuSceneError::SceneNotReady) if !self.active => {
                    if Instant::now() >= self.activation_deadline {
                        debug_line("uiserver: first GPU frame readiness timed out");
                        return Err(ETIMEDOUT);
                    }
                    if !self.scene_wait_logged {
                        debug_line("uiserver: first GPU frame waiting for retained scene");
                        self.scene_wait_logged = true;
                    }
                    return Ok(false);
                }
                Err(err) => return Err(gpu_scene_errno(err)),
            };
            damage = vec![DvmGpuAtlasDamage {
                x: 0,
                y: 0,
                width: self.info.atlas_width,
                height: self.info.atlas_height,
            }];
            self.atlas = next_atlas;
            self.layers = layers;
            self.cursor_source_rect = Some(cursor_source_rect);
            self.cursor_motion = state.cursor_motion;
            self.wayland_bindings = wayland_bindings;
            self.retained_scene_signature = gpu_scene_reuse_signature(state);
        } else if state.cursor_motion != self.cursor_motion {
            let cursor_source_rect = self.cursor_source_rect.ok_or(EINVAL)?;
            update_gpu_cursor_texture(
                self.atlas.as_mut_slice(),
                self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                cursor_source_rect,
                state.cursor_motion,
            )
            .map_err(gpu_scene_errno)?;
            damage.push(damage_from_rect(cursor_source_rect));
            self.cursor_motion = state.cursor_motion;
        }

        let capability = self.capability_for_slot(slot_index, self.next_content_epoch)?;
        let mut layers = self.layers.clone();
        rebind_layers(layers.as_mut_slice(), capability);
        if let Some(cursor) = gpu_cursor_layer(
            capability,
            self.cursor_source_rect.ok_or(EINVAL)?,
            state.cursor_x,
            state.cursor_y,
            state.surface.width,
            state.surface.height,
        ) {
            layers.push(cursor);
        }
        self.compiler
            .signal_acquire(self.next_content_epoch)
            .map_err(gpu_scene_errno)?;
        let batch = self
            .compiler
            .compile(
                state.surface.width,
                state.surface.height,
                self.next_content_epoch,
                0xff00_0000,
                layers.as_slice(),
            )
            .map_err(gpu_scene_errno)?;
        let encoded = batch.encode().map_err(gpu_scene_errno)?;
        // Damage is relative to the immediately preceding complete contents of
        // this backing slot, not merely to the last globally submitted frame.
        // Triple-buffer rotation normally revisits an older slot; in that case
        // a partial patch would expose stale or zero pixels and visibly
        // alternate complete/incomplete frames.  Fail over to a complete
        // snapshot until the selected slot is the exact predecessor.
        let submit_damage = snapshot_damage_for_slot(
            self.slots[slot_index].content_epoch,
            self.next_content_epoch,
            damage.as_slice(),
            &self.damage_history,
            self.info.atlas_width,
            self.info.atlas_height,
        )?;
        copy_damage_to_slot(
            self.atlas.as_slice(),
            &mut self.slots[slot_index].mapping,
            self.info.atlas_stride_bytes as usize / size_of::<u32>(),
            submit_damage.as_slice(),
        )?;
        let surface_handle = self.slots[slot_index].surface.handle;
        display_gpu_submit(
            self.display_fd,
            surface_handle,
            submit_damage.as_slice(),
            encoded.as_slice(),
        )?;
        self.slots[slot_index].content_epoch = self.next_content_epoch;
        self.pending.push(PendingGpuFrame {
            slot_index,
            surface_handle,
        });
        self.damage_history.push_back(AtlasDamageEpoch {
            epoch: self.next_content_epoch,
            damage,
        });
        while self.damage_history.len() > self.slots.len() {
            self.damage_history.pop_front();
        }
        self.next_submission_at = next_frame_deadline(self.next_submission_at, submission_started);
        if !self.active {
            if !self.retire_oldest(true)? {
                return Err(EINVAL);
            }
            let (source_path, zero_copy) = match self.info.flags {
                rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_STAGED_COPY => ("staged-copy", 0),
                rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF => ("dmabuf", 1),
                _ => return Err(EINVAL),
            };
            let active_contract = format!(
                "uiserver: gpu-compositor active contract=3 source-path={} zero-copy={} public-abi=0 atlas={}x{} damage-rects={}",
                source_path,
                zero_copy,
                self.info.atlas_width,
                self.info.atlas_height,
                submit_damage.len(),
            );
            // This one-shot transition is release evidence. Do not let a full
            // asynchronous observability queue erase it.
            debug_line(&active_contract);
            diag_line(active_contract);
        }
        self.active = true;
        self.next_slot = (slot_index + 1) % self.slots.len();
        self.next_content_epoch = self.next_content_epoch.checked_add(1).ok_or(EINVAL)?;
        Ok(true)
    }

    /// Prefer a completed slot that already contains the exact predecessor
    /// snapshot. Reusing it is safe only after the DVM completion releases
    /// that slot, and lets ordinary cursor/damage updates stay incremental
    /// instead of forcing a full atlas copy on every round-robin turn.
    fn select_submission_slot(&self) -> Option<usize> {
        let predecessor = self.next_content_epoch.checked_sub(1)?;
        let is_pending = |slot_index| {
            self.pending
                .iter()
                .any(|frame| frame.slot_index == slot_index)
        };
        for offset in 0..self.slots.len() {
            let slot_index = (self.next_slot + offset) % self.slots.len();
            if !is_pending(slot_index) && self.slots[slot_index].content_epoch == predecessor {
                return Some(slot_index);
            }
        }
        (0..self.slots.len())
            .map(|offset| (self.next_slot + offset) % self.slots.len())
            .find(|&slot_index| !is_pending(slot_index))
    }

    pub(crate) fn poll_completions(&mut self) -> Result<(), i32> {
        if !self.active && Instant::now() >= self.activation_deadline {
            debug_line("uiserver: first GPU frame readiness timed out");
            return Err(ETIMEDOUT);
        }
        while self.retire_oldest(false)? {}
        Ok(())
    }

    fn capability_for_slot(
        &self,
        slot_index: usize,
        content_epoch: u64,
    ) -> Result<GpuTextureCapability, i32> {
        let slot = self.slots.get(slot_index).ok_or(EINVAL)?;
        Ok(GpuTextureCapability {
            token: u64::from(slot.surface.handle),
            generation: slot.surface.generation,
            content_epoch,
            binding_slot: slot.surface.reserved,
            width: self.info.atlas_width,
            height: self.info.atlas_height,
            stride_bytes: self.info.atlas_stride_bytes,
        })
    }

    fn retire_oldest(&mut self, blocking: bool) -> Result<bool, i32> {
        let Some(frame) = self.pending.first().copied() else {
            return Ok(false);
        };
        let deadline = blocking.then(|| Instant::now() + GPU_COMPLETION_TIMEOUT);
        loop {
            match display_gpu_query_completion(self.display_fd, frame.surface_handle) {
                Ok(query) => {
                    let completion =
                        DvmGpuAtlasCompletion::decode(&query.completion).ok_or(EINVAL)?;
                    self.compiler
                        .complete(completion.render)
                        .map_err(gpu_scene_errno)?;
                    self.compiler
                        .present(completion.present)
                        .map_err(gpu_scene_errno)?;
                    self.pending.remove(0);
                    return Ok(true);
                }
                Err(EAGAIN) if !blocking => return Ok(false),
                Err(EAGAIN) if deadline.is_some_and(|end| Instant::now() < end) => {
                    thread::sleep(GPU_COMPLETION_POLL);
                }
                Err(EAGAIN) => return Err(ETIMEDOUT),
                Err(err) => return Err(err),
            }
        }
    }
}

fn next_frame_deadline(mut deadline: Instant, now: Instant) -> Instant {
    deadline += GPU_FRAME_INTERVAL;
    while deadline <= now {
        deadline += GPU_FRAME_INTERVAL;
    }
    deadline
}

fn rebind_layers(layers: &mut [GpuSceneLayer], capability: GpuTextureCapability) {
    for layer in layers {
        if let GpuLayerKind::Texture { region } = &mut layer.kind {
            region.atlas = capability;
        }
    }
}

fn difference_bounds(
    previous: &[u32],
    next: &[u32],
    width: usize,
    height: usize,
    stride: usize,
) -> Option<Rect> {
    if previous.len() != next.len() || width == 0 || height == 0 || stride < width {
        return None;
    }
    let mut left = width;
    let mut top = height;
    let mut right = 0;
    let mut bottom = 0;
    for y in 0..height {
        let row = y.checked_mul(stride)?;
        for x in 0..width {
            let index = row.checked_add(x)?;
            if previous.get(index) != next.get(index) {
                left = left.min(x);
                top = top.min(y);
                right = right.max(x + 1);
                bottom = bottom.max(y + 1);
            }
        }
    }
    if left < right && top < bottom {
        Some(Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    } else {
        None
    }
}

fn damage_from_rect(rect: Rect) -> DvmGpuAtlasDamage {
    DvmGpuAtlasDamage {
        x: rect.x as u32,
        y: rect.y as u32,
        width: rect.width as u32,
        height: rect.height as u32,
    }
}

fn snapshot_damage_for_slot(
    retained_epoch: u64,
    current_epoch: u64,
    requested: &[DvmGpuAtlasDamage],
    history: &VecDeque<AtlasDamageEpoch>,
    atlas_width: u32,
    atlas_height: u32,
) -> Result<Vec<DvmGpuAtlasDamage>, i32> {
    let predecessor = current_epoch.checked_sub(1).ok_or(EINVAL)?;
    if retained_epoch != 0 && retained_epoch == predecessor {
        validate_damage(requested, atlas_width, atlas_height)?;
        return Ok(requested.to_vec());
    }
    if atlas_width == 0 || atlas_height == 0 {
        return Err(EINVAL);
    }
    if retained_epoch == 0 {
        return Ok(full_atlas_damage(atlas_width, atlas_height));
    }
    if retained_epoch >= current_epoch {
        return Err(EINVAL);
    }

    let mut expected_epoch = retained_epoch.checked_add(1).ok_or(EINVAL)?;
    let mut union = None;
    for entry in history {
        if entry.epoch < expected_epoch {
            continue;
        }
        if entry.epoch >= current_epoch {
            break;
        }
        if entry.epoch != expected_epoch {
            return Ok(full_atlas_damage(atlas_width, atlas_height));
        }
        accumulate_damage(
            &mut union,
            entry.damage.as_slice(),
            atlas_width,
            atlas_height,
        )?;
        expected_epoch = expected_epoch.checked_add(1).ok_or(EINVAL)?;
    }
    if expected_epoch != current_epoch {
        return Ok(full_atlas_damage(atlas_width, atlas_height));
    }
    accumulate_damage(&mut union, requested, atlas_width, atlas_height)?;
    Ok(union.into_iter().collect())
}

fn full_atlas_damage(atlas_width: u32, atlas_height: u32) -> Vec<DvmGpuAtlasDamage> {
    vec![DvmGpuAtlasDamage {
        x: 0,
        y: 0,
        width: atlas_width,
        height: atlas_height,
    }]
}

fn validate_damage(
    damage: &[DvmGpuAtlasDamage],
    atlas_width: u32,
    atlas_height: u32,
) -> Result<(), i32> {
    for rect in damage {
        let right = rect.x.checked_add(rect.width).ok_or(EINVAL)?;
        let bottom = rect.y.checked_add(rect.height).ok_or(EINVAL)?;
        if rect.width == 0 || rect.height == 0 || right > atlas_width || bottom > atlas_height {
            return Err(EINVAL);
        }
    }
    Ok(())
}

fn accumulate_damage(
    union: &mut Option<DvmGpuAtlasDamage>,
    damage: &[DvmGpuAtlasDamage],
    atlas_width: u32,
    atlas_height: u32,
) -> Result<(), i32> {
    validate_damage(damage, atlas_width, atlas_height)?;
    for rect in damage {
        let right = rect.x.checked_add(rect.width).ok_or(EINVAL)?;
        let bottom = rect.y.checked_add(rect.height).ok_or(EINVAL)?;
        *union = Some(match *union {
            Some(current) => {
                let current_right = current.x.checked_add(current.width).ok_or(EINVAL)?;
                let current_bottom = current.y.checked_add(current.height).ok_or(EINVAL)?;
                let x = current.x.min(rect.x);
                let y = current.y.min(rect.y);
                DvmGpuAtlasDamage {
                    x,
                    y,
                    width: current_right.max(right).checked_sub(x).ok_or(EINVAL)?,
                    height: current_bottom.max(bottom).checked_sub(y).ok_or(EINVAL)?,
                }
            }
            None => *rect,
        });
    }
    Ok(())
}

fn copy_damage_to_slot(
    atlas: &[u32],
    mapping: &mut SurfaceMapping,
    stride: usize,
    damage: &[DvmGpuAtlasDamage],
) -> Result<(), i32> {
    let destination = mapping.pixels_mut();
    for rect in damage {
        let x = rect.x as usize;
        let width = rect.width as usize;
        for y in rect.y as usize..(rect.y + rect.height) as usize {
            let start = y
                .checked_mul(stride)
                .and_then(|offset| offset.checked_add(x))
                .ok_or(EINVAL)?;
            let end = start.checked_add(width).ok_or(EINVAL)?;
            destination
                .get_mut(start..end)
                .ok_or(EINVAL)?
                .copy_from_slice(atlas.get(start..end).ok_or(EINVAL)?);
        }
    }
    Ok(())
}

fn gpu_scene_errno(_error: GpuSceneError) -> i32 {
    EINVAL
}

#[cfg(test)]
mod tests {
    use super::{
        difference_bounds, gpu_provider_admission, next_frame_deadline, snapshot_damage_for_slot,
        AtlasDamageEpoch, DvmGpuAtlasDamage, GpuProviderAdmission, Rect,
        DISPLAY_INFO_FLAG_DVM_SCANOUT, DISPLAY_INFO_FLAG_GPU_COMPOSITOR, GPU_FRAME_INTERVAL,
    };
    use crate::sys::DisplayInfo;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    fn display_with_flags(flags: u32) -> DisplayInfo {
        DisplayInfo {
            width: 1600,
            height: 900,
            stride_bytes: 7168,
            bytes_per_pixel: 4,
            pixel_format: 1,
            flags,
            generation: 1,
        }
    }

    #[test]
    fn dvm_gpu_admission_waits_without_hiding_behind_software() {
        assert_eq!(
            gpu_provider_admission(display_with_flags(0)),
            GpuProviderAdmission::SoftwareFallback
        );
        assert_eq!(
            gpu_provider_admission(display_with_flags(DISPLAY_INFO_FLAG_DVM_SCANOUT)),
            GpuProviderAdmission::WaitForDvmGpu
        );
        assert_eq!(
            gpu_provider_admission(display_with_flags(
                DISPLAY_INFO_FLAG_DVM_SCANOUT | DISPLAY_INFO_FLAG_GPU_COMPOSITOR,
            )),
            GpuProviderAdmission::Ready
        );
        assert_eq!(
            gpu_provider_admission(display_with_flags(DISPLAY_INFO_FLAG_GPU_COMPOSITOR)),
            GpuProviderAdmission::Invalid
        );
    }

    #[test]
    fn difference_bounds_is_empty_for_identical_atlases() {
        let atlas = vec![0_u32; 4 * 3];
        assert_eq!(difference_bounds(&atlas, &atlas, 4, 3, 4), None);
    }

    #[test]
    fn difference_bounds_covers_only_the_changed_rectangle() {
        let previous = vec![0_u32; 6 * 4];
        let mut next = previous.clone();
        next[1 + 6] = 1;
        next[4 + 3 * 6] = 2;
        assert_eq!(
            difference_bounds(&previous, &next, 5, 4, 6),
            Some(Rect {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            })
        );
    }

    #[test]
    fn difference_bounds_rejects_incompatible_geometry() {
        assert_eq!(difference_bounds(&[0; 4], &[0; 5], 2, 2, 2), None);
        assert_eq!(difference_bounds(&[0; 4], &[0; 4], 3, 2, 2), None);
    }

    #[test]
    fn frame_deadline_skips_missed_slots_without_drift_or_burst() {
        let origin = Instant::now();
        assert_eq!(
            next_frame_deadline(origin, origin),
            origin + GPU_FRAME_INTERVAL
        );
        assert_eq!(
            next_frame_deadline(origin, origin + Duration::from_millis(49)),
            origin + Duration::from_millis(64)
        );
    }

    #[test]
    fn snapshot_damage_keeps_partial_patch_for_exact_slot_predecessor() {
        let requested = [DvmGpuAtlasDamage {
            x: 7,
            y: 11,
            width: 13,
            height: 17,
        }];
        assert_eq!(
            snapshot_damage_for_slot(8, 9, &requested, &VecDeque::new(), 1600, 900),
            Ok(requested.to_vec())
        );
    }

    #[test]
    fn snapshot_damage_forces_full_copy_for_uninitialized_or_stale_slot() {
        let requested = [DvmGpuAtlasDamage {
            x: 7,
            y: 11,
            width: 13,
            height: 17,
        }];
        let full = vec![DvmGpuAtlasDamage {
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
        }];
        assert_eq!(
            snapshot_damage_for_slot(0, 1, &requested, &VecDeque::new(), 1600, 900),
            Ok(full.clone())
        );
        assert_eq!(
            snapshot_damage_for_slot(6, 9, &requested, &VecDeque::new(), 1600, 900),
            Ok(full)
        );
    }

    #[test]
    fn snapshot_damage_replays_bounded_history_for_rotated_slot() {
        let history = VecDeque::from([
            AtlasDamageEpoch {
                epoch: 7,
                damage: vec![DvmGpuAtlasDamage {
                    x: 10,
                    y: 20,
                    width: 4,
                    height: 5,
                }],
            },
            AtlasDamageEpoch {
                epoch: 8,
                damage: vec![DvmGpuAtlasDamage {
                    x: 30,
                    y: 40,
                    width: 6,
                    height: 7,
                }],
            },
        ]);
        let requested = [DvmGpuAtlasDamage {
            x: 50,
            y: 60,
            width: 8,
            height: 9,
        }];
        assert_eq!(
            snapshot_damage_for_slot(6, 9, &requested, &history, 1600, 900),
            Ok(vec![DvmGpuAtlasDamage {
                x: 10,
                y: 20,
                width: 48,
                height: 49,
            }])
        );
    }
}
