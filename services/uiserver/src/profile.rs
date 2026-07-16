use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::sys::{background_thread_demotion_count, profile_line, ui_profile_enabled};

#[derive(Default)]
struct ProfileWindow {
    started_at: Option<Instant>,
    input_loops: u64,
    input_loop_micros: u64,
    input_events: u64,
    last_input_at: Option<Instant>,
    input_gap_millis: u64,
    input_last_age_millis: u64,
    input_drops: u64,
    input_slow: u64,
    input_errors: u64,
    cursor_mismatches: u64,
    cursor_x: u32,
    cursor_y: u32,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
    pointer_motion_events: u64,
    pointer_position_events: u64,
    other_events: u64,
    backlog_loops: u64,
    cursor_moves: u64,
    wayland_motion_calls: u64,
    wayland_motion_micros: u64,
    runtime_refresh_calls: u64,
    runtime_refresh_micros: u64,
    console_refresh_calls: u64,
    console_refresh_changed: u64,
    console_refresh_micros: u64,
    full_presents: u64,
    partial_presents: u64,
    present_rects: u64,
    render_micros: u64,
    present_micros: u64,
    full_render_micros: u64,
    full_present_micros: u64,
    partial_render_micros: u64,
    partial_present_micros: u64,
    present_pixels: u64,
    throttle_spins: u64,
}

fn window() -> &'static Mutex<ProfileWindow> {
    static WINDOW: OnceLock<Mutex<ProfileWindow>> = OnceLock::new();
    WINDOW.get_or_init(|| Mutex::new(ProfileWindow::default()))
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

pub(crate) fn enabled() -> bool {
    ui_profile_enabled()
}

pub(crate) fn record_input_loop(
    elapsed: Duration,
    input_events: u64,
    pointer_motion_events: u64,
    pointer_position_events: u64,
    other_events: u64,
    backlog_remaining: bool,
    cursor_moved: bool,
) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    window.input_loops = window.input_loops.saturating_add(1);
    window.input_loop_micros = window
        .input_loop_micros
        .saturating_add(duration_micros(elapsed));
    window.input_events = window.input_events.saturating_add(input_events);
    window.pointer_motion_events = window
        .pointer_motion_events
        .saturating_add(pointer_motion_events);
    window.pointer_position_events = window
        .pointer_position_events
        .saturating_add(pointer_position_events);
    window.other_events = window.other_events.saturating_add(other_events);
    if backlog_remaining {
        window.backlog_loops = window.backlog_loops.saturating_add(1);
    }
    if cursor_moved {
        window.cursor_moves = window.cursor_moves.saturating_add(1);
    }
}

pub(crate) fn record_wayland_motion(elapsed: Duration) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    window.wayland_motion_calls = window.wayland_motion_calls.saturating_add(1);
    window.wayland_motion_micros = window
        .wayland_motion_micros
        .saturating_add(duration_micros(elapsed));
}

/// Captures the input-side safety and latency state in the same bounded
/// profile window as FPS.  The KVM gate consumes this record rather than a
/// best-effort boot diagnostic channel, so it cannot mistake missing
/// observability for healthy input.
// Keep each acceptance counter and cursor coordinate explicit so callers
// cannot accidentally report a partially initialized aggregate snapshot.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_input_health(
    input_events: u64,
    input_last_age_millis: u64,
    input_drops: u64,
    input_slow: u64,
    input_errors: u64,
    cursor_x: u32,
    cursor_y: u32,
    presented_cursor_x: u32,
    presented_cursor_y: u32,
    cursor_synchronized: bool,
) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    if input_events > 0 {
        let now = Instant::now();
        if let Some(previous) = window.last_input_at.replace(now) {
            window.input_gap_millis = window
                .input_gap_millis
                .max(duration_micros(now.duration_since(previous)) / 1_000);
        }
    }
    window.input_last_age_millis = window.input_last_age_millis.max(input_last_age_millis);
    window.input_drops = window.input_drops.max(input_drops);
    window.input_slow = window.input_slow.max(input_slow);
    window.input_errors = window.input_errors.max(input_errors);
    window.cursor_x = cursor_x;
    window.cursor_y = cursor_y;
    window.presented_cursor_x = presented_cursor_x;
    window.presented_cursor_y = presented_cursor_y;
    if !cursor_synchronized {
        window.cursor_mismatches = window.cursor_mismatches.saturating_add(1);
    }
}

pub(crate) fn record_runtime_refresh(elapsed: Duration) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    window.runtime_refresh_calls = window.runtime_refresh_calls.saturating_add(1);
    window.runtime_refresh_micros = window
        .runtime_refresh_micros
        .saturating_add(duration_micros(elapsed));
}

