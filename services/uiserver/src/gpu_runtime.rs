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
    DVM_GPU_RENDER_MAX_IN_FLIGHT, DvmGpuAtlasCompletion, DvmGpuAtlasDamage, DvmGpuPrimeCompletion,
    DvmGpuPrimeCompletionStatus,
};
use rustos_user_abi::device::{DISPLAY_INFO_FLAG_DVM_SCANOUT, DISPLAY_INFO_FLAG_GPU_COMPOSITOR};
use rustos_user_abi::syscall::PRODUCT_MILESTONE_DISPLAY_READY;

use crate::app::{AppState, CursorMotion};
use crate::canvas::Rect;
use crate::gpu_scene::{
    GpuLayerKind, GpuSceneBatch, GpuSceneCompiler, GpuSceneError, GpuSceneLayer,
    GpuTextureCapability,
};
use crate::render::{
    AtlasRegionEntry, AtlasRegionKind, ConsoleAtlasBinding, CroppedConsoleBinding,
    CroppedWaylandBinding, GpuAtlasScene, WaylandAtlasBinding, build_gpu_atlas_scene,
    cursor_region_kind, gpu_cursor_layer, gpu_dock_reuse_signature, gpu_scene_reuse_signature,
    update_console_gpu_textures, update_cropped_console_textures, update_cropped_dock_texture,
    update_cropped_wayland_textures, update_dock_gpu_texture, update_gpu_cursor_texture,
    update_gpu_desktop_clock, update_gpu_layer_destinations, update_wayland_gpu_textures,
};
use crate::sys::{
    DisplayGpuInfo, DisplayInfo, DisplaySurfaceCreate, EAGAIN, EINVAL, ENODEV, ESTALE,
    SurfaceMapping, UiThreadRole, debug_line, diag_line, display_create_gpu_atlas_surface,
    display_get_info, display_gpu_get_info, display_gpu_query_completion, display_gpu_submit,
    map_surface, spawn_ui_thread,
};

// Upper bound on one atlas backing (the shipped atlas is 2048x2048 BGRA, 16 MiB).
const MAX_GPU_ATLAS_BYTES: usize = 256 * 1024 * 1024;
const GPU_READY_RETRY: Duration = Duration::from_millis(50);
const GPU_READY_RETRY_MAX: Duration = Duration::from_millis(400);
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
// Bound on the time the DVM may go without completing ANY frame while one is
// outstanding. The DVM serialises frames, each waiting for a page flip (up to
// 100 ms in its own fence deadline), and up to `slot_count` frames queue
// behind that. Measuring from each frame's submit time made a healthy,
// vblank-paced pipeline (66.7 Hz submit vs. ~60 Hz flip) saturate its queue
// and trip a 50 ms limit a few seconds after activation. A dead DVM is
// reported by the kernel as ENODEV; this only catches a silent stall.
const GPU_COMPLETION_TIMEOUT: Duration = Duration::from_millis(500);
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
    input_completion: Option<crate::input_completion::Ticket>,
}

