//! Bounded UI diagnostics, separate from presentation and scheduling policy.
//!
//! This owner samples existing state only; it cannot publish frames, change
//! thread class, or substitute diagnostic pixels for the retained scene.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::app::{AppState, VisualUpdate};
use crate::{canvas, loop_timing, profile};
use crate::sys::{boot_line, boot_trace_enabled, diag_line};

const MAX_FRAME_SAMPLE_LOGS: usize = 8;
const MAX_SLOW_CONSOLE_REFRESH_LOGS: usize = 4;
const MAX_SLOW_PRESENT_LOGS: usize = 8;
const MAX_SLOW_RUNTIME_REFRESH_LOGS: usize = 8;
const MAX_WAYLAND_CALLBACK_ONLY_LOGS: usize = 8;
const MAX_CURSOR_PIPELINE_LOGS: usize = 32;
static FRAME_SAMPLE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_CONSOLE_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_PRESENT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static SLOW_RUNTIME_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAYLAND_CALLBACK_ONLY_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static POINTER_MOVED_LOGGED: AtomicUsize = AtomicUsize::new(0);
static CURSOR_PIPELINE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(super) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        diag_line(format!("uiserver panic: {info}"));
    }));
}

pub(super) fn log_frame_sample(stage: &str, state: &mut AppState) {
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

pub(super) fn log_slow_console_refresh(state: &AppState, elapsed: Duration) {
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

pub(super) fn log_slow_runtime_refresh(
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

#[allow(clippy::too_many_arguments)]
pub(super) fn log_slow_present(
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
            "uiserver: slow full present frame_seq={} elapsed_ms={} render_ms={} present_ms={} rects={} pixels={} deferred_rects={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            loop_timing::current_frame_seq(),
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
            "uiserver: slow partial present frame_seq={} elapsed_ms={} render_ms={} present_ms={} rect={}x{}@{},{} rects={} pixels={} deferred_rects={} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
            loop_timing::current_frame_seq(),
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

pub(super) fn log_boot_stage(started_at: Instant, stage: &str) {
    diag_line(format!(
        "uiserver: boot stage={} elapsed_us={}",
        stage,
        started_at.elapsed().as_micros(),
    ));
}

pub(super) fn phase_result<T>(phase: &str, result: Result<T, i32>) -> Result<T, i32> {
    if let Err(errno) = result {
        diag_line(format!(
            "uiserver: phase failed phase={phase} errno={errno}"
        ));
        return Err(errno);
    }
    result
}

pub(super) fn log_pointer_moved_once(state: &AppState) {
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

#[allow(clippy::too_many_arguments)]
pub(super) fn log_cursor_pipeline_sample(
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

pub(super) fn log_wayland_callback_only(state: &AppState, rect: canvas::Rect) {
    if !profile::enabled() {
        return;
    }
    let log_index = WAYLAND_CALLBACK_ONLY_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if log_index >= MAX_WAYLAND_CALLBACK_ONLY_LOGS {
        return;
    }
    diag_line(format!(
        "uiserver: wayland callback-only frame_seq={} dispatch rect={}x{}@{},{} console_windows={} wayland_windows={} focused_session={} focused_wayland={:?}",
        loop_timing::current_frame_seq(),
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

