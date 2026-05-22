//! Renderer module root: orchestrates the per-frame paint pipeline and
//! re-exports the public surface used by other modules. The actual
//! drawing primitives live in focused submodules:
//!
//! * [`colors`] — the Aurora dark palette.
//! * [`background`] — sky gradient, aurora glows, starfield.
//! * [`chrome`] — topbar, dock, window chrome, traffic lights, shadows.
//! * [`icons`] — app icon themes and shape glyphs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::app::{AppState, ConsoleWindow, CursorMotion, DesktopSurfaceCache};
use crate::canvas::{Rect, SurfaceCanvas};
use crate::layout::{
    clamp_window_rect, default_console_window_rect as layout_default_console_window_rect,
    launcher_button_rect as layout_launcher_button_rect, taskbar_rail_rect,
    taskbar_slot_rect as layout_taskbar_slot_rect, topbar_rail_rect,
    wayland_client_rect as layout_wayland_client_rect,
    wayland_outer_rect as layout_wayland_outer_rect,
    window_close_button_rect as layout_window_close_button_rect,
    window_maximize_button_rect as layout_window_maximize_button_rect,
    window_minimize_button_rect as layout_window_minimize_button_rect,
    window_title_bar_rect as layout_window_title_bar_rect, WINDOW_SHADOW_STEPS,
};
use crate::sys::{diag_line, ConsoleSessionHandle};
use crate::wayland::WaylandWindowSnapshot;

mod background;
mod chrome;
mod colors;
mod icons;

pub(crate) use background::{
    build_desktop_background, start_desktop_background_loader, DesktopBackground,
};
pub(crate) use chrome::rebuild_console_window_surface;

const SLOW_DESKTOP_REFRESH_THRESHOLD: Duration = Duration::from_millis(8);
const MAX_DESKTOP_REFRESH_LOGS: usize = 6;
const MAX_DESKTOP_PENDING_LOGS: usize = 3;

static DESKTOP_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DESKTOP_PENDING_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

// ----- public geometry wrappers -----
//
// Several callers across `app/`, `main.rs`, and `wayland.rs` reach into
// the renderer for these geometry helpers. They live in `crate::layout`
// these days; the wrappers here keep the existing call sites intact and
// can be removed once everything switches over.

pub(crate) fn launcher_dirty_rect(width: u32, _height: u32) -> Rect {
    shadow_bounds(topbar_rail_rect(width as usize), 3)
}

pub(crate) fn taskbar_dirty_rect(width: u32, height: u32) -> Rect {
    shadow_bounds(taskbar_rail_rect(width as usize, height as usize), 3)
}

pub(crate) fn console_window_dirty_rect(rect: Rect) -> Rect {
    shadow_bounds(rect, WINDOW_SHADOW_STEPS)
}

pub(crate) fn wayland_window_dirty_rect(window: &WaylandWindowSnapshot) -> Rect {
    shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS)
}

pub(crate) fn default_console_window_rect(width: u32, height: u32, index: usize) -> Rect {
    layout_default_console_window_rect(width, height, index)
}

pub(crate) fn clamp_console_window_rect(width: u32, height: u32, rect: Rect) -> Rect {
    clamp_window_rect(width, height, rect)
}

pub(crate) fn window_title_bar_rect(rect: Rect) -> Rect {
    layout_window_title_bar_rect(rect)
}

/// Chrome outer rect of a Wayland surface.
///
/// We use the *frame* dimensions stored on the surface — which the
/// compositor clamps to the available desktop region on every commit — so
/// even a client that ignores our `xdg_toplevel.configure` cannot push its
/// chrome over the topbar or dock.
pub(crate) fn wayland_window_outer_rect(window: &WaylandWindowSnapshot) -> Rect {
    let client_w = window.width.min(window.frame.width);
    let client_h = window.height.min(window.frame.height);
    layout_wayland_outer_rect(window.frame.x, window.frame.y, client_w, client_h)
}

pub(crate) fn wayland_window_client_rect(outer: Rect) -> Rect {
    layout_wayland_client_rect(outer)
}

pub(crate) fn wayland_window_damage_rect(window: &WaylandWindowSnapshot, damage: Rect) -> Rect {
    if damage.is_empty() {
        return Rect::empty();
    }

    let client = wayland_window_client_rect(wayland_window_outer_rect(window));
    Rect {
        x: client.x.saturating_add(damage.x),
        y: client.y.saturating_add(damage.y),
        width: damage.width,
        height: damage.height,
    }
    .intersect(client)
}