/// The instant from which the head frame's completion deadline runs: a frame
/// queued behind others cannot start before its predecessor completes.
fn completion_wait_origin(submitted_at: Instant, last_completion_at: Instant) -> Instant {
    submitted_at.max(last_completion_at)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AtlasDamageEpoch {
    epoch: u64,
    damage: Vec<DvmGpuAtlasDamage>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct GpuPresentStats {
    pub(crate) damage_rect: Option<Rect>,
    pub(crate) damage_rects: u64,
    pub(crate) damage_pixels: u64,
}

pub(crate) struct GpuCompositor {
    display_fd: RawFd,
    info: DisplayGpuInfo,
    slots: Vec<GpuAtlasSlot>,
    compiler: GpuSceneCompiler,
    atlas: Vec<u32>,
    scratch_atlas: Vec<u32>,
    atlas_used_height: usize,
    scratch_used_height: usize,
    dock_source_rect: Option<Rect>,
    cropped_dock_destination: Rect,
    cropped_dock_fragments: Vec<(Rect, Rect)>,
    retained_dock_signature: u64,
    retained_regions: Vec<AtlasRegionEntry>,
    layers: Vec<GpuSceneLayer>,
    submission_layers: Vec<GpuSceneLayer>,
    submission_batch: GpuSceneBatch,
    encoded_batch: Vec<u8>,
    damage_scratch: Vec<DvmGpuAtlasDamage>,
    submission_damage: Vec<DvmGpuAtlasDamage>,
    cursor_source_rect: Option<Rect>,
    cursor_motion: CursorMotion,
    retained_scene_signature: u64,
    cropped: bool,
    cropped_reusable: bool,
    cropped_wayland_bindings: Vec<CroppedWaylandBinding>,
    cropped_console_bindings: Vec<CroppedConsoleBinding>,
    capacity_pressure_logged_at: Option<Instant>,
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
    activation_epoch: u64,
    activation_damage_rects: usize,
    activation_deadline: Instant,
    next_health_probe: Instant,
    last_completion_at: Instant,
    completion_stall_logged: bool,
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

    /// Whether a frame has reached the display through this provider. A
    /// client's first frame callback is withheld until then: it is the
    /// product's first-frame evidence, and a callback sent while the DVM
    /// compositor is still priming or recovering would claim a frame that
    /// nothing has presented.
    pub(crate) fn has_presented(&self) -> bool {
        match self {
            Self::SoftwareFallback => true,
            Self::Waiting { .. } => false,
            Self::Active(compositor) => compositor.active,
        }
    }

    /// Next pacing deadline when an active GPU compositor deferred submission
    /// solely for its frame interval. Returning `Some(deadline)` lets the UI
    /// loop park cleanly until either that deadline or an input/Wayland wake
    /// instead of spin-sleeping on a 1 ms timer.
    pub(crate) fn next_retry_deadline(&self, now: Instant) -> Option<Instant> {
        match self {
            Self::Active(compositor) if compositor.active && compositor.next_submission_at > now => {
                Some(compositor.next_submission_at)
            }
            _ => None,
        }
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

    pub(crate) fn present(
        &mut self,
        state: &mut AppState,
        scene_dirty: bool,
    ) -> Result<Option<GpuPresentStats>, i32> {
        match self {
            Self::Active(compositor) => match compositor.present(state, scene_dirty) {
                Ok(presented) => Ok(presented),
                Err(ENODEV) => {
                    debug_line("uiserver: DVM GPU compositor lost during submit; recovering");
                    *self = Self::waiting(Instant::now());
                    Ok(None)
                }
                Err(err) => Err(err),
            },
            Self::SoftwareFallback | Self::Waiting { .. } => Ok(None),
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
    let role = if GPU_INITIALIZATION_RETAINS_BOOT_CLASS {
        UiThreadRole::BootstrapGpu
    } else {
        UiThreadRole::Background
    };
    spawn_ui_thread(role, "uiserver-gpu-init", move || {
        debug_line("uiserver: GPU initialization worker started");
        let deadline = Instant::now() + GPU_READY_TIMEOUT;
        let mut attempts = 0_u32;
        let result = loop {
            match GpuCompositor::try_initialize(display_fd, expected_display) {
                Ok(None) if Instant::now() < deadline => {
                    attempts = attempts.saturating_add(1);
                    if attempts == 1 || attempts.is_power_of_two() {
                        debug_line(&format!(
                            "uiserver: GPU provider pending attempt={attempts}"
                        ));
                    }
                    thread::sleep(gpu_ready_retry_backoff(attempts));
                }
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
        debug_line(&format!(
            "uiserver: GPU initialization stage=gpu-info done slots={} atlas={}x{} stride={}",
            info.slot_count, info.atlas_width, info.atlas_height, info.atlas_stride_bytes,
        ));
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
        // The provider descriptor is untrusted: a zero slot count would make
        // every later `% slots.len()` divide by zero, and an unbounded atlas
        // would turn one descriptor into an arbitrary-size allocation.
        let row_bytes = (info.atlas_width as usize).checked_mul(size_of::<u32>());
        if info.slot_count == 0
            || info.slot_count > DVM_GPU_RENDER_MAX_IN_FLIGHT
            || info.atlas_width == 0
            || info.atlas_height == 0
            || info.atlas_stride_bytes as usize % size_of::<u32>() != 0
            || row_bytes.is_none_or(|row| (info.atlas_stride_bytes as usize) < row)
        {
            debug_line("uiserver: GPU initialization rejected stage=atlas-geometry");
            return Err(EINVAL);
        }
        let logical_bytes = usize::try_from(info.atlas_stride_bytes)
            .ok()
            .and_then(|stride| stride.checked_mul(info.atlas_height as usize))
            .filter(|bytes| *bytes <= MAX_GPU_ATLAS_BYTES)
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
            debug_line(&format!(
                "uiserver: GPU initialization stage=atlas-create begin slot={slot_index}"
            ));
            let surface = display_create_gpu_atlas_surface(display_fd, info).map_err(|errno| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=create-atlas slot={slot_index} errno={errno}"
                ));
                errno
            })?;
            debug_line(&format!(
                "uiserver: GPU initialization stage=atlas-create done slot={slot_index} bytes={}",
                surface.mapping_len,
            ));
            let raw_fd = i32::try_from(surface.handle).map_err(|_| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-fd slot={slot_index} handle={}",
                    surface.handle
                ));
                EINVAL
            })?;
            // SAFETY: `surface.handle` is the descriptor DISPLAY_IOCTL_CREATE_SURFACE just
            // created for this process; it was validated non-zero and fits i32, and nothing
            // else closes it, so `OwnedFd` takes unique ownership (closed on any later
            // error return or when the slot drops).
            let surface_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let mapping_len = usize::try_from(surface.mapping_len)
                .ok()
                .filter(|len| *len >= logical_bytes)
                .ok_or_else(|| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-map-size slot={slot_index} bytes={}",
                    surface.mapping_len
                ));
                EINVAL
            })?;
            debug_line(&format!(
                "uiserver: GPU initialization stage=atlas-map begin slot={slot_index} bytes={mapping_len}"
            ));
            let mapping = map_surface(surface_fd.as_raw_fd(), mapping_len).map_err(|errno| {
                debug_line(&format!(
                    "uiserver: GPU initialization rejected stage=atlas-map slot={slot_index} errno={errno}"
                ));
                errno
            })?;
            debug_line(&format!(
                "uiserver: GPU initialization stage=atlas-map done slot={slot_index}"
            ));
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
        let atlas = vec![0_u32; atlas_pixels];
        let scratch_atlas = vec![0_u32; atlas_pixels];
        debug_line("uiserver: GPU initialization stage=atlas-buffers done");
        Ok(Some(GpuInitialization {
            display: current_display,
            compositor: Self {
                display_fd,
                info,
                slots,
                compiler,
                atlas,
                scratch_atlas,
                atlas_used_height: info.atlas_height as usize,
                scratch_used_height: info.atlas_height as usize,
                dock_source_rect: None,
                cropped_dock_destination: Rect::empty(),
                cropped_dock_fragments: Vec::new(),
                retained_dock_signature: 0,
                retained_regions: Vec::new(),
                layers: Vec::new(),
                submission_layers: Vec::new(),
                submission_batch: GpuSceneBatch::empty(),
                encoded_batch: Vec::new(),
                damage_scratch: Vec::new(),
                submission_damage: Vec::new(),
                cursor_source_rect: None,
                cursor_motion: CursorMotion::stationary(),
                retained_scene_signature: 0,
                cropped: false,
                cropped_reusable: false,
                cropped_wayland_bindings: Vec::new(),
                cropped_console_bindings: Vec::new(),
                capacity_pressure_logged_at: None,
                wayland_bindings: Vec::new(),
                console_bindings: Vec::new(),
                next_slot: 0,
                next_content_epoch: 1,
                damage_history: VecDeque::with_capacity(info.slot_count as usize + 1),
                pending: Vec::with_capacity(info.slot_count as usize),
                force_full_snapshot: false,
                active: false,
                activation_epoch: 0,
                activation_damage_rects: 0,
                activation_deadline: Instant::now() + GPU_FIRST_FRAME_TIMEOUT,
                next_health_probe: Instant::now() + GPU_PROVIDER_HEALTH_INTERVAL,
                last_completion_at: Instant::now(),
                completion_stall_logged: false,
                next_submission_at: Instant::now(),
                scene_wait_logged: false,
            },
        }))
    }

    pub(crate) fn present(
        &mut self,
        state: &mut AppState,
        scene_dirty: bool,
    ) -> Result<Option<GpuPresentStats>, i32> {
        let total_started = Instant::now();
        crate::note_present_step(crate::PRESENT_STEP_RETIRE);
        while self.retire_oldest(false)? {}
        if !self.active && !self.pending.is_empty() {
            // The activation frame is in flight; submitting another full
            // snapshot would race it. `retire_oldest` above already reports
            // the first-frame timeout.
            return Err(EAGAIN);
        }
        let retire_elapsed = total_started.elapsed();
        let submission_started = Instant::now();
        if submission_started < self.next_submission_at {
            return Err(EAGAIN);
        }
        let Some(slot_index) = self.select_submission_slot()? else {
            // Steady-state completion waits never own the UI thread. The
            // retained update is retried after the next input/Wayland turn.
            return Err(EAGAIN);
        };
        let prepare_started = Instant::now();
        let cpu_before = crate::input_completion::thread_cpu_ns();
        let mut rebuild_allocate_elapsed = Duration::ZERO;
        let mut rebuild_scene_elapsed = Duration::ZERO;
        let mut rebuild_difference_elapsed = Duration::ZERO;
        let mut damage = core::mem::take(&mut self.damage_scratch);
        damage.clear();
        // Cursor-only turns cannot change window topology or dock content.
        // Avoid allocating draw order and hashing every title at pointer rate.
        let current_scene_signature = if scene_dirty || !self.active {
            gpu_scene_reuse_signature(state, self.cropped)
        } else {
            self.retained_scene_signature
        };
        let current_dock_signature = if scene_dirty || !self.active {
            gpu_dock_reuse_signature(state)
        } else {
            self.retained_dock_signature
        };
        let mut full_rebuild = !self.active
            || (scene_dirty
                && ((self.cropped && !self.cropped_reusable)
                    || self.retained_regions.is_empty()
                    // Full chrome invalidation changes its version only when
                    // repainted. Minute changes can patch the retained atlas.
                    || !state.desktop_cache.fully_valid()
                    || current_scene_signature != self.retained_scene_signature));
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
        let stride_pixels = self.info.atlas_stride_bytes as usize / size_of::<u32>();
        let mut patch_kind = 0;
        if scene_dirty && !full_rebuild {
            match update_wayland_gpu_textures(
                state,
                self.atlas.as_mut_slice(),
                self.info.atlas_width as usize,
                self.info.atlas_height as usize,
                stride_pixels,
                self.wayland_bindings.as_mut_slice(),
            )
            .map_err(gpu_scene_errno)?
            {
                Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                None => full_rebuild = true,
            }
            if !full_rebuild && self.cropped {
                match update_cropped_wayland_textures(
                    state,
                    self.atlas.as_mut_slice(),
                    stride_pixels,
                    self.cropped_wayland_bindings.as_mut_slice(),
                )
                .map_err(gpu_scene_errno)?
                {
                    Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                    None => full_rebuild = true,
                }
            }
            if !damage.is_empty() {
                patch_kind |= 8;
            }
            let wayland_rects = damage.len();
            if !full_rebuild && self.cropped {
                match update_cropped_console_textures(
                    state,
                    self.atlas.as_mut_slice(),
                    stride_pixels,
                    self.cropped_console_bindings.as_mut_slice(),
                )
                .map_err(gpu_scene_errno)?
                {
                    Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                    None => full_rebuild = true,
                }
            }
            if !full_rebuild {
                match update_console_gpu_textures(
                    state,
                    self.atlas.as_mut_slice(),
                    stride_pixels,
                    self.console_bindings.as_mut_slice(),
                )
                .map_err(gpu_scene_errno)?
                {
                    Some(changed) => damage.extend(changed.into_iter().map(damage_from_rect)),
                    None => full_rebuild = true,
                }
            }
            if damage.len() != wayland_rects {
                patch_kind |= 16;
            }
            let console_rects = damage.len();
            if !full_rebuild && current_dock_signature != self.retained_dock_signature {
                if let Some(dock_rect) = self.dock_source_rect {
                    if update_dock_gpu_texture(
                        state,
                        self.atlas.as_mut_slice(),
                        self.info.atlas_width as usize,
                        self.info.atlas_height as usize,
                        stride_pixels,
                        dock_rect,
                    )
                    .map_err(gpu_scene_errno)?
                    {
                        damage.push(damage_from_rect(dock_rect));
                        self.retained_dock_signature = current_dock_signature;
                    } else {
                        full_rebuild = true;
                    }
                } else if self.cropped {
                    match update_cropped_dock_texture(
                        state,
                        self.atlas.as_mut_slice(),
                        self.info.atlas_width as usize,
                        self.info.atlas_height as usize,
                        stride_pixels,
                        self.cropped_dock_destination,
                        self.cropped_dock_fragments.as_slice(),
                    )
                    .map_err(gpu_scene_errno)?
                    {
                        Some(changed) => {
                            damage.extend(changed.into_iter().map(damage_from_rect));
                            self.retained_dock_signature = current_dock_signature;
                        }
                        None => full_rebuild = true,
                    }
                } else {
                    full_rebuild = true;
                }
            }
            if damage.len() != console_rects {
                patch_kind |= 32;
            }
            if !full_rebuild {
                match update_gpu_desktop_clock(
                    state,
                    self.atlas.as_mut_slice(),
                    stride_pixels,
                    self.retained_regions.as_mut_slice(),
                )
                .map_err(gpu_scene_errno)?
                {
                    Some(changed) => {
                        if !changed.is_empty() {
                            patch_kind |= 64;
                            damage.extend(changed.into_iter().map(damage_from_rect));
                        }
                    }
                    None => full_rebuild = true,
                }
            }
            if !full_rebuild {
                sync_retained_region_versions(
                    self.retained_regions.as_mut_slice(),
                    state,
                    self.retained_dock_signature,
                );
            } else {
                // Earlier patch functions may already have changed the local
                // atlas before a later binding rejects reuse. A diff against
                // that partially updated atlas cannot describe the published
                // predecessor, so carry a full snapshot into the history too.
                self.force_full_snapshot = true;
                self.retained_regions.clear();
            }
        }
        let cursor_changed = crate::canvas::cursor_raster_key(state.cursor_motion)
            != crate::canvas::cursor_raster_key(self.cursor_motion);
        if full_rebuild {
            crate::note_present_step(crate::PRESENT_STEP_SCENE);
            let incremental =
                self.active && !self.force_full_snapshot && !self.retained_regions.is_empty();
            let allocate_started = Instant::now();
            if !incremental {
                let clear_len = self
                    .scratch_used_height
                    .min(self.info.atlas_height as usize)
                    .saturating_mul(stride_pixels)
                    .min(self.scratch_atlas.len());
                self.scratch_atlas[..clear_len].fill(0);
                // A rejected build can leave partially placed regions anywhere in
                // the scratch atlas; only a completed build (below) knows the
                // true extent, so assume the whole atlas is dirty until then.
                self.scratch_used_height = self.info.atlas_height as usize;
            }
            rebuild_allocate_elapsed = allocate_started.elapsed();
            let capability = self.capability_for_slot(slot_index, self.next_content_epoch)?;
            let scene_started = Instant::now();
            let mut used_incremental = incremental;
            let GpuAtlasScene {
                layers,
                cursor_source_rect,
                wayland_bindings,
                console_bindings,
                cropped_wayland_bindings,
                cropped_console_bindings,
                cropped_reusable,
                cropped,
                used_height: next_used_height,
                dock_source_rect,
                cropped_dock_destination,
                cropped_dock_fragments,
                regions: next_regions,
                changed_outer_rects,
                border_only_rects,
            } = match build_gpu_atlas_scene(
                state,
                self.scratch_atlas.as_mut_slice(),
                self.info.atlas_width as usize,
                self.info.atlas_height as usize,
                stride_pixels,
                capability,
                self.cropped && current_scene_signature == self.retained_scene_signature,
                incremental.then_some(self.retained_regions.as_slice()),
            ) {
                Ok(scene) => {
                    self.capacity_pressure_logged_at = None;
                    scene
                }
                Err(GpuSceneError::SceneNotReady) if !self.active => {
                    if Instant::now() >= self.activation_deadline {
                        debug_line("uiserver: first GPU frame readiness timed out");
                        return Err(ETIMEDOUT);
                    }
                    if !self.scene_wait_logged {
                        debug_line("uiserver: first GPU frame waiting for retained scene");
                        self.scene_wait_logged = true;
                    }
                    return Ok(None);
                }
                Err(error @ (GpuSceneError::AtlasFull | GpuSceneError::CommandLimit)) => {
                    used_incremental = false;
                    let now = Instant::now();
                    if self
                        .capacity_pressure_logged_at
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
                    {
                        diag_line(format!(
                            "uiserver: gpu scene capacity={error:?}; falling back to bounded composited scene"
                        ));
                        self.capacity_pressure_logged_at = Some(now);
                    }
                    self.scratch_atlas.fill(0);
                    match crate::render::build_composited_gpu_atlas_scene(
                        state,
                        self.scratch_atlas.as_mut_slice(),
                        self.info.atlas_width as usize,
                        self.info.atlas_height as usize,
                        stride_pixels,
                        capability,
                    ) {
                        Ok(scene) => scene,
                        Err(GpuSceneError::AtlasFull | GpuSceneError::CommandLimit) => {
                            self.force_full_snapshot = true;
                            self.retained_regions.clear();
                            self.next_submission_at = Instant::now() + GPU_FRAME_INTERVAL;
                            return Err(EAGAIN);
                        }
                        Err(err) => return Err(gpu_scene_errno(err)),
                    }
                }
                Err(err) => return Err(gpu_scene_errno(err)),
            };
            rebuild_scene_elapsed = scene_started.elapsed();
            crate::note_present_step(crate::PRESENT_STEP_DIFF);
            let difference_started = Instant::now();
            damage.clear();
            if used_incremental {
                diff_and_commit_incremental_rebuild(
                    self.atlas.as_mut_slice(),
                    self.scratch_atlas.as_slice(),
                    self.info.atlas_width as usize,
                    self.info.atlas_height as usize,
                    stride_pixels,
                    changed_outer_rects.as_slice(),
                    border_only_rects.as_slice(),
                    &mut damage,
                )?;
                self.scratch_used_height = self.info.atlas_height as usize;
                self.atlas_used_height = self.atlas_used_height.max(next_used_height);
            } else {
                let scan_height = self
                    .atlas_used_height
                    .max(next_used_height)
                    .min(self.info.atlas_height as usize);
                if self.active && !self.force_full_snapshot {
                    difference_damage_into(
                        self.atlas.as_slice(),
                        self.scratch_atlas.as_slice(),
                        self.info.atlas_width as usize,
                        scan_height,
                        stride_pixels,
                        &mut damage,
                    )?;
                } else {
                    full_atlas_damage(&mut damage, self.info.atlas_width, self.info.atlas_height);
                }
                core::mem::swap(&mut self.atlas, &mut self.scratch_atlas);
                self.scratch_used_height = self.atlas_used_height;
                self.atlas_used_height = next_used_height;
            }
            rebuild_difference_elapsed = difference_started.elapsed();
            self.dock_source_rect = dock_source_rect;
            self.cropped_dock_destination = cropped_dock_destination;
            self.cropped_dock_fragments = cropped_dock_fragments;
            self.retained_dock_signature = gpu_dock_reuse_signature(state);
            self.retained_regions = next_regions;
            self.layers = layers;
            self.cursor_source_rect = Some(cursor_source_rect);
            self.cursor_motion = state.cursor_motion;
            self.wayland_bindings = wayland_bindings;
            self.console_bindings = console_bindings;
            self.cropped = cropped;
            self.cropped_reusable = cropped_reusable;
            self.cropped_wayland_bindings = cropped_wayland_bindings;
            self.cropped_console_bindings = cropped_console_bindings;
            // The rebuilt atlas was cut from the current snapshots, which
            // already contain every accepted commit.
            state.wayland_pending_damage.clear();
            self.retained_scene_signature = gpu_scene_reuse_signature(state, cropped);
        } else if cursor_changed {
            let cursor_source_rect = self.cursor_source_rect.ok_or(EINVAL)?;
            update_gpu_cursor_texture(
                self.atlas.as_mut_slice(),
                self.info.atlas_stride_bytes as usize / size_of::<u32>(),
                cursor_source_rect,
                state.cursor_motion,
            )
            .map_err(gpu_scene_errno)?;
            // Both support rectangles are needed: pixels made transparent
            // by the new sprite must also be overwritten in each DVM slot.
            let support = crate::canvas::cursor_raster_bounds(self.cursor_motion)
                .union(crate::canvas::cursor_raster_bounds(state.cursor_motion));
            damage.push(damage_from_rect(Rect {
                x: cursor_source_rect.x + support.x,
                y: cursor_source_rect.y + support.y,
                ..support
            }));
            self.cursor_motion = state.cursor_motion;
            let next_cursor_kind = cursor_region_kind(state.cursor_motion);
            for entry in &mut self.retained_regions {
                if matches!(entry.kind, AtlasRegionKind::Cursor { .. }) {
                    entry.kind = next_cursor_kind;
                }
            }
        }
        // An empty changed rectangle carries no pixels; the wire contract
        // rejects it, and one such rectangle would end the whole present.
        damage.retain(|rect| rect.width != 0 && rect.height != 0);

        let capability = self.capability_for_slot(slot_index, self.next_content_epoch)?;
        let layers = &mut self.submission_layers;
        layers.clear();
        layers.extend_from_slice(&self.layers);
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
        if let Err(err) = self.compiler.compile_into(
            state.surface.width,
            state.surface.height,
            self.next_content_epoch,
            0xff00_0000,
            layers.as_slice(),
            &mut self.submission_batch,
        ) {
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(gpu_scene_errno(err));
        }
        if let Err(err) = self.submission_batch.encode_into(&mut self.encoded_batch) {
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(gpu_scene_errno(err));
        }
        // Damage is relative to the immediately preceding complete contents of
        // this backing slot, not merely to the last globally submitted frame.
        // Triple-buffer rotation normally revisits an older slot; in that case
        // a partial patch would expose stale or zero pixels and visibly
        // alternate complete/incomplete frames.  Fail over to a complete
        // snapshot until the selected slot is the exact predecessor.
        let retained_epoch = self.slots[slot_index].content_epoch;
        if self.force_full_snapshot {
            // Rejected submissions may have changed local pixels without an
            // accepted history epoch. Recording only this retry's small delta
            // would later reconstruct stale pixels in another backing slot.
            full_atlas_damage(&mut damage, self.info.atlas_width, self.info.atlas_height);
        }
        let source_damage_pixels = damage.iter().fold(0_u64, |total, rect| {
            total.saturating_add(u64::from(rect.width) * u64::from(rect.height))
        });
        if let Err(err) = snapshot_damage_for_slot_into(
                retained_epoch,
                self.next_content_epoch,
                damage.as_slice(),
                &self.damage_history,
                self.info.atlas_width,
                self.info.atlas_height,
                &mut self.submission_damage,
            ) {
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(err);
        }
        let submit_damage = &self.submission_damage;
        let prepare_elapsed = prepare_started.elapsed();
        let surface_handle = self.slots[slot_index].surface.handle;
        crate::note_present_step(crate::PRESENT_STEP_COPY);
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
        crate::note_present_step(crate::PRESENT_STEP_SUBMIT);
        let submit_started = Instant::now();
        if let Err(err) = display_gpu_submit(
            self.display_fd,
            surface_handle,
            submit_damage.as_slice(),
            self.encoded_batch.as_slice(),
        ) {
            // The atlas and retained scene describe the desired local state,
            // but neither the timeline nor damage history may claim that the
            // DVM accepted it. Restore admission and force the next retry to
            // publish a complete snapshot into whichever slot becomes free.
            self.compiler.restore_rejected_submit(compiler_checkpoint);
            self.force_full_snapshot = true;
            return Err(err);
        }
        // The diagnostic split ends at successful ioctl return, before slow
        // logging/formatting or pending-frame bookkeeping can run.
        let submit_accepted = Instant::now();
        let frame_cpu_ns = cpu_before.and_then(|before| {
            crate::input_completion::thread_cpu_ns()?.checked_sub(before)
        });
        self.force_full_snapshot = false;
        let submit_elapsed = submit_accepted.duration_since(submit_started);
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
                self.encoded_batch.len(),
                self.pending.len(),
            ));
        }
        let stats = {
            let mut damage_union = Rect::empty();
            for rect in &damage {
                damage_union = damage_union.union(Rect {
                    x: rect.x as usize,
                    y: rect.y as usize,
                    width: rect.width as usize,
                    height: rect.height as usize,
                });
            }
            GpuPresentStats {
                damage_rect: (!damage_union.is_empty()).then_some(damage_union),
                damage_rects: damage.len() as u64,
                damage_pixels: source_damage_pixels,
            }
        };
        self.slots[slot_index].content_epoch = self.next_content_epoch;
        self.pending.push(PendingGpuFrame {
            slot_index,
            surface_handle,
            submitted_at: submit_accepted,
            // Attach before the first-frame blocking retirement: it can
            // complete synchronously before this present call returns.
            input_completion: crate::input_completion::submitted(
                crate::input_completion::SubmitWork {
                    start: prepare_started,
                    copy: copy_started,
                    ioctl: submit_started,
                    accepted: submit_accepted,
                    cpu_ns: frame_cpu_ns,
                    kind: u8::from(scene_dirty) | (u8::from(full_rebuild) << 1)
                        | (u8::from(cursor_changed) << 2) | patch_kind,
                    upload_pixels: frame_cpu_ns.map_or(0, |_| damage_pixel_count(submit_damage)),
                },
                self.info.generation,
                self.next_content_epoch,
                surface_handle,
            ),
        });
        self.damage_history.push_back(AtlasDamageEpoch {
            epoch: self.next_content_epoch,
            damage,
        });
        while self.damage_history.len() > self.slots.len() {
            if let Some(expired) = self.damage_history.pop_front() {
                self.damage_scratch = expired.damage;
                self.damage_scratch.clear();
            }
        }
        self.next_submission_at = next_frame_deadline(submission_started);
        if !self.active {
            // The activation frame never blocks the UI thread: the first-frame
            // contract allows 5 s, the UI phase watchdog 3 s. Completion is
            // consumed by `retire_oldest`, which then runs `complete_activation`.
            self.activation_epoch = self.next_content_epoch;
            self.activation_damage_rects = submit_damage.len();
        }
        self.next_slot = (slot_index + 1) % self.slots.len();
        self.next_content_epoch = self.next_content_epoch.checked_add(1).ok_or(EINVAL)?;
        Ok(Some(stats))
    }

    /// Prefer a completed slot that already contains the exact predecessor
    /// snapshot. Reusing it is safe only after the DVM completion releases
    /// that slot, and lets ordinary cursor/damage updates stay incremental
    /// instead of forcing a full atlas copy on every round-robin turn.
    fn select_submission_slot(&mut self) -> Result<Option<usize>, i32> {
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
            snapshot_damage_for_slot_into(
                self.slots[slot_index].content_epoch,
                self.next_content_epoch,
                &[],
                &self.damage_history,
                self.info.atlas_width,
                self.info.atlas_height,
                &mut self.submission_damage,
            )?;
            let pixels = damage_pixel_count(self.submission_damage.as_slice());
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

    fn complete_activation(&mut self) -> Result<(), i32> {
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
            self.activation_damage_rects,
        );
        // This one-shot transition is release evidence. Do not let a full
        // asynchronous observability queue erase it.
        debug_line(&active_contract);
        let _ = rustos_svc_runtime::ipc::product_milestone(
            PRODUCT_MILESTONE_DISPLAY_READY,
            self.info.generation,
            self.activation_epoch,
        );
        crate::startup::global().succeed(crate::startup::Dependency::Gpu);
        self.active = true;
        Ok(())
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
                completion_wait_origin(frame.submitted_at, self.last_completion_at),
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
                    self.last_completion_at = Instant::now();
                    self.completion_stall_logged = false;
                    // Both exact render and present receipts validated above.
                    crate::input_completion::completed(frame.input_completion);
                    if !self.active {
                        self.complete_activation()?;
                    }
                    return Ok(true);
                }
                Err(EAGAIN) if !blocking && !timed_out => {
                    return Ok(false);
                }
                Err(EAGAIN) if !timed_out => {
                    thread::sleep(GPU_COMPLETION_POLL);
                }
                // After activation a silent stall must NOT re-initialise: the
                // DVM keeps `expected_submit`, the content epoch and the
                // sequence, while GPU_GET_INFO re-primes a compiler at submit 1
                // / content epoch 1 (and the kernel keeps the stale SUBMITTED
                // slots), so every later batch would fail with EPROTO. Only a
                // DVM restart (kernel epoch reset, reported as ENODEV) makes a
                // fresh context valid. Keep the frame pending, stay alive and
                // keep serving input/Wayland; the queue backpressures with
                // EAGAIN meanwhile.
                Err(EAGAIN) if self.active => {
                    if !self.completion_stall_logged {
                        self.completion_stall_logged = true;
                        diag_line(
                            "uiserver: DVM GPU completion stalled; waiting for completion or provider loss",
                        );
                    }
                    return Ok(false);
                }
                Err(EAGAIN) => return Err(ETIMEDOUT),
                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for GpuCompositor {
    fn drop(&mut self) {
        // Even an idle provider reset after warmup invalidates the cohort;
        // limiting cancellation to outstanding tickets would miss that case.
        crate::input_completion::provider_lost();
    }
}

