use crate::app::{AppState, ConsoleWindow, DesktopSurfaceCache};
use crate::canvas::{Rect, SurfaceCanvas};
use crate::font::{self, TextStyle};
use crate::sys::ConsoleSessionHandle;
use crate::wayland::WaylandWindowSnapshot;

const COLOR_BOOT_BG_BASE: u32 = 0x000f_141b;
const COLOR_DESKTOP_BG_BASE: u32 = 0x0011_171f;
const COLOR_DESKTOP_BAND: u32 = 0x0017_202b;
const COLOR_GRID: u32 = 0x0022_2c35;
const COLOR_GRID_MAJOR: u32 = 0x0032_424d;
const COLOR_PANEL_GLASS: u32 = 0x0015_2027;
const COLOR_PANEL_INNER: u32 = 0x001b_2931;
const COLOR_PANEL_ELEVATED: u32 = 0x0021_323a;
const COLOR_ACCENT_FOCUS: u32 = 0x0047_d6b2;
const COLOR_ACCENT_SOFT: u32 = 0x0098_e6d3;
const COLOR_ACCENT_WARM: u32 = 0x00f2_b84b;
const COLOR_BORDER_IDLE: u32 = 0x0040_555d;
const COLOR_BORDER_FOCUS: u32 = 0x0047_d6b2;
const COLOR_TEXT_PRIMARY: u32 = 0x00f5_f7f4;
const COLOR_TEXT_DIM: u32 = 0x00a9_b9b7;
const COLOR_SHADOW: u32 = 0x0000_0000;
const COLOR_CLIENT_BACKDROP: u32 = 0x0012_181f;
const COLOR_CLIENT_DIVIDER: u32 = 0x0033_444b;

const TOPBAR_MARGIN_TOP: usize = 16;
const TOPBAR_RAIL_HEIGHT: usize = 40;
pub(crate) const TOPBAR_HEIGHT: usize = TOPBAR_MARGIN_TOP + TOPBAR_RAIL_HEIGHT;

const TASKBAR_MARGIN_BOTTOM: usize = 20;
const TASKBAR_RAIL_HEIGHT: usize = 44;
pub(crate) const TASKBAR_HEIGHT: usize = TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT;

const RAIL_SIDE_MARGIN: usize = 20;
const DESKTOP_MARGIN_X: usize = 32;
const DESKTOP_MARGIN_Y: usize = 24;
const TOPBAR_BRAND_WIDTH: usize = 144;
const TOPBAR_STATUS_WIDTH: usize = 212;

const WINDOW_BORDER: usize = 1;
const WINDOW_TITLE_HEIGHT: usize = 36;
const WINDOW_PADDING_X: usize = 16;
const WINDOW_PADDING_Y: usize = 14;
const WINDOW_FOCUS_STRIP_WIDTH: usize = 3;
const WINDOW_SHADOW_STEPS: usize = 6;
const WINDOW_BUTTON_SIZE: usize = 18;
const WINDOW_BUTTON_GAP: usize = 8;
const WINDOW_BUTTON_MARGIN_RIGHT: usize = 12;
const COLOR_BUTTON_IDLE: u32 = 0x0021_3142;
const COLOR_BUTTON_MINIMIZE: u32 = 0x00f0_c44a;
const COLOR_BUTTON_CLOSE: u32 = 0x00ef_6b73;

const DEFAULT_WINDOW_WIDTH: usize = 640;
const DEFAULT_WINDOW_HEIGHT: usize = 400;
const MIN_WINDOW_WIDTH: usize = 360;
const MIN_WINDOW_HEIGHT: usize = 240;
const WINDOW_CASCADE_X: usize = 28;
const WINDOW_CASCADE_Y: usize = 24;
const WINDOW_CASCADE_SLOTS: usize = 6;

const LAUNCHER_BUTTON_WIDTH: usize = 148;
const LAUNCHER_BUTTON_HEIGHT: usize = 26;
const LAUNCHER_BUTTON_GAP: usize = 8;