pub(crate) fn record_console_refresh(elapsed: Duration, changed: bool) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    window.console_refresh_calls = window.console_refresh_calls.saturating_add(1);
    if changed {
        window.console_refresh_changed = window.console_refresh_changed.saturating_add(1);
    }
    window.console_refresh_micros = window
        .console_refresh_micros
        .saturating_add(duration_micros(elapsed));
}

pub(crate) fn record_present(
    full: bool,
    rect_count: u64,
    pixel_count: u64,
    render_elapsed: Duration,
    present_elapsed: Duration,
) {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    if full {
        window.full_presents = window.full_presents.saturating_add(1);
        window.full_render_micros = window
            .full_render_micros
            .saturating_add(duration_micros(render_elapsed));
        window.full_present_micros = window
            .full_present_micros
            .saturating_add(duration_micros(present_elapsed));
    } else {
        window.partial_presents = window.partial_presents.saturating_add(1);
        window.partial_render_micros = window
            .partial_render_micros
            .saturating_add(duration_micros(render_elapsed));
        window.partial_present_micros = window
            .partial_present_micros
            .saturating_add(duration_micros(present_elapsed));
    }
    window.present_rects = window.present_rects.saturating_add(rect_count);
    window.present_pixels = window.present_pixels.saturating_add(pixel_count);
    window.render_micros = window
        .render_micros
        .saturating_add(duration_micros(render_elapsed));
    window.present_micros = window
        .present_micros
        .saturating_add(duration_micros(present_elapsed));
}

pub(crate) fn record_backpressure_retry() {
    if !enabled() {
        return;
    }
    let mut window = window().lock().unwrap();
    window.throttle_spins = window.throttle_spins.saturating_add(1);
}

pub(crate) fn maybe_emit() {
    if !enabled() {
        return;
    }

    let line = {
        let mut window = window().lock().unwrap();
        let now = Instant::now();
        let started_at = window.started_at.get_or_insert(now);
        let elapsed = now.duration_since(*started_at);
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let elapsed_micros = duration_micros(elapsed).max(1);
        let presents = window.full_presents.saturating_add(window.partial_presents);
        // Frames per second scaled by 1,000 keeps the KVM gate integer-only
        // while retaining enough precision for the release criterion.
        let frame_hz_milli = presents
            .saturating_mul(1_000_000_000)
            .saturating_div(elapsed_micros);

        let line = format!(
            "uiserver profile: elapsed_ms={} frame_hz_milli={} loops={} input_events={} input_ms={} input_gap_ms={} input_last_age_ms={} input_drops={} input_slow={} input_errors={} cursor_mismatches={} cursor={},{} presented_cursor={},{} background_thread_demotions={} motion={} position={} other={} backlog={} cursor_moves={} wayland_calls={} wayland_ms={} runtime_calls={} runtime_ms={} con_calls={} con_chg={} con_ms={} full={} part={} rects={} full_ren_ms={} full_prs_ms={} part_ren_ms={} part_prs_ms={} mpix={} spins={}",
            elapsed_micros / 1_000,
            frame_hz_milli,
            window.input_loops,
            window.input_events,
            window.input_loop_micros / 1000,
            window.input_gap_millis,
            window.input_last_age_millis,
            window.input_drops,
            window.input_slow,
            window.input_errors,
            window.cursor_mismatches,
            window.cursor_x,
            window.cursor_y,
            window.presented_cursor_x,
            window.presented_cursor_y,
            background_thread_demotion_count(),
            window.pointer_motion_events,
            window.pointer_position_events,
            window.other_events,
            window.backlog_loops,
            window.cursor_moves,
            window.wayland_motion_calls,
            window.wayland_motion_micros / 1000,
            window.runtime_refresh_calls,
            window.runtime_refresh_micros / 1000,
            window.console_refresh_calls,
            window.console_refresh_changed,
            window.console_refresh_micros / 1000,
            window.full_presents,
            window.partial_presents,
            window.present_rects,
            window.full_render_micros / 1000,
            window.full_present_micros / 1000,
            window.partial_render_micros / 1000,
            window.partial_present_micros / 1000,
            window.present_pixels / 1_000_000,
            window.throttle_spins,
        );
        *window = ProfileWindow {
            started_at: Some(now),
            ..ProfileWindow::default()
        };
        line
    };
    // The nonblocking profile channel is deliberately used after releasing
    // the accounting lock: observability can never hold up render statistics
    // or a future profiling consumer.
    profile_line(&line);
}