fn gpu_ready_retry_backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(3);
    GPU_READY_RETRY
        .checked_mul(1_u32 << shift)
        .unwrap_or(GPU_READY_RETRY_MAX)
        .min(GPU_READY_RETRY_MAX)
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

/// Earliest next submission: a minimum spacing from this one, not a fixed
/// phase grid. A grid made an input event that wakes an idle compositor wait
/// for an unrelated slot boundary (up to a whole interval); spacing keeps the
/// same maximum submission rate while an isolated update presents at once.
fn next_frame_deadline(submitted_at: Instant) -> Instant {
    submitted_at + GPU_FRAME_INTERVAL
}

fn rebind_layers(layers: &mut [GpuSceneLayer], capability: GpuTextureCapability) {
    for layer in layers {
        if let GpuLayerKind::Texture { region } = &mut layer.kind {
            region.atlas = capability;
        }
    }
}

fn sync_retained_region_versions(
    regions: &mut [AtlasRegionEntry],
    state: &AppState,
    dock_signature: u64,
) {
    for entry in regions {
        match entry.kind {
            AtlasRegionKind::WaylandCsd { surface_id }
            | AtlasRegionKind::WaylandSsd { surface_id, .. } => {
                if let Some(window) = state
                    .wayland_windows
                    .iter()
                    .find(|window| window.surface_id == surface_id)
                {
                    entry.content_version = window.content_version;
                }
            }
            AtlasRegionKind::Console { session_handle, .. } => {
                if let Some(window) = state
                    .console_windows
                    .iter()
                    .find(|window| window.session_handle == session_handle)
                {
                    entry.content_version = window.surface_cache.content_version;
                }
            }
            AtlasRegionKind::Dock => {
                entry.content_version = dock_signature;
            }
            AtlasRegionKind::Desktop | AtlasRegionKind::Cursor { .. } => {}
        }
    }
}

