use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::vec::Vec;

use super::{
    AppState, ConsoleWindow, DragTarget, VisualUpdate, CONSOLE_POLL_SLEEP, MAX_RUNNING_PROGRAMS,
};
use crate::canvas;
use crate::render::{self, default_console_window_rect, taskbar_slot_rect};
use crate::runtime_sync::{runtime_program_is_hidden, runtime_program_title, RuntimeState};
use crate::sys::{
    console_get_state, console_set_focus, console_snapshot_session_output,
    console_snapshot_sessions, open_console, ConsoleSessionHandle, ConsoleSessionInfo,
    ConsoleStateInfo, MAX_CONSOLE_SNAPSHOT_BYTES,
};
use crate::wayland::WaylandCompositor;
use runtime_control::RuntimeRunningProgram;

pub(crate) struct ConsoleRefresh {
    state: ConsoleStateInfo,
    sessions: Vec<ConsoleSessionInfo>,
    outputs: Vec<ConsoleSessionOutput>,
}

struct ConsoleSessionOutput {
    session_handle: ConsoleSessionHandle,
    output_generation: u64,
    bytes: Vec<u8>,
}

pub(crate) fn start_console_refresh_worker() -> Receiver<ConsoleRefresh> {
    let (sender, receiver) = mpsc::sync_channel(2);
    thread::spawn(move || {
        let Ok(console_fd) = open_console() else {
            return;
        };
        let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
        let mut output_generations = BTreeMap::<ConsoleSessionHandle, u64>::new();
        loop {
            if let Ok(refresh) = collect_console_refresh(
                console_fd.as_raw_fd(),
                &mut snapshot,
                &mut output_generations,
            ) {
                if sender.try_send(refresh).is_err() {
                    output_generations.clear();
                }
            }
            thread::sleep(CONSOLE_POLL_SLEEP);
        }
    });
    receiver
}

fn collect_console_refresh(
    console_fd: i32,
    snapshot: &mut [u8; MAX_CONSOLE_SNAPSHOT_BYTES],
    output_generations: &mut BTreeMap<ConsoleSessionHandle, u64>,
) -> Result<ConsoleRefresh, i32> {
    let state = console_get_state(console_fd)?;
    let mut sessions = [ConsoleSessionInfo::default(); crate::sys::CONSOLE_SESSION_CAPACITY];
    let session_count = console_snapshot_sessions(console_fd, &mut sessions)?
        .min(crate::sys::CONSOLE_SESSION_CAPACITY);
    let sessions = sessions[..session_count].to_vec();
    output_generations.retain(|session_handle, _| {
        sessions
            .iter()
            .any(|session| session.session_handle == *session_handle)
    });
    let mut outputs = Vec::new();
    for session in &sessions {
        let previous_generation = output_generations
            .get(&session.session_handle)
            .copied()
            .unwrap_or(0);
        if previous_generation == session.output_generation {
            continue;
        }
        match console_snapshot_session_output(console_fd, session.session_handle, snapshot) {
            Ok(count) => outputs.push(ConsoleSessionOutput {
                session_handle: session.session_handle,
                output_generation: session.output_generation,
                bytes: snapshot[..count].to_vec(),
            }),
            Err(err)
                if matches!(
                    err,
                    crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE
                ) => {}
            Err(err) => return Err(err),
        }
        output_generations.insert(session.session_handle, session.output_generation);
    }

    Ok(ConsoleRefresh {
        state,
        sessions,
        outputs,
    })
}

impl AppState {
    pub(crate) fn apply_runtime_state(&mut self, runtime_state: &mut RuntimeState) -> VisualUpdate {
        if !runtime_state.dirty {
            return VisualUpdate::default();
        }

        let mut update = self.sync_windows_from_runtime(
            &runtime_state.running_programs[..runtime_state
                .running_program_count
                .min(MAX_RUNNING_PROGRAMS)],
        );
        runtime_state.dirty = false;
        update.add_partial_rect(self.focused_window_reorder_dirty_rect());
        update
    }

