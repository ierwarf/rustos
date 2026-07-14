mod bootstrap;
mod input;
mod runtime;

pub(crate) use bootstrap::start_launcher_program_loader;
pub(crate) use runtime::{
    ConsoleCommandDispatcher, ConsoleCommandSnapshot, start_console_command_dispatcher,
    start_console_refresh_worker,
};

use std::os::fd::OwnedFd;
use std::string::String;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::vec::Vec;

use crate::canvas;
use crate::cursor_sprites::CURSOR_SPRITE_MAX_DISTANCE;
use crate::profile;
use crate::sys::{
    ConsoleSessionHandle, DisplayInfo, DisplaySurfaceCreate, SurfaceMapping, diag_line,
};
use crate::terminal::TerminalState;
use crate::wayland::WaylandWindowSnapshot;

pub(crate) const INPUT_EVENT_BATCH: usize = 256;
pub(crate) const MAX_INPUT_READ_BATCHES_PER_TICK: usize = 8;
pub(crate) const MAX_RUNNING_PROGRAMS: usize = 8;
pub(crate) const INPUT_PROCESS_BUDGET: Duration = Duration::from_millis(8);
pub(crate) const RUNTIME_POLL_SLEEP: Duration = Duration::from_millis(16);
pub(crate) const CONSOLE_POLL_SLEEP: Duration = Duration::from_millis(16);
pub(crate) const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const CURSOR_MOTION_SETTLE_INTERVAL: Duration = Duration::from_micros(8_333);
pub(crate) const HIDDEN_RUNTIME_PROGRAM_TITLES: &[&str] = &["UI Server"];

const PAGE_SIZE: usize = 4096;
const MAX_DISPLAY_WIDTH: u32 = 7680;
const MAX_DISPLAY_HEIGHT: u32 = 4320;
const CURSOR_MOTION_HOLD_TICKS: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CursorMotion {
    pub(crate) dx: i32,
    pub(crate) dy: i32,
    pub(crate) sprite_index: u8,
}

impl CursorMotion {
    pub(crate) const fn stationary() -> Self {
        Self {
            dx: 1,
            dy: 0,
            sprite_index: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InputProcessingResult {
    pub(crate) visual_update: VisualUpdate,
    pub(crate) backlog_remaining: bool,
    pub(crate) input_events: u64,
    pub(crate) pointer_motion_events: u64,
    pub(crate) pointer_position_events: u64,
    pub(crate) wayland_motion_calls: u64,
}

const MAX_PARTIAL_RECTS: usize = 32;
const MAX_PARTIAL_RECT_PIXELS: u64 = 16 * 1024;
const PARTIAL_RECT_MERGE_NUMERATOR: u64 = 5;
const PARTIAL_RECT_MERGE_DENOMINATOR: u64 = 4;
const FULL_REDRAW_PROMOTION_NUMERATOR: u64 = 9;
const FULL_REDRAW_PROMOTION_DENOMINATOR: u64 = 10;
const MAX_WAYLAND_SYNC_LOGS: usize = 8;

static WAYLAND_SYNC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, Default)]
pub(crate) struct VisualUpdate {
    pub(crate) needs_full_redraw: bool,
    partial_rects: Vec<canvas::Rect>,
}

impl VisualUpdate {
    pub(crate) fn full() -> Self {
        Self {
            needs_full_redraw: true,
            partial_rects: Vec::new(),
        }
    }

    pub(crate) fn partial(rect: canvas::Rect) -> Self {
        let mut update = Self::default();
        update.add_partial_rect(rect);
        update
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.needs_full_redraw && self.partial_rects.is_empty()
    }

    pub(crate) fn absorb(&mut self, other: Self) {
        if self.needs_full_redraw || other.needs_full_redraw {
            self.request_full();
            return;
        }

        for rect in other.partial_rects {
            self.merge_partial_rect(rect);
        }
    }

    pub(crate) fn add_partial_rect(&mut self, rect: canvas::Rect) {
        if self.needs_full_redraw {
            return;
        }
        self.merge_partial_rect(rect);
    }

    pub(crate) fn partial_rects(&self) -> &[canvas::Rect] {
        &self.partial_rects
    }

