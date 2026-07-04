mod app;
mod canvas;
mod cursor_sprites;
mod font;
mod input_loop;
mod layout;
mod profile;
mod render;
mod runtime_sync;
mod simd;
mod sys;
mod terminal;
mod wayland;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use app::{
    start_console_refresh_worker, start_launcher_program_loader, AppState, VisualUpdate,
    CONSOLE_POLL_SLEEP, CURSOR_BLINK_INTERVAL, CURSOR_MOTION_SETTLE_INTERVAL, RUNTIME_POLL_SLEEP,
};
use render::start_desktop_background_loader;
use render::{render_boot_frame, render_debug_white_box, render_frame, render_rect};
use runtime_control::RuntimeClient;
use runtime_sync::{refresh_runtime_state, start_runtime_sync, RuntimeState};
use sys::{boot_line, boot_trace_enabled, diag_line, profile_line};
use wayland::WaylandCompositor;

const DEBUG_FREEZE_ON_WHITE_BOX: bool = false;
const DEBUG_DIRECT_SURFACE_PROBE: bool = false;
const SLOW_CONSOLE_REFRESH_THRESHOLD: Duration = Duration::from_millis(12);
const SLOW_PRESENT_THRESHOLD: Duration = Duration::from_millis(16);
const MAX_FRAME_SAMPLE_LOGS: usize = 8;
const MAX_SLOW_CONSOLE_REFRESH_LOGS: usize = 4;
const MAX_SLOW_PRESENT_LOGS: usize = 8;
const SLOW_RUNTIME_REFRESH_THRESHOLD: Duration = Duration::from_millis(100);
const MAX_SLOW_RUNTIME_REFRESH_LOGS: usize = 8;
const MAX_WAYLAND_CALLBACK_ONLY_LOGS: usize = 8;
const MAX_CURSOR_PIPELINE_LOGS: usize = 32;
const UI_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const UI_INPUT_PULSE_INTERVAL: Duration = Duration::from_millis(40);
const UI_WATCHDOG_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const UI_PHASE_PANIC_THRESHOLD_MS: u64 = 3_000;
const WAYLAND_BACKLOG_SERVICE_INTERVAL: Duration = Duration::from_millis(4);
const PARTIAL_RENDER_BUDGET: Duration = Duration::from_millis(8);
const SLOW_LOOP_THRESHOLD: Duration = Duration::from_millis(50);
const MAX_SLOW_LOOP_LOGS: usize = 16;

const UI_PHASE_IDLE: usize = 0;
const UI_PHASE_INPUT: usize = 1;
const UI_PHASE_INPUT_PRESENT: usize = 2;
const UI_PHASE_WAYLAND: usize = 3;
const UI_PHASE_RUNTIME: usize = 4;
const UI_PHASE_CONSOLE: usize = 5;
const UI_PHASE_MAIN_PRESENT: usize = 6;

static FRAME_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_CONSOLE_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_PRESENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_RUNTIME_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAYLAND_CALLBACK_ONLY_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static POINTER_MOVED_LOGGED: AtomicUsize = AtomicUsize::new(0);
static CURSOR_PIPELINE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_LOOP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct LoopPhaseTimings {
    input: Duration,
    input_present: Duration,
    wayland: Duration,
    runtime: Duration,
    console: Duration,
    cursor: Duration,
    main_present: Duration,
    sleep: Duration,
}

impl LoopPhaseTimings {
    fn total_excluding_sleep(&self) -> Duration {
        self.input
            + self.input_present
            + self.wayland
            + self.runtime
            + self.console
            + self.cursor
            + self.main_present
    }
}

enum PresentUpdateResult {
    Idle,
    Rendered { deferred_update: VisualUpdate },
    StaleSurface,
}

#[derive(Clone)]
struct UiLoopWatchdog {
    boot_started: Instant,
    phase: Arc<AtomicUsize>,
    phase_started_ms: Arc<AtomicU64>,
    loop_seq: Arc<AtomicU64>,
}

impl UiLoopWatchdog {
    fn start(boot_started: Instant) -> Self {
        let watchdog = Self {
            boot_started,
            phase: Arc::new(AtomicUsize::new(UI_PHASE_IDLE)),
            phase_started_ms: Arc::new(AtomicU64::new(0)),
            loop_seq: Arc::new(AtomicU64::new(0)),
        };
        let phase = Arc::clone(&watchdog.phase);
        let phase_started_ms = Arc::clone(&watchdog.phase_started_ms);
        let loop_seq = Arc::clone(&watchdog.loop_seq);
        let thread_started = boot_started;
        let _ = thread::Builder::new()
            .name(String::from("uiserver-watchdog"))
            .spawn(move || loop {
                thread::sleep(UI_WATCHDOG_CHECK_INTERVAL);
                let phase_id = phase.load(Ordering::Acquire);
                if phase_id == UI_PHASE_IDLE {
                    continue;
                }
                let started_ms = phase_started_ms.load(Ordering::Acquire);
                let now_ms = elapsed_ms_since(thread_started);
                let elapsed_ms = now_ms.saturating_sub(started_ms);
                if elapsed_ms < UI_PHASE_PANIC_THRESHOLD_MS {
                    continue;
                }
                diag_line(format!(
                    "uiserver watchdog panic: phase={} elapsed_ms={} loop_seq={}",
                    ui_phase_name(phase_id),
                    elapsed_ms,
                    loop_seq.load(Ordering::Acquire),
                ));
                std::process::exit(134);
            });
        watchdog
    }

