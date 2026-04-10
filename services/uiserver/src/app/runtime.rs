use std::os::fd::AsRawFd;
use std::vec::Vec;

use super::{AppState, ConsoleWindow, DragTarget, MAX_RUNNING_PROGRAMS};
use crate::canvas;
use crate::render::{self, default_console_window_rect, taskbar_slot_rect};
use crate::runtime_sync::{runtime_program_is_hidden, runtime_program_title, RuntimeState};
use crate::sys::{
    console_get_state, console_set_focus, console_snapshot_session_output,
    console_snapshot_sessions, ConsoleSessionHandle, ConsoleSessionInfo,
    MAX_CONSOLE_SNAPSHOT_BYTES,
};
use crate::wayland::WaylandCompositor;
use runtime_control::RuntimeRunningProgram;

impl AppState {
    pub(crate) fn apply_runtime_state(&mut self, runtime_state: &mut RuntimeState) -> bool {
        if !runtime_state.dirty {
            return false;
        }

        let changed = self.sync_windows_from_runtime(
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

        let mut changed = self.prune_windows(|session_handle| {
            sessions
                .iter()
                .any(|session| session.session_handle == session_handle)
        });
        self.clamp_console_snapshot_index();
        changed |= self.reconcile_console_focus(state.focused_session_handle)?;

        let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
        let mut stale_sessions = Vec::new();
        let mut snapshot_candidates = Vec::new();
        for (index, window) in self.console_windows.iter_mut().enumerate() {
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
            snapshot_candidates.push(index);
        }

        if let Some(index) = self.select_console_snapshot_candidate(&snapshot_candidates) {
            let session_handle = self.console_windows[index].session_handle;
            let count = match console_snapshot_session_output(
                self.console_fd.as_raw_fd(),
                session_handle,
                &mut snapshot,
            ) {
                Ok(count) => count,
                Err(err)
                    if matches!(
                        err,
                        crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE
                    ) =>
                {
                    stale_sessions.push(session_handle);
                    0
                }
                Err(err) => return Err(err),
            };
            if stale_sessions.is_empty() {
                let window = &mut self.console_windows[index];
                if window.output_cache.as_slice() != &snapshot[..count] {
                    window.output_cache.clear();
                    window.output_cache.extend_from_slice(&snapshot[..count]);
                    window.terminal_dirty = true;
                    window.invalidate_surface();
                    changed = true;
                }
                window.output_generation = sessions
                    .iter()
                    .find(|session| session.session_handle == session_handle)
                    .map(|session| session.output_generation)
                    .unwrap_or(window.output_generation);
                self.next_console_snapshot_index = (index + 1) % self.console_windows.len().max(1);
            }
        } else {
            self.clamp_console_snapshot_index();
        }

        if !stale_sessions.is_empty() {
            changed |=
                self.prune_windows(|session_handle| !stale_sessions.contains(&session_handle));
            self.clamp_console_snapshot_index();
            changed |= self.reconcile_console_focus(0)?;
        }

        Ok(changed)
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

        if let Some(DragTarget::Console(session_handle)) = self.dragging_window {
            if next
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window = None;
                changed = true;
            }
        }

        if next
            .iter()
            .all(|window| window.session_handle != self.focused_session_handle)
        {
            if self.focused_session_handle != 0 {
                self.focused_session_handle = 0;
                changed = true;
            }
        }

        self.console_windows = next;
        changed
    }

    fn reconcile_console_focus(
        &mut self,
        kernel_focused_session: ConsoleSessionHandle,
    ) -> Result<bool, i32> {
        let previous_focused = self.focused_session_handle;
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if self.console_windows.is_empty() {
            self.focused_session_handle = 0;
            return Ok(previous_focused != 0);
        }

        if wayland_focused {
            self.focused_session_handle = 0;
            return Ok(previous_focused != 0);
        }

        self.focused_session_handle = if self
            .console_windows
            .iter()
            .any(|window| window.session_handle == kernel_focused_session && !window.minimized)
        {
            kernel_focused_session
        } else {
            0
        };

        let mut changed = self.focused_session_handle != previous_focused;
        while self.focused_session_handle == 0 && !self.console_windows.is_empty() {
            let Some(fallback_session) = self
                .console_windows
                .iter()
                .rev()
                .find(|window| !window.minimized)
                .map(|window| window.session_handle)
            else {
                break;
            };
            match console_set_focus(self.console_fd.as_raw_fd(), fallback_session) {
                Ok(()) => {
                    self.focused_session_handle = fallback_session;
                    changed = true;
                }
                Err(err)
                    if matches!(
                        err,
                        crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE
                    ) =>
                {
                    changed |=
                        self.prune_windows(|session_handle| session_handle != fallback_session);
                }
                Err(err) => return Err(err),
            }
        }

        if self.focused_session_handle != 0 {
            changed |= self.bring_window_to_front(self.focused_session_handle);
        }
        Ok(changed)
    }

