use std::string::String;
use std::time::Instant;

use super::launch::{launch_program_async, terminate_session_async};
use super::{AppState, DragTarget, VisualUpdate, WindowId};
use crate::canvas;
use crate::layout::{
    ResizeEdge, WindowChromeHit, clamp_wayland_frame, desktop_bounds, maximized_window_rect,
    min_window_height, min_window_width, resized_window_rect, window_border, window_resize_hit,
    window_title_height,
};
use crate::profile;
use crate::render::{
    clamp_console_window_rect, console_window_dirty_rect, launcher_button_rect, taskbar_slot_rect,
    wayland_window_client_rect, wayland_window_dirty_rect, wayland_window_outer_rect,
};
use crate::sys::{
    self, ConsoleSessionHandle, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_SCROLL, InputEvent,
    POINTER_BUTTON_LEFT,
};
use crate::wayland::WaylandCompositor;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskbarHit {
    Console(ConsoleSessionHandle),
    Wayland(u32),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowHit {
    Console(ConsoleSessionHandle, WindowChromeHit),
    Wayland(u32, WindowChromeHit),
}

impl AppState {
    pub(crate) fn handle_input_event(
        &mut self,
        event: &InputEvent,
        mut wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        match event.kind {
            INPUT_KIND_KEYBOARD => {
                if let Some(wayland) = wayland.filter(|_| self.focused_wayland_surface_id.is_some())
                {
                    wayland.set_keyboard_target(self.focused_wayland_surface_id);
                    if wayland.keyboard_input(*event) {
                        return Ok(VisualUpdate::default());
                    }
                }
                if self.focused_session_handle != 0 {
                    // Arm before the submit, not after the input phase: the
                    // submit happens inside this phase, so arming afterwards
                    // stamped the *next* keystroke's queueing and reported the
                    // gap between two keys as this key's latency.
                    crate::keytrace::arm();
                    self.console_commands
                        .submit_input(self.focused_session_handle, *event);
                }
                Ok(self
                    .reveal_focused_terminal_cursor()
                    .map(VisualUpdate::partial)
                    .unwrap_or_default())
            }
            INPUT_KIND_POINTER_MOTION => {
                let previous_x = self.cursor_x;
                let previous_y = self.cursor_y;
                let previous_dirty =
                    self.cursor_visual_dirty_rect(self.surface.width, self.surface.height);
                let next_x = (i64::from(self.cursor_x) + i64::from(event.value0))
                    .clamp(0, i64::from(u32::MAX)) as u32;
                let next_y = (i64::from(self.cursor_y) + i64::from(event.value1))
                    .clamp(0, i64::from(u32::MAX)) as u32;
                self.cursor_x = next_x.min(self.display.width.saturating_sub(1));
                self.cursor_y = next_y.min(self.display.height.saturating_sub(1));
                self.record_cursor_motion(previous_x, previous_y);
                let mut cursor_update =
                    VisualUpdate::cursor(previous_dirty.union(
                        self.cursor_visual_dirty_rect(self.surface.width, self.surface.height),
                    ));
                let drag_update = self.drag_window_to_cursor(wayland.as_deref_mut());
                if !drag_update.is_empty() {
                    cursor_update.absorb(drag_update);
                    return Ok(cursor_update);
                }
                if previous_x == self.cursor_x && previous_y == self.cursor_y {
                    return Ok(drag_update);
                }
                if let Some(wayland) = wayland.as_deref_mut() {
                    let started = Instant::now();
                    self.sync_wayland_pointer_target(wayland);
                    wayland.pointer_motion(self.cursor_x, self.cursor_y);
                    profile::record_wayland_motion(started.elapsed());
                }
                Ok(cursor_update)
            }
            INPUT_KIND_POINTER_POSITION => {
                let previous_x = self.cursor_x;
                let previous_y = self.cursor_y;
                let previous_dirty =
                    self.cursor_visual_dirty_rect(self.surface.width, self.surface.height);
                self.cursor_x =
                    (event.value0.max(0) as u32).min(self.display.width.saturating_sub(1));
                self.cursor_y =
                    (event.value1.max(0) as u32).min(self.display.height.saturating_sub(1));
                self.record_cursor_motion(previous_x, previous_y);
                let mut cursor_update =
                    VisualUpdate::cursor(previous_dirty.union(
                        self.cursor_visual_dirty_rect(self.surface.width, self.surface.height),
                    ));
                let drag_update = self.drag_window_to_cursor(wayland.as_deref_mut());
                if !drag_update.is_empty() {
                    cursor_update.absorb(drag_update);
                    return Ok(cursor_update);
                }
                if previous_x == self.cursor_x && previous_y == self.cursor_y {
                    return Ok(drag_update);
                }
                if let Some(wayland) = wayland.as_deref_mut() {
                    let started = Instant::now();
                    self.sync_wayland_pointer_target(wayland);
                    wayland.pointer_motion(self.cursor_x, self.cursor_y);
                    profile::record_wayland_motion(started.elapsed());
                }
                Ok(cursor_update)
            }
            INPUT_KIND_POINTER_BUTTON if event.code == POINTER_BUTTON_LEFT => {
                self.left_button_down = event.action == sys::INPUT_ACTION_PRESSED;
                if self.left_button_down {
                    return self.handle_left_press(wayland);
                }
                if self.dragging_window.is_some() {
                    let update = self.drag_window_to_cursor(wayland.as_deref_mut());
                    self.dragging_window = None;
                    return Ok(update);
                }
                if let Some(wayland) = wayland {
                    wayland.pointer_button(POINTER_BUTTON_LEFT, false);
                }
                Ok(VisualUpdate::default())
            }
            INPUT_KIND_POINTER_BUTTON => {
                let pressed = event.action == sys::INPUT_ACTION_PRESSED;
                let Some(wayland) = wayland else {
                    return Ok(VisualUpdate::default());
                };
                if !pressed {
                    wayland.pointer_button(event.code, false);
                    return Ok(VisualUpdate::default());
                }
                if let Some(WindowHit::Wayland(surface_id, WindowChromeHit::Client)) =
                    self.window_under_cursor()
                {
                    let before_dirty = self.wayland_visual_dirty_rect();
                    wayland.focus_surface(surface_id);
                    let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                    self.focus_wayland_window(surface_id);
                    self.sync_wayland_pointer_target(wayland);
                    wayland.pointer_motion(self.cursor_x, self.cursor_y);
                    wayland.pointer_button(event.code, true);
                    return Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)));
                }
                Ok(VisualUpdate::default())
            }
            INPUT_KIND_POINTER_SCROLL => {
                match self.window_under_cursor() {
                    Some(WindowHit::Wayland(_, WindowChromeHit::Client)) => {
                        if let Some(wayland) = wayland {
                            self.sync_wayland_pointer_target(wayland);
                            wayland.pointer_motion(self.cursor_x, self.cursor_y);
                            wayland.pointer_scroll(event.value0, event.value1);
                        }
                    }
                    Some(WindowHit::Console(session_handle, WindowChromeHit::Client))
                        if event.value0 != 0 =>
                    {
                        if let Some(window) = self
                            .console_windows
                            .iter_mut()
                            .find(|w| w.session_handle == session_handle)
                        {
                            let lines = (event.value0 as isize).saturating_mul(3);
                            if window.terminal.scroll_by(lines) {
                                window.invalidate_surface();
                                return Ok(VisualUpdate::partial(console_window_dirty_rect(
                                    window.frame,
                                )));
                            }
                        }
                    }
                    _ => {}
                }
                Ok(VisualUpdate::default())
            }
            _ => Ok(VisualUpdate::default()),
        }
    }

    fn handle_left_press(
        &mut self,
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        // Home-screen launchers sit under the windows, so a visible window
        // over a cell must take the click instead of starting another app.
        if self.window_under_cursor().is_none() {
            if let Some(desktop_file_id) = self.launcher_program_under_cursor() {
                launch_program_async(desktop_file_id);
                return Ok(VisualUpdate::default());
            }
        }

        if let Some(hit) = self.taskbar_window_under_cursor() {
            return match hit {
                TaskbarHit::Console(session_handle) => {
                    if let Some(wayland) = wayland {
                        wayland.clear_focus();
                    }
                    self.restore_console_window(session_handle)
                }
                TaskbarHit::Wayland(surface_id) => self.restore_wayland_window(surface_id, wayland),
            };
        }

        if let Some(hit) = self.window_under_cursor() {
            return self.handle_window_hit(hit, wayland);
        }

        Ok(VisualUpdate::default())
    }

    fn launcher_program_under_cursor(&self) -> Option<String> {
        self.launcher_programs
            .iter()
            .enumerate()
            .find(|(index, _)| {
                launcher_button_rect(
                    self.display.width,
                    self.display.height,
                    self.launcher_programs.len(),
                    *index,
                )
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
            .filter(|window| !window.drag_icon && !window.popup)
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
        super::window_hit_test(
            &self.window_draw_order(),
            &self.console_windows,
            &self.wayland_windows,
            self.cursor_x,
            self.cursor_y,
        )
        .map(|(id, chrome)| match id {
            WindowId::Console(id) => WindowHit::Console(id, chrome),
            WindowId::Wayland(id) => WindowHit::Wayland(id, chrome),
        })
    }

    pub(crate) fn sync_wayland_input_focus(&self, wayland: &mut WaylandCompositor) {
        if self.wayland_windows.is_empty() {
            wayland.set_keyboard_target(None);
            wayland.set_pointer_target(None);
            return;
        }
        wayland.set_keyboard_target(self.focused_wayland_surface_id);
        self.sync_wayland_pointer_target(wayland);
    }

    pub(crate) fn sync_wayland_pointer_target(&self, wayland: &mut WaylandCompositor) {
        let target = match self.window_under_cursor() {
            Some(WindowHit::Wayland(id, WindowChromeHit::Client)) => Some(id),
            _ => None,
        };
        wayland.set_pointer_target(target);
    }

    fn handle_window_hit(
        &mut self,
        hit: WindowHit,
        mut wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        match hit {
            WindowHit::Console(session_handle, chrome_hit) => {
                if !matches!(
                    chrome_hit,
                    WindowChromeHit::Close | WindowChromeHit::Minimize
                ) {
                    if let Some(wayland) = wayland.as_deref_mut() {
                        wayland.clear_focus();
                    }
                }
                match chrome_hit {
                    WindowChromeHit::Close => self.close_console_window(session_handle, wayland),
                    WindowChromeHit::Minimize => {
                        self.minimize_console_window(session_handle, wayland)
                    }
                    WindowChromeHit::Maximize => {
                        self.toggle_console_window_maximize(session_handle)
                    }
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
                        if let Some(edge) = self
                            .console_windows
                            .iter()
                            .find(|w| w.session_handle == session_handle && !w.minimized)
                            .and_then(|w| window_resize_hit(w.frame, self.cursor_x, self.cursor_y))
                        {
                            self.start_console_window_resize(session_handle, edge);
                        }
                        Ok(if dirty_rect.is_empty() {
                            VisualUpdate::default()
                        } else {
                            VisualUpdate::partial(dirty_rect)
                        })
                    }
                }
            }
            WindowHit::Wayland(surface_id, chrome_hit) => {
                let Some(wayland) = wayland else {
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
                        if self.dragging_window.and_then(DragTarget::wayland_surface)
                            == Some(surface_id)
                        {
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
                        if self.dragging_window.and_then(DragTarget::wayland_surface)
                            == Some(surface_id)
                        {
                            self.dragging_window = None;
                        }
                        let focus_dirty = self.recover_focus_after_wayland_change(Some(wayland))?;
                        Ok(VisualUpdate::partial(
                            before_dirty.union(wayland_dirty).union(focus_dirty),
                        ))
                    }
                    WindowChromeHit::Maximize => {
                        self.cancel_wayland_placement(surface_id);
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.toggle_surface_maximized(surface_id);
                        wayland.focus_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        self.focus_wayland_window(surface_id);
                        if self.dragging_window.and_then(DragTarget::wayland_surface)
                            == Some(surface_id)
                        {
                            self.dragging_window = None;
                        }
                        Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)))
                    }
                    WindowChromeHit::TitleBar => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        wayland.focus_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        self.focus_wayland_window(surface_id);
                        self.start_wayland_window_drag(surface_id);
                        Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)))
                    }
                    WindowChromeHit::Client => {
                        let before_dirty = self.wayland_visual_dirty_rect();
                        // A popup receives the click but leaves focus where the
                        // client's grab (or its parent) already put it.
                        let is_popup = self
                            .wayland_windows
                            .iter()
                            .any(|w| w.surface_id == surface_id && w.popup);
                        if is_popup {
                            self.sync_wayland_pointer_target(wayland);
                            wayland.pointer_motion(self.cursor_x, self.cursor_y);
                            if wayland.pointer_button(POINTER_BUTTON_LEFT, true) {
                                return Ok(VisualUpdate::partial(before_dirty));
                            }
                            return Ok(VisualUpdate::default());
                        }
                        wayland.focus_surface(surface_id);
                        let wayland_dirty = self.sync_wayland_windows(wayland.window_snapshots());
                        self.focus_wayland_window(surface_id);
                        if let Some(edge) = self
                            .wayland_windows
                            .iter()
                            .find(|w| {
                                w.surface_id == surface_id && !w.minimized && w.server_decoration
                            })
                            .and_then(|w| {
                                window_resize_hit(
                                    wayland_window_outer_rect(w),
                                    self.cursor_x,
                                    self.cursor_y,
                                )
                            })
                        {
                            self.start_wayland_window_resize(surface_id, edge);
                            return Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)));
                        }
                        self.sync_wayland_pointer_target(wayland);
                        wayland.pointer_motion(self.cursor_x, self.cursor_y);
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

    fn start_console_window_resize(
        &mut self,
        session_handle: ConsoleSessionHandle,
        edge: ResizeEdge,
    ) {
        let Some(window) = self
            .console_windows
            .iter()
            .find(|window| window.session_handle == session_handle)
        else {
            return;
        };

        self.dragging_window = Some(DragTarget::ConsoleResize {
            session_handle,
            edge,
            start_x: self.cursor_x,
            start_y: self.cursor_y,
            start_frame: window.frame,
        });
    }

    fn start_wayland_window_drag(&mut self, surface_id: u32) {
        self.cancel_wayland_placement(surface_id);
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

    fn start_wayland_window_resize(&mut self, surface_id: u32, edge: ResizeEdge) {
        self.cancel_wayland_placement(surface_id);
        let Some(window) = self
            .wayland_windows
            .iter()
            .find(|window| window.surface_id == surface_id)
        else {
            return;
        };

        self.dragging_window = Some(DragTarget::WaylandResize {
            surface_id,
            edge,
            start_x: self.cursor_x,
            start_y: self.cursor_y,
            start_outer: wayland_window_outer_rect(window),
        });
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
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|w| w.session_handle == session_handle)
        {
            window.minimized = true;
        }
        if self.focused_session_handle == session_handle {
            self.focused_session_handle = 0;
            self.pending_console_focus = None;
        }
        if self.dragging_window.and_then(DragTarget::console_session) == Some(session_handle) {
            self.dragging_window = None;
        }
        self.recover_focus_after_wayland_change(wayland)?;
        Ok(VisualUpdate::full())
    }

    fn close_console_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<VisualUpdate, i32> {
        self.mark_console_session_closing(session_handle);
        self.console_windows
            .retain(|w| w.session_handle != session_handle);
        self.reconcile_window_stack();
        if self.console_windows.is_empty() {
            self.next_console_snapshot_index = 0;
        } else {
            self.next_console_snapshot_index %= self.console_windows.len();
        }
        if self.focused_session_handle == session_handle {
            self.focused_session_handle = 0;
            self.pending_console_focus = None;
        }
        self.recover_focus_after_wayland_change(wayland)?;
        // The runtimed RPC may wait out its 5 s I/O deadline, longer than the
        // UI phase watchdog tolerates, so it never runs on the input phase.
        terminate_session_async(session_handle);
        if self.dragging_window.and_then(DragTarget::console_session) == Some(session_handle) {
            self.dragging_window = None;
        }
        Ok(VisualUpdate::full())
    }

    fn toggle_console_window_maximize(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<VisualUpdate, i32> {
        let display_width = self.display.width;
        let display_height = self.display.height;
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
        {
            let next_frame = if let Some(saved) = window.pre_maximize_frame.take() {
                clamp_console_window_rect(display_width, display_height, saved)
            } else {
                window.pre_maximize_frame = Some(window.frame);
                maximized_window_rect(display_width, display_height)
            };
            if window.frame != next_frame {
                window.frame = next_frame;
                window.invalidate_surface();
                window.terminal_dirty = true;
            }
        }
        let dirty_rect = self.focus_window(session_handle)?;
        Ok(if dirty_rect.is_empty() {
            VisualUpdate::full()
        } else {
            VisualUpdate::partial(dirty_rect.union(canvas::Rect::empty()))
        })
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
        self.focus_wayland_window(surface_id);
        Ok(VisualUpdate::partial(before_dirty.union(wayland_dirty)))
    }

    fn drag_window_to_cursor(&mut self, wayland: Option<&mut WaylandCompositor>) -> VisualUpdate {
        // Window follows the cursor 1:1 on every coalesced pointer event.
        // The input loop already collapses motion bursts into one position
        // per process_pending_input tick (see input_loop.rs), and the partial
        // damage list coalesces per-frame, so an additional time/distance
        // throttle here just makes the dragged window lag the cursor.
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
                let target_x = self.cursor_x.saturating_sub(self.drag_offset_x as u32) as usize;
                let target_y = self.cursor_y.saturating_sub(self.drag_offset_y as u32) as usize;
                let next_frame = clamp_console_window_rect(
                    self.display.width,
                    self.display.height,
                    canvas::Rect {
                        x: target_x,
                        y: target_y,
                        ..window.frame
                    },
                );
                if window.frame == next_frame {
                    return VisualUpdate::default();
                }

                window.frame = next_frame;
                // Use shadow-inclusive dirty rects so the previous shadow
                // halo gets repainted instead of leaving a dark ghost
                // behind the dragged window.
                let mut update = VisualUpdate::partial(console_window_dirty_rect(previous_frame));
                update.add_partial_rect(console_window_dirty_rect(next_frame));
                update
            }
            Some(DragTarget::ConsoleResize {
                session_handle,
                edge,
                start_x,
                start_y,
                start_frame,
            }) => {
                let Some(window) = self
                    .console_windows
                    .iter_mut()
                    .find(|window| window.session_handle == session_handle)
                else {
                    self.dragging_window = None;
                    return VisualUpdate::default();
                };

                let previous_frame = window.frame;
                let bounds = desktop_bounds(self.display.width, self.display.height);
                let dx = self.cursor_x as i64 - start_x as i64;
                let dy = self.cursor_y as i64 - start_y as i64;
                let next_frame = resized_window_rect(
                    start_frame,
                    edge,
                    dx,
                    dy,
                    min_window_width(),
                    min_window_height(),
                    bounds,
                );
                if window.frame == next_frame {
                    return VisualUpdate::default();
                }

                window.frame = next_frame;
                window.pre_maximize_frame = None;
                window.invalidate_surface();
                let mut update = VisualUpdate::partial(console_window_dirty_rect(previous_frame));
                update.add_partial_rect(console_window_dirty_rect(next_frame));
                update
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
                let previous_dirty = wayland_window_dirty_rect(&self.wayland_windows[index]);
                // Wayland windows store the *content* size in `frame.width/height`
                // — use `clamp_wayland_frame` (not the console clamp) so the
                // title bar stays grabbable but the window can drag past the
                // desktop edge.
                let target_x = self.cursor_x.saturating_sub(self.drag_offset_x as u32) as usize;
                let target_y = self.cursor_y.saturating_sub(self.drag_offset_y as u32) as usize;
                let frame = self.wayland_windows[index].frame;
                let clamped = clamp_wayland_frame(
                    canvas::Rect {
                        x: target_x,
                        y: target_y,
                        width: frame.width,
                        height: frame.height,
                    },
                    self.display.width,
                    self.display.height,
                );
                if previous_outer.x == clamped.x && previous_outer.y == clamped.y {
                    return VisualUpdate::default();
                }

                if let Some(window) = self.wayland_windows.get_mut(index) {
                    window.frame.x = clamped.x;
                    window.frame.y = clamped.y;
                }
                let next_dirty = self.wayland_window_rect_for_surface(surface_id);
                if let Some(wayland) = wayland {
                    wayland.move_surface(surface_id, clamped.x, clamped.y);
                    self.sync_wayland_windows(wayland.window_snapshots());
                }
                let mut update = VisualUpdate::partial(previous_dirty);
                update.add_partial_rect(next_dirty);
                update
            }
            Some(DragTarget::WaylandResize {
                surface_id,
                edge,
                start_x,
                start_y,
                start_outer,
            }) => {
                let Some(index) = self
                    .wayland_windows
                    .iter()
                    .position(|window| window.surface_id == surface_id)
                else {
                    self.dragging_window = None;
                    return VisualUpdate::default();
                };

                let previous_dirty = wayland_window_dirty_rect(&self.wayland_windows[index]);
                let bounds = desktop_bounds(self.display.width, self.display.height);
                let dx = self.cursor_x as i64 - start_x as i64;
                let dy = self.cursor_y as i64 - start_y as i64;
                let border = window_border();
                let min_outer_w = min_window_width().max(border * 2 + 1);
                let min_outer_h =
                    min_window_height().max(border * 2 + window_title_height() + 1);
                let next_outer = resized_window_rect(
                    start_outer,
                    edge,
                    dx,
                    dy,
                    min_outer_w,
                    min_outer_h,
                    bounds,
                );
                let client = wayland_window_client_rect(next_outer);
                let next_client_frame = canvas::Rect {
                    x: next_outer.x,
                    y: next_outer.y,
                    width: client.width,
                    height: client.height,
                };
                if let Some(wayland) = wayland {
                    if !wayland.resize_surface(surface_id, next_client_frame) {
                        return VisualUpdate::default();
                    }
                    self.sync_wayland_windows(wayland.window_snapshots());
                } else {
                    if self.wayland_windows[index].frame == next_client_frame {
                        return VisualUpdate::default();
                    }
                    self.wayland_windows[index].frame = next_client_frame;
                }
                let next_dirty = self.wayland_window_rect_for_surface(surface_id);
                let mut update = VisualUpdate::partial(previous_dirty);
                update.add_partial_rect(next_dirty);
                update
            }
            None => VisualUpdate::default(),
        }
    }
}
