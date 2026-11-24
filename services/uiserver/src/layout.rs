//! Shared geometry primitives used by both the renderer and the Wayland
//! compositor. Anything that decides "where does this rectangle live on the
//! desktop" belongs here so layout drift between the two modules is
//! impossible.

use crate::canvas::Rect;
use crate::font;
use crate::ui_assets;
use crate::ui_config::FontRole;
use crate::ui_scale;

// ---- top-level chrome & layout (configured via `ui.conf` `[layout]` in logical
// 100% design units and scaled dynamically via `ui_scale`) ----

// The desktop has no top bar: the home screen and windows use the whole top of
// the display. Only the dock (taskbar) rail floats above the desktop.

/// Grid scales to try, largest first, as a percentage of the full size.
const HOME_GRID_SCALES: [usize; 7] = [100, 85, 70, 60, 50, 40, 30];

pub(crate) const WINDOW_SHADOW_STEPS: usize = 6;
pub(crate) const WINDOW_CASCADE_SLOTS: usize = 6;

const BASE_HOME_CLOCK_MIN_WIDTH: usize = 240;
const BASE_RESIZE_BORDER_PX: usize = 6;
const BASE_RESIZE_CORNER_PX: usize = 12;
const BASE_WAYLAND_TITLE_VISIBLE_PX: usize = 120;

fn cfg() -> crate::ui_config::UiLayoutConfig {
    ui_assets::layout()
}

pub(crate) fn taskbar_margin_bottom() -> usize { ui_scale::px(cfg().taskbar_margin_bottom) }
pub(crate) fn taskbar_rail_height() -> usize { ui_scale::px(cfg().taskbar_rail_height) }
pub(crate) fn taskbar_height() -> usize { taskbar_margin_bottom() + taskbar_rail_height() }
pub(crate) fn rail_side_margin() -> usize { ui_scale::px(cfg().rail_side_margin) }
pub(crate) fn rail_radius() -> usize { ui_scale::px(cfg().rail_radius) }
pub(crate) fn desktop_margin_x() -> usize { ui_scale::px(cfg().desktop_margin_x) }
pub(crate) fn desktop_margin_y() -> usize { ui_scale::px(cfg().desktop_margin_y) }
pub(crate) fn home_icon_size() -> usize { ui_scale::px(cfg().home_icon_size) }
pub(crate) fn home_cell_width() -> usize { ui_scale::px(cfg().home_cell_width) }
pub(crate) fn home_grid_top_padding() -> usize { ui_scale::px(cfg().home_grid_top_padding) }
pub(crate) fn home_label_gap() -> usize { ui_scale::px(cfg().home_label_gap) }
pub(crate) fn home_label_height() -> usize {
    font::line_height(FontRole::UiMedium).max(ui_scale::px(12))
}
pub(crate) fn home_cell_bottom_padding() -> usize { ui_scale::px(cfg().home_cell_bottom_padding) }
pub(crate) fn home_clock_top_padding() -> usize { ui_scale::px(cfg().home_clock_top_padding) }
pub(crate) fn home_clock_time_height() -> usize {
    font::line_height(FontRole::Clock).max(ui_scale::px(24))
}
pub(crate) fn home_clock_date_gap() -> usize { ui_scale::px(cfg().home_clock_date_gap) }
pub(crate) fn home_clock_date_height() -> usize {
    font::line_height(FontRole::UiLarge).max(ui_scale::px(12))
}
pub(crate) fn home_clock_bottom_gap() -> usize { ui_scale::px(cfg().home_clock_bottom_gap) }
pub(crate) fn home_clock_block_height() -> usize {
    home_clock_top_padding()
        + home_clock_time_height()
        + home_clock_date_gap()
        + home_clock_date_height()
        + home_clock_bottom_gap()
}
fn home_clock_min_app_rows_height() -> usize {
    (home_icon_size() + home_label_gap() + home_label_height()).max(ui_scale::px(96))
}
pub(crate) fn dock_icon_size() -> usize { ui_scale::px(cfg().dock_icon_size) }
pub(crate) fn dock_icon_gap() -> usize { ui_scale::px(cfg().dock_icon_gap) }
pub(crate) fn dock_inner_padding_x() -> usize { ui_scale::px(cfg().dock_inner_padding_x) }
pub(crate) fn window_radius() -> usize { ui_scale::px(cfg().window_radius) }
pub(crate) fn window_border() -> usize { ui_scale::px(cfg().window_border) }
pub(crate) fn window_title_height() -> usize { ui_scale::px(cfg().window_title_height) }
pub(crate) fn window_padding_x() -> usize { ui_scale::px(cfg().window_padding_x) }
pub(crate) fn window_padding_y() -> usize { ui_scale::px(cfg().window_padding_y) }
pub(crate) fn window_button_height() -> usize { ui_scale::px(cfg().window_button_height) }
pub(crate) fn window_button_width() -> usize { ui_scale::px(cfg().window_button_width) }
pub(crate) fn window_close_button_width() -> usize { ui_scale::px(cfg().window_close_button_width) }
pub(crate) fn window_button_margin_right() -> usize { ui_scale::px(cfg().window_button_margin_right) }
pub(crate) fn window_button_margin_top() -> usize { ui_scale::px(cfg().window_button_margin_top) }
pub(crate) fn default_window_width() -> usize { ui_scale::px(cfg().default_window_width) }
pub(crate) fn default_window_height() -> usize { ui_scale::px(cfg().default_window_height) }
pub(crate) fn min_window_width() -> usize { ui_scale::px(cfg().min_window_width) }
pub(crate) fn min_window_height() -> usize { ui_scale::px(cfg().min_window_height) }
pub(crate) fn window_cascade_x() -> usize { ui_scale::px(cfg().window_cascade_x) }
pub(crate) fn window_cascade_y() -> usize { ui_scale::px(cfg().window_cascade_y) }
pub(crate) fn terminal_padding_x() -> usize { ui_scale::px(cfg().terminal_padding_x) }
pub(crate) fn terminal_padding_y() -> usize { ui_scale::px(cfg().terminal_padding_y) }
fn resize_border_px() -> usize { ui_scale::px(BASE_RESIZE_BORDER_PX) }
fn resize_corner_px() -> usize { ui_scale::px(BASE_RESIZE_CORNER_PX) }
pub(crate) fn wayland_title_visible_px() -> usize { ui_scale::px(BASE_WAYLAND_TITLE_VISIBLE_PX) }

