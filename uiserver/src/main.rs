mod canvas;
mod render;
mod simd;
mod sys;
mod terminal;

use std::os::fd::RawFd;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::string::String;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::vec::Vec;

use render::{
    clamp_console_window_rect, console_window_title_bar_rect, default_console_window_rect,
    launcher_button_rect, render_cursor_only, render_frame, render_rect, taskbar_slot_rect,
};
use sys::{
    console_get_state, console_set_focus, console_snapshot_session_output, display_create_surface,
    display_get_info, display_present, display_present_rect, map_surface, open_console,
    open_display, open_input, open_runtime, raw_stderr_line, read_input, runtime_generation,
    runtime_request_launch_first_available, runtime_snapshot_programs,
    runtime_snapshot_running_programs, DisplayInfo, DisplaySurfaceCreate, InputEvent,
    RuntimeProgram, RuntimeRunningProgram, SurfaceMapping, INPUT_ACTION_RELEASED,
    INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    MAX_CONSOLE_SNAPSHOT_BYTES, PIXEL_FORMAT_BGRA8888, POINTER_BUTTON_LEFT,
};
use terminal::TerminalState;

// Match the kernel-side per-read cap so mouse motion does not spill
// into avoidable extra UI frames under load.
const INPUT_EVENT_BATCH: usize = 64;
const MAX_RUNNING_PROGRAMS: usize = 8;
const MAX_REGISTERED_PROGRAMS: usize = 16;
const IDLE_SLEEP: Duration = Duration::from_millis(16);
const RUNTIME_POLL_SLEEP: Duration = Duration::from_millis(32);
const HIDDEN_RUNTIME_PROGRAM_TITLES: &[&str] = &["UI Server"];

#[derive(Clone, Copy, Debug, Default)]
struct InputProcessingResult {
    previous_cursor_x: u32,
    previous_cursor_y: u32,
    cursor_moved: bool,
    needs_full_redraw: bool,
    partial_redraw_rect: canvas::Rect,
}

#[derive(Clone, Copy, Debug, Default)]
struct RedrawRequest {
    needs_full_redraw: bool,
    partial_redraw_rect: canvas::Rect,
}

impl RedrawRequest {
    fn full() -> Self {
        Self {
            needs_full_redraw: true,
            partial_redraw_rect: canvas::Rect::empty(),
        }
    }

    fn partial(rect: canvas::Rect) -> Self {
        Self {
            needs_full_redraw: false,
            partial_redraw_rect: rect,
        }
    }
}

#[derive(Clone)]
struct RuntimeState {
    generation: u64,
    running_programs: [RuntimeRunningProgram; MAX_RUNNING_PROGRAMS],
    running_program_count: usize,
    registered_programs: [RuntimeProgram; MAX_REGISTERED_PROGRAMS],
    registered_program_count: usize,
    dirty: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            generation: 0,
            running_programs: [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS],
            running_program_count: 0,
            registered_programs: [RuntimeProgram::default(); MAX_REGISTERED_PROGRAMS],
            registered_program_count: 0,
            dirty: false,
        }
    }
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
    pub(crate) session_index: u32,
    pub(crate) title: String,
    pub(crate) frame: canvas::Rect,
    pub(crate) terminal: TerminalState,
    pub(crate) output_cache: Vec<u8>,
    pub(crate) output_generation: u64,
    pub(crate) terminal_dirty: bool,
    pub(crate) surface_cache: WindowSurfaceCache,
}

impl ConsoleWindow {
    fn new(session_index: u32, title: String, frame: canvas::Rect, output_generation: u64) -> Self {
        Self {
            session_index,
            title,
            frame,
            terminal: TerminalState::new(),
            output_cache: Vec::new(),
            output_generation,
            terminal_dirty: true,
            surface_cache: WindowSurfaceCache::default(),
        }
    }

    fn invalidate_surface(&mut self) {
        self.surface_cache.valid = false;
    }
}

