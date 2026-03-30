mod bootstrap;
mod input;
mod runtime;

use std::os::fd::OwnedFd;
use std::string::String;
use std::time::Duration;
use std::vec::Vec;

use crate::canvas;
use crate::sys::{ConsoleSessionHandle, DisplayInfo, DisplaySurfaceCreate, SurfaceMapping};
use crate::terminal::TerminalState;

pub(crate) const INPUT_EVENT_BATCH: usize = 256;
pub(crate) const MAX_INPUT_READ_BATCHES_PER_TICK: usize = 4;
pub(crate) const MAX_RUNNING_PROGRAMS: usize = 8;
pub(crate) const MAX_REGISTERED_PROGRAMS: usize = 16;
pub(crate) const IDLE_SLEEP: Duration = Duration::from_millis(16);
pub(crate) const INPUT_PROCESS_BUDGET: Duration = Duration::from_millis(2);
pub(crate) const TARGET_FRAME_INTERVAL: Duration = Duration::from_millis(8);
pub(crate) const RUNTIME_POLL_SLEEP: Duration = Duration::from_millis(32);
pub(crate) const CURSOR_BLINK_INTERVAL: Duration = Duration::from_millis(500);
pub(crate) const HIDDEN_RUNTIME_PROGRAM_TITLES: &[&str] = &["UI Server"];

const PAGE_SIZE: usize = 4096;
const MAX_DISPLAY_WIDTH: u32 = 7680;
const MAX_DISPLAY_HEIGHT: u32 = 4320;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct InputProcessingResult {
    pub(crate) needs_full_redraw: bool,
    pub(crate) partial_redraw_rect: canvas::Rect,
    pub(crate) secondary_partial_redraw_rect: canvas::Rect,
    pub(crate) backlog_remaining: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VisualUpdate {
    pub(crate) needs_full_redraw: bool,
    pub(crate) partial_redraw_rect: canvas::Rect,
    pub(crate) secondary_partial_redraw_rect: canvas::Rect,
}

impl VisualUpdate {
    pub(crate) fn full() -> Self {
        Self {
            needs_full_redraw: true,
            partial_redraw_rect: canvas::Rect::empty(),
            secondary_partial_redraw_rect: canvas::Rect::empty(),
        }
    }

    pub(crate) fn partial(rect: canvas::Rect) -> Self {
        Self {
            needs_full_redraw: false,
            partial_redraw_rect: rect,
            secondary_partial_redraw_rect: canvas::Rect::empty(),
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        !self.needs_full_redraw
            && self.partial_redraw_rect.is_empty()
            && self.secondary_partial_redraw_rect.is_empty()
    }

    pub(crate) fn absorb(&mut self, other: Self) {
        if self.needs_full_redraw || other.needs_full_redraw {
            self.request_full();
            return;
        }

        self.merge_partial_rect(other.partial_redraw_rect);
        self.merge_partial_rect(other.secondary_partial_redraw_rect);
    }

    pub(crate) fn request_full(&mut self) {
        self.needs_full_redraw = true;
        self.partial_redraw_rect = canvas::Rect::empty();
        self.secondary_partial_redraw_rect = canvas::Rect::empty();
    }

    pub(crate) fn promote_large_partial(&mut self, width: u32, height: u32) {
        if self.needs_full_redraw
            || (self.partial_redraw_rect.is_empty()
                && self.secondary_partial_redraw_rect.is_empty())
        {
            return;
        }

        let screen_area = u64::from(width) * u64::from(height);
        let dirty_area = rect_area(self.partial_redraw_rect)
            .saturating_add(rect_area(self.secondary_partial_redraw_rect))
            .saturating_sub(rect_area(
                self.partial_redraw_rect
                    .intersect(self.secondary_partial_redraw_rect),
            ));
        if dirty_area.saturating_mul(2) >= screen_area {
            self.request_full();
        }
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    fn merge_partial_rect(&mut self, rect: canvas::Rect) {
        if rect.is_empty() {
            return;
        }

        if self.partial_redraw_rect.is_empty() {
            self.partial_redraw_rect = rect;
            return;
        }

        if self.secondary_partial_redraw_rect.is_empty() {
            self.secondary_partial_redraw_rect = rect;
            return;
        }

        self.partial_redraw_rect = self
            .partial_redraw_rect
            .union(self.secondary_partial_redraw_rect)
            .union(rect);
        self.secondary_partial_redraw_rect = canvas::Rect::empty();
    }
}

fn rect_area(rect: canvas::Rect) -> u64 {
    (rect.width as u64).saturating_mul(rect.height as u64)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LauncherProgram {
    pub(crate) program_id: u32,
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
    pub(crate) pixels: Vec<u32>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) valid: bool,
}

pub(crate) struct ConsoleWindow {
    pub(crate) session_handle: ConsoleSessionHandle,
    pub(crate) title: String,
    pub(crate) frame: canvas::Rect,
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
        window.invalidate_surface();
        Some(window.frame)
    }

    pub(crate) fn toggle_focused_terminal_cursor(&mut self) -> Option<canvas::Rect> {
        let window = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == self.focused_session_handle)?;
        if !window.terminal.toggle_cursor() {
            return None;
        }
        window.invalidate_surface();
        Some(window.frame)
    }
}

pub(crate) struct AppState {
    pub(crate) display: DisplayInfo,
    pub(crate) surface: DisplaySurfaceCreate,
    display_fd: OwnedFd,
    pub(crate) input_fd: OwnedFd,
    console_fd: OwnedFd,
    surface_fd: OwnedFd,
    pub(crate) frame: SurfaceMapping,
    pub(crate) cursor_x: u32,
    pub(crate) cursor_y: u32,
    pub(crate) left_button_down: bool,
    pub(crate) focused_session_handle: ConsoleSessionHandle,
    pub(crate) desktop_cache: DesktopSurfaceCache,
    pub(crate) launcher_programs: Vec<LauncherProgram>,
    pub(crate) console_windows: Vec<ConsoleWindow>,
    dragging_window_session: Option<ConsoleSessionHandle>,
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
