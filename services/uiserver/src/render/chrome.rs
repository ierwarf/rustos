//! Chrome painting: the topbar, the dock, individual windows, and the
//! traffic-light controls. The rendering primitives are all
//! `fill_rounded_rect_alpha` / `fill_rect_alpha` against a SurfaceCanvas;
//! no shape larger than a rounded rectangle is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::app::ConsoleWindow;
use crate::canvas::{Rect, SurfaceCanvas};
use crate::clock::ClockReading;
use crate::font::{self, TextStyle};
use crate::layout::{
    window_button_cluster_width, window_client_rect, window_close_button_rect,
    window_maximize_button_rect, window_minimize_button_rect, window_title_bar_rect, HomeCell, HomeClock,
    WINDOW_RADIUS, WINDOW_SHADOW_STEPS,
};
use crate::wayland::WaylandWindowSnapshot;

use super::colors::{
    COLOR_AERO_CLOSE_BOTTOM, COLOR_AERO_CLOSE_TOP, COLOR_AERO_DEEP, COLOR_AERO_GLASS,
    COLOR_AERO_GLASS_IDLE, COLOR_AERO_TEXT, COLOR_AERO_TEXT_IDLE, COLOR_BG_DEEP, COLOR_GLASS_DARK,
    COLOR_GLASS_DEEP, COLOR_ICON_GLYPH, COLOR_PANEL_HIGHLIGHT, COLOR_SHADOW, COLOR_TEXT_DIM,
    COLOR_TEXT_PRIMARY,
};
use super::glass::{
    apply_window_alpha, corner_inset, draw_glass_chip, draw_glass_panel, mix, Backdrop, GlassStyle,
    WindowAlpha,
};
use super::icons::{draw_app_icon, stroke_line};
use super::{wayland_window_client_rect, wayland_window_outer_rect};

const RAIL_RADIUS: usize = 18;

// ---- top-level rails (topbar / dock) ----

