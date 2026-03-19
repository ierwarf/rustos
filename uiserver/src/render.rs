use embedded_graphics::geometry::Point;
use embedded_graphics::mono_font::{ascii::FONT_9X18_BOLD, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::text::{Baseline, Text};
use embedded_graphics::Drawable;

use crate::canvas::{Rect, SurfaceCanvas};
use crate::{AppState, ConsoleWindow, DesktopSurfaceCache};

const COLOR_BG_TOP: u32 = 0x0015_2038;
const COLOR_BG_BOTTOM: u32 = 0x000b_101f;
const COLOR_TOPBAR: u32 = 0x0010_1724;
const COLOR_TASKBAR: u32 = 0x000d_141a;
const COLOR_WINDOW: u32 = 0x0012_1a29;
const COLOR_WINDOW_FOCUS: u32 = 0x0031_5d9a;
const COLOR_WINDOW_UNFOCUSED: u32 = 0x0024_3652;
const COLOR_TITLE_FOCUS: u32 = 0x0025_70b8;
const COLOR_TITLE_UNFOCUSED: u32 = 0x0018_263b;
const COLOR_TITLE_TEXT: Rgb888 = Rgb888::new(232, 238, 246);
const COLOR_LAUNCHER_BUTTON: u32 = 0x001b_2b40;
const COLOR_LAUNCHER_BUTTON_ACTIVE: u32 = 0x0038_84cf;
const COLOR_TASKBAR_SLOT: u32 = 0x0020_3049;
const COLOR_TASKBAR_SLOT_ACTIVE: u32 = 0x004d_c4f5;

pub(crate) const TOPBAR_HEIGHT: usize = 48;
pub(crate) const TASKBAR_HEIGHT: usize = 56;
const WINDOW_TITLE_HEIGHT: usize = 32;
const WINDOW_BORDER: usize = 2;
const DESKTOP_MARGIN_X: usize = 24;
const DESKTOP_MARGIN_Y: usize = 24;
const DEFAULT_WINDOW_WIDTH: usize = 560;
const DEFAULT_WINDOW_HEIGHT: usize = 360;
const MIN_WINDOW_WIDTH: usize = 320;
const MIN_WINDOW_HEIGHT: usize = 220;
const WINDOW_CASCADE_X: usize = 30;
const WINDOW_CASCADE_Y: usize = 24;
const WINDOW_CASCADE_SLOTS: usize = 6;
const LAUNCHER_BUTTON_WIDTH: usize = 164;
const LAUNCHER_BUTTON_HEIGHT: usize = 30;
const LAUNCHER_BUTTON_GAP: usize = 12;
const LAUNCHER_START_X: usize = 24;
const LAUNCHER_Y: usize = 9;
const TASKBAR_SLOT_WIDTH: usize = 156;
const TASKBAR_SLOT_HEIGHT: usize = 28;
const TASKBAR_SLOT_GAP: usize = 14;
const TASKBAR_SLOT_START_X: usize = 28;

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
    let x = LAUNCHER_START_X + index * (LAUNCHER_BUTTON_WIDTH + LAUNCHER_BUTTON_GAP);
    if x >= width as usize {
        return Rect::empty();
    }

    Rect {
        x,
        y: LAUNCHER_Y,
        width: LAUNCHER_BUTTON_WIDTH.min(width as usize - x),
        height: LAUNCHER_BUTTON_HEIGHT,
    }
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    let x = TASKBAR_SLOT_START_X + index * (TASKBAR_SLOT_WIDTH + TASKBAR_SLOT_GAP);
    if x >= width as usize {
        return Rect::empty();
    }

    Rect {
        x,
        y: height as usize - TASKBAR_HEIGHT + 14,
        width: TASKBAR_SLOT_WIDTH.min(width as usize - x),
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
    let focused_session_index = state.focused_session_index;
    let desktop_cache = &state.desktop_cache;
    let console_windows = &mut state.console_windows;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    render_scene(
        &mut canvas,
        width,
        height,
        cursor_x,
        cursor_y,
        focused_session_index,
        desktop_cache,
        console_windows,
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
    let focused_session_index = state.focused_session_index;
    let desktop_cache = &state.desktop_cache;
    let console_windows = &mut state.console_windows;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, rect);

    render_scene(
        &mut canvas,
        width,
        height,
        cursor_x,
        cursor_y,
        focused_session_index,
        desktop_cache,
        console_windows,
    );
}

pub(crate) fn render_cursor_only(state: &mut AppState, previous_rect: Rect, current_rect: Rect) {
    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let cursor_x = state.cursor_x;
    let cursor_y = state.cursor_y;
    let focused_session_index = state.focused_session_index;
    let desktop_cache = &state.desktop_cache;
    let console_windows = &mut state.console_windows;
    let pixels = state.frame.pixels_mut();

    if !previous_rect.is_empty() {
        let mut canvas =
            SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, previous_rect);
        render_scene(
            &mut canvas,
            width,
            height,
            cursor_x,
            cursor_y,
            focused_session_index,
            desktop_cache,
            console_windows,
        );
    }

    if current_rect != previous_rect && !current_rect.is_empty() {
        let mut canvas =
            SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, current_rect);
        render_scene(
            &mut canvas,
            width,
            height,
            cursor_x,
            cursor_y,
            focused_session_index,
            desktop_cache,
            console_windows,
        );
    }
}