pub(crate) struct AppState {
    pub(crate) display: DisplayInfo,
    pub(crate) surface: DisplaySurfaceCreate,
    display_fd: OwnedFd,
    input_fd: OwnedFd,
    console_fd: OwnedFd,
    surface_fd: OwnedFd,
    pub(crate) frame: SurfaceMapping,
    pub(crate) cursor_x: u32,
    pub(crate) cursor_y: u32,
    pub(crate) left_button_down: bool,
    pub(crate) focused_session_index: u32,
    pub(crate) desktop_cache: DesktopSurfaceCache,
    pub(crate) launcher_programs: Vec<LauncherProgram>,
    pub(crate) console_windows: Vec<ConsoleWindow>,
    dragging_window_session: Option<u32>,
    drag_offset_x: usize,
    drag_offset_y: usize,
}

impl AppState {
    fn initialize() -> Result<Self, i32> {
        let display_fd = open_display().map_err(|_| {
            raw_stderr_line("uiserver: open_display failed");
            10
        })?;
        let input_fd = open_input().map_err(|_| {
            raw_stderr_line("uiserver: open_input failed");
            11
        })?;
        let console_fd = open_console().map_err(|_| {
            raw_stderr_line("uiserver: open_console failed");
            12
        })?;

        let display = display_get_info(display_fd.as_raw_fd()).map_err(|_| {
            raw_stderr_line("uiserver: display_get_info failed");
            13
        })?;
        if display.bytes_per_pixel != 4 || display.pixel_format != PIXEL_FORMAT_BGRA8888 {
            raw_stderr_line("uiserver: unsupported display format");
            return Err(14);
        }

        let surface = display_create_surface(display_fd.as_raw_fd(), display.width, display.height)
            .map_err(|_| {
                raw_stderr_line("uiserver: display_create_surface failed");
                15
            })?;
        let surface_fd = unsafe { OwnedFd::from_raw_fd(surface.handle as i32) };
        if surface.width != display.width
            || surface.height != display.height
            || surface.bytes_per_pixel != display.bytes_per_pixel
            || surface.pixel_format != display.pixel_format
            || surface.mapping_len == 0
        {
            raw_stderr_line("uiserver: surface metadata mismatch");
            return Err(16);
        }

        let frame =
            map_surface(surface_fd.as_raw_fd(), surface.mapping_len as usize).map_err(|_| {
                raw_stderr_line("uiserver: map_surface failed");
                17
            })?;
        let console_state = console_get_state(console_fd.as_raw_fd()).map_err(|_| {
            raw_stderr_line("uiserver: console_get_state failed");
            18
        })?;

        Ok(Self {
            display,
            surface,
            display_fd,
            input_fd,
            console_fd,
            surface_fd,
            frame,
            cursor_x: display.width / 2,
            cursor_y: display.height / 2,
            left_button_down: false,
            focused_session_index: console_state.focused_session_index,
            desktop_cache: DesktopSurfaceCache::default(),
            launcher_programs: Vec::new(),
            console_windows: Vec::new(),
            dragging_window_session: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
        })
    }

    fn apply_runtime_state(&mut self, runtime_state: &Arc<Mutex<RuntimeState>>) -> bool {
        let mut runtime_state = runtime_state.lock().unwrap();
        if !runtime_state.dirty {
            return false;
        }

        let mut changed = self.sync_launcher_programs(
            &runtime_state.registered_programs[..runtime_state
                .registered_program_count
                .min(MAX_REGISTERED_PROGRAMS)],
        );
        changed |= self.sync_windows_from_runtime(
            &runtime_state.running_programs[..runtime_state
                .running_program_count
                .min(MAX_RUNNING_PROGRAMS)],
        );
        runtime_state.dirty = false;
        changed || self.bring_window_to_front(self.focused_session_index)
    }