/// The frosted glass rail used by the topbar and the dock. `backdrop` is the
/// unblurred wallpaper the canvas was restored from.
pub(super) fn draw_rail_panel(canvas: &mut SurfaceCanvas<'_>, backdrop: Backdrop<'_>, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    draw_panel_shadow(canvas, rect);
    draw_glass_panel(
        canvas,
        backdrop,
        rect,
        GlassStyle {
            radius: RAIL_RADIUS,
            shade_alpha: 46,
            tint_alpha: 34,
            edge_alpha: 150,
        },
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

/// One home-screen launcher: a large glass icon tile with its name centered
/// underneath, in the same style as a tablet home screen.
pub(super) fn draw_home_launcher(canvas: &mut SurfaceCanvas<'_>, home: HomeCell, title: &str) {
    if home.icon.is_empty() {
        return;
    }
    draw_app_icon(canvas, home.icon, title, 255);

    let label = home.label;
    let text = if title.trim().is_empty() {
        "App"
    } else {
        title.trim()
    };
    let width = font::text_width(text, TextStyle::ui_medium(COLOR_TEXT_PRIMARY)).min(label.width);
    let x = label.x + label.width.saturating_sub(width) / 2;
    // A one-pixel dark offset keeps the label readable on any wallpaper glow.
    font::draw_text_clipped(
        canvas,
        x + 1,
        label.y + 2,
        text,
        label.width,
        TextStyle::ui_medium(COLOR_ICON_GLYPH),
    );
    font::draw_text_clipped(
        canvas,
        x,
        label.y + 1,
        text,
        label.width,
        TextStyle::ui_medium(COLOR_TEXT_PRIMARY),
    );
}

/// The tablet-style clock at the top of the home screen: the time of day large
/// and light, the date under it, both centered on the desktop.
///
/// Digits sit in equal-width cells so the time does not shift sideways as they
/// change; the colon takes its own natural width.
pub(super) fn draw_home_clock(
    canvas: &mut SurfaceCanvas<'_>,
    clock: HomeClock,
    reading: &ClockReading,
) {
    let time = reading.time_text();
    let time_style = TextStyle::clock(COLOR_TEXT_PRIMARY);
    let digit_cell = font::widest_digit(time_style.role);
    let cell_width = |ch: char| {
        if ch.is_ascii_digit() {
            digit_cell
        } else {
            font::glyph_advance(ch, time_style)
        }
    };
    let time_width: usize = time.chars().map(cell_width).sum();
    let line = font::line_height(time_style.role);
    let time_y = clock.time.y + clock.time.height.saturating_sub(line) / 2;
    let start_x = clock.time.x + clock.time.width.saturating_sub(time_width) / 2;
    for (dx, dy, color) in [(0, 2, COLOR_ICON_GLYPH), (0, 0, COLOR_TEXT_PRIMARY)] {
        let mut x = start_x;
        for ch in time.chars() {
            let cell = cell_width(ch);
            font::draw_glyph_centered(canvas, x + dx, time_y + dy, cell, ch, TextStyle::clock(color));
            x += cell;
        }
    }

    let date = reading.date_text();
    let date_style = TextStyle::ui_large(COLOR_TEXT_DIM);
    let date_width = font::text_width(&date, date_style).min(clock.date.width);
    let date_line = font::line_height(date_style.role);
    let date_x = clock.date.x + clock.date.width.saturating_sub(date_width) / 2;
    let date_y = clock.date.y + clock.date.height.saturating_sub(date_line) / 2;
    font::draw_text_clipped(
        canvas,
        date_x + 1,
        date_y + 1,
        &date,
        clock.date.width,
        TextStyle::ui_large(COLOR_ICON_GLYPH),
    );
    font::draw_text_clipped(canvas, date_x, date_y, &date, clock.date.width, date_style);
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
    if focused {
        // The focused window's slot sits in a frosted pill.
        draw_glass_chip(
            canvas,
            Rect {
                x: rect.x.saturating_sub(6),
                y: rect.y.saturating_sub(5),
                width: rect.width + 12,
                height: rect.height + 10,
            },
            16,
            46,
        );
    }
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
            COLOR_PANEL_HIGHLIGHT,
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

    paint_window_chrome_rounded(
        &mut canvas,
        outer,
        client_rect,
        window.title.as_str(),
        focused,
        0,
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
    window.surface_cache.content_version =
        window.surface_cache.content_version.wrapping_add(1).max(1);
}

pub(super) fn paint_window_chrome(
    canvas: &mut SurfaceCanvas<'_>,
    outer: Rect,
    client_rect: Rect,
    title: &str,
    focused: bool,
) {
    paint_window_chrome_rounded(canvas, outer, client_rect, title, focused, WINDOW_RADIUS);
}

/// Window chrome with an explicit corner radius. Console windows are blitted
/// opaquely from a cached surface, so they use radius 0 rather than leaking
/// square corner pixels over the wallpaper.
fn paint_window_chrome_rounded(
    canvas: &mut SurfaceCanvas<'_>,
    outer: Rect,
    client_rect: Rect,
    title: &str,
    focused: bool,
    radius: usize,
) {
    canvas.fill_rounded_rect_alpha(outer, COLOR_GLASS_DARK, 250, radius);

    let title_rect = window_title_bar_rect(outer);
    paint_title_band(canvas, title_rect, focused, radius);
    paint_glass_outline(canvas, outer, focused, radius);

    let title_color = if focused {
        COLOR_AERO_TEXT
    } else {
        COLOR_AERO_TEXT_IDLE
    };
    let button_cluster_width = window_button_cluster_width() + 8;
    let text_x = title_rect.x + 16;
    let text_y = title_rect.y + (title_rect.height.saturating_sub(20)) / 2;
    let text_width = title_rect.width.saturating_sub(button_cluster_width + 16);
    // A soft white glow under dark text keeps the title legible on any glass.
    for (dx, dy) in [(1, 1), (0, 1), (1, 0)] {
        font::draw_text_clipped(
            canvas,
            text_x + dx,
            text_y + dy,
            title,
            text_width,
            TextStyle::ui_medium(COLOR_PANEL_HIGHLIGHT),
        );
    }
    font::draw_text_clipped(
        canvas,
        text_x,
        text_y,
        title,
        text_width,
        TextStyle::ui_medium(title_color),
    );
    draw_window_controls(canvas, outer);

    // Pre-fill so the area shows the right backdrop before the terminal /
    // Wayland buffer paints over it.
    canvas.fill_rect(client_rect, COLOR_GLASS_DEEP);
}

/// The Aero-style title glass. The colors are painted opaque here; the GPU
/// texture path then makes the band translucent with `window_texture_alpha`,
/// so on screen the desktop shows through it. A sky-blue tint is lightened
/// toward white in the upper half for gloss and deepened toward blue below.
/// Rows inside the rounded top corners follow the curve.
fn paint_title_band(
    canvas: &mut SurfaceCanvas<'_>,
    title_rect: Rect,
    focused: bool,
    radius: usize,
) {
    if title_rect.is_empty() {
        return;
    }
    let base = if focused {
        COLOR_AERO_GLASS
    } else {
        COLOR_AERO_GLASS_IDLE
    };
    let height = title_rect.height;
    for row in 0..height {
        let percent = (row * 100 / height) as u32;
        let color = if percent < 50 {
            mix(base, COLOR_PANEL_HIGHLIGHT, (50 - percent) * 130 / 50)
        } else {
            mix(base, COLOR_AERO_DEEP, (percent - 50) * 110 / 50)
        };
        let inset = corner_inset(radius, row);
        canvas.fill_rect(
            Rect {
                x: title_rect.x + inset,
                y: title_rect.y + row,
                width: title_rect.width.saturating_sub(inset * 2),
                height: 1,
            },
            color,
        );
    }
    // Bright inner edge along the top and a pale hairline under the band.
    for row in 0..2 {
        let inset = corner_inset(radius, row) + 1;
        canvas.fill_rect(
            Rect {
                x: title_rect.x + inset,
                y: title_rect.y + row,
                width: title_rect.width.saturating_sub(inset * 2),
                height: 1,
            },
            mix(base, COLOR_PANEL_HIGHLIGHT, 215),
        );
    }
    canvas.fill_rect(
        Rect {
            x: title_rect.x + radius / 2,
            y: title_rect.y + height.saturating_sub(1),
            width: title_rect.width.saturating_sub(radius),
            height: 1,
        },
        mix(base, COLOR_PANEL_HIGHLIGHT, 170),
    );
}

/// The alpha profile of a window texture: translucent glass in the title band
/// and opaque everywhere else, with the caption buttons kept more solid.
pub(super) fn window_texture_alpha(width: usize, height: usize, focused: bool) -> WindowAlpha {
    let local = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    WindowAlpha {
        title_height: window_title_bar_rect(local).height,
        radius: WINDOW_RADIUS,
        title_top: if focused { 196 } else { 164 },
        title_bottom: if focused { 150 } else { 124 },
        title_edge: if focused { 238 } else { 196 },
        solid: [
            window_minimize_button_rect(local),
            window_maximize_button_rect(local),
            window_close_button_rect(local),
        ],
        solid_alpha: 226,
    }
}

/// Make a freshly written window texture region translucent where it should be.
/// Call once per full write of the region, on opaque pixels.
pub(super) fn apply_window_texture_alpha(
    pixels: &mut [u32],
    stride: usize,
    width: usize,
    height: usize,
    focused: bool,
) {
    apply_window_alpha(
        pixels,
        stride,
        width,
        height,
        window_texture_alpha(width, height, focused),
    );
}

/// A soft white edge down both sides and along the bottom of the window.
fn paint_glass_outline(canvas: &mut SurfaceCanvas<'_>, outer: Rect, focused: bool, radius: usize) {
    let alpha = if focused { 64 } else { 34 };
    let side_height = outer.height.saturating_sub(radius * 2);
    for x in [outer.x, outer.x + outer.width.saturating_sub(1)] {
        canvas.fill_rect_alpha(
            Rect {
                x,
                y: outer.y + radius,
                width: 1,
                height: side_height,
            },
            COLOR_PANEL_HIGHLIGHT,
            alpha,
        );
    }
    canvas.fill_rect_alpha(
        Rect {
            x: outer.x + radius,
            y: outer.y + outer.height.saturating_sub(1),
            width: outer.width.saturating_sub(radius * 2),
            height: 1,
        },
        COLOR_PANEL_HIGHLIGHT,
        alpha,
    );
}

fn draw_window_controls(canvas: &mut SurfaceCanvas<'_>, outer: Rect) {
    let minimize = window_minimize_button_rect(outer);
    let maximize = window_maximize_button_rect(outer);
    let close = window_close_button_rect(outer);
    draw_caption_button(canvas, minimize, CaptionGlyph::Minimize, false, true, false);
    draw_caption_button(
        canvas,
        maximize,
        CaptionGlyph::Maximize,
        false,
        false,
        false,
    );
    draw_caption_button(canvas, close, CaptionGlyph::Close, true, false, true);
}

#[derive(Clone, Copy)]
enum CaptionGlyph {
    Minimize,
    Maximize,
    Close,
}

/// Corner radius of the caption group's outer bottom corners.
const CAPTION_RADIUS: usize = 6;

/// One glass caption button. The group's outer bottom corners are rounded;
/// `red` paints the close button's red glass instead of the neutral glass.
fn draw_caption_button(
    canvas: &mut SurfaceCanvas<'_>,
    rect: Rect,
    glyph: CaptionGlyph,
    red: bool,
    first: bool,
    last: bool,
) {
    if rect.is_empty() {
        return;
    }
    let (top, bottom) = if red {
        (COLOR_AERO_CLOSE_TOP, COLOR_AERO_CLOSE_BOTTOM)
    } else {
        (
            mix(COLOR_AERO_GLASS, COLOR_PANEL_HIGHLIGHT, 150),
            mix(COLOR_AERO_GLASS, COLOR_AERO_DEEP, 120),
        )
    };
    for row in 0..rect.height {
        let percent = (row * 100 / rect.height) as u32;
        // Glossy upper half, then the body color, slightly darker at the foot.
        let base = mix(top, bottom, percent * 255 / 100);
        let color = if percent < 48 {
            mix(base, COLOR_PANEL_HIGHLIGHT, (48 - percent) * 90 / 48)
        } else {
            base
        };
        let from_bottom = rect.height - 1 - row;
        let left_inset = if first {
            corner_inset(CAPTION_RADIUS, from_bottom)
        } else {
            0
        };
        let right_inset = if last {
            corner_inset(CAPTION_RADIUS, from_bottom)
        } else {
            0
        };
        canvas.fill_rect(
            Rect {
                x: rect.x + left_inset,
                y: rect.y + row,
                width: rect.width.saturating_sub(left_inset + right_inset),
                height: 1,
            },
            color,
        );
    }
    // Separator between buttons and a bright top edge.
    if !first {
        canvas.fill_rect_alpha(
            Rect {
                x: rect.x,
                y: rect.y,
                width: 1,
                height: rect.height,
            },
            COLOR_SHADOW,
            70,
        );
    }
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y,
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_PANEL_HIGHLIGHT,
        150,
    );
    draw_caption_glyph(canvas, rect, glyph);
}

/// White glyph with a one-pixel dark offset so it reads on light and red glass.
fn draw_caption_glyph(canvas: &mut SurfaceCanvas<'_>, rect: Rect, glyph: CaptionGlyph) {
    let (glyph_w, glyph_h) = match glyph {
        CaptionGlyph::Minimize => (10, 3),
        CaptionGlyph::Maximize => (11, 10),
        CaptionGlyph::Close => (10, 9),
    };
    let x = rect.x + rect.width.saturating_sub(glyph_w) / 2;
    let y = match glyph {
        CaptionGlyph::Minimize => rect.y + rect.height.saturating_sub(glyph_h) / 2 + 4,
        _ => rect.y + rect.height.saturating_sub(glyph_h) / 2,
    };
    for (offset, color, alpha) in [
        (1, COLOR_ICON_GLYPH, 150_u8),
        (0, COLOR_PANEL_HIGHLIGHT, 255),
    ] {
        draw_glyph_strokes(canvas, glyph, x, y + offset, color, alpha);
    }
}

fn draw_glyph_strokes(
    canvas: &mut SurfaceCanvas<'_>,
    glyph: CaptionGlyph,
    x: usize,
    y: usize,
    color: u32,
    alpha: u8,
) {
    let mut fill = |x: usize, y: usize, width: usize, height: usize| {
        canvas.fill_rect_alpha(
            Rect {
                x,
                y,
                width,
                height,
            },
            color,
            alpha,
        );
    };
    match glyph {
        CaptionGlyph::Minimize => fill(x, y, 10, 3),
        CaptionGlyph::Maximize => {
            fill(x, y, 11, 3);
            fill(x, y + 3, 1, 7);
            fill(x + 10, y + 3, 1, 7);
            fill(x, y + 9, 11, 1);
        }
        CaptionGlyph::Close => {
            // Two crossing strokes with edge coverage, so the X reads as
            // smooth lines and not a staircase of 2 px steps.
            let (left, top) = (x as i64 * 8, y as i64 * 8);
            for (from, to) in [
                ((left + 4, top + 4), (left + 76, top + 68)),
                ((left + 76, top + 4), (left + 4, top + 68)),
            ] {
                stroke_line(canvas, from, to, 14, color, alpha);
            }
        }
    }
}

// ---- shadow mask cache ----
//
// The drop shadow under a window used to be six stacked
// `fill_rounded_rect_alpha` passes — about 2.8 M SIMD-blended pixels per
// frame for an 800-wide wayclick window. Since every layer paints the
// same color (black) at different alphas, we precompute the *combined*
// alpha mask once per outer-rect size and blit it as a single alpha-only
// stamp. The on-screen result is identical; the cost drops to a single
// per-pixel scalar blend pass.

const SHADOW_PAD_LEFT: usize = 6;
const SHADOW_PAD_RIGHT: usize = 12;
const SHADOW_PAD_TOP: usize = 0;
const SHADOW_PAD_BOTTOM: usize = 8;

struct ShadowMask {
    alpha: Vec<u8>,
    width: usize,
    height: usize,
}

/// Window sizes are client-chosen, so the mask cache is bounded: it keeps the
/// most recently used masks and never grows past either limit. A mask too
/// large to retain is built, used once, and dropped.
const MAX_SHADOW_MASKS: usize = 16;
const MAX_SHADOW_MASK_CACHE_BYTES: usize = 32 * 1024 * 1024;

struct ShadowMaskCache {
    entries: HashMap<(usize, usize), (Arc<ShadowMask>, u64)>,
    bytes: usize,
    clock: u64,
    max_entries: usize,
    max_bytes: usize,
}

impl ShadowMaskCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            max_entries,
            max_bytes,
        }
    }

    fn get(&mut self, key: (usize, usize)) -> Option<Arc<ShadowMask>> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(&key).map(|(mask, used)| {
            *used = clock;
            Arc::clone(mask)
        })
    }

    /// Cache `mask`, evicting the least recently used masks to make room.
    /// Returns the mask to use, which is the cached one if another thread
    /// built the same size first.
    fn insert(&mut self, key: (usize, usize), mask: Arc<ShadowMask>) -> Arc<ShadowMask> {
        if let Some(existing) = self.get(key) {
            return existing;
        }
        let size = mask.alpha.len();
        if size > self.max_bytes {
            return mask;
        }
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries || self.bytes + size > self.max_bytes)
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some((evicted, _)) = self.entries.remove(&oldest) {
                self.bytes -= evicted.alpha.len();
            }
        }
        self.clock += 1;
        self.bytes += size;
        self.entries.insert(key, (Arc::clone(&mask), self.clock));
        mask
    }
}