    pub(crate) fn apply_console_refresh(
        &mut self,
        refresh: ConsoleRefresh,
    ) -> Result<VisualUpdate, i32> {
        let sessions = refresh.sessions.as_slice();
        let before_dirty = self
            .console_stack_dirty_rect()
            .union(self.taskbar_dirty_rect());
        let mut update = VisualUpdate::default();
        let pruned = self.prune_windows(|session_handle| {
            sessions
                .iter()
                .any(|session| session.session_handle == session_handle)
        });
        let added_update = self.add_missing_console_windows(sessions);
        if pruned {
            update.add_partial_rect(before_dirty);
            update.add_partial_rect(self.console_stack_dirty_rect());
            update.add_partial_rect(self.taskbar_dirty_rect());
        } else if !added_update.is_empty() {
            update.absorb(added_update);
            update.add_partial_rect(self.taskbar_dirty_rect());
        }
        self.clamp_console_snapshot_index();
        update
            .add_partial_rect(self.reconcile_console_focus(refresh.state.focused_session_handle)?);

        let taskbar_dirty_rect = self.taskbar_dirty_rect();
        for window in self.console_windows.iter_mut() {
            let Some(session) = sessions
                .iter()
                .find(|session| session.session_handle == window.session_handle)
            else {
                continue;
            };
            let session_title = console_session_title(session);
            if !session_title.is_empty() && window.title != session_title {
                window.title = session_title;
                window.invalidate_surface();
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
                update.add_partial_rect(taskbar_dirty_rect);
            }
            if session.output_generation == window.output_generation {
                continue;
            }
            let output = refresh
                .outputs
                .iter()
                .find(|output| output.session_handle == window.session_handle);
            let Some(output) = output else {
                continue;
            };
            if window.output_cache.as_slice() != output.bytes.as_slice() {
                window.output_cache.clear();
                window
                    .output_cache
                    .extend_from_slice(output.bytes.as_slice());
                window.terminal_dirty = true;
                window.invalidate_surface();
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
            }
            window.output_generation = output.output_generation;
        }

        Ok(update)
    }

    fn add_missing_console_windows(&mut self, sessions: &[ConsoleSessionInfo]) -> VisualUpdate {
        let mut known_sessions = self
            .console_windows
            .iter()
            .map(|window| window.session_handle)
            .collect::<Vec<_>>();
        let mut update = VisualUpdate::default();

        for session in sessions {
            let session_handle = session.session_handle;
            if session_handle == 0
                || self.is_console_session_closing(session_handle)
                || known_sessions.contains(&session_handle)
            {
                continue;
            }

            let frame = default_console_window_rect(
                self.display.width,
                self.display.height,
                self.console_windows.len(),
            );
            self.console_windows.push(ConsoleWindow::new(
                session_handle,
                console_session_title(session),
                frame,
                0,
            ));
            known_sessions.push(session_handle);
            update.add_partial_rect(crate::render::console_window_dirty_rect(frame));
        }

        update
    }

    fn sync_windows_from_runtime(&mut self, programs: &[RuntimeRunningProgram]) -> VisualUpdate {
        let before_dirty = self
            .console_stack_dirty_rect()
            .union(self.taskbar_dirty_rect());
        let taskbar_dirty_rect = self.taskbar_dirty_rect();
        self.closing_console_sessions.retain(|session_handle| {
            programs
                .iter()
                .any(|program| program.session_handle == *session_handle)
        });
        let existing = std::mem::take(&mut self.console_windows);
        let mut next = Vec::with_capacity(programs.len());
        let mut update = VisualUpdate::default();
        let mut topology_changed = false;
        let mut kept_sessions = Vec::with_capacity(programs.len());

        for mut window in existing {
            if self.is_console_session_closing(window.session_handle) {
                topology_changed = true;
                continue;
            }
            let Some(program) = programs
                .iter()
                .find(|program| program.session_handle == window.session_handle)
            else {
                topology_changed = true;
                continue;
            };
            if runtime_program_is_hidden(program) || program.session_handle == 0 {
                topology_changed = true;
                continue;
            }

            let new_title = runtime_program_title(program);
            if window.title != new_title {
                window.title = new_title;
                window.invalidate_surface();
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
                update.add_partial_rect(taskbar_dirty_rect);
            }
            kept_sessions.push(window.session_handle);
            next.push(window);
        }

        for program in programs {
            if runtime_program_is_hidden(program)
                || program.session_handle == 0
                || self.is_console_session_closing(program.session_handle)
                || kept_sessions.contains(&program.session_handle)
            {
                continue;
            }

            let frame =
                default_console_window_rect(self.display.width, self.display.height, next.len());
            next.push(ConsoleWindow::new(
                program.session_handle,
                runtime_program_title(program),
                frame,
                0,
            ));
            update.add_partial_rect(crate::render::console_window_dirty_rect(frame));
            update.add_partial_rect(taskbar_dirty_rect);
        }

        if let Some(DragTarget::Console(session_handle)) = self.dragging_window {
            if next
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window = None;
                topology_changed = true;
            }
        }

        if next
            .iter()
            .all(|window| window.session_handle != self.focused_session_handle)
        {
            if self.focused_session_handle != 0 {
                self.focused_session_handle = 0;
                topology_changed = true;
            }
        }