// ---- desktop & rail geometry ----

pub(crate) fn taskbar_rail_rect(width: usize, height: usize) -> Rect {
    let side = rail_side_margin();
    let rail_h = taskbar_rail_height();
    let margin_bottom = taskbar_margin_bottom();
    Rect {
        x: side,
        y: height.saturating_sub(margin_bottom + rail_h),
        width: width.saturating_sub(side * 2),
        height: rail_h,
    }
}

pub(crate) fn desktop_bounds(width: u32, height: u32) -> Rect {
    let mx = desktop_margin_x();
    let my = desktop_margin_y();
    Rect {
        x: mx,
        y: my,
        width: (width as usize).saturating_sub(mx * 2),
        height: (height as usize).saturating_sub(taskbar_height() + my * 2),
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

/// Sizes and positions for one home-screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HomeGridMetrics {
    cell_width: usize,
    cell_height: usize,
    icon_size: usize,
    label_gap: usize,
    top_padding: usize,
    columns: usize,
    rows: usize,
}

impl HomeGridMetrics {
    fn capacity(&self) -> usize {
        self.columns * self.rows
    }
}

fn home_clock_visible(bounds: Rect) -> bool {
    bounds.width >= ui_scale::px(BASE_HOME_CLOCK_MIN_WIDTH)
        && bounds.height >= home_clock_block_height() + home_clock_min_app_rows_height()
}

fn home_grid_metrics_at(bounds: Rect, scale_percent: usize) -> Option<HomeGridMetrics> {
    let scaled = |value: usize| value * scale_percent / 100;
    let cell_width = scaled(home_cell_width());
    let icon_size = scaled(home_icon_size());
    let label_gap = scaled(home_label_gap()).max(ui_scale::px(4));
    // The clock block is a fixed size, so the apps below it start at a fixed
    // offset whatever the grid scale; without the clock they sit near the top.
    let top_padding = if home_clock_visible(bounds) {
        home_clock_block_height()
    } else {
        scaled(home_grid_top_padding()).max(ui_scale::px(8))
    };
    let cell_height =
        icon_size + label_gap + home_label_height() + scaled(home_cell_bottom_padding());
    if cell_width == 0 || icon_size == 0 || bounds.width < cell_width {
        return None;
    }
    let columns = bounds.width / cell_width;
    let rows = bounds.height.saturating_sub(top_padding) / cell_height;
    (columns > 0 && rows > 0).then_some(HomeGridMetrics {
        cell_width,
        cell_height,
        icon_size,
        label_gap,
        top_padding,
        columns,
        rows,
    })
}

/// The largest grid that shows all `count` launchers. When even the smallest
/// grid cannot, the smallest is used and the surplus launchers are left off
/// the home screen rather than overlapping the dock.
fn home_grid_metrics(bounds: Rect, count: usize) -> Option<HomeGridMetrics> {
    let mut smallest = None;
    for scale in HOME_GRID_SCALES {
        let Some(metrics) = home_grid_metrics_at(bounds, scale) else {
            continue;
        };
        if metrics.capacity() >= count {
            return Some(metrics);
        }
        smallest = Some(metrics);
    }
    smallest
}

/// One launcher's home-screen geometry: the whole click target and the icon
/// and label inside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HomeCell {
    pub(crate) cell: Rect,
    pub(crate) icon: Rect,
    pub(crate) label: Rect,
}

