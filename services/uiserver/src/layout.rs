//! Shared geometry primitives used by both the renderer and the Wayland
//! compositor. Anything that decides "where does this rectangle live on the
//! desktop" belongs here so layout drift between the two modules is
//! impossible.

use crate::canvas::Rect;

// ---- top-level chrome ----

// The desktop has no top bar: the home screen and windows use the whole top of
// the display. Only the dock (taskbar) rail floats above the desktop.

pub(crate) const TASKBAR_MARGIN_BOTTOM: usize = 14;
pub(crate) const TASKBAR_RAIL_HEIGHT: usize = 64;
pub(crate) const TASKBAR_HEIGHT: usize = TASKBAR_MARGIN_BOTTOM + TASKBAR_RAIL_HEIGHT;

pub(crate) const RAIL_SIDE_MARGIN: usize = 24;
pub(crate) const DESKTOP_MARGIN_X: usize = 36;
pub(crate) const DESKTOP_MARGIN_Y: usize = 24;

// ---- home screen grid ----
//
// Launcher programs live on the home screen as a tablet-style grid of large
// icons with a name label under each. The sizes below are the full-size
// design; when more launchers exist than a full-size grid can show, the whole
// grid scales down (see `home_grid_metrics`) so every launcher stays visible.

pub(crate) const HOME_ICON_SIZE: usize = 88;
pub(crate) const HOME_CELL_WIDTH: usize = 156;
pub(crate) const HOME_GRID_TOP_PADDING: usize = 28;
pub(crate) const HOME_LABEL_GAP: usize = 12;
pub(crate) const HOME_LABEL_HEIGHT: usize = 20;
/// Space under the label, so rows do not touch.
pub(crate) const HOME_CELL_BOTTOM_PADDING: usize = 24;
// The clock sits at the top of the home screen and the app grid starts below
// it, like a tablet. The time's height matches the clock face's line height.
pub(crate) const HOME_CLOCK_TOP_PADDING: usize = 16;
pub(crate) const HOME_CLOCK_TIME_HEIGHT: usize = 107;
pub(crate) const HOME_CLOCK_DATE_GAP: usize = 2;
pub(crate) const HOME_CLOCK_DATE_HEIGHT: usize = 22;
pub(crate) const HOME_CLOCK_BOTTOM_GAP: usize = 30;
pub(crate) const HOME_CLOCK_BLOCK_HEIGHT: usize = HOME_CLOCK_TOP_PADDING
    + HOME_CLOCK_TIME_HEIGHT
    + HOME_CLOCK_DATE_GAP
    + HOME_CLOCK_DATE_HEIGHT
    + HOME_CLOCK_BOTTOM_GAP;
/// The clock is only shown when the desktop is wide enough for it and tall
/// enough to leave room for at least two rows of apps under it.
const HOME_CLOCK_MIN_WIDTH: usize = 320;
const HOME_CLOCK_MIN_APP_ROWS_HEIGHT: usize = 2 * 60;

/// Grid scales to try, largest first, as a percentage of the full size.
const HOME_GRID_SCALES: [usize; 6] = [100, 85, 70, 60, 50, 40];

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
pub(crate) const WINDOW_BUTTON_HEIGHT: usize = 24;
pub(crate) const WINDOW_BUTTON_WIDTH: usize = 34;
pub(crate) const WINDOW_CLOSE_BUTTON_WIDTH: usize = 48;
pub(crate) const WINDOW_BUTTON_MARGIN_RIGHT: usize = 8;
pub(crate) const WINDOW_BUTTON_MARGIN_TOP: usize = 1;

pub(crate) const DEFAULT_WINDOW_WIDTH: usize = 720;
pub(crate) const DEFAULT_WINDOW_HEIGHT: usize = 460;
pub(crate) const MIN_WINDOW_WIDTH: usize = 360;
pub(crate) const MIN_WINDOW_HEIGHT: usize = 240;
pub(crate) const WINDOW_CASCADE_X: usize = 36;
pub(crate) const WINDOW_CASCADE_Y: usize = 32;
pub(crate) const WINDOW_CASCADE_SLOTS: usize = 6;

// ---- desktop & rail geometry ----

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
        y: DESKTOP_MARGIN_Y,
        width: (width as usize).saturating_sub(DESKTOP_MARGIN_X * 2),
        height: (height as usize).saturating_sub(TASKBAR_HEIGHT + DESKTOP_MARGIN_Y * 2),
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
    bounds.width >= HOME_CLOCK_MIN_WIDTH
        && bounds.height >= HOME_CLOCK_BLOCK_HEIGHT + HOME_CLOCK_MIN_APP_ROWS_HEIGHT
}

fn home_grid_metrics_at(bounds: Rect, scale_percent: usize) -> Option<HomeGridMetrics> {
    let scaled = |value: usize| value * scale_percent / 100;
    let cell_width = scaled(HOME_CELL_WIDTH);
    let icon_size = scaled(HOME_ICON_SIZE);
    let label_gap = scaled(HOME_LABEL_GAP).max(4);
    // The clock block is a fixed size, so the apps below it start at a fixed
    // offset whatever the grid scale; without the clock they sit near the top.
    let top_padding = if home_clock_visible(bounds) {
        HOME_CLOCK_BLOCK_HEIGHT
    } else {
        scaled(HOME_GRID_TOP_PADDING).max(8)
    };
    let cell_height = icon_size + label_gap + HOME_LABEL_HEIGHT + scaled(HOME_CELL_BOTTOM_PADDING);
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
    let label = Rect {
        x: cell.x + 4,
        y: icon.y + metrics.icon_size + metrics.label_gap,
        width: cell.width.saturating_sub(8),
        height: HOME_LABEL_HEIGHT,
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
        y: bounds.y + HOME_CLOCK_TOP_PADDING,
        width: bounds.width,
        height: HOME_CLOCK_TIME_HEIGHT,
    };
    let date = Rect {
        x: bounds.x,
        y: time.y + time.height + HOME_CLOCK_DATE_GAP,
        width: bounds.width,
        height: HOME_CLOCK_DATE_HEIGHT,
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
        height: HOME_CLOCK_BLOCK_HEIGHT,
    }
}