    fn sync_launcher_programs(&mut self, programs: &[RuntimeProgram]) -> bool {
        let mut next = Vec::new();
        for program in programs {
            let title = runtime_program_name(program).into_owned();
            if runtime_title_is_hidden(title.as_str()) {
                continue;
            }
            if next
                .iter()
                .any(|existing: &LauncherProgram| existing.title == title)
            {
                continue;
            }
            next.push(LauncherProgram {
                program_id: program.program_id,
                title,
            });
        }

        if self.launcher_programs == next {
            return false;
        }

        self.launcher_programs = next;
        self.desktop_cache.valid = false;
        true
    }

    fn sync_windows_from_runtime(&mut self, programs: &[RuntimeRunningProgram]) -> bool {
        let existing = std::mem::take(&mut self.console_windows);
        let mut next = Vec::with_capacity(programs.len());
        let mut changed = false;
        let mut kept_sessions = Vec::with_capacity(programs.len());

        for mut window in existing {
            let Some(program) = programs
                .iter()
                .find(|program| program.session_index == window.session_index)
            else {
                changed = true;
                continue;
            };
            if runtime_program_is_hidden(program) {
                changed = true;
                continue;
            }

            let new_title = runtime_program_title(program);
            if window.title != new_title {
                window.title = new_title;
                window.invalidate_surface();
                changed = true;
            }
            kept_sessions.push(window.session_index);
            next.push(window);
        }

        for program in programs {
            if runtime_program_is_hidden(program) || kept_sessions.contains(&program.session_index)
            {
                continue;
            }

            next.push(ConsoleWindow::new(
                program.session_index,
                runtime_program_title(program),
                default_console_window_rect(self.display.width, self.display.height, next.len()),
                0,
            ));
            changed = true;
        }

        if let Some(session_index) = self.dragging_window_session {
            if next
                .iter()
                .all(|window| window.session_index != session_index)
            {
                self.dragging_window_session = None;
            }
        }

        self.console_windows = next;
        changed
    }

    fn refresh_console_windows(&mut self) -> Result<bool, i32> {
        let state = console_get_state(self.console_fd.as_raw_fd())?;
        let mut changed = self.focused_session_index != state.focused_session_index;
        self.focused_session_index = state.focused_session_index;

        if !self.console_windows.is_empty()
            && self
                .console_windows
                .iter()
                .all(|window| window.session_index != self.focused_session_index)
        {
            let fallback_session =
                self.console_windows[self.console_windows.len() - 1].session_index;
            console_set_focus(self.console_fd.as_raw_fd(), fallback_session)?;
            self.focused_session_index = fallback_session;
            changed = true;
        }

        changed |= self.bring_window_to_front(self.focused_session_index);

        let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
        for window in &mut self.console_windows {
            let session_generation = state
                .output_generations
                .get(window.session_index as usize)
                .copied()
                .unwrap_or(0);
            if session_generation == window.output_generation {
                continue;
            }
            let count = console_snapshot_session_output(
                self.console_fd.as_raw_fd(),
                window.session_index,
                &mut snapshot,
            )?;
            if window.output_cache.as_slice() != &snapshot[..count] {
                window.output_cache.clear();
                window.output_cache.extend_from_slice(&snapshot[..count]);
                window.terminal_dirty = true;
                window.invalidate_surface();
                changed = true;
            }
            window.output_generation = session_generation;
        }

        Ok(changed)
    }

