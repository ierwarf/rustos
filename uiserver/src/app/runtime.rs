use std::os::fd::AsRawFd;
use std::vec::Vec;

use super::{
    AppState, ConsoleWindow, LauncherProgram, MAX_REGISTERED_PROGRAMS, MAX_RUNNING_PROGRAMS,
};
use crate::canvas;
use crate::render::{self, default_console_window_rect, taskbar_slot_rect};
use crate::runtime_sync::{
    runtime_program_is_hidden, runtime_program_name, runtime_program_title,
    runtime_title_is_hidden, RuntimeState,
};
use crate::sys::{
    console_get_state, console_set_focus, console_snapshot_session_output,
    console_snapshot_sessions, ConsoleSessionHandle, ConsoleSessionInfo, RuntimeProgram,
    RuntimeRunningProgram, MAX_CONSOLE_SNAPSHOT_BYTES,
};

impl AppState {
    pub(crate) fn apply_runtime_state(&mut self, runtime_state: &mut RuntimeState) -> bool {
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
        changed || self.bring_window_to_front(self.focused_session_handle)
    }

    pub(crate) fn refresh_console_windows(&mut self) -> Result<bool, i32> {
        let state = console_get_state(self.console_fd.as_raw_fd())?;
        let mut sessions = [ConsoleSessionInfo::default(); crate::sys::CONSOLE_SESSION_CAPACITY];
        let session_count = console_snapshot_sessions(self.console_fd.as_raw_fd(), &mut sessions)?
            .min(crate::sys::CONSOLE_SESSION_CAPACITY);
        let sessions = &sessions[..session_count];

        let mut changed = self.focused_session_handle != state.focused_session_handle;
        self.focused_session_handle = state.focused_session_handle;

        if !self.console_windows.is_empty()
            && self
                .console_windows
                .iter()
                .all(|window| window.session_handle != self.focused_session_handle)
        {
            let fallback_session =
                self.console_windows[self.console_windows.len() - 1].session_handle;
            console_set_focus(self.console_fd.as_raw_fd(), fallback_session)?;
            self.focused_session_handle = fallback_session;
            changed = true;
        }

        changed |= self.bring_window_to_front(self.focused_session_handle);

        let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
        for window in &mut self.console_windows {
            let Some(session) = sessions
                .iter()
                .find(|session| session.session_handle == window.session_handle)
            else {
                continue;
            };
            let session_generation = session.output_generation;
            if session_generation == window.output_generation {
                continue;
            }
            let session_title = console_session_title(session);
            if !session_title.is_empty() && window.title != session_title {
                window.title = session_title;
                window.invalidate_surface();
                changed = true;
            }
            let count = console_snapshot_session_output(
                self.console_fd.as_raw_fd(),
                window.session_handle,
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
                .find(|program| program.session_handle == window.session_handle)
            else {
                changed = true;
                continue;
            };
            if runtime_program_is_hidden(program) || program.session_handle == 0 {
                changed = true;
                continue;
            }

            let new_title = runtime_program_title(program);
            if window.title != new_title {
                window.title = new_title;
                window.invalidate_surface();
                changed = true;
            }
            kept_sessions.push(window.session_handle);
            next.push(window);
        }

        for program in programs {
            if runtime_program_is_hidden(program)
                || program.session_handle == 0
                || kept_sessions.contains(&program.session_handle)
            {
                continue;
            }

            next.push(ConsoleWindow::new(
                program.session_handle,
                runtime_program_title(program),
                default_console_window_rect(self.display.width, self.display.height, next.len()),
                0,
            ));
            changed = true;
        }

        if let Some(session_handle) = self.dragging_window_session {
            if next
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window_session = None;
            }
        }

        self.console_windows = next;
        changed
    }

    pub(super) fn focus_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<canvas::Rect, i32> {
        let previous_focused = self.focused_session_handle;
        let mut dirty_rect = self.window_rect_for_session(previous_focused);
        dirty_rect = dirty_rect.union(self.window_rect_for_session(session_handle));
        if self.focused_session_handle != session_handle {
            console_set_focus(self.console_fd.as_raw_fd(), session_handle)?;
            self.focused_session_handle = session_handle;
        }

        if self.bring_window_to_front(session_handle) {
            dirty_rect = dirty_rect
                .union(self.window_stack_dirty_rect())
                .union(self.taskbar_dirty_rect());
        } else {
            dirty_rect = dirty_rect
                .union(self.taskbar_slot_rect_for_session(previous_focused))
                .union(self.taskbar_slot_rect_for_session(session_handle));
        }

        Ok(dirty_rect)
    }

    pub(super) fn bring_window_to_front(&mut self, session_handle: ConsoleSessionHandle) -> bool {
        let Some(index) = self
            .console_windows
            .iter()
            .position(|window| window.session_handle == session_handle)
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

    fn window_rect_for_session(&self, session_handle: ConsoleSessionHandle) -> canvas::Rect {
        self.console_windows
            .iter()
            .find(|window| window.session_handle == session_handle)
            .map(|window| window.frame)
            .unwrap_or_default()
    }

    fn taskbar_slot_rect_for_session(&self, session_handle: ConsoleSessionHandle) -> canvas::Rect {
        self.console_windows
            .iter()
            .enumerate()
            .find(|(_, window)| window.session_handle == session_handle)
            .map(|(index, _)| taskbar_slot_rect(self.display.width, self.display.height, index))
            .unwrap_or_default()
    }

    fn window_stack_dirty_rect(&self) -> canvas::Rect {
        self.console_windows
            .iter()
            .fold(canvas::Rect::empty(), |dirty, window| {
                dirty.union(window.frame)
            })
    }

    fn taskbar_dirty_rect(&self) -> canvas::Rect {
        canvas::Rect {
            x: 0,
            y: (self.display.height as usize).saturating_sub(render::TASKBAR_HEIGHT),
            width: self.display.width as usize,
            height: render::TASKBAR_HEIGHT,
        }
    }
}

fn console_session_title(session: &ConsoleSessionInfo) -> String {
    let end = session
        .title
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(session.title.len());
    String::from_utf8_lossy(&session.title[..end]).into_owned()
}