    fn begin_loop(&self, loop_id: u64) {
        self.loop_seq.store(loop_id, Ordering::Release);
    }

    fn enter(&self, phase: usize) {
        self.phase_started_ms
            .store(elapsed_ms_since(self.boot_started), Ordering::Release);
        self.phase.store(phase, Ordering::Release);
    }

    fn leave(&self) {
        self.phase.store(UI_PHASE_IDLE, Ordering::Release);
    }
}

fn elapsed_ms_since(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn ui_phase_name(phase: usize) -> &'static str {
    match phase {
        UI_PHASE_INPUT => "input",
        UI_PHASE_INPUT_PRESENT => "input-present",
        UI_PHASE_WAYLAND => "wayland",
        UI_PHASE_RUNTIME => "runtime",
        UI_PHASE_CONSOLE => "console",
        UI_PHASE_MAIN_PRESENT => "main-present",
        _ => "idle",
    }
}

fn cursor_move_update(
    state: &AppState,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
) -> VisualUpdate {
    if presented_cursor_x == state.cursor_x && presented_cursor_y == state.cursor_y {
        return VisualUpdate::default();
    }

    let mut update = VisualUpdate::partial(canvas::cursor_dirty_rect(
        presented_cursor_x,
        presented_cursor_y,
        state.surface.width,
        state.surface.height,
    ));
    update.add_partial_rect(
        state.cursor_visual_dirty_rect(state.surface.width, state.surface.height),
    );
    update
}

fn prepare_drawable_update(
    state: &AppState,
    pending_update: &VisualUpdate,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
) -> VisualUpdate {
    let mut drawable_update = pending_update.clone();
    drawable_update.absorb(cursor_move_update(
        state,
        presented_cursor_x,
        presented_cursor_y,
    ));
    drawable_update.coalesce_tight_partials();
    drawable_update.promote_large_partial(state.surface.width, state.surface.height);
    drawable_update.split_large_partials();
    drawable_update
}

fn present_drawable_update(
    state: &mut AppState,
    drawable_update: &VisualUpdate,
) -> Result<PresentUpdateResult, i32> {
    if drawable_update.needs_full_redraw {
        let render_started = Instant::now();
        render_frame(state);
        let render_elapsed = render_started.elapsed();
        log_frame_sample("full", state);
        let present_started = Instant::now();
        match state.present() {
            Ok(()) => {}
            Err(err) if state.recover_if_stale_surface_error(err)? => {
                return Ok(PresentUpdateResult::StaleSurface);
            }
            Err(err) => return Err(err),
        }
        let present_elapsed = present_started.elapsed();
        profile::record_present(
            true,
            1,
            u64::from(state.surface.width) * u64::from(state.surface.height),
            render_elapsed,
            present_elapsed,
        );
        let total_elapsed = render_elapsed + present_elapsed;
        if total_elapsed >= SLOW_PRESENT_THRESHOLD {
            log_slow_present(
                state,
                total_elapsed,
                render_elapsed,
                present_elapsed,
                true,
                None,
                1,
                u64::from(state.surface.width) * u64::from(state.surface.height),
                0,
            );
        }
        return Ok(PresentUpdateResult::Rendered {
            deferred_update: VisualUpdate::default(),
        });
    }

    if drawable_update.partial_rects().is_empty() {
        return Ok(PresentUpdateResult::Idle);
    }

    let render_rects = drawable_update.partial_rects().to_vec();
    let mut rendered_rects = Vec::with_capacity(render_rects.len());
    let mut render_elapsed = Duration::ZERO;
    let mut present_elapsed = Duration::ZERO;
    let mut present_union = canvas::Rect::empty();
    let mut pixel_count = 0_u64;
    let mut deferred_update = VisualUpdate::default();

    for (index, rect) in render_rects.iter().enumerate() {
        present_union = present_union.union(*rect);
        pixel_count = pixel_count.saturating_add(rect.width.saturating_mul(rect.height) as u64);

        let render_started = Instant::now();
        render_rect(state, *rect);
        render_elapsed += render_started.elapsed();
        rendered_rects.push(*rect);
        if index + 1 < render_rects.len() && render_elapsed >= PARTIAL_RENDER_BUDGET {
            for deferred_rect in &render_rects[index + 1..] {
                deferred_update.add_partial_rect(*deferred_rect);
            }
            break;
        }
    }

    let present_started = Instant::now();
    for rect in &rendered_rects {
        match state.present_rect(*rect) {
            Ok(()) => {}
            Err(err) if state.recover_if_stale_surface_error(err)? => {
                return Ok(PresentUpdateResult::StaleSurface);
            }
            Err(err) => return Err(err),
        }
    }
    present_elapsed += present_started.elapsed();
    let rect_count = rendered_rects.len() as u64;
    profile::record_present(
        false,
        rect_count,
        pixel_count,
        render_elapsed,
        present_elapsed,
    );
    let total_elapsed = render_elapsed + present_elapsed;
    if total_elapsed >= SLOW_PRESENT_THRESHOLD {
        log_slow_present(
            state,
            total_elapsed,
            render_elapsed,
            present_elapsed,
            false,
            Some(present_union),
            rect_count,
            pixel_count,
            deferred_update.partial_rects().len(),
        );
    }
    Ok(PresentUpdateResult::Rendered { deferred_update })
}