fn push_damage_span(
    damage: &mut Vec<DvmGpuAtlasDamage>,
    bounds: &mut Rect,
    collapsed: &mut bool,
    y: usize,
    first: usize,
    last: usize,
) {
    let limit = driver_domain_protocol::DVM_GPU_ATLAS_MAX_DAMAGE_RECTS as usize;
    let changed = Rect {
        x: first,
        y,
        width: last + 1 - first,
        height: 1,
    };
    *bounds = bounds.union(changed);
    if *collapsed {
        damage[0] = damage_from_rect(*bounds);
        return;
    }
    if let Some(band) = damage.last_mut() {
        if band.x as usize == changed.x
            && band.width as usize == changed.width
            && band.y as usize + band.height as usize == y
        {
            band.height += 1;
            return;
        }
    }
    if damage.len() == limit {
        damage.clear();
        damage.push(damage_from_rect(*bounds));
        *collapsed = true;
    } else {
        damage.push(damage_from_rect(changed));
    }
}

/// Commit only the regions modified during an incremental scene rebuild into
/// `atlas` and record their exact disjoint damage bands. Regions unchanged from
/// the retained atlas and unused pixels outside the new layout stay untouched
/// in `atlas` (and therefore remain identical to every GPU backing slot).
#[allow(clippy::too_many_arguments)]
fn diff_and_commit_incremental_rebuild(
    atlas: &mut [u32],
    scratch: &[u32],
    atlas_width: usize,
    atlas_height: usize,
    stride: usize,
    changed_outer_rects: &[Rect],
    border_only_rects: &[Rect],
    damage: &mut Vec<DvmGpuAtlasDamage>,
) -> Result<(), i32> {
    damage.clear();
    if atlas.len() != scratch.len()
        || atlas_width == 0
        || atlas_height == 0
        || stride < atlas_width
        || u32::try_from(atlas_width).is_err()
        || u32::try_from(atlas_height).is_err()
        || (atlas_height - 1)
            .checked_mul(stride)
            .and_then(|row| row.checked_add(atlas_width))
            .is_none_or(|required| required > atlas.len())
    {
        return Err(EINVAL);
    }
    let mut bounds = Rect::empty();
    let mut collapsed = false;
    for &outer in changed_outer_rects {
        if outer.is_empty() {
            continue;
        }
        if outer.x.checked_add(outer.width).is_none_or(|end| end > atlas_width)
            || outer.y.checked_add(outer.height).is_none_or(|end| end > atlas_height)
        {
            return Err(EINVAL);
        }
        crate::simd::diff_and_commit_rect_u32(
            atlas,
            scratch,
            outer.x,
            outer.y,
            outer.width,
            outer.height,
            stride,
            |y, first, last| push_damage_span(damage, &mut bounds, &mut collapsed, y, first, last),
        );
    }
    for &inner in border_only_rects {
        if inner.is_empty() {
            continue;
        }
        if inner.x == 0
            || inner.y == 0
            || inner.x + inner.width >= atlas_width
            || inner.y + inner.height >= atlas_height
        {
            return Err(EINVAL);
        }
        let outer_x = inner.x - 1;
        let outer_y = inner.y - 1;
        let outer_w = inner.width + 2;
        let outer_right = inner.x + inner.width;
        let outer_bottom = inner.y + inner.height;

        // Top and bottom 1-px padding rows.
        for row_y in [outer_y, outer_bottom] {
            let start = row_y * stride + outer_x;
            let row = &mut atlas[start..start + outer_w];
            if let Some(first) = row.iter().position(|&p| p != 0) {
                let last = row.iter().rposition(|&p| p != 0).unwrap_or(first);
                row[first..=last].fill(0);
                push_damage_span(
                    damage,
                    &mut bounds,
                    &mut collapsed,
                    row_y,
                    outer_x + first,
                    outer_x + last,
                );
            }
        }
        // Left and right 1-px padding columns (between top and bottom rows).
        for col_x in [outer_x, outer_right] {
            for row_y in inner.y..inner.y + inner.height {
                let idx = row_y * stride + col_x;
                if atlas[idx] != 0 {
                    atlas[idx] = 0;
                    push_damage_span(damage, &mut bounds, &mut collapsed, row_y, col_x, col_x);
                }
            }
        }
    }
    Ok(())
}