const TASKBAR_SLOT_WIDTH: usize = 172;
const TASKBAR_SLOT_HEIGHT: usize = 30;
const TASKBAR_SLOT_GAP: usize = 8;

pub(crate) fn launcher_dirty_rect(width: u32, _height: u32) -> Rect {
    shadow_bounds(topbar_rail_rect(width as usize), 2)
}

pub(crate) fn taskbar_dirty_rect(width: u32, height: u32) -> Rect {
    shadow_bounds(taskbar_rail_rect(width as usize, height as usize), 2)
}

pub(crate) fn console_window_dirty_rect(rect: Rect) -> Rect {
    shadow_bounds(rect, WINDOW_SHADOW_STEPS)
}

pub(crate) fn wayland_window_dirty_rect(window: &WaylandWindowSnapshot) -> Rect {
    shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS)
}

pub(crate) fn desktop_bounds(width: u32, height: u32) -> Rect {
    Rect {
        x: DESKTOP_MARGIN_X,
        y: TOPBAR_HEIGHT + DESKTOP_MARGIN_Y,
        width: (width as usize).saturating_sub(DESKTOP_MARGIN_X * 2),
        height: (height as usize)
            .saturating_sub(TOPBAR_HEIGHT + TASKBAR_HEIGHT + DESKTOP_MARGIN_Y * 2),
    }
}

pub(crate) fn default_console_window_rect(width: u32, height: u32, index: usize) -> Rect {
    let bounds = desktop_bounds(width, height);
    let frame_width = DEFAULT_WINDOW_WIDTH
        .min(bounds.width)
        .max(bounds.width.min(MIN_WINDOW_WIDTH));
    let frame_height = DEFAULT_WINDOW_HEIGHT
        .min(bounds.height)
        .max(bounds.height.min(MIN_WINDOW_HEIGHT));
    let step = index % WINDOW_CASCADE_SLOTS;

    clamp_console_window_rect(
        width,
        height,
        Rect {
            x: bounds.x + step * WINDOW_CASCADE_X,
            y: bounds.y + step * WINDOW_CASCADE_Y,
            width: frame_width,
            height: frame_height,
        },
    )
}

pub(crate) fn clamp_console_window_rect(width: u32, height: u32, rect: Rect) -> Rect {
    let bounds = desktop_bounds(width, height);
    let width = rect.width.min(bounds.width);
    let height = rect.height.min(bounds.height);
    let max_x = bounds.x + bounds.width.saturating_sub(width);
    let max_y = bounds.y + bounds.height.saturating_sub(height);

    Rect {
        x: rect.x.clamp(bounds.x, max_x),
        y: rect.y.clamp(bounds.y, max_y),
        width,
        height,
    }
}

pub(crate) fn window_title_bar_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x + WINDOW_BORDER,
        y: rect.y + WINDOW_BORDER,
        width: rect.width.saturating_sub(WINDOW_BORDER * 2),
        height: WINDOW_TITLE_HEIGHT,
    }
}

pub(crate) fn console_window_title_bar_rect(rect: Rect) -> Rect {
    window_title_bar_rect(rect)
}

pub(crate) fn wayland_window_outer_rect(window: &WaylandWindowSnapshot) -> Rect {
    Rect {
        x: window.frame.x,
        y: window.frame.y,
        width: window.width + WINDOW_BORDER * 2,
        height: window.height + WINDOW_TITLE_HEIGHT + WINDOW_BORDER * 2,
    }
}

pub(crate) fn window_close_button_rect(outer: Rect) -> Rect {
    let title_rect = window_title_bar_rect(outer);
    let x = title_rect
        .x
        .saturating_add(title_rect.width)
        .saturating_sub(WINDOW_BUTTON_MARGIN_RIGHT + WINDOW_BUTTON_SIZE);
    Rect {
        x,
        y: title_rect.y + (title_rect.height.saturating_sub(WINDOW_BUTTON_SIZE)) / 2,
        width: WINDOW_BUTTON_SIZE,
        height: WINDOW_BUTTON_SIZE,
    }
}

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    let close_rect = window_close_button_rect(outer);
    Rect {
        x: close_rect
            .x
            .saturating_sub(WINDOW_BUTTON_GAP + WINDOW_BUTTON_SIZE),
        y: close_rect.y,
        width: WINDOW_BUTTON_SIZE,
        height: WINDOW_BUTTON_SIZE,
    }
}

