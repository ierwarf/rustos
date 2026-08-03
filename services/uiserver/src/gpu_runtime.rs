//! Bounded GPU/DVM provider admission, submission, completion, and revoke.
//!
//! - **Owner:** `uiserver` owns scene/frame policy; the DVM owns driver/GPU
//!   execution and ring0 exposes fixed address-free substrate.
//! - **Boundary:** Provider descriptors, surface generations, damage, command
//!   batches, acquire/completion/present fences, and timings are untrusted.
//! - **Lifecycle:** Prime provider, admit exact context/slot, submit bounded
//!   frame, accept exact completion, present/release, timeout/revoke epoch, and
//!   re-prime before new admission.
//! - **Concurrency:** UI thread never blocks on late allocation or unbounded
//!   fence work; old and new provider epochs cannot share authority.
//! - **Failure:** Prime/frame timeout, malformed completion, device loss,
//!   restart, stale fence, and slot mismatch retain the last valid front or
//!   revoke explicitly.
//! - **Forbidden:** No CPU-render success fallback, client shader/raw command,
//!   address-bearing submit, clear-only prime, or stale completion revival.
//! - **Evidence:** `dvm-display-ingress`, `gpu-frame-lifecycle`, and
//!   `commercial-product-boot`.
use core::mem::size_of;
use std::boxed::Box;
use std::collections::VecDeque;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use std::vec::Vec;

use driver_domain_protocol::{
    DvmGpuAtlasCompletion, DvmGpuAtlasDamage, DvmGpuPrimeCompletion, DvmGpuPrimeCompletionStatus,
};
use rustos_user_abi::device::{DISPLAY_INFO_FLAG_DVM_SCANOUT, DISPLAY_INFO_FLAG_GPU_COMPOSITOR};
use rustos_user_abi::syscall::PRODUCT_MILESTONE_DISPLAY_READY;

use crate::app::{AppState, CursorMotion};
use crate::canvas::Rect;
use crate::gpu_scene::{
    GpuLayerKind, GpuSceneCompiler, GpuSceneError, GpuSceneLayer, GpuTextureCapability,
};
use crate::render::{
    build_gpu_atlas_scene, gpu_cursor_layer, gpu_scene_reuse_signature,
    update_console_gpu_textures, update_gpu_cursor_texture, update_gpu_layer_destinations,
    update_wayland_gpu_textures, ConsoleAtlasBinding, GpuAtlasScene, WaylandAtlasBinding,
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
// This one-shot worker is the mandatory boot compositor, not background
// policy. It inherits uiserver's admitted boot-critical class only until it
// publishes one result and exits; steady-state GPU work remains on the UI
// owner, while every unrelated long-lived helper demotes normally.
const GPU_INITIALIZATION_RETAINS_BOOT_CLASS: bool = true;
// An idle desktop may have no completion syscall to drain the kernel's
// wake-only DVM offline IRQ. Probe the descriptor at a bounded low rate so
// process replacement is detected without adding one syscall to every frame.
const GPU_PROVIDER_HEALTH_INTERVAL: Duration = Duration::from_millis(100);
const GPU_COMPLETION_TIMEOUT: Duration = Duration::from_millis(50);
const GPU_COMPLETION_POLL: Duration = Duration::from_micros(100);
const GPU_SLOW_SUBMIT_THRESHOLD: Duration = Duration::from_millis(8);
const MAX_GPU_SLOW_SUBMIT_LOGS: usize = 24;
const MAX_RECONSTRUCTION_DAMAGE_NUMERATOR: u64 = 1;
const MAX_RECONSTRUCTION_DAMAGE_DENOMINATOR: u64 = 8;
// QEMU has no trustworthy display-vblank clock to export to uiserver yet.
// Pace the private compositor at 66.7 Hz instead of submitting at the
// input-source rate. Until the DVM exports a trustworthy vblank deadline, the
// 1.67 ms lead is the bounded scheduling/render margin needed to complete a
// nominal 60 Hz frame before scanout. A cumulative deadline avoids drift, and
// missed slots are skipped rather than burst.
const GPU_FRAME_INTERVAL: Duration = Duration::from_millis(15);
const ETIMEDOUT: i32 = 110;
const EIO: i32 = 5;
static GPU_SLOW_SUBMIT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    submitted_at: Instant,
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
    scratch_atlas: Vec<u32>,
    layers: Vec<GpuSceneLayer>,
    cursor_source_rect: Option<Rect>,
    cursor_motion: CursorMotion,
    retained_scene_signature: u64,
    wayland_bindings: Vec<WaylandAtlasBinding>,
    console_bindings: Vec<ConsoleAtlasBinding>,
    next_slot: usize,
    next_content_epoch: u64,
    damage_history: VecDeque<AtlasDamageEpoch>,
    pending: Vec<PendingGpuFrame>,
    /// A locally prepared scene may outlive a transport rejection, but the
    /// next accepted submit must then replay the complete atlas rather than
    /// treating the rejected damage as externally committed.
    force_full_snapshot: bool,
    active: bool,
    activation_deadline: Instant,
    next_health_probe: Instant,
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
    Active(Box<GpuCompositor>),
}

