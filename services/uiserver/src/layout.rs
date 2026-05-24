//! Shared geometry primitives used by both the renderer and the Wayland
//! compositor. Anything that decides "where does this rectangle live on the
//! desktop" belongs here so layout drift between the two modules is
//! impossible.

use crate::canvas::Rect;

// ---- top-level chrome ----

pub(crate) const TOPBAR_MARGIN_TOP: usize = 12;
pub(crate) const TOPBAR_RAIL_HEIGHT: usize = 48;
pub(crate) const TOPBAR_HEIGHT: usize = TOPBAR_MARGIN_TOP + TOPBAR_RAIL_HEIGHT;

pub(crate) const TASKBAR_MARGIN_BOTTOM: usize = 14;
pub(crate) const TASKBAR_RAIL_HEIGHT: usize = 64;
pub(crate) const TASKBAR_HEIGHT: usize = TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT;

pub(crate) const RAIL_SIDE_MARGIN: usize = 24;
pub(crate) const DESKTOP_MARGIN_X: usize = 36;
pub(crate) const DESKTOP_MARGIN_Y: usize = 24;

pub(crate) const TOPBAR_BRAND_WIDTH: usize = 132;
pub(crate) const TOPBAR_STATUS_WIDTH: usize = 200;
pub(crate) const TOPBAR_INNER_PADDING_X: usize = 18;

pub(crate) const LAUNCHER_ICON_SIZE: usize = 32;
pub(crate) const LAUNCHER_ICON_GAP: usize = 10;

pub(crate) const DOCK_ICON_SIZE: usize = 44;
pub(crate) const DOCK_ICON_GAP: usize = 12;
pub(crate) const DOCK_INNER_PADDING_X: usize = 18;

// ---- window chrome ----

pub(crate) const WINDOW_RADIUS: usize = 12;
pub(crate) const WINDOW_BORDER: usize = 1;
pub(crate) const WINDOW_TITLE_HEIGHT: usize = 36;
pub(crate) const WINDOW_PADDING_X: usize = 16;
pub(crate) const WINDOW_PADDING_Y: usize = 14;
pub(crate) const WINDOW_SHADOW_STEPS: usize = 6;

// Generous click targets so a clumsy tap on the X still registers as close.
pub(crate) const WINDOW_BUTTON_SIZE: usize = 18;
pub(crate) const WINDOW_BUTTON_GAP: usize = 10;
pub(crate) const WINDOW_BUTTON_MARGIN_RIGHT: usize = 14;

pub(crate) const DEFAULT_WINDOW_WIDTH: usize = 720;
pub(crate) const DEFAULT_WINDOW_HEIGHT: usize = 460;
pub(crate) const MIN_WINDOW_WIDTH: usize = 360;
pub(crate) const MIN_WINDOW_HEIGHT: usize = 240;
pub(crate) const WINDOW_CASCADE_X: usize = 36;
pub(crate) const WINDOW_CASCADE_Y: usize = 32;
pub(crate) const WINDOW_CASCADE_SLOTS: usize = 6;

// ---- desktop & rail geometry ----

pub(crate) fn topbar_rail_rect(width: usize) -> Rect {
    Rect {
        x: RAIL_SIDE_MARGIN,
        y: TOPBAR_MARGIN_TOP,
        width: width.saturating_sub(RAIL_SIDE_MARGIN * 2),
        height: TOPBAR_RAIL_HEIGHT,
    }
}

pub(crate) fn taskbar_rail_rect(width: usize, height: usize) -> Rect {
    Rect {
        x: RAIL_SIDE_MARGIN,
        y: height.saturating_sub(TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT),
        width: width.saturating_sub(RAIL_SIDE_MARGIN * 2),
        height: TASKBAR_RAIL_HEIGHT,
    }
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

pub(crate) fn clamp_window_rect(display_width: u32, display_height: u32, rect: Rect) -> Rect {
    let bounds = desktop_bounds(display_width, display_height);
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

pub(crate) fn maximized_window_rect(display_width: u32, display_height: u32) -> Rect {
    desktop_bounds(display_width, display_height)
}

// ---- launcher / dock slot rects ----

pub(crate) fn launcher_button_rect(width: u32, index: usize) -> Rect {
    let rail = topbar_rail_rect(width as usize);
    let launcher_x = rail.x + TOPBAR_INNER_PADDING_X + TOPBAR_BRAND_WIDTH + 12;
    let x = launcher_x + index * (LAUNCHER_ICON_SIZE + LAUNCHER_ICON_GAP);
    let max_x = rail
        .x
        .saturating_add(rail.width)
        .saturating_sub(TOPBAR_STATUS_WIDTH + TOPBAR_INNER_PADDING_X);
    if x + LAUNCHER_ICON_SIZE > max_x {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(LAUNCHER_ICON_SIZE)) / 2,
        width: LAUNCHER_ICON_SIZE,
        height: LAUNCHER_ICON_SIZE,
    }
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    let rail = taskbar_rail_rect(width as usize, height as usize);
    let x = rail.x + DOCK_INNER_PADDING_X + index * (DOCK_ICON_SIZE + DOCK_ICON_GAP);
    let max_x = rail
        .x
        .saturating_add(rail.width)
        .saturating_sub(DOCK_INNER_PADDING_X);
    if x + DOCK_ICON_SIZE > max_x {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(DOCK_ICON_SIZE)) / 2,
        width: DOCK_ICON_SIZE,
        height: DOCK_ICON_SIZE,
    }
}

// ---- window chrome rects ----

pub(crate) fn window_title_bar_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x,
        y: outer.y,
        width: outer.width,
        height: WINDOW_TITLE_HEIGHT,
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

