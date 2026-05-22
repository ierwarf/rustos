//! Chrome painting: the topbar, the dock, individual windows, and the
//! traffic-light controls. The rendering primitives are all
//! `fill_rounded_rect_alpha` / `fill_rect_alpha` against a SurfaceCanvas;
//! no shape larger than a rounded rectangle is needed.

use crate::app::ConsoleWindow;
use crate::canvas::{Rect, SurfaceCanvas};
use crate::font::{self, TextStyle};
use crate::layout::{
    window_button_cluster_width, window_client_rect, window_close_button_rect,
    window_maximize_button_rect, window_minimize_button_rect, window_title_bar_rect,
    TOPBAR_BRAND_WIDTH, TOPBAR_INNER_PADDING_X, TOPBAR_STATUS_WIDTH, WINDOW_RADIUS,
    WINDOW_SHADOW_STEPS,
};
use crate::wayland::WaylandWindowSnapshot;

use super::colors::{
    COLOR_ACCENT_GOLD, COLOR_ACCENT_MINT, COLOR_ACCENT_PEACH, COLOR_AURORA_VIOLET,
    COLOR_BG_DEEP, COLOR_BORDER_SUBTLE, COLOR_GLASS_DARK, COLOR_GLASS_DEEP, COLOR_PANEL_HIGHLIGHT,
    COLOR_SHADOW, COLOR_TEXT_DIM, COLOR_TEXT_PRIMARY,
};
use super::icons::draw_app_icon;
use super::{wayland_window_client_rect, wayland_window_outer_rect};

const RAIL_RADIUS: usize = 18;

// ---- top-level rails (topbar / dock) ----

pub(super) fn draw_rail_panel(canvas: &mut SurfaceCanvas<'_>, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    draw_panel_shadow(canvas, rect);

    canvas.fill_rounded_rect_alpha(rect, COLOR_GLASS_DARK, 200, RAIL_RADIUS);
    canvas.fill_rounded_rect_alpha(rect, COLOR_AURORA_VIOLET, 14, RAIL_RADIUS);

    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + RAIL_RADIUS,
            y: rect.y + 1,
            width: rect.width.saturating_sub(RAIL_RADIUS * 2),
            height: 1,
        },
        COLOR_PANEL_HIGHLIGHT,
        160,
    );
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + RAIL_RADIUS,
            y: rect.y + rect.height.saturating_sub(2),
            width: rect.width.saturating_sub(RAIL_RADIUS * 2),
            height: 1,
        },
        COLOR_SHADOW,
        70,
    );
}

fn draw_panel_shadow(canvas: &mut SurfaceCanvas<'_>, rect: Rect) {
    for step in 0..4 {
        let alpha = 36u32.saturating_sub((step as u32).saturating_mul(8)) as u8;
        if alpha == 0 {
            break;
        }
        let shadow = Rect {
            x: rect.x.saturating_sub(step),
            y: rect.y.saturating_sub(step / 2).saturating_add(2),
            width: rect.width.saturating_add(step * 2),
            height: rect.height.saturating_add(step + 1),
        };
        canvas.fill_rounded_rect_alpha(shadow, COLOR_SHADOW, alpha, RAIL_RADIUS + step);
    }
}

pub(super) fn draw_brand_block(canvas: &mut SurfaceCanvas<'_>, topbar: Rect) {
    let brand_rect = Rect {
        x: topbar.x + TOPBAR_INNER_PADDING_X,
        y: topbar.y + 8,
        width: TOPBAR_BRAND_WIDTH,
        height: topbar.height.saturating_sub(16),
    };
    let dot_size = 14;
    let dot_y = brand_rect.y + (brand_rect.height.saturating_sub(dot_size)) / 2;
    let dot_rect = Rect {
        x: brand_rect.x,
        y: dot_y,
        width: dot_size,
        height: dot_size,
    };
    // Soft halo, then the mint tile, then a small white core.
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: dot_rect.x.saturating_sub(2),
            y: dot_rect.y.saturating_sub(2),
            width: dot_rect.width + 4,
            height: dot_rect.height + 4,
        },
        COLOR_ACCENT_MINT,
        72,
        9,
    );
    canvas.fill_rounded_rect_alpha(dot_rect, COLOR_ACCENT_MINT, 255, 7);
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: dot_rect.x + 3,
            y: dot_rect.y + 3,
            width: dot_size - 6,
            height: dot_size - 6,
        },
        COLOR_PANEL_HIGHLIGHT,
        160,
        4,
    );

    font::draw_text(
        canvas,
        brand_rect.x + dot_size + 10,
        brand_rect.y + (brand_rect.height.saturating_sub(20)) / 2,
        "RustOS",
        TextStyle::ui_large(COLOR_TEXT_PRIMARY),
    );
}