fn shadow_mask_cache() -> &'static Mutex<ShadowMaskCache> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<ShadowMaskCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(ShadowMaskCache::new(
            MAX_SHADOW_MASKS,
            MAX_SHADOW_MASK_CACHE_BYTES,
        ))
    })
}

fn ensure_shadow_mask(outer_w: usize, outer_h: usize) -> Arc<ShadowMask> {
    let key = (outer_w, outer_h);
    if let Ok(mut cache) = shadow_mask_cache().lock() {
        if let Some(mask) = cache.get(key) {
            return mask;
        }
    }
    // Build outside the lock so a slow build can't stall other paints, then
    // insert under the lock.
    let mask = Arc::new(build_shadow_mask(outer_w, outer_h));
    match shadow_mask_cache().lock() {
        Ok(mut cache) => cache.insert(key, mask),
        Err(_) => mask,
    }
}

fn build_shadow_mask(outer_w: usize, outer_h: usize) -> ShadowMask {
    let mask_w = outer_w + SHADOW_PAD_LEFT + SHADOW_PAD_RIGHT;
    let mask_h = outer_h + SHADOW_PAD_TOP + SHADOW_PAD_BOTTOM;
    // Paint shadow layers onto a white scratch surface; after blending,
    // each pixel's red channel records (255 − combined_shadow_alpha).
    let mut scratch = vec![0x00ff_ffff_u32; mask_w * mask_h];
    {
        let mut canvas =
            SurfaceCanvas::new(scratch.as_mut_slice(), mask_w as u32, mask_h as u32, mask_w);
        let local_rect = Rect {
            x: SHADOW_PAD_LEFT,
            y: SHADOW_PAD_TOP,
            width: outer_w,
            height: outer_h,
        };
        for step in 0..WINDOW_SHADOW_STEPS {
            let alpha = 52u32.saturating_sub((step as u32).saturating_mul(7)) as u8;
            if alpha == 0 {
                break;
            }
            let inset = step + 1;
            let layer = Rect {
                x: local_rect.x.saturating_sub(inset),
                y: local_rect.y.saturating_sub(inset / 2).saturating_add(3),
                width: local_rect.width.saturating_add(inset * 2),
                height: local_rect.height.saturating_add(inset + 2),
            };
            canvas.fill_rounded_rect_alpha(layer, COLOR_SHADOW, alpha, WINDOW_RADIUS + step);
        }
    }
    let alpha: Vec<u8> = scratch
        .iter()
        .map(|p| 255u8.saturating_sub(((p >> 16) & 0xff) as u8))
        .collect();
    ShadowMask {
        alpha,
        width: mask_w,
        height: mask_h,
    }
}