fn commit_rendered_update(
    phase: &str,
    state: &AppState,
    wayland: &mut Option<WaylandCompositor>,
    pending_update: &mut VisualUpdate,
    deferred_update: VisualUpdate,
    presented_cursor_x: &mut u32,
    presented_cursor_y: &mut u32,
    frames_rendered_window: &mut u64,
    cursor_moves_window: &mut u64,
) {
    *frames_rendered_window = frames_rendered_window.saturating_add(1);
    if let Some(compositor) = wayland.as_mut() {
        compositor.frame_presented();
    }
    let previous_presented_cursor_x = *presented_cursor_x;
    let previous_presented_cursor_y = *presented_cursor_y;
    let cursor_moved = previous_presented_cursor_x != state.cursor_x
        || previous_presented_cursor_y != state.cursor_y;
    let deferred_rects = deferred_update.partial_rects().len();
    pending_update.clear();
    pending_update.absorb(deferred_update);
    *presented_cursor_x = state.cursor_x;
    *presented_cursor_y = state.cursor_y;
    if cursor_moved {
        *cursor_moves_window = cursor_moves_window.saturating_add(1);
        log_pointer_moved_once(state);
        log_cursor_pipeline_sample(
            phase,
            state,
            pending_update,
            deferred_rects,
            previous_presented_cursor_x,
            previous_presented_cursor_y,
            cursor_moved,
            true,
        );
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        diag_line(format!("uiserver panic: {info}"));
    }));
}

fn log_frame_sample(stage: &str, state: &mut AppState) {
    if !boot_trace_enabled() {
        return;
    }
    let sample_index = FRAME_SAMPLE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_FRAME_SAMPLE_LOGS {
        return;
    }

    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    if pixels.is_empty() {
        boot_line(&format!(
            "uiserver: frame sample #{} stage={} pixels=empty",
            sample_index + 1,
            stage,
        ));
        return;
    }

    let pixel0 = pixels[0];
    let row1 = pixels
        .get(stride_pixels.min(pixels.len().saturating_sub(1)))
        .copied()
        .unwrap_or(pixel0);
    let center_index = ((state.surface.height as usize) / 2)
        .saturating_mul(stride_pixels)
        .saturating_add((state.surface.width as usize) / 2)
        .min(pixels.len().saturating_sub(1));
    let center = pixels[center_index];
    boot_line(&format!(
        "uiserver: frame sample #{} stage={} pixel0={:#010x} row1={:#010x} center={:#010x}",
        sample_index + 1,
        stage,
        pixel0,
        row1,
        center,
    ));
}

fn halt_after_debug_present() -> ! {
    loop {
        input_loop::sleep_until(Instant::now() + Duration::from_secs(60));
    }
}

fn stamp_raw_surface_probe(stage: &str, state: &mut AppState) {
    if !boot_trace_enabled() {
        return;
    }
    let surface_width = state.surface.width as usize;
    let surface_height = state.surface.height as usize;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let probe_width = surface_width.min(192);
    let probe_height = surface_height.min(128);
    let pixels = state.frame.pixels_mut();
    if pixels.is_empty() || stride_pixels == 0 || probe_width == 0 || probe_height == 0 {
        boot_line(&format!(
            "uiserver: raw surface probe stage={} skipped pixels={} stride_pixels={} surface={}x{} probe={}x{}",
            stage,
            pixels.len(),
            stride_pixels,
            surface_width,
            surface_height,
            probe_width,
            probe_height,
        ));
        return;
    }

    let before = pixels[0];
    for row in 0..probe_height {
        let Some(row_start) = row.checked_mul(stride_pixels) else {
            break;
        };
        let Some(row_end) = row_start.checked_add(probe_width) else {
            break;
        };
        let Some(row_pixels) = pixels.get_mut(row_start..row_end) else {
            break;
        };
        row_pixels.fill(0x00ff_ffff);
    }
    let after = pixels[0];
    boot_line(&format!(
        "uiserver: raw surface probe stage={} before={:#010x} after={:#010x} width={} height={} surface={}x{} stride_pixels={}",
        stage,
        before,
        after,
        probe_width,
        probe_height,
        surface_width,
        surface_height,
        stride_pixels,
    ));
}