pub(super) fn draw_status_block(
    canvas: &mut SurfaceCanvas<'_>,
    topbar: Rect,
    launcher_count: usize,
) {
    let status_width = TOPBAR_STATUS_WIDTH.min(topbar.width.saturating_sub(24));
    let status_rect = Rect {
        x: topbar
            .x
            .saturating_add(topbar.width)
            .saturating_sub(status_width + TOPBAR_INNER_PADDING_X),
        y: topbar.y + 10,
        width: status_width,
        height: topbar.height.saturating_sub(20),
    };
    if status_rect.width < 120 {
        return;
    }

    canvas.fill_rounded_rect_alpha(status_rect, COLOR_GLASS_DEEP, 130, 12);
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: status_rect.x + 1,
            y: status_rect.y,
            width: status_rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_PANEL_HIGHLIGHT,
        90,
        1,
    );

    let label_x = status_rect.x + 14;
    let label_y = status_rect.y + (status_rect.height.saturating_sub(16)) / 2;
    let text = if launcher_count == 0 {
        "Wayland Ready"
    } else {
        "Wayland Launcher"
    };
    font::draw_text_clipped(
        canvas,
        label_x,
        label_y,
        text,
        status_rect.width.saturating_sub(80),
        TextStyle::ui_small(COLOR_TEXT_DIM),
    );

    let count_label = format!("{launcher_count} apps");
    let count_x = status_rect
        .x
        .saturating_add(status_rect.width)
        .saturating_sub(58);
    font::draw_text_clipped(
        canvas,
        count_x,
        label_y,
        count_label.as_str(),
        50,
        TextStyle::ui_small(COLOR_ACCENT_MINT),
    );
}

pub(super) fn draw_launcher_icon(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str) {
    if rect.is_empty() {
        return;
    }
    draw_app_icon(canvas, rect, title, 255);
}

pub(super) fn draw_dock_slot(
    canvas: &mut SurfaceCanvas<'_>,
    rect: Rect,
    title: &str,
    focused: bool,
    minimized: bool,
) {
    if rect.is_empty() {
        return;
    }

    let alpha = if minimized { 150 } else { 255 };
    draw_app_icon(canvas, rect, title, alpha);

    let indicator_y = rect.y.saturating_add(rect.height).saturating_add(4);
    if focused {
        let dot_width = 14;
        let dot_x = rect.x + (rect.width.saturating_sub(dot_width)) / 2;
        canvas.fill_rounded_rect_alpha(
            Rect {
                x: dot_x,
                y: indicator_y,
                width: dot_width,
                height: 3,
            },
            COLOR_ACCENT_MINT,
            255,
            1,
        );
    } else if !minimized {
        let dot_width = 4;
        let dot_x = rect.x + (rect.width.saturating_sub(dot_width)) / 2;
        canvas.fill_rounded_rect_alpha(
            Rect {
                x: dot_x,
                y: indicator_y,
                width: dot_width,
                height: 3,
            },
            COLOR_TEXT_DIM,
            220,
            1,
        );
    }
}

// ---- window chrome ----

pub(super) fn draw_console_window(
    canvas: &mut SurfaceCanvas<'_>,
    window: &mut ConsoleWindow,
    focused: bool,
) {
    rebuild_console_window_surface(window, focused);
    draw_window_shadow(canvas, window.frame);
    canvas.draw_surface(
        &window.surface_cache.pixels,
        window.surface_cache.width,
        window.surface_cache.height,
        window.surface_cache.width,
        window.frame.x,
        window.frame.y,
    );
}

pub(super) fn draw_wayland_window(
    canvas: &mut SurfaceCanvas<'_>,
    window: &WaylandWindowSnapshot,
    focused: bool,
) {
    if window.width == 0 || window.height == 0 {
        return;
    }

    let outer = wayland_window_outer_rect(window);
    let client_rect = wayland_window_client_rect(outer);
    let title = if window.title.is_empty() {
        "Wayland App"
    } else {
        window.title.as_str()
    };

    draw_window_shadow(canvas, outer);
    paint_window_chrome(canvas, outer, client_rect, title, focused);

    // The client may have committed a buffer larger than what the desktop
    // can hold (e.g. wayclick hardcodes 800×520 and ignores our configure).
    // Cap the blit at the clamped client rectangle so it can never spill
    // into the topbar or dock.
    let blit_w = window.width.min(client_rect.width);
    let blit_h = window.height.min(client_rect.height);
    if blit_w == 0 || blit_h == 0 {
        return;
    }
    canvas.draw_surface(
        window.pixels.as_slice(),
        blit_w,
        blit_h,
        window.stride_pixels,
        client_rect.x,
        client_rect.y,
    );
}