fn render_scene(
    canvas: &mut SurfaceCanvas<'_>,
    width: u32,
    height: u32,
    cursor_x: u32,
    cursor_y: u32,
    focused_session_index: u32,
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
) {
    canvas.draw_surface(
        &desktop_cache.pixels,
        desktop_cache.width,
        desktop_cache.height,
        desktop_cache.width,
        0,
        0,
    );

    for (index, window) in console_windows.iter_mut().enumerate() {
        let focused = window.session_index == focused_session_index;
        draw_console_window(canvas, window, focused);
        draw_taskbar_slot(
            canvas,
            taskbar_slot_rect(width, height, index),
            window.title.as_str(),
            focused,
        );
    }

    canvas.draw_cursor(cursor_x, cursor_y);
}

fn draw_launcher_button(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    canvas.fill_rect_alpha(rect, COLOR_LAUNCHER_BUTTON, 245);
    canvas.fill_rect(
        Rect {
            x: rect.x,
            y: rect.y + rect.height.saturating_sub(2),
            width: rect.width,
            height: 2,
        },
        COLOR_LAUNCHER_BUTTON_ACTIVE,
    );
    draw_label(
        canvas,
        (rect.x + 10) as i32,
        (rect.y + 5) as i32,
        title,
        COLOR_TITLE_TEXT,
    );
}

fn draw_console_window(
    canvas: &mut SurfaceCanvas<'_>,
    window: &mut crate::ConsoleWindow,
    focused: bool,
) {
    refresh_window_surface(window, focused);
    canvas.draw_surface(
        &window.surface_cache.pixels,
        window.surface_cache.width,
        window.surface_cache.height,
        window.surface_cache.width,
        window.frame.x,
        window.frame.y,
    );
}

fn draw_taskbar_slot(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str, focused: bool) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    canvas.fill_rect(
        rect,
        if focused {
            COLOR_TASKBAR_SLOT_ACTIVE
        } else {
            COLOR_TASKBAR_SLOT
        },
    );
    draw_label(
        canvas,
        (rect.x + 10) as i32,
        (rect.y + 5) as i32,
        title,
        COLOR_TITLE_TEXT,
    );
}

fn draw_label(canvas: &mut SurfaceCanvas<'_>, x: i32, y: i32, text: &str, color: Rgb888) {
    let style = MonoTextStyle::new(&FONT_9X18_BOLD, color);
    let _ = Text::with_baseline(text, Point::new(x, y), style, Baseline::Top).draw(canvas);
}

fn refresh_desktop_surface(state: &mut AppState) {
    let width = state.surface.width as usize;
    let height = state.surface.height as usize;
    let resized = state.desktop_cache.width != width || state.desktop_cache.height != height;
    if resized {
        state.desktop_cache.width = width;
        state.desktop_cache.height = height;
        state.desktop_cache
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
    canvas.draw_vertical_gradient(COLOR_BG_TOP, COLOR_BG_BOTTOM);
    canvas.fill_rect_alpha(
        Rect {
            x: 0,
            y: 0,
            width,
            height: TOPBAR_HEIGHT,
        },
        COLOR_TOPBAR,
        220,
    );
    canvas.fill_rect_alpha(
        Rect {
            x: 0,
            y: height.saturating_sub(TASKBAR_HEIGHT),
            width,
            height: TASKBAR_HEIGHT,
        },
        COLOR_TASKBAR,
        220,
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
        window.surface_cache.pixels.resize(width.saturating_mul(height), 0);
        window.surface_cache.valid = false;
    }

    let inner = Rect {
        x: WINDOW_BORDER,
        y: WINDOW_BORDER,
        width: width.saturating_sub(WINDOW_BORDER * 2),
        height: height.saturating_sub(WINDOW_BORDER * 2),
    };
    let client_rect = Rect {
        x: inner.x + 6,
        y: inner.y + WINDOW_TITLE_HEIGHT + 6,
        width: inner.width.saturating_sub(12),
        height: inner.height.saturating_sub(WINDOW_TITLE_HEIGHT + 12),
    };
    let terminal_needs_rebuild =
        window.terminal_dirty || window.terminal.needs_layout_rebuild(client_rect);

    if window.surface_cache.valid && window.surface_cache.focused == focused && !terminal_needs_rebuild
    {
        window.terminal.set_client_rect(client_rect);
        window.terminal.set_focused(focused);
        return;
    }

    let border_color = if focused {
        COLOR_WINDOW_FOCUS
    } else {
        COLOR_WINDOW_UNFOCUSED
    };
    let title_color = if focused {
        COLOR_TITLE_FOCUS
    } else {
        COLOR_TITLE_UNFOCUSED
    };
    let title_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: WINDOW_TITLE_HEIGHT,
    };

    let mut canvas = SurfaceCanvas::new(
        window.surface_cache.pixels.as_mut_slice(),
        width as u32,
        height as u32,
        width,
    );
    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        },
        border_color,
    );
    canvas.fill_rect(inner, COLOR_WINDOW);
    canvas.fill_rect(title_rect, title_color);
    draw_label(
        &mut canvas,
        (title_rect.x + 10) as i32,
        (title_rect.y + 7) as i32,
        window.title.as_str(),
        COLOR_TITLE_TEXT,
    );

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