/// The home-screen cell for launcher `index` of `count`, or `None` when it
/// does not fit inside the desktop above the dock. The grid is centered
/// horizontally so the home screen stays balanced.
pub(crate) fn home_cell(width: u32, height: u32, count: usize, index: usize) -> Option<HomeCell> {
    let bounds = desktop_bounds(width, height);
    let metrics = home_grid_metrics(bounds, count)?;
    if index >= metrics.capacity() {
        return None;
    }
    let left = bounds.x + (bounds.width - metrics.columns * metrics.cell_width) / 2;
    let cell = Rect {
        x: left + (index % metrics.columns) * metrics.cell_width,
        y: bounds.y + metrics.top_padding + (index / metrics.columns) * metrics.cell_height,
        width: metrics.cell_width,
        height: metrics.cell_height,
    };
    let icon = Rect {
        x: cell.x + (cell.width - metrics.icon_size) / 2,
        y: cell.y,
        width: metrics.icon_size,
        height: metrics.icon_size,
    };
    let label_pad = ui_scale::px(4);
    let label = Rect {
        x: cell.x + label_pad,
        y: icon.y + metrics.icon_size + metrics.label_gap,
        width: cell.width.saturating_sub(label_pad * 2),
        height: home_label_height(),
    };
    Some(HomeCell { cell, icon, label })
}

/// Where the home screen clock draws: the time and the date under it, each
/// spanning the desktop width so the text can be centered. `None` when the
/// display is too small to show a clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HomeClock {
    pub(crate) time: Rect,
    pub(crate) date: Rect,
}

pub(crate) fn home_clock(width: u32, height: u32) -> Option<HomeClock> {
    let bounds = desktop_bounds(width, height);
    if !home_clock_visible(bounds) {
        return None;
    }
    let time = Rect {
        x: bounds.x,
        y: bounds.y + home_clock_top_padding(),
        width: bounds.width,
        height: home_clock_time_height(),
    };
    let date = Rect {
        x: bounds.x,
        y: time.y + time.height + home_clock_date_gap(),
        width: bounds.width,
        height: home_clock_date_height(),
    };
    Some(HomeClock { time, date })
}

/// What a clock change repaints: the whole clock block at the top of the home
/// screen. Empty when there is no clock.
pub(crate) fn home_clock_dirty_rect(width: u32, height: u32) -> Rect {
    let bounds = desktop_bounds(width, height);
    if !home_clock_visible(bounds) {
        return Rect::empty();
    }
    Rect {
        x: bounds.x,
        y: bounds.y,
        width: bounds.width,
        height: home_clock_block_height(),
    }
}

/// The click target of launcher `index`, or an empty rect when it is not shown.
pub(crate) fn launcher_button_rect(width: u32, height: u32, count: usize, index: usize) -> Rect {
    home_cell(width, height, count, index).map_or_else(Rect::empty, |home| home.cell)
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    let rail = taskbar_rail_rect(width as usize, height as usize);
    let icon_size = dock_icon_size();
    let pad_x = dock_inner_padding_x();
    let x = rail.x + pad_x + index * (icon_size + dock_icon_gap());
    let max_x = rail
        .x
        .saturating_add(rail.width)
        .saturating_sub(pad_x);
    if x + icon_size > max_x {
        return Rect::empty();
    }
    Rect {
        x,
        y: rail.y + (rail.height.saturating_sub(icon_size)) / 2,
        width: icon_size,
        height: icon_size,
    }
}

// ---- window chrome rects ----

pub(crate) fn window_title_bar_rect(outer: Rect) -> Rect {
    Rect {
        x: outer.x,
        y: outer.y,
        width: outer.width,
        height: window_title_height(),
    }
}

// The caption buttons form one joined group hanging from the top-right of the
// title bar: minimize, maximize, close. They are flush to each other so the
// group reads as a single glass control, and each is a generous click target.

pub(crate) fn window_close_button_rect(outer: Rect) -> Rect {
    let title_rect = window_title_bar_rect(outer);
    let close_w = window_close_button_width();
    let x = title_rect
        .x
        .saturating_add(title_rect.width)
        .saturating_sub(window_button_margin_right() + close_w);
    Rect {
        x,
        y: title_rect.y + window_button_margin_top(),
        width: close_w,
        height: window_button_height(),
    }
}