    pub(crate) fn request_full(&mut self) {
        self.needs_full_redraw = true;
        self.partial_rects.clear();
    }

    pub(crate) fn promote_large_partial(&mut self, width: u32, height: u32) {
        if self.needs_full_redraw || self.partial_rects.is_empty() {
            return;
        }

        let screen_area = u64::from(width) * u64::from(height);
        let dirty_area = self
            .partial_rects
            .iter()
            .fold(0_u64, |area, rect| area.saturating_add(rect_area(*rect)));
        if dirty_area.saturating_mul(FULL_REDRAW_PROMOTION_DENOMINATOR)
            >= screen_area.saturating_mul(FULL_REDRAW_PROMOTION_NUMERATOR)
        {
            self.request_full();
        }
    }

    pub(crate) fn split_large_partials(&mut self) {
        if self.needs_full_redraw || self.partial_rects.is_empty() {
            return;
        }

        let mut split = Vec::with_capacity(self.partial_rects.len());
        for rect in self.partial_rects.drain(..) {
            let area = rect_area(rect);
            if area <= MAX_PARTIAL_RECT_PIXELS || rect.width == 0 {
                split.push(rect);
                continue;
            }

            let strip_height = ((MAX_PARTIAL_RECT_PIXELS / rect.width as u64) as usize)
                .max(1)
                .min(rect.height);
            let mut y = rect.y;
            let end_y = rect.y.saturating_add(rect.height);
            while y < end_y {
                let height = strip_height.min(end_y - y);
                split.push(canvas::Rect {
                    x: rect.x,
                    y,
                    width: rect.width,
                    height,
                });
                y = y.saturating_add(height);
            }
        }
        self.partial_rects = split;
    }

