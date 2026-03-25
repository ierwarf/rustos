use std::os::fd::{AsRawFd, RawFd};

use super::{AppState, VisualUpdate};
use crate::canvas;
use crate::render::{
    clamp_console_window_rect, console_window_title_bar_rect, launcher_button_rect,
    taskbar_slot_rect,
};
use crate::sys::{
    self, console_send_input_event, runtime_request_launch_new_session, ConsoleSessionHandle,
    InputEvent, INPUT_KIND_KEYBOARD, INPUT_KIND_POINTER_BUTTON, INPUT_KIND_POINTER_MOTION,
    INPUT_KIND_POINTER_POSITION, POINTER_BUTTON_LEFT,
};

impl AppState {
    pub(crate) fn handle_input_event(
        &mut self,
        runtime_fd: RawFd,
        event: &InputEvent,
    ) -> Result<VisualUpdate, i32> {
        match event.kind {
            INPUT_KIND_KEYBOARD => {
                if self.focused_session_handle != 0 {
                    console_send_input_event(
                        self.console_fd.as_raw_fd(),
                        self.focused_session_handle,
                        *event,
                    )?;
                }
                Ok(self
                    .reveal_focused_terminal_cursor()
                    .map(VisualUpdate::partial)
                    .unwrap_or_default())
            }
            INPUT_KIND_POINTER_MOTION => {
                let next_x = (self.cursor_x as i32 + event.value0).max(0) as u32;
                let next_y = (self.cursor_y as i32 + event.value1).max(0) as u32;
                self.cursor_x = next_x.min(self.display.width.saturating_sub(1));
                self.cursor_y = next_y.min(self.display.height.saturating_sub(1));
                Ok(if self.drag_window_to_cursor().is_some() {
                    VisualUpdate::full()
                } else {
                    VisualUpdate::default()
                })
            }
            INPUT_KIND_POINTER_POSITION => {
                self.cursor_x =
                    (event.value0.max(0) as u32).min(self.display.width.saturating_sub(1));
                self.cursor_y =
                    (event.value1.max(0) as u32).min(self.display.height.saturating_sub(1));
                Ok(if self.drag_window_to_cursor().is_some() {
                    VisualUpdate::full()
                } else {
                    VisualUpdate::default()
                })
            }
            INPUT_KIND_POINTER_BUTTON if event.code == POINTER_BUTTON_LEFT => {
                self.left_button_down = event.action == sys::INPUT_ACTION_PRESSED;
                if self.left_button_down {
                    return self.handle_left_press(runtime_fd);
                }
                self.dragging_window_session = None;
                Ok(VisualUpdate::default())
            }
            _ => Ok(VisualUpdate::default()),
        }
    }

    fn handle_left_press(&mut self, runtime_fd: RawFd) -> Result<VisualUpdate, i32> {
        if let Some(program_id) = self.launcher_program_under_cursor() {
            let _ = runtime_request_launch_new_session(runtime_fd, program_id);
            return Ok(VisualUpdate::default());
        }

        if let Some(session_handle) = self.taskbar_window_under_cursor() {
            let dirty_rect = self.focus_window(session_handle)?;
            return Ok(if dirty_rect.is_empty() {
                VisualUpdate::default()
            } else {
                VisualUpdate::partial(dirty_rect)
            });
        }

        if let Some((session_handle, title_bar_hit)) = self.window_under_cursor() {
            let dirty_rect = self.focus_window(session_handle)?;
            if title_bar_hit {
                self.start_window_drag(session_handle);
            }
            if dirty_rect.is_empty() {
                return Ok(VisualUpdate::default());
            }
            return Ok(VisualUpdate::partial(dirty_rect));
        }

        Ok(VisualUpdate::default())
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

    fn taskbar_window_under_cursor(&self) -> Option<ConsoleSessionHandle> {
        self.console_windows
            .iter()
            .enumerate()
            .find(|(index, _)| {
                taskbar_slot_rect(self.display.width, self.display.height, *index)
                    .contains(self.cursor_x, self.cursor_y)
            })
            .map(|(_, window)| window.session_handle)
    }

    fn window_under_cursor(&self) -> Option<(ConsoleSessionHandle, bool)> {
        for window in self.console_windows.iter().rev() {
            if !window.frame.contains(self.cursor_x, self.cursor_y) {
                continue;
            }

            let title_bar_hit =
                console_window_title_bar_rect(window.frame).contains(self.cursor_x, self.cursor_y);
            return Some((window.session_handle, title_bar_hit));
        }

        None
    }

    fn start_window_drag(&mut self, session_handle: ConsoleSessionHandle) {
        let Some(window) = self
            .console_windows
            .iter()
            .find(|window| window.session_handle == session_handle)
        else {
            return;
        };

        self.dragging_window_session = Some(session_handle);
        self.drag_offset_x = self.cursor_x.saturating_sub(window.frame.x as u32) as usize;
        self.drag_offset_y = self.cursor_y.saturating_sub(window.frame.y as u32) as usize;
    }

    fn drag_window_to_cursor(&mut self) -> Option<canvas::Rect> {
        let Some(session_handle) = self.dragging_window_session else {
            return None;
        };

        let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
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
}