pub(crate) fn launcher_button_rect(width: u32, index: usize) -> Rect {
    let rail = topbar_rail_rect(width as usize);
    let launcher_x = rail.x + 16 + TOPBAR_BRAND_WIDTH;
    let x = launcher_x + index * (LAUNCHER_BUTTON_WIDTH + LAUNCHER_BUTTON_GAP);
    let max_x = rail
        .x
        .saturating_add(rail.width)
        .saturating_sub(TOPBAR_STATUS_WIDTH + 18);
    if x >= max_x {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(LAUNCHER_BUTTON_HEIGHT)) / 2,
        width: LAUNCHER_BUTTON_WIDTH.min(max_x - x),
        height: LAUNCHER_BUTTON_HEIGHT,
    }
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    let rail = taskbar_rail_rect(width as usize, height as usize);
    let x = rail.x + 12 + index * (TASKBAR_SLOT_WIDTH + TASKBAR_SLOT_GAP);
    if x >= rail.x.saturating_add(rail.width) {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(TASKBAR_SLOT_HEIGHT)) / 2,
        width: TASKBAR_SLOT_WIDTH.min(rail.x + rail.width - x),
        height: TASKBAR_SLOT_HEIGHT,
    }
}

pub(crate) fn render_frame(state: &mut AppState) {
    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let cursor_x = state.cursor_x;
    let cursor_y = state.cursor_y;
    let focused_session_handle = state.focused_session_handle;
    let focused_wayland_surface_id = state.focused_wayland_surface_id;
    let desktop_cache = &state.desktop_cache;
    let console_windows = &mut state.console_windows;
    let wayland_windows = &state.wayland_windows;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    render_scene(
        &mut canvas,
        width,
        height,
        cursor_x,
        cursor_y,
        focused_session_handle,
        focused_wayland_surface_id,
        desktop_cache,
        console_windows,
        wayland_windows,
    );
}

pub(crate) fn render_boot_frame(state: &mut AppState) {
    let pixels = state.frame.pixels_mut();
    pixels.fill(COLOR_BOOT_BG_BASE);
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
    let rect = Rect {
        x: 0,
        y: 0,
        width: box_width,
        height: box_height,
    };
    canvas.fill_rect(rect, 0x00ff_ffff);
}

pub(crate) fn render_rect(state: &mut AppState, rect: Rect) {
    if rect.is_empty() {
        return;
    }

    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let cursor_x = state.cursor_x;
    let cursor_y = state.cursor_y;
    let focused_session_handle = state.focused_session_handle;
    let focused_wayland_surface_id = state.focused_wayland_surface_id;
    let desktop_cache = &state.desktop_cache;
    let console_windows = &mut state.console_windows;
    let wayland_windows = &state.wayland_windows;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, rect);

    render_scene(
        &mut canvas,
        width,
        height,
        cursor_x,
        cursor_y,
        focused_session_handle,
        focused_wayland_surface_id,
        desktop_cache,
        console_windows,
        wayland_windows,
    );
}