pub(crate) fn rebuild_console_window_surface(window: &mut ConsoleWindow, focused: bool) {
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
    // Clear the cache so the new rounded outer doesn't expose stale pixels
    // in the corner triangles.
    canvas.fill_rect(outer, COLOR_BG_DEEP);

    paint_window_chrome(&mut canvas, outer, client_rect, window.title.as_str(), focused);

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

pub(super) fn paint_window_chrome(
    canvas: &mut SurfaceCanvas<'_>,
    outer: Rect,
    client_rect: Rect,
    title: &str,
    focused: bool,
) {
    canvas.fill_rounded_rect_alpha(outer, COLOR_GLASS_DARK, 250, WINDOW_RADIUS);

    let title_rect = window_title_bar_rect(outer);
    paint_title_band(canvas, title_rect, focused);

    let stripe_color = if focused {
        COLOR_ACCENT_MINT
    } else {
        COLOR_BORDER_SUBTLE
    };
    canvas.fill_rect_alpha(
        Rect {
            x: outer.x + WINDOW_RADIUS / 2,
            y: title_rect.y + title_rect.height.saturating_sub(1),
            width: outer.width.saturating_sub(WINDOW_RADIUS),
            height: 1,
        },
        stripe_color,
        220,
    );

    let title_color = if focused {
        COLOR_TEXT_PRIMARY
    } else {
        COLOR_TEXT_DIM
    };
    let style = TextStyle::ui_medium(title_color);
    let button_cluster_width = window_button_cluster_width() + 8;
    font::draw_text_clipped(
        canvas,
        title_rect.x + 16,
        title_rect.y + (title_rect.height.saturating_sub(20)) / 2,
        title,
        title_rect.width.saturating_sub(button_cluster_width + 16),
        style,
    );
    draw_window_controls(canvas, outer);

    // Pre-fill so the area shows the right backdrop before the terminal /
    // Wayland buffer paints over it.
    canvas.fill_rect(client_rect, COLOR_GLASS_DEEP);
}

fn paint_title_band(canvas: &mut SurfaceCanvas<'_>, title_rect: Rect, focused: bool) {
    if title_rect.is_empty() {
        return;
    }
    let tint_alpha = if focused { 36 } else { 22 };
    canvas.fill_rect_alpha(title_rect, COLOR_PANEL_HIGHLIGHT, tint_alpha);
    canvas.fill_rect_alpha(
        Rect {
            x: title_rect.x + WINDOW_RADIUS / 2,
            y: title_rect.y + 1,
            width: title_rect.width.saturating_sub(WINDOW_RADIUS),
            height: 1,
        },
        COLOR_PANEL_HIGHLIGHT,
        if focused { 110 } else { 70 },
    );
}

fn draw_window_controls(canvas: &mut SurfaceCanvas<'_>, outer: Rect) {
    let minimize_rect = window_minimize_button_rect(outer);
    let maximize_rect = window_maximize_button_rect(outer);
    let close_rect = window_close_button_rect(outer);
    draw_traffic_light(canvas, minimize_rect, COLOR_ACCENT_GOLD);
    draw_traffic_light(canvas, maximize_rect, COLOR_ACCENT_MINT);
    draw_traffic_light(canvas, close_rect, COLOR_ACCENT_PEACH);
}

fn draw_traffic_light(canvas: &mut SurfaceCanvas<'_>, rect: Rect, accent: u32) {
    if rect.is_empty() {
        return;
    }
    let radius = rect.width.min(rect.height) / 2;
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: rect.x.saturating_sub(1),
            y: rect.y.saturating_sub(1),
            width: rect.width + 2,
            height: rect.height + 2,
        },
        accent,
        56,
        radius + 1,
    );
    canvas.fill_rounded_rect_alpha(rect, accent, 235, radius);
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: rect.x + 2,
            y: rect.y + 2,
            width: rect.width.saturating_sub(4),
            height: rect.height.saturating_sub(4) / 2,
        },
        COLOR_PANEL_HIGHLIGHT,
        90,
        radius / 2,
    );
}

pub(super) fn draw_window_shadow(canvas: &mut SurfaceCanvas<'_>, rect: Rect) {
    // Fewer, larger steps with a steeper falloff: visually similar to a
    // 10-step shadow but ~40 % cheaper per repaint — which directly helps
    // dragging windows at 60 fps.
    for step in 0..WINDOW_SHADOW_STEPS {
        let alpha = 52u32.saturating_sub((step as u32).saturating_mul(7)) as u8;
        if alpha == 0 {
            break;
        }
        let inset = step + 1;
        let shadow = Rect {
            x: rect.x.saturating_sub(inset),
            y: rect.y.saturating_sub(inset / 2).saturating_add(3),
            width: rect.width.saturating_add(inset * 2),
            height: rect.height.saturating_add(inset + 2),
        };
        canvas.fill_rounded_rect_alpha(shadow, COLOR_SHADOW, alpha, WINDOW_RADIUS + step);
    }
}