pub(super) fn draw_window_shadow(canvas: &mut SurfaceCanvas<'_>, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    let mask = ensure_shadow_mask(rect.width, rect.height);
    let dst_x = rect.x.saturating_sub(SHADOW_PAD_LEFT);
    let dst_y = rect.y.saturating_add(SHADOW_PAD_TOP);
    canvas.blit_alpha_mask(
        &mask.alpha,
        mask.width,
        mask.height,
        dst_x,
        dst_y,
        COLOR_SHADOW,
    );
}

#[cfg(test)]
mod shadow_cache_tests {
    use super::*;

    fn mask(width: usize, height: usize) -> Arc<ShadowMask> {
        Arc::new(build_shadow_mask(width, height))
    }

    #[test]
    fn the_cache_never_holds_more_than_its_entry_limit() {
        let mut cache = ShadowMaskCache::new(4, usize::MAX);
        for width in 40..80 {
            cache.insert((width, 50), mask(width, 50));
        }
        assert_eq!(cache.entries.len(), 4);
        let counted: usize = cache.entries.values().map(|(m, _)| m.alpha.len()).sum();
        assert_eq!(cache.bytes, counted, "byte accounting matches the entries");
    }

    #[test]
    fn the_least_recently_used_mask_is_evicted_first() {
        let mut cache = ShadowMaskCache::new(2, usize::MAX);
        cache.insert((40, 50), mask(40, 50));
        cache.insert((41, 50), mask(41, 50));
        assert!(cache.get((40, 50)).is_some(), "touch the older entry");
        cache.insert((42, 50), mask(42, 50));
        assert!(
            cache.get((40, 50)).is_some(),
            "recently used entry survives"
        );
        assert!(cache.get((41, 50)).is_none(), "the idle entry was evicted");
    }

    #[test]
    fn a_mask_over_the_byte_limit_is_used_but_not_retained() {
        let mut cache = ShadowMaskCache::new(8, 1024);
        let big = mask(200, 200);
        assert!(big.alpha.len() > 1024);
        let used = cache.insert((200, 200), Arc::clone(&big));
        assert!(Arc::ptr_eq(&used, &big));
        assert!(cache.entries.is_empty() && cache.bytes == 0);
    }

    #[test]
    fn inserting_a_size_twice_keeps_the_first_mask() {
        let mut cache = ShadowMaskCache::new(4, usize::MAX);
        let first = cache.insert((40, 50), mask(40, 50));
        let second = cache.insert((40, 50), mask(40, 50));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.entries.len(), 1);
    }
}
