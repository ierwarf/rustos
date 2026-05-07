mod app;
mod canvas;
mod font;
mod input_loop;
mod profile;
mod render;
mod runtime_sync;
mod simd;
mod sys;
mod terminal;
mod wayland;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use app::{
    AppState, VisualUpdate, CONSOLE_POLL_SLEEP, CURSOR_BLINK_INTERVAL, IDLE_SLEEP,
    INPUT_EVENT_BATCH, RUNTIME_POLL_SLEEP, TARGET_FRAME_INTERVAL,
};
use render::{render_boot_frame, render_debug_white_box, render_frame, render_rect};
use runtime_control::RuntimeClient;
use runtime_sync::{refresh_runtime_state, start_runtime_sync, RuntimeState};
use sys::{boot_line, boot_trace_enabled, diag_line, profile_line, InputEvent};
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

static FRAME_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_CONSOLE_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_PRESENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_RUNTIME_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

fn cursor_move_update(
    state: &AppState,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
) -> VisualUpdate {
    if presented_cursor_x == state.cursor_x && presented_cursor_y == state.cursor_y {
        return VisualUpdate::default();
    }

    VisualUpdate {
        needs_full_redraw: false,
        partial_redraw_rect: canvas::cursor_dirty_rect(
            presented_cursor_x,
            presented_cursor_y,
            state.surface.width,
            state.surface.height,
        ),
        secondary_partial_redraw_rect: canvas::cursor_dirty_rect(
            state.cursor_x,
            state.cursor_y,
            state.surface.width,
            state.surface.height,
        ),
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        diag_line(&format!("uiserver panic: {info}"));
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
    diag_line(
        format!(
            "uiserver: slow console refresh elapsed_ms={} console_windows={} focused_session={} wayland_windows={}",
            elapsed.as_millis(),
            state.console_windows.len(),
            state.focused_session_handle,
            state.wayland_windows.len(),
        )
        .as_str(),
    );
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
    diag_line(
        format!(
            "uiserver: slow runtime refresh elapsed_ms={} runtime_changed={} apply_changed={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            runtime_changed,
            apply_changed,
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
        .as_str(),
    );
}

fn log_slow_present(
    state: &AppState,
    elapsed: Duration,
    full_redraw: bool,
    rect: Option<crate::canvas::Rect>,
) {
    let sample_index = SLOW_PRESENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_PRESENT_LOGS {
        return;
    }
    let message = if full_redraw {
        format!(
            "uiserver: slow full present elapsed_ms={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
    } else {
        let rect = rect.unwrap_or_default();
        format!(
            "uiserver: slow partial present elapsed_ms={} rect={}x{}@{},{} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
    };
    diag_line(message.as_str());
}

fn run() -> Result<(), i32> {
    boot_line("uiserver: run initialize begin");
    let mut state = AppState::initialize()?;
    boot_line("uiserver: run initialize done");
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
    boot_line("uiserver: boot frame begin");
    render_boot_frame(&mut state);
    if DEBUG_DIRECT_SURFACE_PROBE {
        stamp_raw_surface_probe("boot", &mut state);
    }
    log_frame_sample("boot", &mut state);
    boot_line("uiserver: boot frame done");
    boot_line("uiserver: first present begin");
    state.present()?;
    boot_line("uiserver: first present done");
    boot_line("uiserver: post-present init begin");
    let mut pending_update = VisualUpdate::default();
    let mut presented_cursor_x = state.cursor_x;
    let mut presented_cursor_y = state.cursor_y;
    boot_line("uiserver: runtime client open begin");
    let mut runtime_state = RuntimeState::default();
    let runtime = RuntimeClient::open_default().map_err(|_| {
        diag_line("uiserver: open runtimed socket failed");
        19
    })?;
    boot_line("uiserver: runtime client open done");
    match runtime.notify_ui_ready() {
        Ok(()) => boot_line("uiserver: ui ready notified"),
        Err(err) => boot_line(format!("uiserver: ui ready notify failed errno={err}").as_str()),
    }
    let runtime_sync = RuntimeClient::open_default()
        .map(start_runtime_sync)
        .map_err(|_| {
            diag_line("uiserver: open runtimed sync socket failed");
            19
        })?;
    boot_line("uiserver: wayland initialize begin");
    let mut wayland = WaylandCompositor::initialize(state.display.width, state.display.height);
    boot_line("uiserver: wayland initialize done");
    boot_line("uiserver: post-present init done");
    let mut launcher_programs_loaded = false;
    let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
    let mut next_runtime_poll = Instant::now();
    let mut next_console_poll = Instant::now() + CONSOLE_POLL_SLEEP;
    let mut next_present_at = Instant::now() + TARGET_FRAME_INTERVAL;
    let mut next_cursor_blink = Instant::now() + CURSOR_BLINK_INTERVAL;
    let mut next_loop_summary = Instant::now() + Duration::from_secs(1);
    let mut loop_count = 0_u64;

    loop {
        loop_count = loop_count.saturating_add(1);
        if !launcher_programs_loaded {
            if state.populate_launcher_programs() {
                pending_update.absorb(VisualUpdate::partial(render::launcher_dirty_rect(
                    state.surface.width,
                    state.surface.height,
                )));
            }
            launcher_programs_loaded = true;
        }

        let input =
            input_loop::process_pending_input(&mut state, wayland.as_mut(), &runtime, &mut events)?;
        pending_update.absorb(VisualUpdate {
            needs_full_redraw: input.needs_full_redraw,
            partial_redraw_rect: input.partial_redraw_rect,
            secondary_partial_redraw_rect: input.secondary_partial_redraw_rect,
        });

        let now = Instant::now();
        if !input.backlog_remaining {
            if now >= next_runtime_poll {
                let refresh_started = Instant::now();
                let runtime_changed = refresh_runtime_state(&runtime_sync, &mut runtime_state)?;
                let apply_dirty = state.apply_runtime_state(&mut runtime_state);
                let apply_changed = !apply_dirty.is_empty();
                let refresh_elapsed = refresh_started.elapsed();
                if refresh_elapsed >= SLOW_RUNTIME_REFRESH_THRESHOLD {
                    log_slow_runtime_refresh(
                        &state,
                        refresh_elapsed,
                        runtime_changed,
                        apply_changed,
                    );
                }
                next_runtime_poll = now + RUNTIME_POLL_SLEEP;
                pending_update.absorb(VisualUpdate::partial(apply_dirty));
            }
            if let Some(compositor) = wayland.as_mut() {
                if compositor.tick() {
                    let wayland_dirty = state.sync_wayland_windows(compositor.window_snapshots());
                    let focus_dirty = state.recover_focus_after_wayland_change(Some(compositor))?;
                    pending_update.absorb(VisualUpdate::partial(wayland_dirty.union(focus_dirty)));
                }
            }
            if now >= next_console_poll {
                let refresh_started = Instant::now();
                let dirty_rect = state.refresh_console_windows()?;
                let changed = !dirty_rect.is_empty();
                pending_update.absorb(VisualUpdate::partial(dirty_rect));
                let refresh_elapsed = refresh_started.elapsed();
                profile::record_console_refresh(refresh_elapsed, changed);
                if refresh_elapsed >= SLOW_CONSOLE_REFRESH_THRESHOLD {
                    log_slow_console_refresh(&state, refresh_elapsed);
                }
                next_console_poll = now + CONSOLE_POLL_SLEEP;
            }
        }

        let now = Instant::now();
        if now >= next_cursor_blink {
            if let Some(rect) = state.toggle_focused_terminal_cursor() {
                pending_update.absorb(VisualUpdate::partial(rect));
            }
            next_cursor_blink = now + CURSOR_BLINK_INTERVAL;
        }
        let mut drawable_update = pending_update;
        drawable_update.absorb(cursor_move_update(
            &state,
            presented_cursor_x,
            presented_cursor_y,
        ));
        drawable_update.coalesce_tight_partials();
        drawable_update.promote_large_partial(state.surface.width, state.surface.height);

        let now = Instant::now();
        if now >= next_loop_summary {
            diag_line(
                format!(
                    "uiserver: loop summary loops={} pending_update={} backlog={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
                    loop_count,
                    !drawable_update.is_empty(),
                    input.backlog_remaining,
                    state.console_windows.len(),
                    state.wayland_windows.len(),
                    state.focused_session_handle,
                    state.focused_wayland_surface_id,
                )
                .as_str(),
            );
            next_loop_summary = now + Duration::from_secs(1);
            loop_count = 0;
        }
        if drawable_update.is_empty() {
            if input.backlog_remaining {
                continue;
            }
            let sleep_deadline = next_runtime_poll
                .min(next_console_poll)
                .min(now + IDLE_SLEEP);
            input_loop::sleep_until(sleep_deadline);
            continue;
        }

        if now < next_present_at {
            if input.backlog_remaining {
                profile::record_throttle_spin();
                continue;
            }
            let sleep_deadline = next_present_at
                .min(next_runtime_poll)
                .min(next_console_poll);
            input_loop::sleep_until(sleep_deadline);
            continue;
        }

        if drawable_update.needs_full_redraw {
            let render_started = Instant::now();
            render_frame(&mut state);
            let render_elapsed = render_started.elapsed();
            log_frame_sample("full", &mut state);
            let present_started = Instant::now();
            match state.present() {
                Ok(()) => {}
                Err(err) if state.recover_if_stale_surface_error(err)? => {
                    pending_update.request_full();
                    next_present_at = Instant::now();
                    continue;
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
                log_slow_present(&state, total_elapsed, true, None);
            }
        } else if !drawable_update.partial_redraw_rect.is_empty() {
            let render_started = Instant::now();
            let primary_rect = drawable_update.partial_redraw_rect;
            render_rect(&mut state, drawable_update.partial_redraw_rect);
            let mut rect_count = 1_u64;
            let mut pixel_count = primary_rect.width.saturating_mul(primary_rect.height) as u64;
            let first_present_started = Instant::now();
            match state.present_rect(drawable_update.partial_redraw_rect) {
                Ok(()) => {}
                Err(err) if state.recover_if_stale_surface_error(err)? => {
                    pending_update.request_full();
                    next_present_at = Instant::now();
                    continue;
                }
                Err(err) => return Err(err),
            }
            let mut present_elapsed = first_present_started.elapsed();
            if !drawable_update.secondary_partial_redraw_rect.is_empty() {
                rect_count = rect_count.saturating_add(1);
                pixel_count = pixel_count.saturating_add(
                    drawable_update
                        .secondary_partial_redraw_rect
                        .width
                        .saturating_mul(drawable_update.secondary_partial_redraw_rect.height)
                        as u64,
                );
                render_rect(&mut state, drawable_update.secondary_partial_redraw_rect);
                let second_present_started = Instant::now();
                match state.present_rect(drawable_update.secondary_partial_redraw_rect) {
                    Ok(()) => {}
                    Err(err) if state.recover_if_stale_surface_error(err)? => {
                        pending_update.request_full();
                        next_present_at = Instant::now();
                        continue;
                    }
                    Err(err) => return Err(err),
                }
                present_elapsed += second_present_started.elapsed();
            }
            let render_elapsed = render_started.elapsed().saturating_sub(present_elapsed);
            profile::record_present(
                false,
                rect_count,
                pixel_count,
                render_elapsed,
                present_elapsed,
            );
            let total_elapsed = render_elapsed + present_elapsed;
            if total_elapsed >= SLOW_PRESENT_THRESHOLD {
                log_slow_present(&state, total_elapsed, false, Some(primary_rect));
            }
        }

        if let Some(compositor) = wayland.as_mut() {
            compositor.frame_presented();
        }
        pending_update.clear();
        presented_cursor_x = state.cursor_x;
        presented_cursor_y = state.cursor_y;
        next_present_at = Instant::now() + TARGET_FRAME_INTERVAL;
        profile::maybe_emit();
    }
}

fn main() {
    boot_line("uiserver: main enter");
    profile_line("uiserver profile: startup");
    install_panic_hook();
    boot_line("uiserver: panic hook installed");
    let exit_code = match run() {
        Ok(()) => 0,
        Err(code) => code,
    };
    if exit_code != 0 {
        diag_line("uiserver: exiting with nonzero status");
    }
    std::process::exit(exit_code);
}
