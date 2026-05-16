mod app;
mod canvas;
mod cursor_sprites;
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
    start_console_refresh_worker, start_launcher_program_loader, AppState, VisualUpdate,
    CONSOLE_POLL_SLEEP, CURSOR_BLINK_INTERVAL, CURSOR_MOTION_SETTLE_INTERVAL, IDLE_SLEEP,
    INPUT_EVENT_BATCH, RUNTIME_POLL_SLEEP,
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
const WAYLAND_BACKLOG_SERVICE_INTERVAL: Duration = Duration::from_millis(4);

static FRAME_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_CONSOLE_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_PRESENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_RUNTIME_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static POINTER_MOVED_LOGGED: AtomicUsize = AtomicUsize::new(0);

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
    render_elapsed: Duration,
    present_elapsed: Duration,
    full_redraw: bool,
    rect: Option<crate::canvas::Rect>,
) {
    let sample_index = SLOW_PRESENT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_PRESENT_LOGS {
        return;
    }
    let message = if full_redraw {
        format!(
            "uiserver: slow full present elapsed_ms={} render_ms={} present_ms={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            render_elapsed.as_millis(),
            present_elapsed.as_millis(),
            state.console_windows.len(),
            state.wayland_windows.len(),
            state.focused_session_handle,
            state.focused_wayland_surface_id,
        )
    } else {
        let rect = rect.unwrap_or_default();
        format!(
            "uiserver: slow partial present elapsed_ms={} render_ms={} present_ms={} rect={}x{}@{},{} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            elapsed.as_millis(),
            render_elapsed.as_millis(),
            present_elapsed.as_millis(),
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

fn log_pointer_moved_once(state: &AppState) {
    if POINTER_MOVED_LOGGED
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        diag_line(
            format!(
                "uiserver: pointer moved x={} y={}",
                state.cursor_x, state.cursor_y
            )
            .as_str(),
        );
    }
}

fn run() -> Result<(), i32> {
    diag_line("uiserver: run initialize begin");
    let mut state = AppState::initialize()?;
    diag_line("uiserver: run initialize done");
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
    diag_line("uiserver: first present begin");
    state.present()?;
    diag_line("uiserver: first present done");
    diag_line("uiserver: post-present init begin");
    let mut pending_update = VisualUpdate::default();
    let mut presented_cursor_x = state.cursor_x;
    let mut presented_cursor_y = state.cursor_y;
    diag_line("uiserver: runtime client open begin");
    let mut runtime_state = RuntimeState::default();
    let runtime = RuntimeClient::open_default().map_err(|_| {
        diag_line("uiserver: open runtimed socket failed");
        19
    })?;
    diag_line("uiserver: runtime client open done");
    match runtime.notify_ui_ready() {
        Ok(()) => diag_line("uiserver: ui ready notified"),
        Err(err) => diag_line(format!("uiserver: ui ready notify failed errno={err}").as_str()),
    }
    let runtime_sync = RuntimeClient::open_default()
        .map(start_runtime_sync)
        .map_err(|_| {
            diag_line("uiserver: open runtimed sync socket failed");
            19
        })?;
    diag_line("uiserver: wayland initialize begin");
    let mut wayland = WaylandCompositor::initialize(state.display.width, state.display.height);
    diag_line("uiserver: wayland initialize done");
    diag_line("uiserver: post-present init done");
    let launcher_programs = start_launcher_program_loader();
    let console_refreshes = start_console_refresh_worker();
    let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
    let mut next_runtime_poll = Instant::now();
    let mut next_console_poll = Instant::now() + CONSOLE_POLL_SLEEP;
    let mut next_cursor_blink = Instant::now() + CURSOR_BLINK_INTERVAL;
    let mut next_cursor_motion_settle = Instant::now() + CURSOR_MOTION_SETTLE_INTERVAL;
    let mut next_loop_summary = Instant::now() + Duration::from_secs(1);
    let mut next_wayland_backlog_service = Instant::now();
    let mut loop_count = 0_u64;

    loop {
        loop_count = loop_count.saturating_add(1);

        while let Ok(programs) = launcher_programs.try_recv() {
            if state.apply_launcher_programs(programs) {
                pending_update.absorb(VisualUpdate::partial(render::launcher_dirty_rect(
                    state.surface.width,
                    state.surface.height,
                )));
            }
        }

        let input =
            input_loop::process_pending_input(&mut state, wayland.as_mut(), &runtime, &mut events)?;
        pending_update.absorb(input.visual_update);

        let now = Instant::now();
        let service_wayland = !input.backlog_remaining || now >= next_wayland_backlog_service;
        if let Some(compositor) = wayland.as_mut() {
            if service_wayland {
                if compositor.tick() {
                    let wayland_dirty = state.sync_wayland_windows(compositor.window_snapshots());
                    let focus_dirty = state.recover_focus_after_wayland_change(Some(compositor))?;
                    let mut update = VisualUpdate::partial(wayland_dirty);
                    update.add_partial_rect(focus_dirty);
                    pending_update.absorb(update);
                }
                pending_update.absorb(VisualUpdate::partial(
                    compositor.pending_frame_callback_rect(),
                ));
                next_wayland_backlog_service = now + WAYLAND_BACKLOG_SERVICE_INTERVAL;
            } else if input.backlog_remaining {
                compositor.flush_clients();
            }
        }

        let now = Instant::now();
        if now >= next_runtime_poll {
            let refresh_started = Instant::now();
            let runtime_changed = refresh_runtime_state(&runtime_sync, &mut runtime_state)?;
            let apply_dirty = state.apply_runtime_state(&mut runtime_state);
            let apply_changed = !apply_dirty.is_empty();
            let refresh_elapsed = refresh_started.elapsed();
            if refresh_elapsed >= SLOW_RUNTIME_REFRESH_THRESHOLD {
                log_slow_runtime_refresh(&state, refresh_elapsed, runtime_changed, apply_changed);
            }
            next_runtime_poll = now + RUNTIME_POLL_SLEEP;
            pending_update.absorb(VisualUpdate::partial(apply_dirty));
        }

        let now = Instant::now();
        if now >= next_console_poll {
            let refresh_started = Instant::now();
            let mut dirty_rect = canvas::Rect::empty();
            while let Ok(refresh) = console_refreshes.try_recv() {
                dirty_rect = dirty_rect.union(state.apply_console_refresh(refresh)?);
            }
            let changed = !dirty_rect.is_empty();
            pending_update.absorb(VisualUpdate::partial(dirty_rect));
            let refresh_elapsed = refresh_started.elapsed();
            profile::record_console_refresh(refresh_elapsed, changed);
            if refresh_elapsed >= SLOW_CONSOLE_REFRESH_THRESHOLD {
                log_slow_console_refresh(&state, refresh_elapsed);
            }
            next_console_poll = now + CONSOLE_POLL_SLEEP;
        }

        let now = Instant::now();
        if now >= next_cursor_blink {
            if let Some(rect) = state.toggle_focused_terminal_cursor() {
                pending_update.absorb(VisualUpdate::partial(rect));
            }
            next_cursor_blink = now + CURSOR_BLINK_INTERVAL;
        }
        if now >= next_cursor_motion_settle {
            let cursor_dirty_rect =
                state.settle_cursor_motion(state.surface.width, state.surface.height);
            pending_update.absorb(VisualUpdate::partial(cursor_dirty_rect));
            next_cursor_motion_settle = now + CURSOR_MOTION_SETTLE_INTERVAL;
        }
        let mut drawable_update = pending_update.clone();
        if !input.backlog_remaining || !pending_update.is_empty() {
            drawable_update.absorb(cursor_move_update(
                &state,
                presented_cursor_x,
                presented_cursor_y,
            ));
        }
        drawable_update.coalesce_tight_partials();
        drawable_update.promote_large_partial(state.surface.width, state.surface.height);

        let now = Instant::now();
        if now >= next_loop_summary {
            if profile::enabled() {
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
            }
            next_loop_summary = now + Duration::from_secs(1);
            loop_count = 0;
        }
        let rendered = if drawable_update.needs_full_redraw {
            let render_started = Instant::now();
            render_frame(&mut state);
            let render_elapsed = render_started.elapsed();
            log_frame_sample("full", &mut state);
            let present_started = Instant::now();
            match state.present() {
                Ok(()) => {}
                Err(err) if state.recover_if_stale_surface_error(err)? => {
                    pending_update.request_full();
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
                log_slow_present(
                    &state,
                    total_elapsed,
                    render_elapsed,
                    present_elapsed,
                    true,
                    None,
                );
            }
            true
        } else if !drawable_update.partial_rects().is_empty() {
            let render_rects = drawable_update.partial_rects().to_vec();
            let mut render_elapsed = Duration::ZERO;
            let mut present_elapsed = Duration::ZERO;
            let mut present_union = canvas::Rect::empty();
            let mut pixel_count = 0_u64;
            let mut recovered_stale_surface = false;

            for rect in &render_rects {
                present_union = present_union.union(*rect);
                pixel_count =
                    pixel_count.saturating_add(rect.width.saturating_mul(rect.height) as u64);

                let render_started = Instant::now();
                render_rect(&mut state, *rect);
                render_elapsed += render_started.elapsed();

                let present_started = Instant::now();
                match state.present_rect(*rect) {
                    Ok(()) => {}
                    Err(err) if state.recover_if_stale_surface_error(err)? => {
                        pending_update.request_full();
                        recovered_stale_surface = true;
                        break;
                    }
                    Err(err) => return Err(err),
                }
                present_elapsed += present_started.elapsed();
            }
            if recovered_stale_surface {
                continue;
            }
            let rect_count = render_rects.len() as u64;
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
                    &state,
                    total_elapsed,
                    render_elapsed,
                    present_elapsed,
                    false,
                    Some(present_union),
                );
            }
            true
        } else {
            false
        };

        if rendered {
            if let Some(compositor) = wayland.as_mut() {
                compositor.frame_presented();
            }
            let cursor_moved =
                presented_cursor_x != state.cursor_x || presented_cursor_y != state.cursor_y;
            pending_update.clear();
            presented_cursor_x = state.cursor_x;
            presented_cursor_y = state.cursor_y;
            if cursor_moved {
                log_pointer_moved_once(&state);
            }
        }

        let now = Instant::now();
        if !rendered && pending_update.is_empty() && !input.backlog_remaining {
            let sleep_deadline = next_runtime_poll
                .min(next_console_poll)
                .min(next_cursor_blink)
                .min(next_cursor_motion_settle)
                .min(now + IDLE_SLEEP);
            input_loop::sleep_until(sleep_deadline);
        }
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
        diag_line(format!("uiserver: exiting with nonzero status errno={exit_code}").as_str());
    }
    std::process::exit(exit_code);
}