    pub(crate) fn recover_focus_after_wayland_change(
        &mut self,
        _wayland: Option<&mut WaylandCompositor>,
    ) -> Result<bool, i32> {
        let previous_wayland_focus = self.focused_wayland_surface_id;
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if wayland_focused {
            return Ok(false);
        }

        self.focused_wayland_surface_id = None;

        let console_focused = self.focused_session_handle != 0
            && self.console_windows.iter().any(|window| {
                !window.minimized && window.session_handle == self.focused_session_handle
            });
        if console_focused {
            return Ok(previous_wayland_focus.is_some());
        }

        let console_refocused = self.refocus_visible_console_window()?;
        Ok(previous_wayland_focus.is_some() || console_refocused)
    }

    fn prune_windows(&mut self, mut keep: impl FnMut(ConsoleSessionHandle) -> bool) -> bool {
        let previous_len = self.console_windows.len();
        let previous_focused = self.focused_session_handle;
        let previous_dragging = self.dragging_window;
        self.console_windows
            .retain(|window| keep(window.session_handle));
        self.clamp_console_snapshot_index();

        if self
            .console_windows
            .iter()
            .all(|window| window.session_handle != self.focused_session_handle)
        {
            self.focused_session_handle = 0;
        }

        if let Some(DragTarget::Console(session_handle)) = self.dragging_window {
            if self
                .console_windows
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window = None;
            }
        }

        previous_len != self.console_windows.len()
            || previous_focused != self.focused_session_handle
            || previous_dragging != self.dragging_window
    }

    fn select_console_snapshot_candidate(&self, candidates: &[usize]) -> Option<usize> {
        if candidates.is_empty() {
            return None;
        }

        let start = self
            .next_console_snapshot_index
            .min(self.console_windows.len().saturating_sub(1));
        candidates
            .iter()
            .copied()
            .find(|index| *index >= start)
            .or_else(|| candidates.first().copied())
    }

    fn clamp_console_snapshot_index(&mut self) {
        if self.console_windows.is_empty() {
            self.next_console_snapshot_index = 0;
        } else if self.next_console_snapshot_index >= self.console_windows.len() {
            self.next_console_snapshot_index %= self.console_windows.len();
        }
    }

    pub(super) fn focus_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<canvas::Rect, i32> {
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
        {
            window.minimized = false;
        }
        self.focused_wayland_surface_id = None;
        let previous_focused = self.focused_session_handle;
        let mut dirty_rect = self.window_rect_for_session(previous_focused);
        dirty_rect = dirty_rect.union(self.window_rect_for_session(session_handle));
        if self.focused_session_handle != session_handle {
            match console_set_focus(self.console_fd.as_raw_fd(), session_handle) {
                Ok(()) => {
                    self.focused_session_handle = session_handle;
                }
                Err(err)
                    if matches!(
                        err,
                        crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE
                    ) =>
                {
                    self.prune_windows(|existing_session| existing_session != session_handle);
                    return Ok(dirty_rect
                        .union(self.window_stack_dirty_rect())
                        .union(self.taskbar_dirty_rect()));
                }
                Err(err) => return Err(err),
            }
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

    pub(super) fn refocus_visible_console_window(&mut self) -> Result<bool, i32> {
        let previous_focused = self.focused_session_handle;
        let Some(session_handle) = self
            .console_windows
            .iter()
            .rev()
            .find(|window| !window.minimized)
            .map(|window| window.session_handle)
        else {
            self.focused_session_handle = 0;
            return Ok(previous_focused != 0);
        };

        if self.focused_session_handle != session_handle {
            console_set_focus(self.console_fd.as_raw_fd(), session_handle)?;
            self.focused_session_handle = session_handle;
        }

        let reordered = self.bring_window_to_front(session_handle);
        Ok(previous_focused != self.focused_session_handle || reordered)
    }

    fn window_rect_for_session(&self, session_handle: ConsoleSessionHandle) -> canvas::Rect {
        self.console_windows
            .iter()
            .find(|window| window.session_handle == session_handle && !window.minimized)
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
                if window.minimized {
                    dirty
                } else {
                    dirty.union(window.frame)
                }
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