fn log_slow_console_refresh(state: &AppState, elapsed: Duration) {
    let sample_index = SLOW_CONSOLE_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_CONSOLE_REFRESH_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: slow console refresh elapsed_ms={} console_windows={} focused_session={} wayland_windows={}",
        elapsed.as_millis(),
        state.console_windows.len(),
        state.focused_session_handle,
        state.wayland_windows.len(),
    ));
}

fn log_slow_runtime_refresh(
    state: &AppState,
    elapsed: Duration,
    runtime_changed: bool,
    apply_changed: bool,
) {
    let sample_index = SLOW_RUNTIME_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_RUNTIME_REFRESH_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: slow runtime refresh elapsed_ms={} runtime_changed={} apply_changed={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
        elapsed.as_millis(),
        runtime_changed,
        apply_changed,
        state.console_windows.len(),
        state.wayland_windows.len(),
        state.focused_session_handle,
        state.focused_wayland_surface_id,
    ));
}

fn log_slow_present(
    state: &AppState,
    elapsed: Duration,
    render_elapsed: Duration,
    present_elapsed: Duration,
    full_redraw: bool,
    rect: Option<crate::canvas::Rect>,
    rect_count: u64,
    pixel_count: u64,
    deferred_rects: usize,
) {
    let sample_index = SLOW_PRESENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_PRESENT_LOGS {
        return;
    }
    let message = if full_redraw {
        format!(
            "uiserver: slow full present elapsed_ms={} render_ms={} present_ms={} rects={} pixels={} deferred_rects={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            render_elapsed.as_millis(),
            present_elapsed.as_millis(),
            rect_count,
            pixel_count,
            deferred_rects,
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
    } else {
        let rect = rect.unwrap_or_default();
        format!(
            "uiserver: slow partial present elapsed_ms={} render_ms={} present_ms={} rect={}x{}@{},{} rects={} pixels={} deferred_rects={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            render_elapsed.as_millis(),
            present_elapsed.as_millis(),
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            rect_count,
            pixel_count,
            deferred_rects,
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
    };
    diag_line(message);
}

fn log_boot_stage(started_at: Instant, stage: &str) {
    diag_line(format!(
        "uiserver: boot stage={} elapsed_us={}",
        stage,
        started_at.elapsed().as_micros(),
    ));
}

fn phase_result<T>(phase: &str, result: Result<T, i32>) -> Result<T, i32> {
    if let Err(errno) = result {
        diag_line(format!(
            "uiserver: phase failed phase={phase} errno={errno}"
        ));
        return Err(errno);
    }
    result
}

fn log_pointer_moved_once(state: &AppState) {
    if POINTER_MOVED_LOGGED
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        diag_line(format!(
            "uiserver: pointer moved x={} y={}",
            state.cursor_x, state.cursor_y
        ));
    }
}

fn log_cursor_pipeline_sample(
    phase: &str,
    state: &AppState,
    update: &VisualUpdate,
    deferred_rects: usize,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
    cursor_mismatch: bool,
    rendered: bool,
) {
    if !profile::enabled() {
        return;
    }
    if !cursor_mismatch && update.is_empty() {
        return;
    }
    let index = CURSOR_PIPELINE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_CURSOR_PIPELINE_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: cursor pipeline phase={} rendered={} cursor={},{} presented={},{} mismatch={} full={} rects={} deferred_rects={} update_empty={}",
        phase,
        rendered,
        state.cursor_x,
        state.cursor_y,
        presented_cursor_x,
        presented_cursor_y,
        cursor_mismatch,
        update.needs_full_redraw,
        update.partial_rects().len(),
        deferred_rects,
        update.is_empty(),
    ));
}

fn log_slow_loop_iteration(
    state: &AppState,
    iteration_elapsed: Duration,
    timings: &LoopPhaseTimings,
    backlog_remaining: bool,
) {
    let index = SLOW_LOOP_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if index >= MAX_SLOW_LOOP_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: slow loop iter_ms={} input_ms={} input_present_ms={} wayland_ms={} runtime_ms={} console_ms={} cursor_ms={} present_ms={} sleep_ms={} backlog={} console_windows={} wayland_windows={}",
        iteration_elapsed.as_millis(),
        timings.input.as_millis(),
        timings.input_present.as_millis(),
        timings.wayland.as_millis(),
        timings.runtime.as_millis(),
        timings.console.as_millis(),
        timings.cursor.as_millis(),
        timings.main_present.as_millis(),
        timings.sleep.as_millis(),
        backlog_remaining,
        state.console_windows.len(),
        state.wayland_windows.len(),
    ));
}