impl GpuCompositorRuntime {
    /// CPU presentation is a provider, not a retry path. Once the display
    /// contract requires the DVM compositor, Waiting and paced Active turns
    /// must retain the last valid front buffer instead of silently performing
    /// the same full scene on the UI thread.
    pub(crate) fn admits_cpu_present(&self) -> bool {
        matches!(self, Self::SoftwareFallback)
    }

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
            Self::Active(compositor) => {
                let now = Instant::now();
                if now >= compositor.next_health_probe {
                    compositor.next_health_probe = now + GPU_PROVIDER_HEALTH_INTERVAL;
                    let current_display = display_get_info(display_fd)?;
                    if !same_display_contract(*display, current_display) {
                        return Err(ESTALE);
                    }
                    if gpu_provider_admission(current_display) != GpuProviderAdmission::Ready {
                        debug_line(
                            "uiserver: DVM GPU provider lease withdrawn; bounded recovery started",
                        );
                        *self = Self::waiting(now);
                        return Ok(false);
                    }
                }
                match compositor.poll_completions() {
                    Ok(()) => Ok(false),
                    Err(ENODEV) => {
                        debug_line(
                            "uiserver: DVM GPU compositor offline; bounded recovery started",
                        );
                        *self = Self::waiting(Instant::now());
                        Ok(false)
                    }
                    Err(err) => Err(err),
                }
            }
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
                            *self = Self::Active(Box::new(initialized.compositor));
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
            if !GPU_INITIALIZATION_RETAINS_BOOT_CLASS {
                require_background_thread_class();
            }
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
            .ok_or_else(|| {
                debug_line("uiserver: GPU initialization rejected stage=atlas-size-overflow");
                EINVAL
            })?;
        if logical_bytes == 0 || !logical_bytes.is_multiple_of(size_of::<u32>()) {
            debug_line(&format!(
                "uiserver: GPU initialization rejected stage=atlas-size bytes={logical_bytes}"
            ));
            return Err(EINVAL);
        }
        let mut slots = Vec::with_capacity(info.slot_count as usize);
        for slot_index in 0..info.slot_count {
            let surface = display_create_gpu_atlas_surface(display_fd, info).map_err(|errno| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=create-atlas slot={slot_index} errno={errno}"
                ));
                errno
            })?;
            let raw_fd = i32::try_from(surface.handle).map_err(|_| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-fd slot={slot_index} handle={}",
                    surface.handle
                ));
                EINVAL
            })?;
            let surface_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let mapping_len = usize::try_from(surface.mapping_len).map_err(|_| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-map-size slot={slot_index} bytes={}",
                    surface.mapping_len
                ));
                EINVAL
            })?;
            let mapping = map_surface(surface_fd.as_raw_fd(), mapping_len).map_err(|errno| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-map slot={slot_index} errno={errno}"
                ));
                errno
            })?;
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
            debug_line("uiserver: GPU initialization rejected stage=atlas-slot-identity");
            return Err(EINVAL);
        }
        let mut compiler =
            GpuSceneCompiler::new(info.context_id, info.context_epoch).map_err(|error| {
                let errno = gpu_scene_errno(error);
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=compiler-create errno={errno}"
                ));
                errno
            })?;
        compiler
            .begin_prime(info.prime_fence_value)
            .map_err(|error| {
                let errno = gpu_scene_errno(error);
                debug_line(&format!(
                "uiserver: GPU initialization rejected stage=compiler-prime-begin errno={errno}"
            ));
                errno
            })?;
        // GPU_GET_INFO carries the DVM-produced, kernel-module-validated
        // completion for the fixed GLES pipeline and initial KMS frame. The
        // local scheduler advances only from that measured transport record.
        let submit_flags = match info.flags {
            rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_STAGED_COPY => {
                driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_STAGED_COPY
            }
            rustos_user_abi::device::DISPLAY_GPU_INFO_FLAG_DIRECT_DMABUF => {
                driver_domain_protocol::DVM_GPU_ATLAS_SUBMIT_FLAG_DIRECT_DMABUF
            }
            _ => {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=provider-flags flags={:#x}",
                    info.flags
                ));
                return Err(EINVAL);
            }
        };
        compiler
            .complete_prime(DvmGpuPrimeCompletion {
                context_id: info.context_id,
                context_epoch: info.context_epoch,
                status: DvmGpuPrimeCompletionStatus::Ready,
                submit_flags,
                fence_value: info.prime_fence_value,
                duration_ns: info.prime_duration_ns,
            })
            .map_err(|error| {
                let errno = gpu_scene_errno(error);
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=compiler-prime-complete errno={errno}"
                ));
                errno
            })?;
        let atlas_pixels = logical_bytes / size_of::<u32>();
        Ok(Some(GpuInitialization {
            display: current_display,
            compositor: Self {
                display_fd,
                info,
                slots,
                compiler,
                atlas: vec![0_u32; atlas_pixels],
                scratch_atlas: vec![0_u32; atlas_pixels],
                layers: Vec::new(),
                cursor_source_rect: None,
                cursor_motion: CursorMotion::stationary(),
                retained_scene_signature: 0,
                wayland_bindings: Vec::new(),
                console_bindings: Vec::new(),
                next_slot: 0,
                next_content_epoch: 1,
                damage_history: VecDeque::with_capacity(info.slot_count as usize + 1),
                pending: Vec::with_capacity(info.slot_count as usize),
                force_full_snapshot: false,
                active: false,
                activation_deadline: Instant::now() + GPU_FIRST_FRAME_TIMEOUT,
                next_health_probe: Instant::now() + GPU_PROVIDER_HEALTH_INTERVAL,
                next_submission_at: Instant::now(),
                scene_wait_logged: false,
            },
        }))
    }

    pub(crate) fn present(&mut self, state: &mut AppState, scene_dirty: bool) -> Result<bool, i32> {
        let total_started = Instant::now();
        while self.retire_oldest(false)? {}
        let retire_elapsed = total_started.elapsed();
        let submission_started = Instant::now();
        if self.active && submission_started < self.next_submission_at {
            return Err(EAGAIN);
        }
        let Some(slot_index) = self.select_submission_slot()? else {
            // Steady-state completion waits never own the UI thread. The
            // retained update is retried after the next input/Wayland turn.
            return Err(EAGAIN);
        };
        let prepare_started = Instant::now();
        let mut rebuild_allocate_elapsed = Duration::ZERO;
        let mut rebuild_scene_elapsed = Duration::ZERO;
        let mut rebuild_difference_elapsed = Duration::ZERO;
        let mut damage = Vec::new();
        let current_scene_signature = gpu_scene_reuse_signature(state);
        let mut full_rebuild = !self.active
            || (scene_dirty && current_scene_signature != self.retained_scene_signature);
        if scene_dirty
            && !full_rebuild
            && !update_gpu_layer_destinations(
                state,
                self.layers.as_mut_slice(),
                self.wayland_bindings.as_slice(),
                self.console_bindings.as_slice(),
            )
        {
            full_rebuild = true;
        }
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
            if !full_rebuild {
                match update_console_gpu_textures(
                    state,
                    self.atlas.as_mut_slice(),
                    self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                    self.console_bindings.as_mut_slice(),
                )
                .map_err(gpu_scene_errno)?
                {
                    Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                    None => full_rebuild = true,
                }
            }
        }
        if full_rebuild {
            let allocate_started = Instant::now();
            self.scratch_atlas.fill(0);
            rebuild_allocate_elapsed = allocate_started.elapsed();
            let capability = self.capability_for_slot(slot_index, self.next_content_epoch)?;
            let scene_started = Instant::now();
            let GpuAtlasScene {
                layers,
                cursor_source_rect,
                wayland_bindings,
                console_bindings,
            } = match build_gpu_atlas_scene(
                state,
                self.scratch_atlas.as_mut_slice(),
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
            rebuild_scene_elapsed = scene_started.elapsed();
            let difference_started = Instant::now();
            damage = if self.active {
                difference_bounds(
                    self.atlas.as_slice(),
                    self.scratch_atlas.as_slice(),
                    self.info.atlas_width as usize,
                    self.info.atlas_height as usize,
                    self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                )
                .map(damage_from_rect)
                .into_iter()
                .collect()
            } else {
                vec![DvmGpuAtlasDamage {
                    x: 0,
                    y: 0,
                    width: self.info.atlas_width,
                    height: self.info.atlas_height,
                }]
            };
            rebuild_difference_elapsed = difference_started.elapsed();
            core::mem::swap(&mut self.atlas, &mut self.scratch_atlas);
            self.layers = layers;
            self.cursor_source_rect = Some(cursor_source_rect);
            self.cursor_motion = state.cursor_motion;
            self.wayland_bindings = wayland_bindings;
            self.console_bindings = console_bindings;
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
        let compiler_checkpoint = self.compiler.checkpoint();
        if let Err(err) = self.compiler.signal_acquire(self.next_content_epoch) {
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(gpu_scene_errno(err));
        }
        let batch = match self.compiler.compile(
            state.surface.width,
            state.surface.height,
            self.next_content_epoch,
            0xff00_0000,
            layers.as_slice(),
        ) {
            Ok(batch) => batch,
            Err(err) => {
                self.compiler.restore_rejected_submit(compiler_checkpoint);
                self.force_full_snapshot = true;
                return Err(gpu_scene_errno(err));
            }
        };
        let encoded = match batch.encode() {
            Ok(encoded) => encoded,
            Err(err) => {
                self.compiler.restore_rejected_submit(compiler_checkpoint);
                self.force_full_snapshot = true;
                return Err(gpu_scene_errno(err));
            }
        };
        // Damage is relative to the immediately preceding complete contents of
        // this backing slot, not merely to the last globally submitted frame.
        // Triple-buffer rotation normally revisits an older slot; in that case
        // a partial patch would expose stale or zero pixels and visibly
        // alternate complete/incomplete frames.  Fail over to a complete
        // snapshot until the selected slot is the exact predecessor.
        let retained_epoch = self.slots[slot_index].content_epoch;
        let source_damage_pixels = damage.iter().fold(0_u64, |total, rect| {
            total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
        });
        let submit_damage = if self.force_full_snapshot {
            full_atlas_damage(self.info.atlas_width, self.info.atlas_height)
        } else {
            match snapshot_damage_for_slot(
                retained_epoch,
                self.next_content_epoch,
                damage.as_slice(),
                &self.damage_history,
                self.info.atlas_width,
                self.info.atlas_height,
            ) {
                Ok(damage) => damage,
                Err(err) => {
                    self.compiler.restore_rejected_submit(compiler_checkpoint);
                    self.force_full_snapshot = true;
                    return Err(err);
                }
            }
        };
        let prepare_elapsed = prepare_started.elapsed();
        let surface_handle = self.slots[slot_index].surface.handle;
        let copy_started = Instant::now();
        if let Err(err) = copy_atlas_damage_to_slot(
            self.slots[slot_index].mapping.pixels_mut(),
            self.atlas.as_slice(),
            self.info.atlas_stride_bytes as usize / size_of::<u32>(),
            submit_damage.as_slice(),
            self.info.atlas_width,
            self.info.atlas_height,
        ) {
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(err);
        }
        let copy_elapsed = copy_started.elapsed();
        let submit_started = Instant::now();
        if let Err(err) = display_gpu_submit(
            self.display_fd,
            surface_handle,
            submit_damage.as_slice(),
            encoded.as_slice(),
        ) {
            // The atlas and retained scene describe the desired local state,
            // but neither the timeline nor damage history may claim that the
            // DVM accepted it. Restore admission and force the next retry to
            // publish a complete snapshot into whichever slot becomes free.
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(err);
        }
        self.force_full_snapshot = false;
        let submit_elapsed = submit_started.elapsed();
        let total_elapsed = total_started.elapsed();
        if total_elapsed >= GPU_SLOW_SUBMIT_THRESHOLD
            && GPU_SLOW_SUBMIT_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_GPU_SLOW_SUBMIT_LOGS
        {
            let damage_pixels = submit_damage.iter().fold(0_u64, |total, rect| {
                total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
            });
            let history_first = self.damage_history.front().map_or(0, |entry| entry.epoch);
            let history_last = self.damage_history.back().map_or(0, |entry| entry.epoch);
            diag_line(format!(
                "uiserver: slow gpu submit total_us={} retire_us={} prepare_us={} rebuild_allocate_us={} rebuild_scene_us={} rebuild_difference_us={} copy_us={} ioctl_us={} damage_rects={} damage_pixels={} source_damage_pixels={} full_rebuild={} slot={} retained_epoch={} current_epoch={} history={}..{} batch_bytes={} pending={}",
                total_elapsed.as_micros(),
                retire_elapsed.as_micros(),
                prepare_elapsed.as_micros(),
                rebuild_allocate_elapsed.as_micros(),
                rebuild_scene_elapsed.as_micros(),
                rebuild_difference_elapsed.as_micros(),
                copy_elapsed.as_micros(),
                submit_elapsed.as_micros(),
                submit_damage.len(),
                damage_pixels,
                source_damage_pixels,
                full_rebuild,
                slot_index,
                retained_epoch,
                self.next_content_epoch,
                history_first,
                history_last,
                encoded.len(),
                self.pending.len(),
            ));
        }
        self.slots[slot_index].content_epoch = self.next_content_epoch;
        self.pending.push(PendingGpuFrame {
            slot_index,
            surface_handle,
            submitted_at: Instant::now(),
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
            let _ = rustos_svc_runtime::ipc::product_milestone(
                PRODUCT_MILESTONE_DISPLAY_READY,
                self.info.generation,
                self.next_content_epoch,
            );
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
    fn select_submission_slot(&self) -> Result<Option<usize>, i32> {
        let predecessor = self.next_content_epoch.checked_sub(1).ok_or(EINVAL)?;
        let is_pending = |slot_index| {
            self.pending
                .iter()
                .any(|frame| frame.slot_index == slot_index)
        };
        for offset in 0..self.slots.len() {
            let slot_index = (self.next_slot + offset) % self.slots.len();
            if !is_pending(slot_index) && self.slots[slot_index].content_epoch == predecessor {
                return Ok(Some(slot_index));
            }
        }
        if !self.active {
            return Ok((0..self.slots.len())
                .map(|offset| (self.next_slot + offset) % self.slots.len())
                .find(|&slot_index| !is_pending(slot_index)));
        }

        let mut best = None;
        for offset in 0..self.slots.len() {
            let slot_index = (self.next_slot + offset) % self.slots.len();
            if is_pending(slot_index) {
                continue;
            }
            let reconstruction = snapshot_damage_for_slot(
                self.slots[slot_index].content_epoch,
                self.next_content_epoch,
                &[],
                &self.damage_history,
                self.info.atlas_width,
                self.info.atlas_height,
            )?;
            let pixels = damage_pixel_count(reconstruction.as_slice());
            if !reconstruction_damage_within_budget(
                pixels,
                self.info.atlas_width,
                self.info.atlas_height,
            ) {
                continue;
            }
            if best.is_none_or(|(_, best_pixels)| pixels < best_pixels) {
                best = Some((slot_index, pixels));
            }
        }
        Ok(best.map(|(slot_index, _)| slot_index))
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
        loop {
            let now = Instant::now();
            let timed_out = gpu_completion_timed_out(
                frame.submitted_at,
                self.activation_deadline,
                self.active,
                now,
            );
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
                Err(EAGAIN) if !blocking && !timed_out => {
                    return Ok(false);
                }
                Err(EAGAIN) if !timed_out => {
                    thread::sleep(GPU_COMPLETION_POLL);
                }
                Err(EAGAIN) => return Err(ETIMEDOUT),
                Err(err) => return Err(err),
            }
        }
    }
}

fn gpu_completion_timed_out(
    submitted_at: Instant,
    activation_deadline: Instant,
    active: bool,
    now: Instant,
) -> bool {
    if active {
        now.saturating_duration_since(submitted_at) >= GPU_COMPLETION_TIMEOUT
    } else {
        // Initial provider activation already has a product-bounded deadline.
        // Reusing the steady-state 50 ms recovery limit here made a healthy
        // first frame fail nondeterministically under host scheduling load,
        // despite the explicit five-second first-frame contract.
        now >= activation_deadline
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

fn damage_pixel_count(damage: &[DvmGpuAtlasDamage]) -> u64 {
    damage.iter().fold(0_u64, |total, rect| {
        total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
    })
}

fn reconstruction_damage_within_budget(pixels: u64, atlas_width: u32, atlas_height: u32) -> bool {
    let atlas_pixels = u64::from(atlas_width).saturating_mul(u64::from(atlas_height));
    pixels.saturating_mul(MAX_RECONSTRUCTION_DAMAGE_DENOMINATOR)
        <= atlas_pixels.saturating_mul(MAX_RECONSTRUCTION_DAMAGE_NUMERATOR)
}

fn copy_atlas_damage_to_slot(
    destination: &mut [u32],
    source: &[u32],
    stride_pixels: usize,
    damage: &[DvmGpuAtlasDamage],
    atlas_width: u32,
    atlas_height: u32,
) -> Result<(), i32> {
    let required = stride_pixels
        .checked_mul(atlas_height as usize)
        .ok_or(EINVAL)?;
    if stride_pixels < atlas_width as usize
        || destination.len() < required
        || source.len() < required
    {
        return Err(EINVAL);
    }
    validate_damage(damage, atlas_width, atlas_height)?;
    let mut used_streaming_store = false;
    for rect in damage {
        let width = rect.width as usize;
        let x = rect.x as usize;
        for row in rect.y as usize..(rect.y + rect.height) as usize {
            let start = row
                .checked_mul(stride_pixels)
                .and_then(|offset| offset.checked_add(x))
                .ok_or(EINVAL)?;
            let end = start.checked_add(width).ok_or(EINVAL)?;
            let destination_row = destination.get_mut(start..end).ok_or(EINVAL)?;
            let source_row = source.get(start..end).ok_or(EINVAL)?;
            used_streaming_store |=
                copy_row_to_write_combining_mapping(destination_row, source_row);
        }
    }
    finish_write_combining_copy(used_streaming_store);
    Ok(())
}

fn copy_row_to_write_combining_mapping(destination: &mut [u32], source: &[u32]) -> bool {
    debug_assert_eq!(destination.len(), source.len());
    #[cfg(target_arch = "x86_64")]
    {
        // SSE2 is mandatory on x86-64. Streaming stores avoid the read-for-
        // ownership traffic and cache pollution caused by a normal memcpy into
        // the write-combine atlas aperture. Keep tiny damage on the compiler's
        // scalar/vector copy path; setup and the final fence cost more there.
        const STREAMING_STORE_MIN_PIXELS: usize = 64;
        if destination.len() >= STREAMING_STORE_MIN_PIXELS {
            use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_stream_si128};

            let alignment_pixels =
                ((16 - (destination.as_mut_ptr() as usize & 15)) & 15) / size_of::<u32>();
            let prefix = alignment_pixels.min(destination.len());
            destination[..prefix].copy_from_slice(&source[..prefix]);

            let mut index = prefix;
            while index + 4 <= destination.len() {
                // SAFETY: `index + 4` is bounded above. The source load may be
                // unaligned; the destination prefix makes every streaming
                // store 16-byte aligned as required by `_mm_stream_si128`.
                unsafe {
                    let value = _mm_loadu_si128(source.as_ptr().add(index).cast::<__m128i>());
                    _mm_stream_si128(destination.as_mut_ptr().add(index).cast::<__m128i>(), value);
                }
                index += 4;
            }
            destination[index..].copy_from_slice(&source[index..]);
            return true;
        }
    }

    destination.copy_from_slice(source);
    false
}

fn finish_write_combining_copy(used_streaming_store: bool) {
    #[cfg(target_arch = "x86_64")]
    if used_streaming_store {
        // SAFETY: SSE2 is part of the x86-64 baseline. The fence makes all
        // non-temporal slot writes globally visible before the commit ioctl
        // publishes the generation and signals the DVM.
        unsafe {
            core::arch::x86_64::_mm_sfence();
        }
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
    let mut accumulated = Vec::new();
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
        if !accumulate_damage(
            &mut accumulated,
            entry.damage.as_slice(),
            atlas_width,
            atlas_height,
        )? {
            return Ok(full_atlas_damage(atlas_width, atlas_height));
        }
        expected_epoch = expected_epoch.checked_add(1).ok_or(EINVAL)?;
    }
    if expected_epoch != current_epoch {
        return Ok(full_atlas_damage(atlas_width, atlas_height));
    }
    if !accumulate_damage(&mut accumulated, requested, atlas_width, atlas_height)? {
        return Ok(full_atlas_damage(atlas_width, atlas_height));
    }
    Ok(accumulated)
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
    accumulated: &mut Vec<DvmGpuAtlasDamage>,
    damage: &[DvmGpuAtlasDamage],
    atlas_width: u32,
    atlas_height: u32,
) -> Result<bool, i32> {
    validate_damage(damage, atlas_width, atlas_height)?;
    for rect in damage {
        let mut merged = *rect;
        let mut index = 0;
        while index < accumulated.len() {
            if accumulated[index].overlaps(merged) {
                merged = damage_union(accumulated.remove(index), merged)?;
                // The larger union may now overlap an earlier rectangle.
                index = 0;
            } else {
                index += 1;
            }
        }
        accumulated.push(merged);
        if accumulated.len() > driver_domain_protocol::DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize {
            return Ok(false);
        }
    }
    Ok(true)
}

fn damage_union(
    left: DvmGpuAtlasDamage,
    right: DvmGpuAtlasDamage,
) -> Result<DvmGpuAtlasDamage, i32> {
    let left_right = left.x.checked_add(left.width).ok_or(EINVAL)?;
    let left_bottom = left.y.checked_add(left.height).ok_or(EINVAL)?;
    let right_right = right.x.checked_add(right.width).ok_or(EINVAL)?;
    let right_bottom = right.y.checked_add(right.height).ok_or(EINVAL)?;
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    Ok(DvmGpuAtlasDamage {
        x,
        y,
        width: left_right.max(right_right).checked_sub(x).ok_or(EINVAL)?,
        height: left_bottom.max(right_bottom).checked_sub(y).ok_or(EINVAL)?,
    })
}

fn gpu_scene_errno(_error: GpuSceneError) -> i32 {
    EINVAL
}

#[cfg(test)]
#[path = "gpu_runtime_tests.rs"]
mod tests;