/// The click target of launcher `index`, or an empty rect when it is not shown.
pub(crate) fn launcher_button_rect(width: u32, height: u32, count: usize, index: usize) -> Rect {
    home_cell(width, height, count, index).map_or_else(Rect::empty, |home| home.cell)
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

// The caption buttons form one joined group hanging from the top-right of the
// title bar: minimize, maximize, close. They are flush to each other so the
// group reads as a single glass control, and each is a generous click target.

pub(crate) fn window_close_button_rect(outer: Rect) -> Rect {
    let title_rect = window_title_bar_rect(outer);
    let x = title_rect
        .x
        .saturating_add(title_rect.width)
        .saturating_sub(WINDOW_BUTTON_MARGIN_RIGHT + WINDOW_CLOSE_BUTTON_WIDTH);
    Rect {
        x,
        y: title_rect.y + WINDOW_BUTTON_MARGIN_TOP,
        width: WINDOW_CLOSE_BUTTON_WIDTH,
        height: WINDOW_BUTTON_HEIGHT,
    }
}

pub(crate) fn window_maximize_button_rect(outer: Rect) -> Rect {
    let close_rect = window_close_button_rect(outer);
    Rect {
        x: close_rect.x.saturating_sub(WINDOW_BUTTON_WIDTH),
        y: close_rect.y,
        width: WINDOW_BUTTON_WIDTH,
        height: WINDOW_BUTTON_HEIGHT,
    }
}

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    let max_rect = window_maximize_button_rect(outer);
    Rect {
        x: max_rect.x.saturating_sub(WINDOW_BUTTON_WIDTH),
        y: max_rect.y,
        width: WINDOW_BUTTON_WIDTH,
        height: WINDOW_BUTTON_HEIGHT,
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
    WINDOW_BUTTON_MARGIN_RIGHT + WINDOW_BUTTON_WIDTH * 2 + WINDOW_CLOSE_BUTTON_WIDTH
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
pub(crate) fn clamp_wayland_frame(frame: Rect, display_width: u32, display_height: u32) -> Rect {
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
    let min_x = bounds.x.saturating_add(min_visible).saturating_sub(outer_w);
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let (width, height) = (1600_u32, 900_u32);
        let bounds = desktop_bounds(width, height);
        let cells = home_cells(width, height, 36);
        assert_eq!(
            cells.len(),
            36,
            "one full-size home page fits under the clock"
        );
        assert_eq!(
            cells[0].icon.width, HOME_ICON_SIZE,
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
        let (width, height) = (1600_u32, 900_u32);
        // A full-size page holds 36 launchers (9 columns, 4 rows) under the clock.
        for count in [37_usize, 48, 90, 130] {
            let cells = home_cells(width, height, count);
            assert_eq!(cells.len(), count, "all {count} launchers stay visible");
            assert!(cells[0].icon.width < HOME_ICON_SIZE, "the grid scaled down");
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
        let narrow = home_cell(HOME_CELL_WIDTH as u32, 900, 1, 0).expect("scaled grid fits");
        assert!(narrow.icon.width < HOME_ICON_SIZE);
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
                WINDOW_BUTTON_MARGIN_RIGHT,
                "the group ends at the right margin"
            );
            for button in [minimize, maximize, close] {
                assert!(button.y >= title.y && button.y + button.height <= title.y + title.height);
                assert!(button.x >= title.x);
                assert_eq!(button.height, WINDOW_BUTTON_HEIGHT);
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
        let group = window_button_cluster_width() - WINDOW_BUTTON_MARGIN_RIGHT;
        assert_eq!(
            buttons,
            group * WINDOW_BUTTON_HEIGHT,
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
        let (width, height) = (1600_u32, 900_u32);
        let bounds = desktop_bounds(width, height);
        let clock = home_clock(width, height).expect("a 1600x900 display shows a clock");
        assert!(clock.time.y >= bounds.y && clock.date.y >= clock.time.y + clock.time.height);
        assert_eq!((clock.time.x, clock.time.width), (bounds.x, bounds.width));

        for count in [1_usize, 20, 36, 90] {
            let cells = home_cells(width, height, count);
            assert!(!cells.is_empty());
            for home in &cells {
                assert!(
                    home.cell.y >= clock.date.y + clock.date.height,
                    "apps start below the date"
                );
            }
        }
        let block = home_clock_dirty_rect(width, height);
        assert_eq!(block.height, HOME_CLOCK_BLOCK_HEIGHT);
        assert!(home_cells(width, height, 90)[0].cell.y >= block.y + block.height);
    }

    #[test]
    fn a_display_too_small_for_the_clock_still_shows_its_apps() {
        // Short: no room for the clock plus two rows of apps.
        let height =
            (HOME_CLOCK_BLOCK_HEIGHT + 2 * 60) as u32 - 1 + 24 + TASKBAR_HEIGHT as u32 + 24;
        assert!(home_clock(1600, height).is_none());
        assert!(home_clock_dirty_rect(1600, height).is_empty());
        assert!(
            home_cell(1600, height, 4, 0).is_some(),
            "apps fill the desktop instead"
        );
        // Narrow.
        assert!(home_clock(200, 900).is_none());
    }
}