    pub(crate) fn coalesce_tight_partials(&mut self) {
        if self.needs_full_redraw || self.partial_rects.len() < 2 {
            return;
        }

        let mut index = 0;
        while index < self.partial_rects.len() {
            let mut merged = false;
            let mut other = index + 1;
            while other < self.partial_rects.len() {
                let a = self.partial_rects[index];
                let b = self.partial_rects[other];
                let union = a.union(b);
                let separate_area = rect_area(a)
                    .saturating_add(rect_area(b))
                    .saturating_sub(rect_area(a.intersect(b)));
                if should_merge_partial_rects(union, separate_area) {
                    self.partial_rects[index] = union;
                    self.partial_rects.swap_remove(other);
                    merged = true;
                    break;
                }
                other += 1;
            }
            if !merged {
                index += 1;
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn merge_partial_rect(&mut self, rect: canvas::Rect) {
        if rect.is_empty() {
            return;
        }

        if let Some(index) = self.tight_merge_target(rect) {
            self.partial_rects[index] = self.partial_rects[index].union(rect);
            return;
        }

        if self.partial_rects.len() < MAX_PARTIAL_RECTS {
            self.partial_rects.push(rect);
            return;
        }

        let index = self.lowest_growth_merge_target(rect).unwrap_or(0);
        self.partial_rects[index] = self.partial_rects[index].union(rect);
    }

    fn tight_merge_target(&self, rect: canvas::Rect) -> Option<usize> {
        self.partial_rects.iter().position(|existing| {
            let union = existing.union(rect);
            let separate_area = rect_area(*existing)
                .saturating_add(rect_area(rect))
                .saturating_sub(rect_area(existing.intersect(rect)));
            should_merge_partial_rects(union, separate_area)
        })
    }

    fn lowest_growth_merge_target(&self, rect: canvas::Rect) -> Option<usize> {
        self.partial_rects
            .iter()
            .enumerate()
            .min_by_key(|(_, existing)| {
                rect_area(existing.union(rect)).saturating_sub(rect_area(**existing))
            })
            .map(|(index, _)| index)
    }
}

fn rect_area(rect: canvas::Rect) -> u64 {
    (rect.width as u64).saturating_mul(rect.height as u64)
}

fn should_merge_partial_rects(union: canvas::Rect, separate_area: u64) -> bool {
    rect_area(union).saturating_mul(PARTIAL_RECT_MERGE_DENOMINATOR)
        <= separate_area.saturating_mul(PARTIAL_RECT_MERGE_NUMERATOR)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LauncherProgram {
    pub(crate) desktop_file_id: String,
    pub(crate) title: String,
}

#[derive(Default)]
pub(crate) struct WindowSurfaceCache {
    pub(crate) pixels: Vec<u32>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) focused: bool,
    pub(crate) valid: bool,
}

#[derive(Default)]
pub(crate) struct DesktopSurfaceCache {
    // Composite output, ready to blit into the framebuffer surface.
    pub(crate) pixels: Vec<u32>,
    // Background-only render (gradient bands + grid). Kept separately so a
    // launcher list change only repaints the topbar strip on top of a clean
    // background copy instead of regenerating ~5 MiB of alpha blends.
    pub(crate) background_pixels: Vec<u32>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) background_valid: bool,
    pub(crate) chrome_valid: bool,
}

impl DesktopSurfaceCache {
    pub(crate) fn invalidate_all(&mut self) {
        self.background_valid = false;
        self.chrome_valid = false;
    }

    pub(crate) fn invalidate_chrome(&mut self) {
        self.chrome_valid = false;
    }

    pub(crate) fn fully_valid(&self) -> bool {
        self.background_valid && self.chrome_valid
    }
}

pub(crate) struct ConsoleWindow {
    pub(crate) session_handle: ConsoleSessionHandle,
    pub(crate) title: String,
    pub(crate) frame: canvas::Rect,
    pub(crate) minimized: bool,
    /// Saved frame from before the most recent maximize; `Some` while the
    /// window is currently maximized so the second click on the maximize
    /// button can restore it.
    pub(crate) pre_maximize_frame: Option<canvas::Rect>,
    pub(crate) terminal: TerminalState,
    pub(crate) output_cache: Vec<u8>,
    pub(crate) output_generation: u64,
    pub(crate) terminal_dirty: bool,
    pub(crate) surface_cache: WindowSurfaceCache,
}

impl ConsoleWindow {
    pub(crate) fn new(
        session_handle: ConsoleSessionHandle,
        title: String,
        frame: canvas::Rect,
        output_generation: u64,
    ) -> Self {
        Self {
            session_handle,
            title,
            frame,
            minimized: false,
            pre_maximize_frame: None,
            terminal: TerminalState::new(),
            output_cache: Vec::new(),
            output_generation,
            terminal_dirty: true,
            surface_cache: WindowSurfaceCache::default(),
        }
    }

    pub(crate) fn invalidate_surface(&mut self) {
        self.surface_cache.valid = false;
    }

    fn repaint_cursor_surface(&mut self) -> Option<canvas::Rect> {
        if !self.surface_cache.valid
            || self.surface_cache.width == 0
            || self.surface_cache.height == 0
        {
            return None;
        }
        let rect = self.terminal.cursor_cell_rect()?;
        let mut canvas = canvas::SurfaceCanvas::with_clip(
            self.surface_cache.pixels.as_mut_slice(),
            self.surface_cache.width as u32,
            self.surface_cache.height as u32,
            self.surface_cache.width,
            rect,
        );
        self.terminal.render_cursor_cell(&mut canvas);
        Some(canvas::Rect {
            x: self.frame.x + rect.x,
            y: self.frame.y + rect.y,
            width: rect.width,
            height: rect.height,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DragTarget {
    Console(ConsoleSessionHandle),
    Wayland(u32),
}

impl AppState {
    pub(crate) fn reveal_focused_terminal_cursor(&mut self) -> Option<canvas::Rect> {
        let window = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == self.focused_session_handle)?;
        if !window.terminal.show_cursor() {
            return None;
        }
        window.repaint_cursor_surface().or_else(|| {
            window.invalidate_surface();
            Some(window.frame)
        })
    }

    pub(crate) fn toggle_focused_terminal_cursor(&mut self) -> Option<canvas::Rect> {
        let window = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == self.focused_session_handle)?;
        if !window.terminal.toggle_cursor() {
            return None;
        }
        window.repaint_cursor_surface().or_else(|| {
            window.invalidate_surface();
            Some(window.frame)
        })
    }
}

pub(crate) struct AppState {
    pub(crate) display: DisplayInfo,
    pub(crate) surface: DisplaySurfaceCreate,
    display_fd: OwnedFd,
    pub(crate) input_fds: Vec<OwnedFd>,
    console_commands: ConsoleCommandDispatcher,
    surface_fd: OwnedFd,
    pub(crate) frame: SurfaceMapping,
    pub(crate) cursor_x: u32,
    pub(crate) cursor_y: u32,
    pub(crate) cursor_motion: CursorMotion,
    cursor_motion_hold_ticks: u8,
    pub(crate) left_button_down: bool,
    pub(crate) focused_session_handle: ConsoleSessionHandle,
    pub(crate) focused_wayland_surface_id: Option<u32>,
    pending_console_focus: Option<ConsoleSessionHandle>,
    pub(crate) desktop_cache: DesktopSurfaceCache,
    pub(crate) launcher_programs: Vec<LauncherProgram>,
    pub(crate) console_windows: Vec<ConsoleWindow>,
    closing_console_sessions: Vec<ConsoleSessionHandle>,
    pub(crate) next_console_snapshot_index: usize,
    pub(crate) wayland_windows: Vec<WaylandWindowSnapshot>,
    dragging_window: Option<DragTarget>,
    drag_offset_x: usize,
    drag_offset_y: usize,
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    if align == 0 {
        return None;
    }
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

fn integer_sqrt(value: i64) -> u64 {
    if value <= 0 {
        return 0;
    }

    let mut estimate = value as u64;
    let mut next = estimate.div_ceil(2);
    while next < estimate {
        estimate = next;
        next = (estimate + value as u64 / estimate) / 2;
    }
    estimate
}

impl AppState {
    pub(crate) fn console_command_snapshot(&self) -> ConsoleCommandSnapshot {
        self.console_commands.snapshot()
    }

    pub(crate) fn sync_wayland_windows(
        &mut self,
        windows: Vec<WaylandWindowSnapshot>,
    ) -> canvas::Rect {
        let before_dirty = self
            .wayland_stack_dirty_rect()
            .union(self.wayland_taskbar_dirty_rect());
        let previous_windows = std::mem::take(&mut self.wayland_windows);
        let previous_dragging = self.dragging_window;
        let previous_focus = self.focused_wayland_surface_id;
        let has_damage = windows.iter().any(|window| !window.damage.is_empty());
        let order_unchanged = previous_windows.len() == windows.len()
            && previous_windows
                .iter()
                .zip(windows.iter())
                .all(|(previous, next)| previous.surface_id == next.surface_id);
        if let Some(DragTarget::Wayland(surface_id)) = self.dragging_window {
            if windows
                .iter()
                .all(|window| window.surface_id != surface_id || window.minimized)
            {
                self.dragging_window = None;
            }
        }
        if let Some(surface_id) = self.focused_wayland_surface_id {
            if windows
                .iter()
                .all(|window| window.surface_id != surface_id || window.minimized)
            {
                self.focused_wayland_surface_id = None;
            }
        }

        if order_unchanged
            && previous_windows == windows
            && previous_dragging == self.dragging_window
            && previous_focus == self.focused_wayland_surface_id
            && !has_damage
        {
            self.wayland_windows = previous_windows;
            return canvas::Rect::empty();
        }

        if !order_unchanged {
            self.wayland_windows = windows;
            return before_dirty
                .union(self.wayland_stack_dirty_rect())
                .union(self.wayland_taskbar_dirty_rect());
        }

        let mut dirty = canvas::Rect::empty();
        for (previous, window) in previous_windows.iter().zip(windows.iter()) {
            if previous.minimized != window.minimized || previous.frame != window.frame {
                dirty = dirty.union(crate::render::wayland_window_dirty_rect(previous));
                dirty = dirty.union(crate::render::wayland_window_dirty_rect(window));
                continue;
            }

            if previous.title != window.title {
                let outer = crate::render::wayland_window_outer_rect(window);
                dirty = dirty.union(outer);
                dirty = dirty.union(self.wayland_taskbar_dirty_rect());
                continue;
            }

            if previous.content_version != window.content_version || !window.damage.is_empty() {
                let outer = crate::render::wayland_window_outer_rect(window);
                let rect = if window.damage.is_empty() {
                    crate::render::wayland_window_client_rect(outer)
                } else {
                    crate::render::wayland_window_damage_rect(window, window.damage)
                };
                dirty = dirty.union(rect);
            }
        }

        self.wayland_windows = windows;
        let focus_or_drag_changed = previous_dragging != self.dragging_window
            || previous_focus != self.focused_wayland_surface_id;
        let returned_dirty = if focus_or_drag_changed {
            before_dirty
                .union(dirty)
                .union(self.wayland_stack_dirty_rect())
                .union(self.wayland_taskbar_dirty_rect())
        } else {
            dirty
        };
        if profile::enabled() && has_damage {
            let log_index = WAYLAND_SYNC_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if log_index < MAX_WAYLAND_SYNC_LOGS {
                diag_line(
                    format!(
                        "uiserver: wayland sync damage dirty={}x{}@{},{} returned={}x{}@{},{} stack_included={} windows={} focused_wayland={:?}",
                        dirty.width,
                        dirty.height,
                        dirty.x,
                        dirty.y,
                        returned_dirty.width,
                        returned_dirty.height,
                        returned_dirty.x,
                        returned_dirty.y,
                        focus_or_drag_changed,
                        self.wayland_windows.len(),
                        self.focused_wayland_surface_id,
                    )
                    .as_str(),
                );
            }
        }
        returned_dirty
    }

    pub(crate) fn record_cursor_motion(&mut self, previous_x: u32, previous_y: u32) {
        if previous_x == self.cursor_x && previous_y == self.cursor_y {
            return;
        }

        let dx = self.cursor_x as i64 - previous_x as i64;
        let dy = self.cursor_y as i64 - previous_y as i64;
        let distance = integer_sqrt(dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)));
        self.cursor_motion = CursorMotion {
            dx: dx.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            dy: dy.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
            sprite_index: distance.min(u64::from(CURSOR_SPRITE_MAX_DISTANCE)) as u8,
        };
        self.cursor_motion_hold_ticks = CURSOR_MOTION_HOLD_TICKS;
    }

    pub(crate) fn settle_cursor_motion(&mut self, width: u32, height: u32) -> canvas::Rect {
        if self.cursor_motion.sprite_index == 0 {
            return canvas::Rect::empty();
        }
        if self.cursor_motion_hold_ticks != 0 {
            self.cursor_motion_hold_ticks = self.cursor_motion_hold_ticks.saturating_sub(1);
            return canvas::Rect::empty();
        }

        let before = self.cursor_visual_dirty_rect(width, height);
        self.cursor_motion = CursorMotion::stationary();
        before.union(self.cursor_visual_dirty_rect(width, height))
    }

    pub(crate) fn cursor_motion_active(&self) -> bool {
        self.cursor_motion.sprite_index != 0
    }

    pub(crate) fn cursor_visual_dirty_rect(&self, width: u32, height: u32) -> canvas::Rect {
        canvas::cursor_dirty_rect(self.cursor_x, self.cursor_y, width, height)
    }

    pub(crate) fn wayland_stack_dirty_rect(&self) -> canvas::Rect {
        self.wayland_windows
            .iter()
            .fold(canvas::Rect::empty(), |dirty, window| {
                if window.minimized {
                    dirty
                } else {
                    dirty.union(crate::render::wayland_window_dirty_rect(window))
                }
            })
    }

    pub(crate) fn wayland_taskbar_dirty_rect(&self) -> canvas::Rect {
        crate::render::taskbar_dirty_rect(self.display.width, self.display.height)
    }

    pub(crate) fn wayland_window_rect_for_surface(&self, surface_id: u32) -> canvas::Rect {
        self.wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
            .map(crate::render::wayland_window_dirty_rect)
            .unwrap_or_default()
    }

    pub(crate) fn wayland_visual_dirty_rect(&self) -> canvas::Rect {
        self.wayland_stack_dirty_rect()
            .union(self.wayland_taskbar_dirty_rect())
    }
}