fn render_scene(
    canvas: &mut SurfaceCanvas<'_>,
    width: u32,
    height: u32,
    cursor_x: u32,
    cursor_y: u32,
    focused_session_handle: ConsoleSessionHandle,
    focused_wayland_surface_id: Option<u32>,
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
    wayland_windows: &[WaylandWindowSnapshot],
) {
    let clip_rect = canvas.clip_rect();
    canvas.draw_surface(
        &desktop_cache.pixels,
        desktop_cache.width,
        desktop_cache.height,
        desktop_cache.width,
        0,
        0,
    );

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
        draw_wayland_window(
            canvas,
            window,
            Some(window.surface_id) == focused_wayland_surface_id,
        );
    }

    for window in console_windows.iter_mut() {
        if window.minimized || window.session_handle == focused_session_handle {
            continue;
        }
        if !rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
            continue;
        }
        draw_console_window(canvas, window, false);
    }

    if let Some(surface_id) = focused_wayland_surface_id {
        if let Some(window) = wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
        {
            if rect_intersects_clip(
                clip_rect,
                shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
            ) {
                draw_wayland_window(canvas, window, true);
            }
        }
    }

    if focused_wayland_surface_id.is_none() && focused_session_handle != 0 {
        if let Some(window) = console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == focused_session_handle)
        {
            if rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
                draw_console_window(canvas, window, true);
            }
        }
    }

    for (index, window) in console_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, index);
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            draw_taskbar_slot(
                canvas,
                rect,
                window.title.as_str(),
                !window.minimized && window.session_handle == focused_session_handle,
            );
        }
    }
    for (index, window) in wayland_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, console_windows.len().saturating_add(index));
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            draw_taskbar_slot(
                canvas,
                rect,
                if window.title.is_empty() {
                    "Wayland App"
                } else {
                    window.title.as_str()
                },
                !window.minimized && Some(window.surface_id) == focused_wayland_surface_id,
            );
        }
    }

    if rect_intersects_clip(
        clip_rect,
        crate::canvas::cursor_dirty_rect(cursor_x, cursor_y, width, height),
    ) {
        canvas.draw_cursor(cursor_x, cursor_y);
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
            .saturating_add(steps.saturating_mul(2).saturating_add(1)),
        height: rect
            .height
            .saturating_add(steps.saturating_mul(2).saturating_add(1)),
    }
}

fn draw_console_window(canvas: &mut SurfaceCanvas<'_>, window: &mut ConsoleWindow, focused: bool) {
    refresh_window_surface(window, focused);
    draw_shadow(canvas, window.frame, WINDOW_SHADOW_STEPS, 28);
    canvas.draw_surface(
        &window.surface_cache.pixels,
        window.surface_cache.width,
        window.surface_cache.height,
        window.surface_cache.width,
        window.frame.x,
        window.frame.y,
    );
}

fn draw_wayland_window(
    canvas: &mut SurfaceCanvas<'_>,
    window: &WaylandWindowSnapshot,
    focused: bool,
) {
    if window.width == 0 || window.height == 0 {
        return;
    }

    let outer = wayland_window_outer_rect(window);
    let title = if window.title.is_empty() {
        "Wayland App"
    } else {
        window.title.as_str()
    };
    let client_rect = window_client_rect(outer);
    let clipped_outer = outer.intersect(canvas.clip_rect());
    let clipped_client = client_rect.intersect(canvas.clip_rect());
    if !clipped_outer.is_empty() && clipped_outer == clipped_client {
        canvas.draw_surface(
            window.pixels.as_slice(),
            window.width,
            window.height,
            window.stride_pixels,
            client_rect.x,
            client_rect.y,
        );
        return;
    }

    let (_, client_rect) = paint_window_chrome(canvas, outer, title, focused);
    canvas.draw_surface(
        window.pixels.as_slice(),
        window.width,
        window.height,
        window.stride_pixels,
        client_rect.x,
        client_rect.y,
    );
    canvas.stroke_rect(client_rect, COLOR_CLIENT_DIVIDER);
}