/// Compare owned atlases without turning distant changed rows into one large
/// upload. Equal horizontal spans merge only across adjacent changed rows;
/// each resulting band has a disjoint Y range. If fragmentation exceeds the
/// wire limit, one conservative bounding rectangle covers every changed pixel.
fn difference_damage_into(
    previous: &[u32],
    next: &[u32],
    width: usize,
    height: usize,
    stride: usize,
    damage: &mut Vec<DvmGpuAtlasDamage>,
) -> Result<(), i32> {
    damage.clear();
    if previous.len() != next.len()
        || width == 0
        || height == 0
        || stride < width
        || u32::try_from(width).is_err()
        || u32::try_from(height).is_err()
        || (height - 1).checked_mul(stride)
            .and_then(|row| row.checked_add(width)).is_none()
    {
        return Err(EINVAL);
    }
    let mut bounds = Rect::empty();
    let mut collapsed = false;
    crate::simd::for_each_diff_row_u32(
        previous,
        next,
        0,
        0,
        width,
        height,
        stride,
        |y, first, last| push_damage_span(damage, &mut bounds, &mut collapsed, y, first, last),
    );
    Ok(())
}

/// Keep exact bounding-box expectations readable in existing regression
/// cases while exercising the same band extraction used by real submissions.
#[cfg(test)]
fn difference_bounds(
    previous: &[u32],
    next: &[u32],
    width: usize,
    height: usize,
    stride: usize,
) -> Option<Rect> {
    let mut damage = Vec::new();
    difference_damage_into(previous, next, width, height, stride, &mut damage).ok()?;
    let mut bounds = Rect::empty();
    for band in damage {
        bounds = bounds.union(Rect {
            x: band.x as usize,
            y: band.y as usize,
            width: band.width as usize,
            height: band.height as usize,
        });
    }
    (!bounds.is_empty()).then_some(bounds)
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
    if !driver_domain_protocol::dvm_gpu_atlas_damage_is_valid(
        damage,
        atlas_width,
        atlas_height,
        false,
    ) {
        return Err(EINVAL);
    }
    for rect in damage {
        let width = rect.width as usize;
        let x = rect.x as usize;
        // The complete geometry and every rectangle were checked before any
        // stores. Every offset below is bounded by `required`, including the
        // final row; no fallible branch may bypass the publication fence.
        let start = rect.y as usize * stride_pixels + x;
        if width == stride_pixels {
            let end = start + rect.height as usize * stride_pixels;
            copy_row_to_write_combining_mapping(
                &mut destination[start..end],
                &source[start..end],
            );
            continue;
        }
        for row in 0..rect.height as usize {
            let row_start = start + row * stride_pixels;
            let row_end = row_start + width;
            copy_row_to_write_combining_mapping(
                &mut destination[row_start..row_end],
                &source[row_start..row_end],
            );
        }
    }
    finish_write_combining_copy(!damage.is_empty());
    Ok(())
}