fn log_wayland_callback_only(state: &AppState, rect: canvas::Rect) {
    if !profile::enabled() {
        return;
    }
    let log_index = WAYLAND_CALLBACK_ONLY_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if log_index >= MAX_WAYLAND_CALLBACK_ONLY_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: wayland callback-only dispatch rect={}x{}@{},{} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
        rect.width,
        rect.height,
        rect.x,
        rect.y,
        state.console_windows.len(),
        state.wayland_windows.len(),
        state.focused_session_handle,
        state.focused_wayland_surface_id,
    ));
}

fn run() -> Result<(), i32> {
    let boot_started = Instant::now();
    diag_line("uiserver: run initialize begin");
    let mut state = AppState::initialize()?;
    sys::publish_display_policy_metadata(state.display);
    diag_line("uiserver: run initialize done");
    log_boot_stage(boot_started, "initialize");
    let desktop_background = start_desktop_background_loader(
        state.surface.width as usize,
        state.surface.height as usize,
    );
    if DEBUG_FREEZE_ON_WHITE_BOX {
        boot_line("uiserver: debug white box begin");
        render_debug_white_box(&mut state);
        log_frame_sample("debug-white-box", &mut state);
        boot_line("uiserver: debug white box present begin");
        state.present()?;
        boot_line("uiserver: debug white box present done");
        boot_line("uiserver: debug white box halt");
        halt_after_debug_present();
    }
    diag_line("uiserver: boot frame begin");
    render_boot_frame(&mut state);
    if DEBUG_DIRECT_SURFACE_PROBE {
        stamp_raw_surface_probe("boot", &mut state);
    }
    log_frame_sample("boot", &mut state);
    diag_line("uiserver: boot frame done");
    log_boot_stage(boot_started, "boot_frame");
    diag_line("uiserver: first present begin");
    let first_present_started = Instant::now();
    state.present()?;
    diag_line("uiserver: first present done");
    log_boot_stage(first_present_started, "first_present");
    diag_line("uiserver: post-present init begin");
    let mut pending_update = VisualUpdate::default();
    let mut presented_cursor_x = state.cursor_x;
    let mut presented_cursor_y = state.cursor_y;
    diag_line("uiserver: runtime client open begin");
    let runtime_open_started = Instant::now();
    let mut runtime_state = RuntimeState::default();
    let runtime = RuntimeClient::open_default().map_err(|_| {
        diag_line("uiserver: open runtimed socket failed");
        19
    })?;
    diag_line("uiserver: runtime client open done");
    log_boot_stage(runtime_open_started, "runtime_client_open");
    let runtime_sync = RuntimeClient::open_default()
        .map(start_runtime_sync)
        .map_err(|_| {
            diag_line("uiserver: open runtimed sync socket failed");
            19
        })?;
    diag_line("uiserver: wayland initialize begin");
    let wayland_init_started = Instant::now();
    let mut wayland = WaylandCompositor::initialize(state.display.width, state.display.height);
    diag_line("uiserver: wayland initialize done");
    log_boot_stage(wayland_init_started, "wayland_initialize");
    sys::mark_display_policy_ready();
    match runtime.notify_ui_ready() {
        Ok(()) => diag_line("uiserver: ui ready notified"),
        Err(err) => diag_line(format!("uiserver: ui ready notify failed errno={err}")),
    }
    diag_line("uiserver: post-present init done");
    log_boot_stage(boot_started, "post_present_init");
    let launcher_programs = start_launcher_program_loader();
    let console_refreshes = start_console_refresh_worker();
    let input_events = input_loop::start_input_reader(std::mem::take(&mut state.input_fds));
    let mut next_runtime_poll = Instant::now();
    let mut next_console_poll = Instant::now() + CONSOLE_POLL_SLEEP;
    let mut next_cursor_blink = Instant::now() + CURSOR_BLINK_INTERVAL;
    let mut next_cursor_motion_settle = Instant::now() + CURSOR_MOTION_SETTLE_INTERVAL;
    let mut next_loop_summary = Instant::now() + UI_HEARTBEAT_INTERVAL;
    let mut next_input_pulse = Instant::now();
    let mut next_wayland_backlog_service = Instant::now();
    let mut loop_count = 0_u64;
    let mut total_loop_count = 0_u64;
    let mut frames_rendered_window = 0_u64;
    let mut cursor_moves_window = 0_u64;
    let mut input_events_window = 0_u64;
    let mut pointer_motion_events_window = 0_u64;
    let mut pointer_position_events_window = 0_u64;
    let mut wayland_motion_calls_window = 0_u64;
    let mut input_backlog_window = 0_u64;
    let mut processed_input_events_total = 0_u64;
    let mut last_input_processed_at: Option<Instant> = None;
    let mut max_input_gap_ms_window = 0_u64;
    let watchdog = UiLoopWatchdog::start(boot_started);

    sys::debug_line("uiserver: main loop entered");
    'main_loop: loop {
        loop_count = loop_count.saturating_add(1);
        total_loop_count = total_loop_count.saturating_add(1);
        watchdog.begin_loop(total_loop_count);
        let iteration_started = Instant::now();
        let mut phase_timings = LoopPhaseTimings::default();

        let mut background_ready = false;
        while let Ok(background) = desktop_background.try_recv() {
            if background.width == state.surface.width as usize
                && background.height == state.surface.height as usize
            {
                state.desktop_cache.width = background.width;
                state.desktop_cache.height = background.height;
                state.desktop_cache.background_pixels = background.pixels;
                state.desktop_cache.background_valid = true;
                state.desktop_cache.chrome_valid = false;
                background_ready = true;
            }
        }
        if background_ready {
            diag_line("uiserver: desktop background ready");
            pending_update.request_full();
        }

        while let Ok(programs) = launcher_programs.try_recv() {
            if state.apply_launcher_programs(programs) {
                pending_update.absorb(VisualUpdate::partial(render::launcher_dirty_rect(
                    state.surface.width,
                    state.surface.height,
                )));
            }
        }

        let input_started = Instant::now();
        watchdog.enter(UI_PHASE_INPUT);
        let input = phase_result(
            "input",
            input_loop::process_pending_input(
                &mut state,
                wayland.as_mut(),
                &runtime,
                &input_events,
            ),
        )?;
        watchdog.leave();
        if input.input_events > 0 {
            let now = Instant::now();
            if let Some(previous) = last_input_processed_at {
                max_input_gap_ms_window = max_input_gap_ms_window
                    .max(previous.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
            }
            last_input_processed_at = Some(now);
            if now >= next_input_pulse {
                let input_reader = input_events.snapshot();
                diag_line(format!(
                    "uiserver: input pulse events={} cursor={},{} pointer_motion={} pointer_position={} wayland_motion_calls={} backlog={} input_raw_events={} input_events={} input_drops={} input_errors={}",
                    input.input_events,
                    state.cursor_x,
                    state.cursor_y,
                    input.pointer_motion_events,
                    input.pointer_position_events,
                    input.wayland_motion_calls,
                    input.backlog_remaining,
                    input_reader.raw_events,
                    input_reader.delivered_events,
                    input_reader.queue_drops,
                    input_reader.errors,
                ));
                next_input_pulse = now + UI_INPUT_PULSE_INTERVAL;
            }
        }
        processed_input_events_total =
            processed_input_events_total.saturating_add(input.input_events);
        input_events_window = input_events_window.saturating_add(input.input_events);
        pointer_motion_events_window =
            pointer_motion_events_window.saturating_add(input.pointer_motion_events);
        pointer_position_events_window =
            pointer_position_events_window.saturating_add(input.pointer_position_events);
        wayland_motion_calls_window =
            wayland_motion_calls_window.saturating_add(input.wayland_motion_calls);
        if input.backlog_remaining {
            input_backlog_window = input_backlog_window.saturating_add(1);
        }
        pending_update.absorb(input.visual_update);
        phase_timings.input = input_started.elapsed();

        let input_present_started = Instant::now();
        let drawable_update = prepare_drawable_update(
            &state,
            &pending_update,
            presented_cursor_x,
            presented_cursor_y,
        );
        log_cursor_pipeline_sample(
            "input-present-ready",
            &state,
            &drawable_update,
            0,
            presented_cursor_x,
            presented_cursor_y,
            state.cursor_x != presented_cursor_x || state.cursor_y != presented_cursor_y,
            false,
        );
        watchdog.enter(UI_PHASE_INPUT_PRESENT);
        let rendered_input_update = match phase_result(
            "input-present",
            present_drawable_update(&mut state, &drawable_update),
        )? {
            PresentUpdateResult::Rendered { deferred_update } => {
                commit_rendered_update(
                    "input-present",
                    &state,
                    &mut wayland,
                    &mut pending_update,
                    deferred_update,
                    &mut presented_cursor_x,
                    &mut presented_cursor_y,
                    &mut frames_rendered_window,
                    &mut cursor_moves_window,
                );
                true
            }
            PresentUpdateResult::StaleSurface => {
                watchdog.leave();
                pending_update.request_full();
                continue 'main_loop;
            }
            PresentUpdateResult::Idle => false,
        };
        watchdog.leave();
        phase_timings.input_present = input_present_started.elapsed();
        if rendered_input_update
            && input.backlog_remaining
            && Instant::now() < next_wayland_backlog_service
        {
            let iteration_elapsed = iteration_started.elapsed();
            if iteration_elapsed >= SLOW_LOOP_THRESHOLD {
                log_slow_loop_iteration(
                    &state,
                    iteration_elapsed,
                    &phase_timings,
                    input.backlog_remaining,
                );
            }
            profile::maybe_emit();
            continue 'main_loop;
        }

        let wayland_started = Instant::now();
        let now = wayland_started;
        let mut wayland_callback_only = false;
        let service_wayland = !input.backlog_remaining || now >= next_wayland_backlog_service;
        watchdog.enter(UI_PHASE_WAYLAND);
        if let Some(compositor) = wayland.as_mut() {
            if service_wayland {
                if compositor.tick() {
                    let wayland_dirty = state.sync_wayland_windows(compositor.window_snapshots());
                    compositor.clear_window_damage();
                    let focus_dirty = phase_result(
                        "wayland-focus",
                        state.recover_focus_after_wayland_change(Some(compositor)),
                    )?;
                    let mut update = VisualUpdate::partial(wayland_dirty);
                    update.add_partial_rect(focus_dirty);
                    pending_update.absorb(update);
                }
                let frame_callback_rect = compositor.pending_frame_callback_rect();
                wayland_callback_only =
                    pending_update.is_empty() && !frame_callback_rect.is_empty();
                if wayland_callback_only {
                    log_wayland_callback_only(&state, frame_callback_rect);
                }
                next_wayland_backlog_service = now + WAYLAND_BACKLOG_SERVICE_INTERVAL;
            } else if input.backlog_remaining {
                compositor.flush_clients();
            }
        }
        watchdog.leave();
        phase_timings.wayland = wayland_started.elapsed();

        let runtime_phase_started = Instant::now();
        let now = runtime_phase_started;
        watchdog.enter(UI_PHASE_RUNTIME);
        if now >= next_runtime_poll {
            let refresh_started = Instant::now();
            let runtime_changed = phase_result(
                "runtime-refresh",
                refresh_runtime_state(&runtime_sync, &mut runtime_state),
            )?;
            let apply_update = state.apply_runtime_state(&mut runtime_state);
            let apply_changed = !apply_update.is_empty();
            let refresh_elapsed = refresh_started.elapsed();
            profile::record_runtime_refresh(refresh_elapsed);
            if refresh_elapsed >= SLOW_RUNTIME_REFRESH_THRESHOLD {
                log_slow_runtime_refresh(&state, refresh_elapsed, runtime_changed, apply_changed);
            }
            next_runtime_poll = now + RUNTIME_POLL_SLEEP;
            pending_update.absorb(apply_update);
        }
        watchdog.leave();
        phase_timings.runtime = runtime_phase_started.elapsed();

        let console_phase_started = Instant::now();
        let now = console_phase_started;
        watchdog.enter(UI_PHASE_CONSOLE);
        if now >= next_console_poll {
            let refresh_started = Instant::now();
            let mut console_update = VisualUpdate::default();
            while let Ok(refresh) = console_refreshes.try_recv() {
                console_update.absorb(phase_result(
                    "console-refresh",
                    state.apply_console_refresh(refresh),
                )?);
            }
            let changed = !console_update.is_empty();
            pending_update.absorb(console_update);
            let refresh_elapsed = refresh_started.elapsed();
            profile::record_console_refresh(refresh_elapsed, changed);
            if refresh_elapsed >= SLOW_CONSOLE_REFRESH_THRESHOLD {
                log_slow_console_refresh(&state, refresh_elapsed);
            }
            next_console_poll = now + CONSOLE_POLL_SLEEP;
        }
        watchdog.leave();
        phase_timings.console = console_phase_started.elapsed();

        let cursor_phase_started = Instant::now();
        let now = cursor_phase_started;
        if now >= next_cursor_blink {
            if let Some(rect) = state.toggle_focused_terminal_cursor() {
                pending_update.absorb(VisualUpdate::partial(rect));
            }
            next_cursor_blink = now + CURSOR_BLINK_INTERVAL;
        }
        if state.cursor_motion_active() && now >= next_cursor_motion_settle {
            let cursor_dirty_rect =
                state.settle_cursor_motion(state.surface.width, state.surface.height);
            pending_update.absorb(VisualUpdate::partial(cursor_dirty_rect));
            next_cursor_motion_settle = now + CURSOR_MOTION_SETTLE_INTERVAL;
        }
        let drawable_update = prepare_drawable_update(
            &state,
            &pending_update,
            presented_cursor_x,
            presented_cursor_y,
        );
        phase_timings.cursor = cursor_phase_started.elapsed();

        let now = Instant::now();
        if now >= next_loop_summary {
            let input_reader = input_events.snapshot();
            let input_pending_reader = input_reader
                .delivered_events
                .saturating_sub(processed_input_events_total);
            diag_line(format!(
                "uiserver: update tick loops={} total_loops={} frames={} cursor_moves={} cursor={},{} presented_cursor={},{} pending_update={} pending_full={} pending_rects={} drawable_full={} drawable_rects={} backlog={} backlog_loops={} input_loop_events={} pointer_motion={} pointer_position={} wayland_motion_calls={} input_pending={} input_gap_ms={} input_read_active={} input_read_ms={} input_last_age_ms={} input_reads={}/{} input_raw_events={} input_events={} input_drops={} input_slow={} input_errors={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
                loop_count,
                total_loop_count,
                frames_rendered_window,
                cursor_moves_window,
                state.cursor_x,
                state.cursor_y,
                presented_cursor_x,
                presented_cursor_y,
                !drawable_update.is_empty(),
                pending_update.needs_full_redraw,
                pending_update.partial_rects().len(),
                drawable_update.needs_full_redraw,
                drawable_update.partial_rects().len(),
                input.backlog_remaining,
                input_backlog_window,
                input_events_window,
                pointer_motion_events_window,
                pointer_position_events_window,
                wayland_motion_calls_window,
                input_pending_reader,
                max_input_gap_ms_window,
                input_reader.read_active,
                input_reader.read_elapsed_ms,
                input_reader.last_delivery_age_ms,
                input_reader.completed_reads,
                input_reader.read_attempts,
                input_reader.raw_events,
                input_reader.delivered_events,
                input_reader.queue_drops,
                input_reader.slow_reads,
                input_reader.errors,
                state.console_windows.len(),
                state.wayland_windows.len(),
                state.focused_session_handle,
                state.focused_wayland_surface_id,
            ));
            next_loop_summary = now + UI_HEARTBEAT_INTERVAL;
            loop_count = 0;
            frames_rendered_window = 0;
            cursor_moves_window = 0;
            input_events_window = 0;
            pointer_motion_events_window = 0;
            pointer_position_events_window = 0;
            wayland_motion_calls_window = 0;
            input_backlog_window = 0;
            max_input_gap_ms_window = 0;
        }
        let main_present_started = Instant::now();
        log_cursor_pipeline_sample(
            "main-present-ready",
            &state,
            &drawable_update,
            0,
            presented_cursor_x,
            presented_cursor_y,
            state.cursor_x != presented_cursor_x || state.cursor_y != presented_cursor_y,
            false,
        );
        watchdog.enter(UI_PHASE_MAIN_PRESENT);
        let rendered = match phase_result(
            "main-present",
            present_drawable_update(&mut state, &drawable_update),
        )? {
            PresentUpdateResult::Rendered { deferred_update } => {
                commit_rendered_update(
                    "main-present",
                    &state,
                    &mut wayland,
                    &mut pending_update,
                    deferred_update,
                    &mut presented_cursor_x,
                    &mut presented_cursor_y,
                    &mut frames_rendered_window,
                    &mut cursor_moves_window,
                );
                true
            }
            PresentUpdateResult::StaleSurface => {
                watchdog.leave();
                pending_update.request_full();
                continue 'main_loop;
            }
            PresentUpdateResult::Idle => false,
        };
        watchdog.leave();

        if !rendered && wayland_callback_only {
            if let Some(compositor) = wayland.as_mut() {
                compositor.frame_presented();
            }
        }
        phase_timings.main_present = main_present_started.elapsed();

        let sleep_started = Instant::now();
        let now = sleep_started;
        if !rendered
            && drawable_update.is_empty()
            && pending_update.is_empty()
            && !input.backlog_remaining
        {
            let mut sleep_deadline = next_runtime_poll
                .min(next_console_poll)
                .min(next_cursor_blink)
                .min(now + RUNTIME_POLL_SLEEP);
            if state.cursor_motion_active() {
                sleep_deadline = sleep_deadline.min(next_cursor_motion_settle);
            }
            input_loop::sleep_until_input_or(&input_events, sleep_deadline);
        }
        phase_timings.sleep = sleep_started.elapsed();

        let iteration_elapsed = iteration_started.elapsed();
        let active_elapsed = phase_timings.total_excluding_sleep();
        if active_elapsed >= SLOW_LOOP_THRESHOLD {
            log_slow_loop_iteration(
                &state,
                iteration_elapsed,
                &phase_timings,
                input.backlog_remaining,
            );
        }
        profile::maybe_emit();
    }
}

fn main() {
    boot_line("uiserver: main enter");
    profile_line("uiserver profile: startup");
    install_panic_hook();
    boot_line("uiserver: panic hook installed");
    if let Err(err) = sys::start_display_policy_endpoint() {
        diag_line(format!(
            "uiserver: display policy endpoint failed errno={err}"
        ));
        std::process::exit(err);
    }
    let exit_code = match run() {
        Ok(()) => 0,
        Err(code) => code,
    };
    if exit_code != 0 {
        diag_line(format!(
            "uiserver: exiting with nonzero status errno={exit_code}"
        ));
    }
    std::process::exit(exit_code);
}