fn draw_taskbar_slot(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str, focused: bool) {
    if rect.is_empty() {
        return;
    }

    draw_shadow(canvas, rect, 2, 18);
    canvas.fill_rect_alpha(rect, COLOR_PANEL_GLASS, 208);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
        if focused {
            COLOR_PANEL_ELEVATED
        } else {
            COLOR_PANEL_INNER
        },
        220,
    );
    canvas.stroke_rect(
        rect,
        if focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_BORDER_IDLE
        },
    );
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        if focused {
            COLOR_ACCENT_SOFT
        } else {
            COLOR_BORDER_IDLE
        },
        145,
    );
    if focused {
        canvas.fill_rect(
            Rect {
                x: rect.x + 1,
                y: rect.y + rect.height.saturating_sub(3),
                width: rect.width.saturating_sub(2),
                height: 2,
            },
            COLOR_ACCENT_FOCUS,
        );
    }
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 7,
            y: rect.y + 8,
            width: 6,
            height: rect.height.saturating_sub(16),
        },
        if focused {
            COLOR_ACCENT_FOCUS
        } else {
            COLOR_TEXT_DIM
        },
        if focused { 230 } else { 100 },
    );

    let style = TextStyle::ui_medium(if focused {
        COLOR_TEXT_PRIMARY
    } else {
        COLOR_TEXT_DIM
    });
    font::draw_text_clipped(
        canvas,
        rect.x + 20,
        rect.y + 5,
        title,
        rect.width.saturating_sub(30),
        style,
    );
}

fn draw_launcher_button(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str) {
    if rect.is_empty() {
        return;
    }

    draw_shadow(canvas, rect, 2, 12);
    canvas.fill_rect_alpha(rect, COLOR_PANEL_GLASS, 196);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
        COLOR_PANEL_ELEVATED,
        218,
    );
    canvas.stroke_rect(rect, COLOR_BORDER_IDLE);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_ACCENT_SOFT,
        135,
    );
    canvas.fill_rect(
        Rect {
            x: rect.x + 1,
            y: rect.y + rect.height.saturating_sub(3),
            width: rect.width.saturating_sub(2),
            height: 2,
        },
        COLOR_ACCENT_FOCUS,
    );
    draw_launcher_badge(canvas, rect, title);

    let style = TextStyle::ui_medium(COLOR_TEXT_PRIMARY);
    font::draw_text_clipped(
        canvas,
        rect.x + 30,
        rect.y + 4,
        title,
        rect.width.saturating_sub(39),
        style,
    );
}

fn draw_launcher_badge(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str) {
    let badge = Rect {
        x: rect.x + 7,
        y: rect.y + 5,
        width: 16,
        height: 16,
    };
    canvas.fill_rect_alpha(badge, COLOR_ACCENT_FOCUS, 210);
    canvas.stroke_rect(badge, COLOR_ACCENT_SOFT);

    let label = title
        .chars()
        .find(|ch| ch.is_ascii_alphanumeric())
        .unwrap_or('>');
    let mut text = [0_u8; 1];
    let label = label.to_ascii_uppercase().encode_utf8(&mut text);
    font::draw_text(
        canvas,
        badge.x + 5,
        badge.y + 2,
        label,
        TextStyle::ui_small(COLOR_DESKTOP_BG_BASE),
    );
}

fn refresh_desktop_surface(state: &mut AppState) {
    let width = state.surface.width as usize;
    let height = state.surface.height as usize;
    let resized = state.desktop_cache.width != width || state.desktop_cache.height != height;
    if resized {
        state.desktop_cache.width = width;
        state.desktop_cache.height = height;
        state
            .desktop_cache
            .pixels
            .resize(width.saturating_mul(height), 0);
        state.desktop_cache.valid = false;
    }
    if state.desktop_cache.valid {
        return;
    }

    {
        let mut canvas = SurfaceCanvas::new(
            state.desktop_cache.pixels.as_mut_slice(),
            width as u32,
            height as u32,
            width,
        );
        let screen = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        canvas.fill_rect(screen, COLOR_DESKTOP_BG_BASE);
        canvas.fill_rect_alpha(
            Rect {
                x: 0,
                y: 0,
                width,
                height: (height / 3).max(1),
            },
            COLOR_DESKTOP_BAND,
            94,
        );
        canvas.fill_rect_alpha(
            Rect {
                x: 0,
                y: height.saturating_sub(height / 5),
                width,
                height: height / 5,
            },
            COLOR_CLIENT_BACKDROP,
            86,
        );
        canvas.fill_pattern_grid(screen, 28, COLOR_GRID, 34);
        canvas.fill_pattern_grid(screen, 112, COLOR_GRID_MAJOR, 48);

        let topbar = topbar_rail_rect(width);
        let taskbar = taskbar_rail_rect(width, height);
        draw_rail_panel(&mut canvas, topbar);
        draw_rail_panel(&mut canvas, taskbar);

        draw_brand_block(&mut canvas, topbar);
        draw_status_block(&mut canvas, topbar, state.launcher_programs.len());

        for (index, program) in state.launcher_programs.iter().enumerate() {
            draw_launcher_button(
                &mut canvas,
                launcher_button_rect(width as u32, index),
                program.title.as_str(),
            );
        }
    }
    state.desktop_cache.valid = true;
}