    fn handle_input_event(
        &mut self,
        runtime_fd: RawFd,
        event: &InputEvent,
    ) -> Result<RedrawRequest, i32> {
        match event.kind {
            INPUT_KIND_KEYBOARD => Ok(if event.action != INPUT_ACTION_RELEASED {
                RedrawRequest::full()
            } else {
                RedrawRequest::default()
            }),
            INPUT_KIND_POINTER_MOTION => {
                let next_x = (self.cursor_x as i32 + event.value0).max(0) as u32;
                let next_y = (self.cursor_y as i32 + event.value1).max(0) as u32;
                self.cursor_x = next_x.min(self.display.width.saturating_sub(1));
                self.cursor_y = next_y.min(self.display.height.saturating_sub(1));
                Ok(self
                    .drag_window_to_cursor()
                    .map(RedrawRequest::partial)
                    .unwrap_or_default())
            }
            INPUT_KIND_POINTER_BUTTON if event.code == POINTER_BUTTON_LEFT => {
                self.left_button_down = event.action == sys::INPUT_ACTION_PRESSED;
                if self.left_button_down {
                    return Ok(if self.handle_left_press(runtime_fd)? {
                        RedrawRequest::full()
                    } else {
                        RedrawRequest::default()
                    });
                }
                self.dragging_window_session = None;
                Ok(RedrawRequest::default())
            }
            _ => Ok(RedrawRequest::default()),
        }
    }

    fn handle_left_press(&mut self, runtime_fd: RawFd) -> Result<bool, i32> {
        if let Some(program_id) = self.launcher_program_under_cursor() {
            let _ = runtime_request_launch_first_available(runtime_fd, program_id);
            return Ok(true);
        }

        if let Some(session_index) = self.taskbar_window_under_cursor() {
            return self.focus_window(session_index);
        }

        if let Some((session_index, title_bar_hit)) = self.window_under_cursor() {
            let mut changed = self.focus_window(session_index)?;
            if title_bar_hit {
                self.start_window_drag(session_index);
                changed = true;
            }
            return Ok(changed);
        }

        Ok(false)
    }

    fn launcher_program_under_cursor(&self) -> Option<u32> {
        self.launcher_programs
            .iter()
            .enumerate()
            .find(|(index, _)| {
                launcher_button_rect(self.display.width, *index)
                    .contains(self.cursor_x, self.cursor_y)
            })
            .map(|(_, program)| program.program_id)
    }

    fn taskbar_window_under_cursor(&self) -> Option<u32> {
        self.console_windows
            .iter()
            .enumerate()
            .find(|(index, _)| {
                taskbar_slot_rect(self.display.width, self.display.height, *index)
                    .contains(self.cursor_x, self.cursor_y)
            })
            .map(|(_, window)| window.session_index)
    }

    fn window_under_cursor(&self) -> Option<(u32, bool)> {
        for window in self.console_windows.iter().rev() {
            if !window.frame.contains(self.cursor_x, self.cursor_y) {
                continue;
            }

            let title_bar_hit =
                console_window_title_bar_rect(window.frame).contains(self.cursor_x, self.cursor_y);
            return Some((window.session_index, title_bar_hit));
        }

        None
    }

    fn focus_window(&mut self, session_index: u32) -> Result<bool, i32> {
        let mut changed = false;
        if self.focused_session_index != session_index {
            console_set_focus(self.console_fd.as_raw_fd(), session_index)?;
            self.focused_session_index = session_index;
            changed = true;
        }

        Ok(self.bring_window_to_front(session_index) || changed)
    }

    fn bring_window_to_front(&mut self, session_index: u32) -> bool {
        let Some(index) = self
            .console_windows
            .iter()
            .position(|window| window.session_index == session_index)
        else {
            return false;
        };
        if index + 1 == self.console_windows.len() {
            return false;
        }

        let window = self.console_windows.remove(index);
        self.console_windows.push(window);
        true
    }

    fn start_window_drag(&mut self, session_index: u32) {
        let Some(window) = self
            .console_windows
            .iter()
            .find(|window| window.session_index == session_index)
        else {
            return;
        };

        self.dragging_window_session = Some(session_index);
        self.drag_offset_x = self.cursor_x.saturating_sub(window.frame.x as u32) as usize;
        self.drag_offset_y = self.cursor_y.saturating_sub(window.frame.y as u32) as usize;
    }

    fn drag_window_to_cursor(&mut self) -> Option<canvas::Rect> {
        let Some(session_index) = self.dragging_window_session else {
            return None;
        };

        let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_index == session_index)
        else {
            self.dragging_window_session = None;
            return None;
        };