fn copy_row_to_write_combining_mapping(destination: &mut [u32], source: &[u32]) {
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
            while index + 16 <= destination.len() {
                // SAFETY: a whole 64-byte block is in both slices. Each
                // destination is 16-byte aligned after the prefix above;
                // source reads permit unaligned addresses.
                unsafe {
                    let source_ptr = source.as_ptr().add(index).cast::<__m128i>();
                    let destination_ptr = destination.as_mut_ptr().add(index).cast::<__m128i>();
                    let a = _mm_loadu_si128(source_ptr);
                    let b = _mm_loadu_si128(source_ptr.add(1));
                    let c = _mm_loadu_si128(source_ptr.add(2));
                    let d = _mm_loadu_si128(source_ptr.add(3));
                    _mm_stream_si128(destination_ptr, a);
                    _mm_stream_si128(destination_ptr.add(1), b);
                    _mm_stream_si128(destination_ptr.add(2), c);
                    _mm_stream_si128(destination_ptr.add(3), d);
                }
                index += 16;
            }
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
            return;
        }
    }

    destination.copy_from_slice(source);
}

fn finish_write_combining_copy(wrote_pixels: bool) {
    #[cfg(target_arch = "x86_64")]
    if wrote_pixels {
        // SAFETY: SSE2 is part of the x86-64 baseline. The fence makes all
        // slot writes globally visible before the commit ioctl publishes the
        // generation and signals the DVM. Small scalar tails also target a
        // write-combining mapping, so they need the same publication fence.
        unsafe {
            core::arch::x86_64::_mm_sfence();
        }
    }
}