pub(crate) fn window_close_button_rect(outer: Rect) -> Rect {
    layout_window_close_button_rect(outer)
}

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    layout_window_minimize_button_rect(outer)
}

pub(crate) fn window_maximize_button_rect(outer: Rect) -> Rect {
    layout_window_maximize_button_rect(outer)
}

pub(crate) fn launcher_button_rect(width: u32, index: usize) -> Rect {
    layout_launcher_button_rect(width, index)
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    layout_taskbar_slot_rect(width, height, index)
}

// ----- top-level entry points -----

pub(crate) fn render_frame(state: &mut AppState) {
    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    render_scene(
        &mut canvas,
        width,
        height,
        state.cursor_x,
        state.cursor_y,
        state.cursor_motion,
        state.focused_session_handle,
        state.focused_wayland_surface_id,
        &state.desktop_cache,
        &mut state.console_windows,
        &state.wayland_windows,
    );
}

pub(crate) fn render_boot_frame(state: &mut AppState) {
    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);
    // Fast boot fill — just the vertical sky gradient with no glow/star
    // passes. The full Aurora desktop background loads asynchronously and
    // takes over once it's ready.
    background::paint_sky_gradient(&mut canvas, width as usize, height as usize);
}

pub(crate) fn render_debug_white_box(state: &mut AppState) {
    render_boot_frame(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    let box_width = ((width as usize) / 3).clamp(160, 400);
    let box_height = ((height as usize) / 3).clamp(120, 320);
    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: box_width,
            height: box_height,
        },
        0x00ff_ffff,
    );
}

pub(crate) fn render_rect(state: &mut AppState, rect: Rect) {
    if rect.is_empty() {
        return;
    }

    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, rect);

    render_scene(
        &mut canvas,
        width,
        height,
        state.cursor_x,
        state.cursor_y,
        state.cursor_motion,
        state.focused_session_handle,
        state.focused_wayland_surface_id,
        &state.desktop_cache,
        &mut state.console_windows,
        &state.wayland_windows,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_scene(
    canvas: &mut SurfaceCanvas<'_>,
    width: u32,
    height: u32,
    cursor_x: u32,
    cursor_y: u32,
    cursor_motion: CursorMotion,
    focused_session_handle: ConsoleSessionHandle,
    focused_wayland_surface_id: Option<u32>,
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
    wayland_windows: &[WaylandWindowSnapshot],
) {
    let clip_rect = canvas.clip_rect();
    if desktop_cache.background_valid {
        canvas.draw_surface(
            &desktop_cache.pixels,
            desktop_cache.width,
            desktop_cache.height,
            desktop_cache.width,
            0,
            0,
        );
    } else if DESKTOP_PENDING_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_PENDING_LOGS {
        diag_line("uiserver: desktop background not ready; skipping desktop blit");
    }

    // Background wayland windows.
    for window in wayland_windows {
        if window.minimized || Some(window.surface_id) == focused_wayland_surface_id {
            continue;
        }
        if !rect_intersects_clip(
            clip_rect,
            shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
        ) {
            continue;
        }
        chrome::draw_wayland_window(canvas, window, false);
    }

    // Background console windows.
    for window in console_windows.iter_mut() {
        if window.minimized || window.session_handle == focused_session_handle {
            continue;
        }
        if !rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
            continue;
        }
        chrome::draw_console_window(canvas, window, false);
    }

    // Focused window painted last so it sits on top of everything else.
    if let Some(surface_id) = focused_wayland_surface_id {
        if let Some(window) = wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
        {
            if rect_intersects_clip(
                clip_rect,
                shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
            ) {
                chrome::draw_wayland_window(canvas, window, true);
            }
        }
    }

    if focused_wayland_surface_id.is_none() && focused_session_handle != 0 {
        if let Some(window) = console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == focused_session_handle)
        {
            if rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
                chrome::draw_console_window(canvas, window, true);
            }
        }
    }

    // Dock slots — one per console window, then one per wayland window.
    for (index, window) in console_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, index);
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            chrome::draw_dock_slot(
                canvas,
                rect,
                window.title.as_str(),
                !window.minimized && window.session_handle == focused_session_handle,
                window.minimized,
            );
        }
    }
    for (index, window) in wayland_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, console_windows.len().saturating_add(index));
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            let title = if window.title.is_empty() {
                "Wayland App"
            } else {
                window.title.as_str()
            };
            chrome::draw_dock_slot(
                canvas,
                rect,
                title,
                !window.minimized && Some(window.surface_id) == focused_wayland_surface_id,
                window.minimized,
            );
        }
    }

    if rect_intersects_clip(
        clip_rect,
        crate::canvas::cursor_dirty_rect(cursor_x, cursor_y, width, height),
    ) {
        canvas.draw_cursor(cursor_x, cursor_y, cursor_motion);
    }
}