pub(crate) fn window_maximize_button_rect(outer: Rect) -> Rect {
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

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    let max_rect = window_maximize_button_rect(outer);
    Rect {
        x: max_rect
            .x
            .saturating_sub(WINDOW_BUTTON_GAP + WINDOW_BUTTON_SIZE),
        y: max_rect.y,
        width: WINDOW_BUTTON_SIZE,
        height: WINDOW_BUTTON_SIZE,
    }
}

pub(crate) fn window_button_cluster_width() -> usize {
    WINDOW_BUTTON_MARGIN_RIGHT + WINDOW_BUTTON_SIZE * 3 + WINDOW_BUTTON_GAP * 2
}

pub(crate) fn window_client_rect(outer: Rect) -> Rect {
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

pub(crate) fn default_console_window_rect(width: u32, height: u32, index: usize) -> Rect {
    let bounds = desktop_bounds(width, height);
    let frame_width = DEFAULT_WINDOW_WIDTH
        .min(bounds.width)
        .max(bounds.width.min(MIN_WINDOW_WIDTH));
    let frame_height = DEFAULT_WINDOW_HEIGHT
        .min(bounds.height)
        .max(bounds.height.min(MIN_WINDOW_HEIGHT));
    let step = index % WINDOW_CASCADE_SLOTS;

    clamp_window_rect(
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

// ---- wayland window chrome ----
//
// Wayland clients give us an arbitrary buffer size. We treat the buffer
// dimensions as the *desired* content area but cap it to the available
// desktop region so a misbehaving client (e.g. wayclick ignoring our
// configure) cannot spill into the topbar or dock.

pub(crate) fn wayland_max_client_size(display_width: u32, display_height: u32) -> (usize, usize) {
    let bounds = desktop_bounds(display_width, display_height);
    let max_w = bounds.width.saturating_sub(WINDOW_BORDER * 2);
    let max_h = bounds
        .height
        .saturating_sub(WINDOW_BORDER * 2 + WINDOW_TITLE_HEIGHT);
    (max_w.max(1), max_h.max(1))
}

pub(crate) fn wayland_client_size_for_buffer(
    buffer_width: usize,
    buffer_height: usize,
    display_width: u32,
    display_height: u32,
) -> (usize, usize) {
    let (max_w, max_h) = wayland_max_client_size(display_width, display_height);
    (buffer_width.min(max_w), buffer_height.min(max_h))
}

/// How much of the title bar must remain inside the desktop region after
/// a clamp. We require enough horizontal overlap to keep the close button
/// reachable and full vertical visibility so the title stays clickable
/// for re-grab.
pub(crate) const WAYLAND_TITLE_VISIBLE_PX: usize = 120;

/// Clamp a Wayland surface frame so the title bar stays grabbable.
///
/// `frame.width` / `frame.height` are interpreted as the *client content*
/// size — the title bar and border are added on top. The content size is
/// capped to what the desktop can show so a misbehaving client (e.g.
/// wayclick ignoring our configure) cannot push the buffer over the
/// topbar or dock. The *position*, however, only needs to keep
/// `WAYLAND_TITLE_VISIBLE_PX` of the title bar inside the desktop — we
/// deliberately allow windows to be dragged partly off-screen so the
/// user has free range of motion even when the chrome size matches the
/// available area.
pub(crate) fn clamp_wayland_frame(
    frame: Rect,
    display_width: u32,
    display_height: u32,
) -> Rect {
    let (max_w, max_h) = wayland_max_client_size(display_width, display_height);
    let width = frame.width.min(max_w).max(1);
    let height = frame.height.min(max_h).max(1);
    let bounds = desktop_bounds(display_width, display_height);
    let outer_w = width.saturating_add(WINDOW_BORDER * 2);
    let outer_h = height.saturating_add(WINDOW_TITLE_HEIGHT + WINDOW_BORDER * 2);

    // Horizontal: keep at least `WAYLAND_TITLE_VISIBLE_PX` of chrome inside
    // the desktop on either side. Allows dragging off the left or right
    // edge until the title bar would disappear.
    let min_visible = WAYLAND_TITLE_VISIBLE_PX.min(outer_w);
    let min_x = bounds
        .x
        .saturating_add(min_visible)
        .saturating_sub(outer_w);
    let max_x = bounds
        .x
        .saturating_add(bounds.width)
        .saturating_sub(min_visible);

    // Vertical: title bar must stay below the topbar and above the dock
    // (otherwise the window can't be grabbed back). The body may extend
    // past the bottom margin.
    let min_y = bounds.y;
    let title_strip = WINDOW_TITLE_HEIGHT.saturating_add(WINDOW_BORDER);
    let max_y = bounds
        .y
        .saturating_add(bounds.height)
        .saturating_sub(title_strip.min(outer_h));

    Rect {
        x: frame.x.clamp(min_x, max_x),
        y: frame.y.clamp(min_y, max_y),
        width,
        height,
    }
}

pub(crate) fn wayland_outer_rect(
    frame_x: usize,
    frame_y: usize,
    client_width: usize,
    client_height: usize,
) -> Rect {
    Rect {
        x: frame_x,
        y: frame_y,
        width: client_width.saturating_add(WINDOW_BORDER * 2),
        height: client_height.saturating_add(WINDOW_TITLE_HEIGHT + WINDOW_BORDER * 2),
    }
}

pub(crate) fn wayland_client_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x + WINDOW_BORDER,
        y: outer.y + WINDOW_BORDER + WINDOW_TITLE_HEIGHT,
        width: outer.width.saturating_sub(WINDOW_BORDER * 2),
        height: outer
            .height
            .saturating_sub(WINDOW_BORDER * 2 + WINDOW_TITLE_HEIGHT),
    }
}
