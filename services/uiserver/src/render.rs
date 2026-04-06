use std::string::String;

use crate::app::{AppState, ConsoleWindow, DesktopSurfaceCache};
use crate::canvas::{Rect, SurfaceCanvas};
use crate::font::{self, TextStyle};
use crate::sys::ConsoleSessionHandle;
use crate::wayland::WaylandWindowSnapshot;

const COLOR_BG_BASE: u32 = 0x000a_0f16;
const COLOR_GRID: u32 = 0x0018_2533;
const COLOR_GRID_MAJOR: u32 = 0x001f_3041;
const COLOR_PANEL_GLASS: u32 = 0x0012_1c28;
const COLOR_PANEL_INNER: u32 = 0x0016_2434;
const COLOR_ACCENT_FOCUS: u32 = 0x0069_d5ff;
const COLOR_ACCENT_SOFT: u32 = 0x00bf_efff;
const COLOR_BORDER_IDLE: u32 = 0x0034_485e;
const COLOR_BORDER_FOCUS: u32 = 0x0069_d5ff;
const COLOR_TEXT_PRIMARY: u32 = 0x00f3_faff;
const COLOR_TEXT_DIM: u32 = 0x00a7_bcd0;
const COLOR_SHADOW: u32 = 0x0000_0000;
const COLOR_CLIENT_BACKDROP: u32 = 0x0010_1724;
const COLOR_CLIENT_DIVIDER: u32 = 0x0026_3748;

const TOPBAR_MARGIN_TOP: usize = 16;
const TOPBAR_RAIL_HEIGHT: usize = 40;
pub(crate) const TOPBAR_HEIGHT: usize = TOPBAR_MARGIN_TOP + TOPBAR_RAIL_HEIGHT;

const TASKBAR_MARGIN_BOTTOM: usize = 20;
const TASKBAR_RAIL_HEIGHT: usize = 44;
pub(crate) const TASKBAR_HEIGHT: usize = TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT;

const RAIL_SIDE_MARGIN: usize = 20;
const DESKTOP_MARGIN_X: usize = 32;
const DESKTOP_MARGIN_Y: usize = 24;

const WINDOW_BORDER: usize = 1;
const WINDOW_TITLE_HEIGHT: usize = 36;
const WINDOW_PADDING_X: usize = 16;
const WINDOW_PADDING_Y: usize = 14;
const WINDOW_FOCUS_STRIP_WIDTH: usize = 3;
const WINDOW_SHADOW_STEPS: usize = 6;

const DEFAULT_WINDOW_WIDTH: usize = 640;
const DEFAULT_WINDOW_HEIGHT: usize = 400;
const MIN_WINDOW_WIDTH: usize = 360;
const MIN_WINDOW_HEIGHT: usize = 240;
const WINDOW_CASCADE_X: usize = 28;
const WINDOW_CASCADE_Y: usize = 24;
const WINDOW_CASCADE_SLOTS: usize = 6;

const LAUNCHER_BUTTON_WIDTH: usize = 148;
const LAUNCHER_BUTTON_HEIGHT: usize = 24;
const LAUNCHER_BUTTON_GAP: usize = 10;

const TASKBAR_SLOT_WIDTH: usize = 172;
const TASKBAR_SLOT_HEIGHT: usize = 30;
const TASKBAR_SLOT_GAP: usize = 12;

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

pub(crate) fn console_window_title_bar_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x + WINDOW_BORDER,
        y: rect.y + WINDOW_BORDER,
        width: rect.width.saturating_sub(WINDOW_BORDER * 2),
        height: WINDOW_TITLE_HEIGHT,
    }
}