fn rect_intersects_clip(clip_rect: Rect, rect: Rect) -> bool {
    !clip_rect.intersect(rect).is_empty()
}

fn shadow_bounds(rect: Rect, steps: usize) -> Rect {
    if rect.is_empty() {
        return rect;
    }
    Rect {
        x: rect.x.saturating_sub(steps),
        y: rect.y.saturating_sub(steps),
        width: rect
            .width
            .saturating_add(steps.saturating_mul(2).saturating_add(2)),
        height: rect
            .height
            .saturating_add(steps.saturating_mul(2).saturating_add(steps).saturating_add(2)),
    }
}

/// Lazily rebuilds the cached desktop composite (background + chrome
/// strips) whenever the launchers or display geometry change. The
/// expensive aurora background is built once on a background thread and
/// the chrome strips are repainted on demand.
fn refresh_desktop_surface(state: &mut AppState) {
    let refresh_started = Instant::now();
    let width = state.surface.width as usize;
    let height = state.surface.height as usize;
    let resized = state.desktop_cache.width != width || state.desktop_cache.height != height;
    if resized {
        state.desktop_cache.width = width;
        state.desktop_cache.height = height;
        let total = width.saturating_mul(height);
        state.desktop_cache.pixels.resize(total, 0);
        state.desktop_cache.background_pixels.resize(total, 0);
        state.desktop_cache.invalidate_all();
    }
    if state.desktop_cache.fully_valid() {
        return;
    }

    if !state.desktop_cache.background_valid {
        if DESKTOP_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_REFRESH_LOGS {
            diag_line(
                format!(
                    "uiserver: desktop background pending width={} height={} resized={} chrome_valid={} pixels_len={}",
                    width,
                    height,
                    resized,
                    state.desktop_cache.chrome_valid,
                    state.desktop_cache.pixels.len(),
                )
                .as_str(),
            );
        }
        return;
    }

    if !state.desktop_cache.chrome_valid {
        let total = state.desktop_cache.background_pixels.len();
        if state.desktop_cache.pixels.len() != total {
            state.desktop_cache.pixels.resize(total, 0);
        }

        let screen = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        // Restore the chrome strips from clean background pixels so the
        // new chrome paints over a fresh substrate instead of
        // accumulating alpha across rebuilds.
        let topbar = topbar_rail_rect(width);
        let taskbar = taskbar_rail_rect(width, height);
        let chrome_strips: [Rect; 2] = [
            shadow_bounds(topbar, 3),
            shadow_bounds(taskbar, 3),
        ];
        for strip in chrome_strips {
            let strip = strip.intersect(screen);
            if strip.is_empty() {
                continue;
            }
            for row in strip.y..strip.y.saturating_add(strip.height) {
                let row_start = row.saturating_mul(width).saturating_add(strip.x);
                let row_end = row_start.saturating_add(strip.width);
                if row_end > total {
                    continue;
                }
                state.desktop_cache.pixels[row_start..row_end]
                    .copy_from_slice(&state.desktop_cache.background_pixels[row_start..row_end]);
            }
        }

        let mut canvas = SurfaceCanvas::new(
            state.desktop_cache.pixels.as_mut_slice(),
            width as u32,
            height as u32,
            width,
        );

        chrome::draw_rail_panel(&mut canvas, topbar);
        chrome::draw_rail_panel(&mut canvas, taskbar);

        chrome::draw_brand_block(&mut canvas, topbar);
        chrome::draw_status_block(&mut canvas, topbar, state.launcher_programs.len());

        for (index, program) in state.launcher_programs.iter().enumerate() {
            chrome::draw_launcher_icon(
                &mut canvas,
                launcher_button_rect(width as u32, index),
                program.title.as_str(),
            );
        }
        state.desktop_cache.chrome_valid = true;
    }

    let refresh_elapsed = refresh_started.elapsed();
    if refresh_elapsed >= SLOW_DESKTOP_REFRESH_THRESHOLD
        && DESKTOP_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_REFRESH_LOGS
    {
        diag_line(
            format!(
                "uiserver: desktop refresh elapsed_ms={} resized={} background_valid={} chrome_valid={} background_pixels={} composite_pixels={}",
                refresh_elapsed.as_millis(),
                resized,
                state.desktop_cache.background_valid,
                state.desktop_cache.chrome_valid,
                state.desktop_cache.background_pixels.len(),
                state.desktop_cache.pixels.len(),
            )
            .as_str(),
        );
    }
}