pub(crate) fn window_maximize_button_rect(outer: Rect) -> Rect {
    let close_rect = window_close_button_rect(outer);
    let btn_w = window_button_width();
    Rect {
        x: close_rect.x.saturating_sub(btn_w),
        y: close_rect.y,
        width: btn_w,
        height: window_button_height(),
    }
}

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    let max_rect = window_maximize_button_rect(outer);
    let btn_w = window_button_width();
    Rect {
        x: max_rect.x.saturating_sub(btn_w),
        y: max_rect.y,
        width: btn_w,
        height: window_button_height(),
    }
}

/// What part of a window a pointer position falls on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowChromeHit {
    Client,
    TitleBar,
    Minimize,
    Maximize,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResizeEdge {
    pub(crate) left: bool,
    pub(crate) right: bool,
    pub(crate) bottom: bool,
}

pub(crate) fn window_resize_hit(outer: Rect, x: u32, y: u32) -> Option<ResizeEdge> {
    if !outer.contains(x, y) {
        return None;
    }
    let px = x as usize;
    let py = y as usize;
    let title_bottom = outer.y + window_border() + window_title_height();
    if py < title_bottom {
        return None;
    }
    let right_edge = outer.x + outer.width;
    let bottom_edge = outer.y + outer.height;
    let resize_border = resize_border_px();
    let resize_corner = resize_corner_px();
    let near_bottom = py >= bottom_edge.saturating_sub(resize_border);
    let corner_y = py >= bottom_edge.saturating_sub(resize_corner);
    let left_margin = if corner_y { resize_corner } else { resize_border };
    let right_margin = if corner_y { resize_corner } else { resize_border };
    let left = px < outer.x + left_margin;
    let right = !left && px >= right_edge.saturating_sub(right_margin);
    let bottom = near_bottom || (corner_y && (left || right));
    (left || right || bottom).then_some(ResizeEdge {
        left,
        right,
        bottom,
    })
}

/// Compute a new window rectangle when dragging `edge` by `(dx, dy)` relative
/// to `start`, respecting `min_w`/`min_h` and desktop `bounds`.
pub(crate) fn resized_window_rect(
    start: Rect,
    edge: ResizeEdge,
    dx: i64,
    dy: i64,
    min_w: usize,
    min_h: usize,
    bounds: Rect,
) -> Rect {
    let start_left = start.x as i64;
    let start_right = (start.x + start.width) as i64;
    let start_top = start.y as i64;
    let start_bottom = (start.y + start.height) as i64;
    let bounds_left = bounds.x as i64;
    let bounds_right = (bounds.x + bounds.width) as i64;
    let bounds_bottom = (bounds.y + bounds.height) as i64;
    let min_w = min_w.min(bounds.width) as i64;
    let min_h = min_h.min(bounds.height) as i64;

    let (x, width) = if edge.left {
        let next_left = (start_left + dx).clamp(bounds_left, start_right - min_w);
        (next_left as usize, (start_right - next_left) as usize)
    } else if edge.right {
        let next_right = (start_right + dx).clamp(start_left + min_w, bounds_right);
        (start.x, (next_right - start_left) as usize)
    } else {
        (start.x, start.width)
    };

    let height = if edge.bottom {
        let next_bottom = (start_bottom + dy).clamp(start_top + min_h, bounds_bottom);
        (next_bottom - start_top) as usize
    } else {
        start.height
    };

    Rect {
        x,
        y: start.y,
        width,
        height,
    }
}

/// Classify a pointer position that is inside `outer`. The caption buttons win
/// over the title bar they sit in, so the rest of the title bar is the drag
/// handle and a button is never mistaken for a drag.
pub(crate) fn window_chrome_hit(outer: Rect, x: u32, y: u32) -> WindowChromeHit {
    if window_close_button_rect(outer).contains(x, y) {
        WindowChromeHit::Close
    } else if window_maximize_button_rect(outer).contains(x, y) {
        WindowChromeHit::Maximize
    } else if window_minimize_button_rect(outer).contains(x, y) {
        WindowChromeHit::Minimize
    } else if window_title_bar_rect(outer).contains(x, y) {
        WindowChromeHit::TitleBar
    } else {
        WindowChromeHit::Client
    }
}

pub(crate) fn window_button_cluster_width() -> usize {
    window_button_margin_right() + window_button_width() * 2 + window_close_button_width()
}