pub(crate) fn launcher_button_rect(width: u32, index: usize) -> Rect {
    let rail = topbar_rail_rect(width as usize);
    let x = rail.x + 16 + index * (LAUNCHER_BUTTON_WIDTH + LAUNCHER_BUTTON_GAP);
    if x >= rail.x.saturating_add(rail.width) {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(LAUNCHER_BUTTON_HEIGHT)) / 2,
        width: LAUNCHER_BUTTON_WIDTH.min(rail.x + rail.width - x),
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
        desktop_cache,
        console_windows,
        wayland_windows,
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
    let cursor_x = state.cursor_x;
    let cursor_y = state.cursor_y;
    let focused_session_handle = state.focused_session_handle;
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
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
    wayland_windows: &[WaylandWindowSnapshot],
) {
    canvas.draw_surface(
        &desktop_cache.pixels,
        desktop_cache.width,
        desktop_cache.height,
        desktop_cache.width,
        0,
        0,
    );

    for window in wayland_windows {
        draw_wayland_window(canvas, window);
    }

    for window in console_windows.iter_mut() {
        let focused = window.session_handle == focused_session_handle;
        draw_console_window(canvas, window, focused);
    }

    for (index, window) in console_windows.iter().enumerate() {
        draw_taskbar_slot(
            canvas,
            taskbar_slot_rect(width, height, index),
            window.title.as_str(),
            window.session_handle == focused_session_handle,
        );
    }
    for (index, window) in wayland_windows.iter().enumerate() {
        draw_taskbar_slot(
            canvas,
            taskbar_slot_rect(width, height, console_windows.len().saturating_add(index)),
            if window.title.is_empty() {
                "Wayland App"
            } else {
                window.title.as_str()
            },
            false,
        );
    }

    canvas.draw_cursor(cursor_x, cursor_y);
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

fn draw_wayland_window(canvas: &mut SurfaceCanvas<'_>, window: &WaylandWindowSnapshot) {
    if window.width == 0 || window.height == 0 {
        return;
    }

    let outer = Rect {
        x: window.frame.x,
        y: window.frame.y,
        width: window.width + WINDOW_BORDER * 2,
        height: window.height + WINDOW_TITLE_HEIGHT + WINDOW_BORDER * 2,
    };
    let title = if window.title.is_empty() {
        "Wayland App"
    } else {
        window.title.as_str()
    };
    let (_, client_rect) = paint_window_chrome(canvas, outer, title, false);
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
    canvas.fill_rect_alpha(rect, COLOR_PANEL_GLASS, 204);
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
        COLOR_ACCENT_SOFT,
        150,
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

    let style = TextStyle::ui_medium(if focused {
        COLOR_TEXT_PRIMARY
    } else {
        COLOR_TEXT_DIM
    });
    let text = truncate_text(title, rect.width.saturating_sub(20), style);
    font::draw_text(canvas, rect.x + 10, rect.y + 5, &text, style);
}

fn draw_launcher_button(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str) {
    if rect.is_empty() {
        return;
    }

    canvas.fill_rect_alpha(rect, COLOR_PANEL_GLASS, 192);
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(2),
        },
        COLOR_PANEL_INNER,
        212,
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

    let style = TextStyle::ui_medium(COLOR_TEXT_PRIMARY);
    let text = truncate_text(title, rect.width.saturating_sub(18), style);
    font::draw_text(canvas, rect.x + 9, rect.y + 4, &text, style);
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
    canvas.fill_rect(screen, COLOR_BG_BASE);
    canvas.fill_pattern_grid(screen, 28, COLOR_GRID, 34);
    canvas.fill_pattern_grid(screen, 112, COLOR_GRID_MAJOR, 48);

    let topbar = topbar_rail_rect(width);
    let taskbar = taskbar_rail_rect(width, height);
    draw_rail_panel(&mut canvas, topbar);
    draw_rail_panel(&mut canvas, taskbar);

    font::draw_text(
        &mut canvas,
        topbar.x + topbar.width.saturating_sub(198),
        topbar.y + 6,
        "WAYLAND // AERO HUD",
        TextStyle::ui_small(COLOR_TEXT_DIM),
    );

    for (index, program) in state.launcher_programs.iter().enumerate() {
        draw_launcher_button(
            &mut canvas,
            launcher_button_rect(width as u32, index),
            program.title.as_str(),
        );
    }

    state.desktop_cache.valid = true;
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
    canvas.fill_rect(client_rect, COLOR_CLIENT_BACKDROP);
    canvas.stroke_rect(client_rect, COLOR_CLIENT_DIVIDER);

    let style = TextStyle::ui_large(if focused {
        COLOR_TEXT_PRIMARY
    } else {
        COLOR_TEXT_DIM
    });
    let truncated = truncate_text(title, title_rect.width.saturating_sub(26), style);
    font::draw_text(
        canvas,
        title_rect.x + 12,
        title_rect.y + 8,
        &truncated,
        style,
    );

    (title_rect, client_rect)
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

fn truncate_text(text: &str, max_width: usize, style: TextStyle) -> String {
    if font::measure_text(text, style) <= max_width {
        return text.to_string();
    }

    let ellipsis = "...";
    let ellipsis_width = font::measure_text(ellipsis, style);
    if ellipsis_width >= max_width {
        return ellipsis.to_string();
    }

    let mut out = String::new();
    for ch in text.chars() {
        let next_len = out.len();
        out.push(ch);
        if font::measure_text(out.as_str(), style).saturating_add(ellipsis_width) > max_width {
            out.truncate(next_len);
            break;
        }
    }
    out.push_str(ellipsis);
    out
}
