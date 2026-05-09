use std::os::fd::AsRawFd;
use std::string::String;
use std::time::Instant;

use runtime_control::RuntimeClient;

use super::{AppState, DragTarget, VisualUpdate};
use crate::canvas;
use crate::profile;
use crate::render::{
    clamp_console_window_rect, launcher_button_rect, taskbar_slot_rect, wayland_window_outer_rect,
    window_close_button_rect, window_minimize_button_rect, window_title_bar_rect,
};
use crate::sys::{
    self, console_send_input_event, console_set_focus, ConsoleSessionHandle, InputEvent,
    INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    INPUT_KIND_POINTER_POSITION, POINTER_BUTTON_LEFT,
};
use crate::wayland::WaylandCompositor;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskbarHit {
    Console(ConsoleSessionHandle),
    Wayland(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowChromeHit {
    Client,
    TitleBar,
    Minimize,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowHit {
    Console(ConsoleSessionHandle, WindowChromeHit),
    Wayland(u32, WindowChromeHit),
}

impl AppState {
    pub(crate) fn handle_input_event(
        &mut self,
        runtime: &RuntimeClient,
        event: &InputEvent,
        mut wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        match event.kind {
            INPUT_KIND_KEYBOARD => {
                if let Some(wayland) = wayland {
                    if wayland.keyboard_input(*event) {
                        return Ok(VisualUpdate::default());
                    }
                }
                if self.focused_session_handle != 0 {
                    match console_send_input_event(
                        self.console_fd.as_raw_fd(),
                        self.focused_session_handle,
                        *event,
                    ) {
                        Ok(()) => {}
                        Err(err) if matches!(err, sys::ENOENT | sys::EINVAL | sys::ESTALE) => {
                            self.focused_session_handle = 0;
                            return Ok(VisualUpdate::default());
                        }
                        Err(err) => return Err(err),
                    }
                }
                Ok(self
                    .reveal_focused_terminal_cursor()
                    .map(VisualUpdate::partial)
                    .unwrap_or_default())
            }
            INPUT_KIND_POINTER_MOTION => {
                let previous_x = self.cursor_x;
                let previous_y = self.cursor_y;
                let next_x = (self.cursor_x as i32 + event.value0).max(0) as u32;
                let next_y = (self.cursor_y as i32 + event.value1).max(0) as u32;
                self.cursor_x = next_x.min(self.display.width.saturating_sub(1));
                self.cursor_y = next_y.min(self.display.height.saturating_sub(1));
                self.record_cursor_motion(previous_x, previous_y);
                let drag_update = self.drag_window_to_cursor(wayland.as_deref_mut());
                if !drag_update.is_empty() {
                    return Ok(drag_update);
                }
                if let Some(wayland) = wayland.as_deref_mut() {
                    let started = Instant::now();
                    wayland.pointer_motion(self.cursor_x, self.cursor_y);
                    profile::record_wayland_motion(started.elapsed());
                }
                Ok(VisualUpdate::default())
            }
            INPUT_KIND_POINTER_POSITION => {
                let previous_x = self.cursor_x;
                let previous_y = self.cursor_y;
                self.cursor_x =
                    (event.value0.max(0) as u32).min(self.display.width.saturating_sub(1));
                self.cursor_y =
                    (event.value1.max(0) as u32).min(self.display.height.saturating_sub(1));
                self.record_cursor_motion(previous_x, previous_y);
                let drag_update = self.drag_window_to_cursor(wayland.as_deref_mut());
                if !drag_update.is_empty() {
                    return Ok(drag_update);
                }
                if let Some(wayland) = wayland.as_deref_mut() {
                    let started = Instant::now();
                    wayland.pointer_motion(self.cursor_x, self.cursor_y);
                    profile::record_wayland_motion(started.elapsed());
                }
                Ok(VisualUpdate::default())
            }
            INPUT_KIND_POINTER_BUTTON if event.code == POINTER_BUTTON_LEFT => {
                self.left_button_down = event.action == sys::INPUT_ACTION_PRESSED;
                if self.left_button_down {
                    return self.handle_left_press(runtime, wayland);
                }
                let was_dragging = self.dragging_window.take().is_some();
                if was_dragging {
                    return Ok(VisualUpdate::default());
                }
                if let Some(wayland) = wayland {
                    wayland.pointer_button(POINTER_BUTTON_LEFT, false);
                }
                Ok(VisualUpdate::default())
            }
            _ => Ok(VisualUpdate::default()),
        }
    }

    fn handle_left_press(
        &mut self,
        runtime: &RuntimeClient,
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        if let Some(desktop_file_id) = self.launcher_program_under_cursor() {
            let _ = runtime.request_launch_program_new_session(desktop_file_id.as_str());
            return Ok(VisualUpdate::default());
        }

        if let Some(hit) = self.taskbar_window_under_cursor() {
            return match hit {
                TaskbarHit::Console(session_handle) => {
                    if let Some(wayland) = wayland {
                        wayland.clear_focus();
                    }
                    self.focused_wayland_surface_id = None;
                    self.restore_console_window(session_handle)
                }
                TaskbarHit::Wayland(surface_id) => self.restore_wayland_window(surface_id, wayland),
            };
        }

        if let Some(hit) = self.window_under_cursor() {
            return self.handle_window_hit(runtime, hit, wayland);
        }

        if let Some(wayland) = wayland {
            if wayland.pointer_button(POINTER_BUTTON_LEFT, true) {
                return Ok(VisualUpdate::full());
            }
        }

        Ok(VisualUpdate::default())
    }

    fn launcher_program_under_cursor(&self) -> Option<String> {
        self.launcher_programs
            .iter()
            .enumerate()
            .find(|(index, _)| {
                launcher_button_rect(self.display.width, *index)
                    .contains(self.cursor_x, self.cursor_y)
            })
            .map(|(_, program)| program.desktop_file_id.clone())
    }

    fn taskbar_window_under_cursor(&self) -> Option<TaskbarHit> {
        if let Some((_, window)) = self.console_windows.iter().enumerate().find(|(index, _)| {
            taskbar_slot_rect(self.display.width, self.display.height, *index)
                .contains(self.cursor_x, self.cursor_y)
        }) {
            return Some(TaskbarHit::Console(window.session_handle));
        }

        self.wayland_windows
            .iter()
            .enumerate()
            .find(|(index, _)| {
                taskbar_slot_rect(
                    self.display.width,
                    self.display.height,
                    self.console_windows.len().saturating_add(*index),
                )
                .contains(self.cursor_x, self.cursor_y)
            })
            .map(|(_, window)| TaskbarHit::Wayland(window.surface_id))
    }

    fn window_under_cursor(&self) -> Option<WindowHit> {
        if let Some(surface_id) = self.focused_wayland_surface_id {
            if let Some(window) = self
                .wayland_windows
                .iter()
                .find(|window| !window.minimized && window.surface_id == surface_id)
            {
                let outer = wayland_window_outer_rect(window);
                if outer.contains(self.cursor_x, self.cursor_y) {
                    return Some(WindowHit::Wayland(
                        window.surface_id,
                        self.hit_window_chrome(outer),
                    ));
                }
            }
        }

        if self.focused_wayland_surface_id.is_none() && self.focused_session_handle != 0 {
            if let Some(window) = self.console_windows.iter().find(|window| {
                !window.minimized
                    && window.session_handle == self.focused_session_handle
                    && window.frame.contains(self.cursor_x, self.cursor_y)
            }) {
                return Some(WindowHit::Console(
                    window.session_handle,
                    self.hit_window_chrome(window.frame),
                ));
            }
        }

        for window in self.console_windows.iter().rev() {
            if window.minimized
                || window.session_handle == self.focused_session_handle
                || !window.frame.contains(self.cursor_x, self.cursor_y)
            {
                continue;
            }
            return Some(WindowHit::Console(
                window.session_handle,
                self.hit_window_chrome(window.frame),
            ));
        }

        for window in self.wayland_windows.iter().rev() {
            if window.minimized || Some(window.surface_id) == self.focused_wayland_surface_id {
                continue;
            }
            let outer = wayland_window_outer_rect(window);
            if !outer.contains(self.cursor_x, self.cursor_y) {
                continue;
            }
            return Some(WindowHit::Wayland(
                window.surface_id,
                self.hit_window_chrome(outer),
            ));
        }

        None
    }

    fn hit_window_chrome(&self, outer: canvas::Rect) -> WindowChromeHit {
        if window_close_button_rect(outer).contains(self.cursor_x, self.cursor_y) {
            return WindowChromeHit::Close;
        }
        if window_minimize_button_rect(outer).contains(self.cursor_x, self.cursor_y) {
            return WindowChromeHit::Minimize;
        }
        if window_title_bar_rect(outer).contains(self.cursor_x, self.cursor_y) {
            return WindowChromeHit::TitleBar;
        }
        WindowChromeHit::Client
    }

    fn handle_window_hit(
        &mut self,
        runtime: &RuntimeClient,
        hit: WindowHit,
        mut wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        match hit {
            WindowHit::Console(session_handle, chrome_hit) => {
                if let Some(wayland) = wayland.as_deref_mut() {
                    wayland.clear_focus();
                }
                self.focused_wayland_surface_id = None;
                match chrome_hit {
                    WindowChromeHit::Close => self.close_console_window(runtime, session_handle),
                    WindowChromeHit::Minimize => self.minimize_console_window(session_handle),
                    WindowChromeHit::TitleBar => {
                        let dirty_rect = self.focus_window(session_handle)?;
                        self.start_console_window_drag(session_handle);
                        Ok(if dirty_rect.is_empty() {
                            VisualUpdate::default()
                        } else {
                            VisualUpdate::partial(dirty_rect)
                        })
                    }
                    WindowChromeHit::Client => {
                        let dirty_rect = self.focus_window(session_handle)?;
                        Ok(if dirty_rect.is_empty() {
                            VisualUpdate::default()
                        } else {
                            VisualUpdate::partial(dirty_rect)
                        })
                    }
                }
            }
            WindowHit::Wayland(surface_id, chrome_hit) => {
                let Some(wayland) = wayland.as_deref_mut() else {
                    return Ok(VisualUpdate::default());
                };
                match chrome_hit {
                    WindowChromeHit::Close => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.close_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        if self.focused_wayland_surface_id == Some(surface_id) {
                            self.focused_wayland_surface_id = None;
                        }
                        if self.dragging_window == Some(DragTarget::Wayland(surface_id)) {
                            self.dragging_window = None;
                        }
                        let focus_dirty = self.recover_focus_after_wayland_change(Some(wayland))?;
                        Ok(VisualUpdate::partial(
                            before_dirty.union(wayland_dirty).union(focus_dirty),
                        ))
                    }
                    WindowChromeHit::Minimize => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.set_surface_minimized(surface_id, true);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        if self.focused_wayland_surface_id == Some(surface_id) {
                            self.focused_wayland_surface_id = None;
                        }
                        if self.dragging_window == Some(DragTarget::Wayland(surface_id)) {
                            self.dragging_window = None;
                        }
                        let focus_dirty = self.recover_focus_after_wayland_change(Some(wayland))?;
                        Ok(VisualUpdate::partial(
                            before_dirty.union(wayland_dirty).union(focus_dirty),
                        ))
                    }
                    WindowChromeHit::TitleBar => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.focus_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        self.focused_wayland_surface_id = Some(surface_id);
                        self.focused_session_handle = 0;
                        self.start_wayland_window_drag(surface_id);
                        Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)))
                    }
                    WindowChromeHit::Client => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.focus_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        self.focused_wayland_surface_id = Some(surface_id);
                        self.focused_session_handle = 0;
                        if wayland.pointer_button(POINTER_BUTTON_LEFT, true) {
                            return Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)));
                        }
                        Ok(VisualUpdate::default())
                    }
                }
            }
        }
    }

    fn start_console_window_drag(&mut self, session_handle: ConsoleSessionHandle) {
        let Some(window) = self
            .console_windows
            .iter()
            .find(|window| window.session_handle == session_handle)
        else {
            return;
        };

        self.dragging_window = Some(DragTarget::Console(session_handle));
        self.drag_offset_x = self.cursor_x.saturating_sub(window.frame.x as u32) as usize;
        self.drag_offset_y = self.cursor_y.saturating_sub(window.frame.y as u32) as usize;
    }

    fn start_wayland_window_drag(&mut self, surface_id: u32) {
        let Some(window) = self
            .wayland_windows
            .iter()
            .find(|window| window.surface_id == surface_id)
        else {
            return;
        };

        let outer = wayland_window_outer_rect(window);
        self.dragging_window = Some(DragTarget::Wayland(surface_id));
        self.drag_offset_x = self.cursor_x.saturating_sub(outer.x as u32) as usize;
        self.drag_offset_y = self.cursor_y.saturating_sub(outer.y as u32) as usize;
    }

    fn restore_console_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<VisualUpdate, i32> {
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
        {
            window.minimized = false;
        }
        let dirty_rect = self.focus_window(session_handle)?;
        Ok(if dirty_rect.is_empty() {
            VisualUpdate::full()
        } else {
            VisualUpdate::partial(dirty_rect)
        })
    }

    fn minimize_console_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<VisualUpdate, i32> {
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
        {
            window.minimized = true;
        }
        if self.focused_session_handle == session_handle {
            let fallback = self
                .console_windows
                .iter()
                .rev()
                .find(|window| !window.minimized && window.session_handle != session_handle)
                .map(|window| window.session_handle);
            self.focused_session_handle = 0;
            if let Some(fallback_session) = fallback {
                let _ = console_set_focus(self.console_fd.as_raw_fd(), fallback_session);
                self.focused_session_handle = fallback_session;
            }
        }
        if self.dragging_window == Some(DragTarget::Console(session_handle)) {
            self.dragging_window = None;
        }
        Ok(VisualUpdate::full())
    }

    fn close_console_window(
        &mut self,
        runtime: &RuntimeClient,
        session_handle: ConsoleSessionHandle,
    ) -> Result<VisualUpdate, i32> {
        self.mark_console_session_closing(session_handle);
        let closed_was_focused = self.focused_session_handle == session_handle;
        self.console_windows
            .retain(|window| window.session_handle != session_handle);
        if self.console_windows.is_empty() {
            self.next_console_snapshot_index = 0;
        } else if self.next_console_snapshot_index >= self.console_windows.len() {
            self.next_console_snapshot_index %= self.console_windows.len();
        }
        if closed_was_focused {
            self.focused_session_handle = 0;
            for window in &mut self.console_windows {
                window.minimized = true;
            }
        }
        let _ = runtime.request_terminate_session(session_handle);
        if self.dragging_window == Some(DragTarget::Console(session_handle)) {
            self.dragging_window = None;
        }
        Ok(VisualUpdate::full())
    }

    fn restore_wayland_window(
        &mut self,
        surface_id: u32,
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        let Some(wayland) = wayland else {
            return Ok(VisualUpdate::default());
        };
        let before_dirty = self.wayland_visual_dirty_rect();
        wayland.set_surface_minimized(surface_id, false);
        wayland.focus_surface(surface_id);
        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
        self.focused_wayland_surface_id = Some(surface_id);
        self.focused_session_handle = 0;
        Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)))
    }

    fn drag_window_to_cursor(&mut self, wayland: Option<&mut WaylandCompositor>) -> VisualUpdate {
        match self.dragging_window {
            Some(DragTarget::Console(session_handle)) => {
                let Some(window) = self
                    .console_windows
                    .iter_mut()
                    .find(|window| window.session_handle == session_handle)
                else {
                    self.dragging_window = None;
                    return VisualUpdate::default();
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
                    return VisualUpdate::default();
                }

                window.frame = next_frame;
                VisualUpdate::partial(previous_frame.union(next_frame))
            }
            Some(DragTarget::Wayland(surface_id)) => {
                let Some(index) = self
                    .wayland_windows
                    .iter()
                    .position(|window| window.surface_id == surface_id)
                else {
                    self.dragging_window = None;
                    return VisualUpdate::default();
                };
                let previous_outer = wayland_window_outer_rect(&self.wayland_windows[index]);
                let next_outer = clamp_console_window_rect(
                    self.display.width,
                    self.display.height,
                    canvas::Rect {
                        x: self.cursor_x.saturating_sub(self.drag_offset_x as u32) as usize,
                        y: self.cursor_y.saturating_sub(self.drag_offset_y as u32) as usize,
                        ..previous_outer
                    },
                );
                if previous_outer.x == next_outer.x && previous_outer.y == next_outer.y {
                    return VisualUpdate::default();
                }

                if let Some(window) = self.wayland_windows.get_mut(index) {
                    window.frame.x = next_outer.x;
                    window.frame.y = next_outer.y;
                }
                let next_dirty = self.wayland_window_rect_for_surface(surface_id);
                if let Some(wayland) = wayland {
                    wayland.move_surface(surface_id, next_outer.x, next_outer.y);
                    self.sync_wayland_windows(wayland.window_snapshots());
                }
                VisualUpdate::partial(previous_outer.union(next_outer).union(next_dirty))
            }
            None => VisualUpdate::default(),
        }
    }
}