        self.console_windows = next;
        if topology_changed {
            let mut update = VisualUpdate::partial(before_dirty);
            update.add_partial_rect(self.console_stack_dirty_rect());
            update.add_partial_rect(self.taskbar_dirty_rect());
            update
        } else {
            update
        }
    }

    fn reconcile_console_focus(
        &mut self,
        kernel_focused_session: ConsoleSessionHandle,
    ) -> Result<canvas::Rect, i32> {
        let previous_focused = self.focused_session_handle;
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if self.console_windows.is_empty() {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
        }

        if wayland_focused {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
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

        let mut dirty_rect = if previous_focused == 0 {
            self.window_rect_for_session(self.focused_session_handle)
        } else {
            self.window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused))
                .union(self.taskbar_slot_rect_for_session(self.focused_session_handle))
                .union(self.window_rect_for_session(self.focused_session_handle))
        };
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
                    dirty_rect = dirty_rect.union(self.window_rect_for_session(fallback_session));
                    if previous_focused != 0 {
                        dirty_rect =
                            dirty_rect.union(self.taskbar_slot_rect_for_session(fallback_session));
                    }
                }
                Err(err)
                    if matches!(
                        err,
                        crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE
                    ) =>
                {
                    let pruned =
                        self.prune_windows(|session_handle| session_handle != fallback_session);
                    if pruned {
                        dirty_rect = dirty_rect
                            .union(self.console_stack_dirty_rect())
                            .union(self.taskbar_dirty_rect());
                    }
                }
                Err(err) => return Err(err),
            }
        }

        if self.focused_session_handle != 0 {
            dirty_rect = dirty_rect.union(self.focused_window_reorder_dirty_rect());
        }
        Ok(dirty_rect)
    }

    pub(crate) fn recover_focus_after_wayland_change(
        &mut self,
        _wayland: Option<&mut WaylandCompositor>,
    ) -> Result<canvas::Rect, i32> {
        let previous_wayland_focus = self.focused_wayland_surface_id;
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if wayland_focused {
            return Ok(canvas::Rect::empty());
        }

        self.focused_wayland_surface_id = None;

        let console_focused = self.focused_session_handle != 0
            && self.console_windows.iter().any(|window| {
                !window.minimized && window.session_handle == self.focused_session_handle
            });
        if console_focused {
            return Ok(if previous_wayland_focus.is_some() {
                self.wayland_stack_dirty_rect()
                    .union(self.wayland_taskbar_dirty_rect())
            } else {
                canvas::Rect::empty()
            });
        }

        let console_refocused = self.refocus_visible_console_window()?;
        Ok(if previous_wayland_focus.is_some() {
            self.wayland_stack_dirty_rect()
                .union(self.wayland_taskbar_dirty_rect())
                .union(console_refocused)
        } else {
            console_refocused
        })
    }

    pub(super) fn mark_console_session_closing(&mut self, session_handle: ConsoleSessionHandle) {
        if session_handle == 0 || self.is_console_session_closing(session_handle) {
            return;
        }
        self.closing_console_sessions.push(session_handle);
        if self.closing_console_sessions.len() > MAX_RUNNING_PROGRAMS {
            let overflow = self.closing_console_sessions.len() - MAX_RUNNING_PROGRAMS;
            self.closing_console_sessions.drain(..overflow);
        }
    }

    fn is_console_session_closing(&self, session_handle: ConsoleSessionHandle) -> bool {
        self.closing_console_sessions.contains(&session_handle)
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

    pub(super) fn refocus_visible_console_window(&mut self) -> Result<canvas::Rect, i32> {
        let previous_focused = self.focused_session_handle;
        let Some(session_handle) = self
            .console_windows
            .iter()
            .rev()
            .find(|window| !window.minimized)
            .map(|window| window.session_handle)
        else {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
        };

        if self.focused_session_handle != session_handle {
            console_set_focus(self.console_fd.as_raw_fd(), session_handle)?;
            self.focused_session_handle = session_handle;
        }

        Ok(self
            .window_rect_for_session(previous_focused)
            .union(self.window_rect_for_session(session_handle))
            .union(self.taskbar_slot_rect_for_session(previous_focused))
            .union(self.taskbar_slot_rect_for_session(session_handle))
            .union(self.focused_window_reorder_dirty_rect()))
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
                    dirty.union(crate::render::console_window_dirty_rect(window.frame))
                }
            })
    }

    fn taskbar_dirty_rect(&self) -> canvas::Rect {
        render::taskbar_dirty_rect(self.display.width, self.display.height)
    }

    fn console_stack_dirty_rect(&self) -> canvas::Rect {
        self.window_stack_dirty_rect()
    }

    fn focused_window_reorder_dirty_rect(&mut self) -> canvas::Rect {
        let session_handle = self.focused_session_handle;
        if session_handle == 0 {
            return canvas::Rect::empty();
        }
        if self.bring_window_to_front(session_handle) {
            self.window_stack_dirty_rect()
                .union(self.taskbar_dirty_rect())
        } else {
            self.window_rect_for_session(session_handle)
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