        let previous_frame = window.frame;
        let next_x = self.cursor_x.saturating_sub(self.drag_offset_x as u32) as usize;
        let next_y = self.cursor_y.saturating_sub(self.drag_offset_y as u32) as usize;
        let next_frame = clamp_console_window_rect(
            self.display.width,
            self.display.height,
            canvas::Rect {
                x: next_x,
                y: next_y,
                ..window.frame
            },
        );
        if window.frame == next_frame {
            return None;
        }

        window.frame = next_frame;
        Some(previous_frame.union(next_frame))
    }

    fn present(&self) -> Result<(), i32> {
        let _keep_surface_alive = self.surface_fd.as_raw_fd();
        display_present(self.display_fd.as_raw_fd(), self.surface.handle)
    }

    fn present_rect(&self, rect: canvas::Rect) -> Result<(), i32> {
        if rect.is_empty() {
            return Ok(());
        }

        let _keep_surface_alive = self.surface_fd.as_raw_fd();
        display_present_rect(
            self.display_fd.as_raw_fd(),
            self.surface.handle,
            rect.x as u32,
            rect.y as u32,
            rect.width as u32,
            rect.height as u32,
        )
    }
}

fn refresh_runtime_state(
    runtime_fd: &OwnedFd,
    runtime_state: &Arc<Mutex<RuntimeState>>,
) -> Result<bool, i32> {
    let generation = runtime_generation(runtime_fd.as_raw_fd())?;
    let mut registered_programs = [RuntimeProgram::default(); MAX_REGISTERED_PROGRAMS];
    let registered_count =
        runtime_snapshot_programs(runtime_fd.as_raw_fd(), &mut registered_programs)?
            .min(MAX_REGISTERED_PROGRAMS);
    let mut running_programs = [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS];
    let running_count =
        runtime_snapshot_running_programs(runtime_fd.as_raw_fd(), &mut running_programs)?
            .min(MAX_RUNNING_PROGRAMS);
    let mut shared_state = runtime_state.lock().unwrap();
    let running_changed = generation != shared_state.generation
        || running_count != shared_state.running_program_count
        || shared_state.running_programs[..running_count] != running_programs[..running_count];
    let registered_changed = registered_count != shared_state.registered_program_count
        || shared_state.registered_programs[..registered_count]
            != registered_programs[..registered_count];
    if !running_changed && !registered_changed {
        return Ok(false);
    }

    shared_state.generation = generation;
    shared_state.running_programs = running_programs;
    shared_state.running_program_count = running_count;
    shared_state.registered_programs = registered_programs;
    shared_state.registered_program_count = registered_count;
    shared_state.dirty = true;
    Ok(true)
}

fn process_pending_input(
    state: &mut AppState,
    runtime_fd: RawFd,
    events: &mut [InputEvent],
) -> Result<InputProcessingResult, i32> {
    let previous_cursor_x = state.cursor_x;
    let previous_cursor_y = state.cursor_y;
    let mut needs_full_redraw = false;
    let mut partial_redraw_rect = canvas::Rect::empty();

    loop {
        let read_count = read_input(state.input_fd.as_raw_fd(), events)?;
        if read_count == 0 {
            break;
        }

        for event in &events[..read_count] {
            let redraw = state.handle_input_event(runtime_fd, event)?;
            needs_full_redraw |= redraw.needs_full_redraw;
            partial_redraw_rect = partial_redraw_rect.union(redraw.partial_redraw_rect);
        }
    }

    let cursor_moved = state.cursor_x != previous_cursor_x || state.cursor_y != previous_cursor_y;
    if cursor_moved && !partial_redraw_rect.is_empty() {
        partial_redraw_rect = partial_redraw_rect
            .union(canvas::cursor_dirty_rect(
                previous_cursor_x,
                previous_cursor_y,
                state.surface.width,
                state.surface.height,
            ))
            .union(canvas::cursor_dirty_rect(
                state.cursor_x,
                state.cursor_y,
                state.surface.width,
                state.surface.height,
            ));
    }

    Ok(InputProcessingResult {
        previous_cursor_x,
        previous_cursor_y,
        cursor_moved,
        needs_full_redraw,
        partial_redraw_rect,
    })
}