fn draw_brand_block(canvas: &mut SurfaceCanvas<'_>, topbar: Rect) {
    let brand_rect = Rect {
        x: topbar.x + 10,
        y: topbar.y + 7,
        width: TOPBAR_BRAND_WIDTH.saturating_sub(18),
        height: topbar.height.saturating_sub(14),
    };
    canvas.fill_rect_alpha(brand_rect, COLOR_PANEL_ELEVATED, 208);
    canvas.stroke_rect(brand_rect, COLOR_BORDER_IDLE);
    canvas.fill_rect(
        Rect {
            x: brand_rect.x,
            y: brand_rect.y,
            width: 3,
            height: brand_rect.height,
        },
        COLOR_ACCENT_WARM,
    );
    font::draw_text(
        canvas,
        brand_rect.x + 12,
        brand_rect.y + 4,
        "RustOS",
        TextStyle::ui_medium(COLOR_TEXT_PRIMARY),
    );
}

fn draw_status_block(canvas: &mut SurfaceCanvas<'_>, topbar: Rect, launcher_count: usize) {
    let status_width = TOPBAR_STATUS_WIDTH.min(topbar.width.saturating_sub(24));
    let status_rect = Rect {
        x: topbar.x + topbar.width.saturating_sub(status_width + 10),
        y: topbar.y + 8,
        width: status_width,
        height: topbar.height.saturating_sub(16),
    };
    if status_rect.width < 120 {
        return;
    }

    canvas.fill_rect_alpha(status_rect, COLOR_CLIENT_BACKDROP, 168);
    canvas.stroke_rect(status_rect, COLOR_BORDER_IDLE);
    canvas.fill_rect_alpha(
        Rect {
            x: status_rect.x + 1,
            y: status_rect.y + 1,
            width: status_rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_ACCENT_SOFT,
        110,
    );
    let text = if launcher_count == 0 {
        "WAYLAND READY"
    } else {
        "WAYLAND LAUNCHER"
    };
    font::draw_text_clipped(
        canvas,
        status_rect.x + 10,
        status_rect.y + 4,
        text,
        status_rect.width.saturating_sub(62),
        TextStyle::ui_small(COLOR_TEXT_DIM),
    );
    let apps_label = format!("{launcher_count} APPS");
    let count_x = status_rect.x + status_rect.width.saturating_sub(58);
    font::draw_text_clipped(
        canvas,
        count_x,
        status_rect.y + 4,
        apps_label.as_str(),
        50,
        TextStyle::ui_small(COLOR_ACCENT_WARM),
    );
}

fn refresh_window_surface(window: &mut ConsoleWindow, focused: bool) {
    let width = window.frame.width;
    let height = window.frame.height;
    if width == 0 || height == 0 {
        window.surface_cache.valid = false;
        return;
    }

    let resized = window.surface_cache.width != width || window.surface_cache.height != height;
    if resized {
        window.surface_cache.width = width;
        window.surface_cache.height = height;
        window
            .surface_cache
            .pixels
            .resize(width.saturating_mul(height), 0);
        window.surface_cache.valid = false;
    }

    let outer = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let title_rect = console_window_title_bar_rect(outer);
    let client_rect = window_client_rect(outer);
    let terminal_needs_rebuild =
        window.terminal_dirty || window.terminal.needs_layout_rebuild(client_rect);

    if window.surface_cache.valid
        && window.surface_cache.focused == focused
        && !terminal_needs_rebuild
    {
        window.terminal.set_client_rect(client_rect);
        window.terminal.set_focused(focused);
        return;
    }

    let mut canvas = SurfaceCanvas::new(
        window.surface_cache.pixels.as_mut_slice(),
        width as u32,
        height as u32,
        width,
    );
    let (drawn_title_rect, drawn_client_rect) =
        paint_window_chrome(&mut canvas, outer, window.title.as_str(), focused);
    debug_assert_eq!(title_rect, drawn_title_rect);
    debug_assert_eq!(client_rect, drawn_client_rect);

    if terminal_needs_rebuild {
        window
            .terminal
            .rebuild_from_bytes(client_rect, focused, &window.output_cache);
        window.terminal_dirty = false;
    } else {
        window.terminal.set_client_rect(client_rect);
        window.terminal.set_focused(focused);
    }
    window.terminal.render(&mut canvas);

    window.surface_cache.focused = focused;
    window.surface_cache.valid = true;
}

fn paint_window_chrome(
    canvas: &mut SurfaceCanvas<'_>,
    outer: Rect,
    title: &str,
    focused: bool,
) -> (Rect, Rect) {
    let inner = Rect {
        x: outer.x + WINDOW_BORDER,
        y: outer.y + WINDOW_BORDER,
        width: outer.width.saturating_sub(WINDOW_BORDER * 2),
        height: outer.height.saturating_sub(WINDOW_BORDER * 2),
    };
    let title_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: WINDOW_TITLE_HEIGHT,
    };
    let client_rect = window_client_rect(outer);

    draw_shadow(canvas, outer, WINDOW_SHADOW_STEPS, 28);
    canvas.fill_rect(
        outer,
        if focused {
            COLOR_BORDER_FOCUS
        } else {
            COLOR_BORDER_IDLE
        },
    );
    canvas.fill_rect(inner, COLOR_PANEL_INNER);
    canvas.fill_rect_alpha(title_rect, COLOR_PANEL_GLASS, 230);
    if focused {
        canvas.fill_rect_alpha(title_rect, COLOR_PANEL_ELEVATED, 118);
    }
    canvas.fill_rect_alpha(
        Rect {
            x: title_rect.x,
            y: title_rect.y,
            width: title_rect.width,
            height: 1,
        },
        COLOR_ACCENT_SOFT,
        165,
    );
    canvas.fill_rect(
        Rect {
            x: title_rect.x,
            y: title_rect.y,
            width: WINDOW_FOCUS_STRIP_WIDTH,
            height: title_rect.height,
        },
        if focused {
            COLOR_ACCENT_FOCUS
        } else {
            COLOR_BORDER_IDLE
        },
    );
    if focused {
        canvas.fill_rect(
            Rect {
                x: title_rect.x + WINDOW_FOCUS_STRIP_WIDTH,
                y: title_rect.y,
                width: title_rect.width.saturating_sub(WINDOW_FOCUS_STRIP_WIDTH),
                height: 2,
            },
            COLOR_ACCENT_WARM,
        );
    }
    canvas.fill_rect(client_rect, COLOR_CLIENT_BACKDROP);
    canvas.stroke_rect(client_rect, COLOR_CLIENT_DIVIDER);

    let style = TextStyle::ui_large(if focused {
        COLOR_TEXT_PRIMARY
    } else {
        COLOR_TEXT_DIM
    });
    let button_cluster_width =
        WINDOW_BUTTON_MARGIN_RIGHT + WINDOW_BUTTON_SIZE * 2 + WINDOW_BUTTON_GAP + 12;
    font::draw_text_clipped(
        canvas,
        title_rect.x + 12,
        title_rect.y + 8,
        title,
        title_rect.width.saturating_sub(button_cluster_width),
        style,
    );
    draw_window_controls(canvas, outer);

    (title_rect, client_rect)
}