#[cfg(test)]
fn snapshot_damage_for_slot(
    retained_epoch: u64,
    current_epoch: u64,
    requested: &[DvmGpuAtlasDamage],
    history: &VecDeque<AtlasDamageEpoch>,
    atlas_width: u32,
    atlas_height: u32,
) -> Result<Vec<DvmGpuAtlasDamage>, i32> {
    let mut damage = Vec::new();
    snapshot_damage_for_slot_into(
        retained_epoch,
        current_epoch,
        requested,
        history,
        atlas_width,
        atlas_height,
        &mut damage,
    )?;
    Ok(damage)
}

fn snapshot_damage_for_slot_into(
    retained_epoch: u64,
    current_epoch: u64,
    requested: &[DvmGpuAtlasDamage],
    history: &VecDeque<AtlasDamageEpoch>,
    atlas_width: u32,
    atlas_height: u32,
    accumulated: &mut Vec<DvmGpuAtlasDamage>,
) -> Result<(), i32> {
    accumulated.clear();
    let predecessor = current_epoch.checked_sub(1).ok_or(EINVAL)?;
    if atlas_width == 0 || atlas_height == 0 {
        return Err(EINVAL);
    }
    validate_damage(requested, atlas_width, atlas_height)?;
    if retained_epoch != 0 && retained_epoch == predecessor {
        // The wire rejects overlapping damage even for an exact predecessor.
        // Normalizing here also avoids copying the same texels repeatedly.
        if !accumulate_damage(accumulated, requested, atlas_width, atlas_height)? {
            full_atlas_damage(accumulated, atlas_width, atlas_height);
        }
        return Ok(());
    }
    if retained_epoch == 0 {
        full_atlas_damage(accumulated, atlas_width, atlas_height);
        return Ok(());
    }
    if retained_epoch >= current_epoch {
        debug_line(&format!(
            "uiserver: gpu damage epoch rejected retained={retained_epoch} current={current_epoch}"
        ));
        return Err(EINVAL);
    }

    let mut expected_epoch = retained_epoch.checked_add(1).ok_or(EINVAL)?;
    for entry in history {
        if entry.epoch < expected_epoch {
            continue;
        }
        if entry.epoch >= current_epoch {
            break;
        }
        if entry.epoch != expected_epoch {
            full_atlas_damage(accumulated, atlas_width, atlas_height);
            return Ok(());
        }
        if !accumulate_damage(
            accumulated,
            entry.damage.as_slice(),
            atlas_width,
            atlas_height,
        )? {
            full_atlas_damage(accumulated, atlas_width, atlas_height);
            return Ok(());
        }
        expected_epoch = expected_epoch.checked_add(1).ok_or(EINVAL)?;
    }
    if expected_epoch != current_epoch {
        full_atlas_damage(accumulated, atlas_width, atlas_height);
        return Ok(());
    }
    if !accumulate_damage(accumulated, requested, atlas_width, atlas_height)? {
        full_atlas_damage(accumulated, atlas_width, atlas_height);
    }
    Ok(())
}

fn full_atlas_damage(
    damage: &mut Vec<DvmGpuAtlasDamage>,
    atlas_width: u32,
    atlas_height: u32,
) {
    damage.clear();
    damage.push(DvmGpuAtlasDamage {
        x: 0,
        y: 0,
        width: atlas_width,
        height: atlas_height,
    });
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
            debug_line(&format!(
                "uiserver: gpu damage rejected rect={},{} {}x{} atlas={}x{}",
                rect.x, rect.y, rect.width, rect.height, atlas_width, atlas_height
            ));
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
                merged = damage_union(accumulated.swap_remove(index), merged)?;
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

/// Malformed scenes are contract failures. Representability pressure is
/// handled before this conversion as retryable whole-scene backpressure.
fn gpu_scene_errno(error: GpuSceneError) -> i32 {
    debug_line(&format!("uiserver: gpu scene rejected reason={error:?}"));
    EINVAL
}

#[cfg(test)]
#[path = "gpu_runtime_tests.rs"]
mod tests;