fn spawn_runtime_thread(runtime_state: Arc<Mutex<RuntimeState>>) {
    thread::spawn(move || {
        let Ok(runtime_fd) = open_runtime() else {
            raw_stderr_line("uiserver: runtime worker open failed");
            return;
        };

        loop {
            let _ = refresh_runtime_state(&runtime_fd, &runtime_state);
            thread::sleep(RUNTIME_POLL_SLEEP);
        }
    });
}

fn runtime_program_title(program: &RuntimeRunningProgram) -> String {
    runtime_program_display_name(program).into_owned()
}

fn runtime_program_is_hidden(program: &RuntimeRunningProgram) -> bool {
    let title = runtime_program_display_name(program);
    runtime_title_is_hidden(title.as_ref())
}

fn runtime_title_is_hidden(title: &str) -> bool {
    HIDDEN_RUNTIME_PROGRAM_TITLES
        .iter()
        .any(|hidden| *hidden == title)
}

fn runtime_program_display_name(program: &RuntimeRunningProgram) -> std::borrow::Cow<'_, str> {
    let end = program
        .display_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(program.display_name.len());
    let bytes = &program.display_name[..end];
    String::from_utf8_lossy(bytes)
}

fn run() -> Result<(), i32> {
    let mut state = AppState::initialize()?;
    let runtime_state = Arc::new(Mutex::new(RuntimeState::default()));
    let runtime_fd = open_runtime().map_err(|_| {
        raw_stderr_line("uiserver: open_runtime failed");
        19
    })?;
    let _ = refresh_runtime_state(&runtime_fd, &runtime_state);
    let _ = state.apply_runtime_state(&runtime_state);
    let _ = state.refresh_console_windows();
    let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];

    render_frame(&mut state);
    state.present()?;

    spawn_runtime_thread(runtime_state.clone());

    loop {
        let input = process_pending_input(&mut state, runtime_fd.as_raw_fd(), &mut events)?;
        let mut needs_full_redraw = input.needs_full_redraw;
        needs_full_redraw |= state.apply_runtime_state(&runtime_state);
        needs_full_redraw |= state.refresh_console_windows()?;
        if needs_full_redraw {
            render_frame(&mut state);
            state.present()?;
            continue;
        }
        if !input.partial_redraw_rect.is_empty() {
            render_rect(&mut state, input.partial_redraw_rect);
            state.present_rect(input.partial_redraw_rect)?;
            continue;
        }
        if input.cursor_moved {
            let previous_rect = canvas::cursor_dirty_rect(
                input.previous_cursor_x,
                input.previous_cursor_y,
                state.surface.width,
                state.surface.height,
            );
            let current_rect = canvas::cursor_dirty_rect(
                state.cursor_x,
                state.cursor_y,
                state.surface.width,
                state.surface.height,
            );
            render_cursor_only(&mut state, previous_rect, current_rect);
            state.present_rect(previous_rect)?;
            if current_rect != previous_rect {
                state.present_rect(current_rect)?;
            }
            continue;
        }

        thread::sleep(IDLE_SLEEP);
    }
}

fn main() {
    let exit_code = match run() {
        Ok(()) => 0,
        Err(code) => code,
    };
    if exit_code != 0 {
        raw_stderr_line("uiserver: exiting with nonzero status");
    }
    std::process::exit(exit_code);
}
fn runtime_program_name(program: &RuntimeProgram) -> std::borrow::Cow<'_, str> {
    let end = program
        .display_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(program.display_name.len());
    let bytes = &program.display_name[..end];
    String::from_utf8_lossy(bytes)
}