fn draw_window_controls(canvas: &mut SurfaceCanvas<'_>, outer: Rect) {
    let minimize_rect = window_minimize_button_rect(outer);
    let close_rect = window_close_button_rect(outer);
    draw_window_button(canvas, minimize_rect, COLOR_BUTTON_MINIMIZE, "_");
    draw_window_button(canvas, close_rect, COLOR_BUTTON_CLOSE, "X");
}

fn draw_window_button(canvas: &mut SurfaceCanvas<'_>, rect: Rect, accent: u32, label: &str) {
    canvas.fill_rect_alpha(rect, COLOR_BUTTON_IDLE, 230);
    canvas.stroke_rect(rect, accent);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 2,
            y: rect.y + 2,
            width: rect.width.saturating_sub(4),
            height: rect.height.saturating_sub(4),
        },
        accent,
        38,
    );
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        accent,
        140,
    );
    let style = TextStyle::ui_small(COLOR_TEXT_PRIMARY);
    let text_x = rect.x + rect.width.saturating_sub(8) / 2;
    let text_y = rect.y + rect.height.saturating_sub(12) / 2;
    font::draw_text(canvas, text_x, text_y, label, style);
}

fn draw_rail_panel(canvas: &mut SurfaceCanvas<'_>, rect: Rect) {
    draw_shadow(canvas, rect, 4, 18);
    canvas.fill_rect_alpha(rect, COLOR_PANEL_GLASS, 186);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
        COLOR_PANEL_INNER,
        214,
    );
    canvas.stroke_rect(rect, COLOR_BORDER_IDLE);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_ACCENT_SOFT,
        130,
    );
}