pub(crate) fn window_client_rect(outer: Rect) -> Rect {
    let border = window_border();
    let title_h = window_title_height();
    let pad_x = window_padding_x();
    let pad_y = window_padding_y();
    Rect {
        x: outer.x + border + pad_x,
        y: outer.y + border + title_h + pad_y,
        width: outer
            .width
            .saturating_sub(border * 2 + pad_x * 2),
        height: outer
            .height
            .saturating_sub(border * 2 + title_h + pad_y * 2),
    }
}

pub(crate) fn default_console_window_rect(width: u32, height: u32, index: usize) -> Rect {
    let bounds = desktop_bounds(width, height);
    let frame_width = default_window_width()
        .min(bounds.width)
        .max(bounds.width.min(min_window_width()));
    let frame_height = default_window_height()
        .min(bounds.height)
        .max(bounds.height.min(min_window_height()));
    let (x, y) = cascade_origin(bounds, index);

    clamp_window_rect(
        width,
        height,
        Rect {
            x,
            y,
            width: frame_width,
            height: frame_height,
        },
    )
}

/// Top-left corner of the `index`-th window in the shared cascade. Console and
/// Wayland windows draw from the same sequence: when each kind restarted at the
/// desktop origin, a freshly launched Wayland window (larger than a console
/// frame) was placed exactly over the consoles and hid every one of them
/// behind the focused window.
pub(crate) fn cascade_origin(bounds: Rect, index: usize) -> (usize, usize) {
    let step = index % WINDOW_CASCADE_SLOTS;
    (
        bounds.x + step * window_cascade_x(),
        bounds.y + step * window_cascade_y(),
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
    let border = window_border();
    let max_w = bounds.width.saturating_sub(border * 2);
    let max_h = bounds
        .height
        .saturating_sub(border * 2 + window_title_height());
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

/// Clamp a Wayland surface frame so the title bar stays grabbable.
pub(crate) fn clamp_wayland_frame(frame: Rect, display_width: u32, display_height: u32) -> Rect {
    let (max_w, max_h) = wayland_max_client_size(display_width, display_height);
    let width = frame.width.min(max_w).max(1);
    let height = frame.height.min(max_h).max(1);
    let bounds = desktop_bounds(display_width, display_height);
    let border = window_border();
    let title_h = window_title_height();
    let outer_w = width.saturating_add(border * 2);
    let outer_h = height.saturating_add(title_h + border * 2);

    // Horizontal: keep at least `wayland_title_visible_px()` of chrome inside
    // the desktop on either side.
    let min_visible = wayland_title_visible_px().min(outer_w);
    let min_x = bounds.x.saturating_add(min_visible).saturating_sub(outer_w);
    let max_x = bounds
        .x
        .saturating_add(bounds.width)
        .saturating_sub(min_visible)
        .max(min_x);

    // Vertical: title bar must stay below the topbar and above the dock.
    let min_y = bounds.y;
    let title_strip = title_h.saturating_add(border);
    let max_y = bounds
        .y
        .saturating_add(bounds.height)
        .saturating_sub(title_strip.min(outer_h))
        .max(min_y);

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
    let border = window_border();
    Rect {
        x: frame_x,
        y: frame_y,
        width: client_width.saturating_add(border * 2),
        height: client_height.saturating_add(window_title_height() + border * 2),
    }
}

pub(crate) fn wayland_client_rect(outer: Rect) -> Rect {
    let border = window_border();
    let title_h = window_title_height();
    Rect {
        x: outer.x + border,
        y: outer.y + border + title_h,
        width: outer.width.saturating_sub(border * 2),
        height: outer
            .height
            .saturating_sub(border * 2 + title_h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cascade_never_stacks_a_window_exactly_over_its_predecessor() {
        let bounds = desktop_bounds(2560, 1440);
        let mut previous = cascade_origin(bounds, 0);
        for index in 1..WINDOW_CASCADE_SLOTS * 2 {
            let origin = cascade_origin(bounds, index);
            if index % WINDOW_CASCADE_SLOTS != 0 {
                assert!(
                    origin.0 > previous.0 && origin.1 > previous.1,
                    "index {index}"
                );
            }
            previous = origin;
        }
        // Console windows take the same sequence: the third console sits at
        // the third slot, not at the origin.
        let third = default_console_window_rect(2560, 1440, 2);
        assert_eq!((third.x, third.y), cascade_origin(bounds, 2));
    }

    #[test]
    fn clamping_a_wayland_frame_on_a_tiny_desktop_never_panics() {
        for (width, height) in [
            (0_u32, 0_u32),
            (60, 100),
            (80, 150),
            (200, 160),
            (1280, 720),
        ] {
            let frame = clamp_wayland_frame(
                Rect {
                    x: usize::MAX / 2,
                    y: usize::MAX / 2,
                    width: 800,
                    height: 600,
                },
                width,
                height,
            );
            assert!(frame.width >= 1 && frame.height >= 1);
        }
    }

    fn home_cells(width: u32, height: u32, count: usize) -> Vec<HomeCell> {
        (0..count)
            .map_while(|index| home_cell(width, height, count, index))
            .collect()
    }

    fn assert_grid_is_sound(width: u32, height: u32, cells: &[HomeCell]) {
        let bounds = desktop_bounds(width, height);
        let dock = taskbar_rail_rect(width as usize, height as usize);
        for home in cells {
            let cell = home.cell;
            assert!(cell.x >= bounds.x && cell.x + cell.width <= bounds.x + bounds.width);
            assert!(cell.y >= bounds.y && cell.y + cell.height <= bounds.y + bounds.height);
            assert!(cell.intersect(dock).is_empty());
            assert!(home.icon.y + home.icon.height <= home.label.y);
            assert!(home.label.y + home.label.height <= cell.y + cell.height);
            assert!(home.icon.x >= cell.x && home.icon.x + home.icon.width <= cell.x + cell.width);
        }
        for (index, home) in cells.iter().enumerate() {
            for other in cells.iter().skip(index + 1) {
                assert!(home.cell.intersect(other.cell).is_empty());
            }
        }
    }

    #[test]
    fn home_grid_is_centered_and_stays_between_the_rails() {
        let (width, height) = (ui_scale::px(1600) as u32, ui_scale::px(900) as u32);
        let bounds = desktop_bounds(width, height);
        let cells = home_cells(width, height, 27);
        assert_eq!(
            cells.len(),
            27,
            "one full-size home page fits under the clock"
        );
        assert_eq!(
            cells[0].icon.width, home_icon_size(),
            "a small list keeps full size"
        );
        assert_grid_is_sound(width, height, &cells);

        let first_row: Vec<_> = cells
            .iter()
            .take_while(|home| home.cell.y == cells[0].cell.y)
            .collect();
        let left_gap = first_row[0].cell.x - bounds.x;
        let last = first_row[first_row.len() - 1].cell;
        let right_gap = (bounds.x + bounds.width) - (last.x + last.width);
        assert!(
            left_gap.abs_diff(right_gap) <= 1,
            "grid is horizontally centered"
        );
    }

    #[test]
    fn a_long_launcher_list_shrinks_the_grid_instead_of_dropping_launchers() {
        let (width, height) = (ui_scale::px(1600) as u32, ui_scale::px(900) as u32);
        let full_capacity = home_grid_metrics_at(desktop_bounds(width, height), 100)
            .expect("full-size grid fits")
            .capacity();
        for count in [
            full_capacity + 1,
            full_capacity + 12,
            full_capacity * 2,
            full_capacity * 3,
        ] {
            let cells = home_cells(width, height, count);
            assert_eq!(cells.len(), count, "all {count} launchers stay visible");
            assert!(cells[0].icon.width < home_icon_size(), "the grid scaled down");
            assert!(cells[0].icon.width >= 8, "icons stay usable");
            assert_grid_is_sound(width, height, &cells);
        }
    }

    #[test]
    fn launchers_beyond_the_smallest_grid_are_left_off_without_overlap() {
        let (width, height) = (1600_u32, 900_u32);
        let count = 100_000;
        let cells = home_cells(width, height, count);
        assert!(!cells.is_empty() && cells.len() < count);
        assert_grid_is_sound(width, height, &cells);
        assert!(launcher_button_rect(width, height, count, count - 1).is_empty());
    }

    #[test]
    fn a_display_too_small_for_any_grid_has_no_cells() {
        // A narrow display still gets a scaled-down grid...
        let narrow = home_cell(home_cell_width() as u32, 900, 1, 0).expect("scaled grid fits");
        assert!(narrow.icon.width < home_icon_size());
        // ...but one narrower than the smallest scale has none.
        assert!(home_cell(100, 900, 1, 0).is_none());
        assert!(home_cell(1600, 120, 1, 0).is_none());
        assert!(home_cell(0, 0, 1, 0).is_none());
        assert!(launcher_button_rect(0, 0, 3, 0).is_empty());
    }

    #[test]
    fn an_empty_launcher_list_has_no_cells() {
        assert!(home_cells(1600, 900, 0).is_empty());
    }

    #[test]
    fn caption_buttons_form_a_joined_group_at_the_top_right_of_the_title_bar() {
        for (width, height) in [(360_usize, 240_usize), (720, 460), (1528, 700)] {
            let outer = Rect {
                x: 40,
                y: 60,
                width,
                height,
            };
            let title = window_title_bar_rect(outer);
            let minimize = window_minimize_button_rect(outer);
            let maximize = window_maximize_button_rect(outer);
            let close = window_close_button_rect(outer);

            assert_eq!(
                minimize.x + minimize.width,
                maximize.x,
                "flush to each other"
            );
            assert_eq!(maximize.x + maximize.width, close.x, "flush to each other");
            assert_eq!(
                title.x + title.width - (close.x + close.width),
                window_button_margin_right(),
                "the group ends at the right margin"
            );
            for button in [minimize, maximize, close] {
                assert!(button.y >= title.y && button.y + button.height <= title.y + title.height);
                assert!(button.x >= title.x);
                assert_eq!(button.height, window_button_height());
            }
            assert!(close.width > maximize.width, "close is the wider target");
            assert_eq!(
                title.x + title.width - minimize.x,
                window_button_cluster_width(),
                "title text is clipped by exactly the group width"
            );
            assert!(
                minimize.intersect(maximize).is_empty() && maximize.intersect(close).is_empty()
            );
        }
    }

    #[test]
    fn every_caption_button_and_the_title_bar_are_hit_where_they_are_drawn() {
        for outer in [
            Rect {
                x: 40,
                y: 60,
                width: 720,
                height: 400,
            },
            Rect {
                x: 0,
                y: 0,
                width: 360,
                height: 240,
            },
            Rect {
                x: 36,
                y: 24,
                width: 1528,
                height: 774,
            },
        ] {
            let center = |rect: Rect| {
                (
                    (rect.x + rect.width / 2) as u32,
                    (rect.y + rect.height / 2) as u32,
                )
            };
            let (x, y) = center(window_minimize_button_rect(outer));
            assert_eq!(window_chrome_hit(outer, x, y), WindowChromeHit::Minimize);
            let (x, y) = center(window_maximize_button_rect(outer));
            assert_eq!(window_chrome_hit(outer, x, y), WindowChromeHit::Maximize);
            let (x, y) = center(window_close_button_rect(outer));
            assert_eq!(window_chrome_hit(outer, x, y), WindowChromeHit::Close);

            // The left of the title bar is the drag handle; below it is the client.
            let title = window_title_bar_rect(outer);
            assert_eq!(
                window_chrome_hit(outer, (title.x + 10) as u32, (title.y + 10) as u32),
                WindowChromeHit::TitleBar
            );
            assert_eq!(
                window_chrome_hit(
                    outer,
                    (outer.x + outer.width / 2) as u32,
                    (title.y + title.height + 20) as u32
                ),
                WindowChromeHit::Client
            );
        }
    }

    #[test]
    fn every_pixel_of_the_title_bar_is_exactly_one_chrome_part() {
        let outer = Rect {
            x: 40,
            y: 60,
            width: 520,
            height: 300,
        };
        let title = window_title_bar_rect(outer);
        let (mut buttons, mut handle) = (0_usize, 0_usize);
        for y in title.y..title.y + title.height {
            for x in title.x..title.x + title.width {
                match window_chrome_hit(outer, x as u32, y as u32) {
                    WindowChromeHit::Client => panic!("({x},{y}) inside the title bar is client"),
                    WindowChromeHit::TitleBar => handle += 1,
                    _ => buttons += 1,
                }
            }
        }
        let group = window_button_cluster_width() - window_button_margin_right();
        assert_eq!(
            buttons,
            group * window_button_height(),
            "buttons are the drawn group"
        );
        assert!(handle > buttons, "most of the title bar still drags");
    }

    #[test]
    fn maximizing_fills_the_desktop_and_restoring_stays_inside_it() {
        let (width, height) = (1600_u32, 900_u32);
        let bounds = desktop_bounds(width, height);
        let maximized = maximized_window_rect(width, height);
        assert_eq!(maximized, bounds);
        assert_eq!(clamp_window_rect(width, height, maximized), maximized);

        // A window dragged partly off-screen before maximizing is brought back.
        let saved = Rect {
            x: bounds.x + bounds.width + 200,
            y: 5,
            width: 720,
            height: 460,
        };
        let restored = clamp_window_rect(width, height, saved);
        assert!(restored.x + restored.width <= bounds.x + bounds.width);
        assert!(restored.y >= bounds.y);
        assert_eq!((restored.width, restored.height), (720, 460));
    }

    #[test]
    fn the_clock_sits_above_the_app_grid_and_never_overlaps_it() {
        let (width, height) = (ui_scale::px(1600) as u32, ui_scale::px(900) as u32);
        let bounds = desktop_bounds(width, height);
        let clock = home_clock(width, height).expect("a 1600x900 display shows a clock");
        assert_eq!(clock.time.y - bounds.y, home_clock_top_padding());
        assert!(clock.time.y >= bounds.y && clock.date.y >= clock.time.y + clock.time.height);
        assert_eq!((clock.time.x, clock.time.width), (bounds.x, bounds.width));

        for count in [1_usize, 20, 36, 90] {
            let cells = home_cells(width, height, count);
            assert!(!cells.is_empty());
            assert_eq!(
                cells[0].cell.y - (clock.date.y + clock.date.height),
                home_clock_bottom_gap()
            );
            for home in &cells {
                assert!(
                    home.cell.y >= clock.date.y + clock.date.height + home_clock_bottom_gap(),
                    "apps start below the clock block padding"
                );
            }
        }
        let block = home_clock_dirty_rect(width, height);
        assert_eq!(block.height, home_clock_block_height());
        assert!(home_cells(width, height, 90)[0].cell.y >= block.y + block.height);
    }

    #[test]
    fn a_display_too_small_for_the_clock_still_shows_its_apps() {
        // Short: no room for the clock plus two rows of apps.
        let height = (home_clock_block_height() + home_clock_min_app_rows_height())
            as u32
            - 1
            + (desktop_margin_y() * 2 + taskbar_height()) as u32;
        assert!(home_clock(1600, height).is_none());
        assert!(home_clock_dirty_rect(1600, height).is_empty());
        assert!(
            home_cell(1600, height, 4, 0).is_some(),
            "apps fill the desktop instead"
        );
        // Narrow.
        assert!(home_clock(200, 900).is_none());
    }

    #[test]
    fn resize_hit_and_rect_respect_titlebar_corners_and_bounds() {
        let outer = Rect {
            x: 100,
            y: 80,
            width: 800,
            height: 600,
        };
        assert_eq!(window_resize_hit(outer, 102, 84), None);
        assert_eq!(
            window_resize_hit(outer, 102, 300),
            Some(ResizeEdge {
                left: true,
                right: false,
                bottom: false,
            })
        );
        assert_eq!(
            window_resize_hit(outer, 895, 675),
            Some(ResizeEdge {
                left: false,
                right: true,
                bottom: true,
            })
        );
        assert_eq!(window_resize_hit(outer, 400, 300), None);

        let bounds = desktop_bounds(2880, 1800);
        let shrunk = resized_window_rect(
            outer,
            ResizeEdge {
                left: true,
                right: false,
                bottom: true,
            },
            500,
            -300,
            min_window_width(),
            min_window_height(),
            bounds,
        );
        assert_eq!(shrunk.width, min_window_width());
        assert_eq!(shrunk.height, min_window_height());
        assert_eq!(shrunk.x + shrunk.width, outer.x + outer.width);
    }

    #[test]
    fn hidpi_200_viewports_fit_clock_grid_rail_and_caption_buttons_without_overlap() {
        assert_eq!(ui_scale::UiScale::default_scale().percent(), 200);
        for (width, height) in [(2880_u32, 1800_u32), (1440, 900)] {
            let bounds = desktop_bounds(width, height);
            let rail = taskbar_rail_rect(width as usize, height as usize);
            assert!(rail.x + rail.width <= width as usize);
            assert!(rail.y + rail.height <= height as usize);
            assert!(bounds.y + bounds.height <= rail.y);

            let clock = home_clock(width, height).expect("clock must fit at HiDPI viewports");
            for rect in [clock.time, clock.date] {
                assert!(rect.x >= bounds.x && rect.x + rect.width <= bounds.x + bounds.width);
                assert!(rect.y >= bounds.y && rect.y + rect.height <= bounds.y + bounds.height);
            }
            assert!(clock.time.intersect(clock.date).is_empty());

            let cells = home_cells(width, height, 6);
            assert_eq!(cells.len(), 6);
            assert_grid_is_sound(width, height, &cells);
            for cell in &cells {
                assert!(cell.cell.y >= clock.date.y + clock.date.height);
            }

            let dock_slot = taskbar_slot_rect(width, height, 0);
            assert!(!dock_slot.is_empty());
            assert!(dock_slot.x >= rail.x && dock_slot.x + dock_slot.width <= rail.x + rail.width);
            assert!(dock_slot.y >= rail.y && dock_slot.y + dock_slot.height <= rail.y + rail.height);

            let win = Rect {
                x: bounds.x,
                y: bounds.y,
                width: default_window_width().min(bounds.width),
                height: default_window_height().min(bounds.height),
            };
            let title = window_title_bar_rect(win);
            let minimize = window_minimize_button_rect(win);
            let maximize = window_maximize_button_rect(win);
            let close = window_close_button_rect(win);
            assert!(minimize.x >= title.x && close.x + close.width <= title.x + title.width);
            assert!(minimize.intersect(maximize).is_empty());
            assert!(maximize.intersect(close).is_empty());
        }
    }
}