fn topbar_rail_rect(width: usize) -> Rect {
    Rect {
        x: RAIL_SIDE_MARGIN,
        y: TOPBAR_MARGIN_TOP,
        width: width.saturating_sub(RAIL_SIDE_MARGIN * 2),
        height: TOPBAR_RAIL_HEIGHT,
    }
}

fn taskbar_rail_rect(width: usize, height: usize) -> Rect {
    Rect {
        x: RAIL_SIDE_MARGIN,
        y: height.saturating_sub(TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT),
        width: width.saturating_sub(RAIL_SIDE_MARGIN * 2),
        height: TASKBAR_RAIL_HEIGHT,
    }
}

fn window_client_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x + WINDOW_BORDER + WINDOW_PADDING_X,
        y: outer.y + WINDOW_BORDER + WINDOW_TITLE_HEIGHT + WINDOW_PADDING_Y,
        width: outer
            .width
            .saturating_sub(WINDOW_BORDER * 2 + WINDOW_PADDING_X * 2),
        height: outer
            .height
            .saturating_sub(WINDOW_BORDER * 2 + WINDOW_TITLE_HEIGHT + WINDOW_PADDING_Y * 2),
    }
}

fn draw_shadow(canvas: &mut SurfaceCanvas<'_>, rect: Rect, steps: usize, base_alpha: u8) {
    if rect.is_empty() || steps == 0 || base_alpha == 0 {
        return;
    }

    for step in 0..steps {
        let alpha = base_alpha.saturating_sub((step as u8).saturating_mul(4));
        if alpha == 0 {
            break;
        }
        let ring = Rect {
            x: rect.x.saturating_sub(step),
            y: rect.y.saturating_sub(step),
            width: rect.width.saturating_add(step * 2),
            height: rect.height.saturating_add(step * 2),
        };
        canvas.fill_rect_alpha(
            Rect {
                x: ring.x.saturating_add(ring.width),
                y: ring.y.saturating_add(step),
                width: 1,
                height: ring.height.saturating_sub(step),
            },
            COLOR_SHADOW,
            alpha,
        );
        canvas.fill_rect_alpha(
            Rect {
                x: ring.x.saturating_add(step),
                y: ring.y.saturating_add(ring.height),
                width: ring.width.saturating_sub(step),
                height: 1,
            },
            COLOR_SHADOW,
            alpha,
        );
    }
}
