//! Pointer hit testing, pointer focus and keyboard delivery.

use super::*;

impl WaylandState {
    pub(super) fn hit_test_pointer(&self, x: u32, y: u32) -> Option<PointerHit> {
        self.hit_test_pointer_impl(x, y, false)
    }

    pub(super) fn hit_test_drag(&self, x: u32, y: u32) -> Option<PointerHit> {
        self.hit_test_pointer_impl(x, y, true)
    }

    fn hit_test_pointer_impl(&self, x: u32, y: u32, dragging: bool) -> Option<PointerHit> {
        for surface in self.surfaces.iter().rev() {
            let Ok(surface) = surface.shared.lock() else {
                continue;
            };
            let grabbed = !dragging
                && self.pointer_button_down
                && self.pointer_focus.as_ref().is_some_and(|focus| {
                    surface
                        .resource
                        .as_ref()
                        .is_some_and(|resource| resource.id() == focus.surface.id())
                });
            if (!grabbed
                && ((!dragging && self.pointer_button_down)
                    || self.pointer_target != Some(surface.surface_id)))
                || !surface.alive
                || surface.minimized
                || surface.width == 0
                || surface.height == 0
            {
                continue;
            }
            let Some(resource) = surface.resource.clone() else {
                continue;
            };
            let Some(client_id) = surface.client_id.clone() else {
                continue;
            };
            // The compositor clamps `surface.frame` to the available desktop
            // region; the client buffer might be larger but only the clamped
            // area is actually rendered. Hit-test against that visible area so
            // clicks outside the painted chrome can't fake-hit invisible
            // pixels.
            let visible_w = surface.width.min(surface.frame.width);
            let visible_h = surface.height.min(surface.frame.height);
            let client_rect = Rect {
                x: surface.frame.x
                    + if surface.decoration_server {
                        WINDOW_BORDER
                    } else {
                        0
                    },
                y: surface.frame.y
                    + if surface.decoration_server {
                        WINDOW_TITLE_HEIGHT + WINDOW_BORDER
                    } else {
                        0
                    },
                width: visible_w,
                height: visible_h,
            };
            if !grabbed && !client_rect.contains(x, y) {
                continue;
            }
            return Some(PointerHit {
                surface: resource,
                client_id,
                surface_x: f64::from(x) - client_rect.x as f64,
                surface_y: f64::from(y) - client_rect.y as f64,
            });
        }
        None
    }

    pub(super) fn update_pointer_focus(&mut self, hit: Option<PointerHit>) {
        self.prune_pointer_resources();

        let previous_surface = self
            .pointer_focus
            .as_ref()
            .map(|focus| focus.surface.id().clone());
        let next_surface = hit.as_ref().map(|next| next.surface.id().clone());
        if previous_surface == next_surface {
            if let Some(hit) = hit {
                self.pointer_focus = Some(PointerFocus::from_hit(hit));
            }
            return;
        }

        let leave_serial = self.next_configure_serial();
        if let Some(previous) = self.pointer_focus.take() {
            for pointer in self
                .pointer_resources
                .iter()
                .filter(|pointer| pointer.client_id == previous.client_id)
            {
                pointer.resource.leave(leave_serial, &previous.surface);
                if pointer.resource.version() >= 5 {
                    pointer.resource.frame();
                }
            }
        }

        if let Some(hit) = hit {
            let enter_serial = self.next_configure_serial();
            for pointer in self
                .pointer_resources
                .iter()
                .filter(|pointer| pointer.client_id == hit.client_id)
            {
                pointer
                    .resource
                    .enter(enter_serial, &hit.surface, hit.surface_x, hit.surface_y);
                if pointer.resource.version() >= 5 {
                    pointer.resource.frame();
                }
            }
            self.pointer_focus = Some(PointerFocus::from_hit(hit));
        }
    }

    pub(super) fn pointer_motion(&mut self, x: u32, y: u32) {
        let next_x = x.min(self.display_width.saturating_sub(1));
        let next_y = y.min(self.display_height.saturating_sub(1));
        let previous_x = self.pointer_x;
        let previous_y = self.pointer_y;
        let previous_surface = self
            .pointer_focus
            .as_ref()
            .map(|focus| focus.surface.id().clone());
        self.pointer_x = next_x;
        self.pointer_y = next_y;
        if self.drag.is_some() {
            self.drag_motion();
            return;
        }
        if let Some((id, dx, dy)) = self.moving_surface {
            let x = (i64::from(self.pointer_x) - dx).max(0) as usize;
            let y = (i64::from(self.pointer_y) - dy).max(0) as usize;
            self.move_surface(id, x, y);
            return;
        }
        let hit = self.hit_test_pointer(self.pointer_x, self.pointer_y);
        self.update_pointer_focus(hit.clone());
        let Some(focus) = self.pointer_focus.as_ref() else {
            self.pointer_motion_pending = false;
            return;
        };
        let focus_unchanged = previous_surface
            .as_ref()
            .is_some_and(|surface| *surface == focus.surface.id());
        if !focus_unchanged {
            // `update_pointer_focus` already emitted an enter event carrying
            // the exact new coordinates.
            self.pointer_motion_pending = false;
            self.next_pointer_motion_flush = Instant::now() + WAYLAND_POINTER_FRAME_INTERVAL;
            return;
        }
        if previous_x == self.pointer_x && previous_y == self.pointer_y {
            return;
        }
        self.pointer_motion_pending = true;
        self.flush_pointer_motion(false);
    }

    pub(super) fn flush_pointer_motion(&mut self, force: bool) {
        if !self.pointer_motion_pending {
            return;
        }
        let now = Instant::now();
        if !force && now < self.next_pointer_motion_flush {
            return;
        }
        let time = self.event_time_ms();
        if let Some(focus) = self.pointer_focus.as_ref() {
            for pointer in self
                .pointer_resources
                .iter()
                .filter(|pointer| pointer.client_id == focus.client_id)
            {
                pointer
                    .resource
                    .motion(time, focus.surface_x, focus.surface_y);
                if pointer.resource.version() >= 5 {
                    pointer.resource.frame();
                }
            }
        }
        self.pointer_motion_pending = false;
        self.next_pointer_motion_flush = now + WAYLAND_POINTER_FRAME_INTERVAL;
    }

    pub(super) fn set_keyboard_focus(&mut self, next: Option<KeyboardFocus>) {
        self.prune_keyboard_resources();

        let previous_surface = self
            .keyboard_focus
            .as_ref()
            .map(|focus| focus.surface.id().clone());
        let next_surface = next.as_ref().map(|focus| focus.surface.id().clone());
        if previous_surface == next_surface {
            self.keyboard_focus = next;
            return;
        }

        let old_client = self
            .keyboard_focus
            .as_ref()
            .map(|focus| focus.client_id.clone());
        let new_client = next.as_ref().map(|focus| focus.client_id.clone());
        let client_changed = old_client != new_client;
        if client_changed {
            self.invalidate_selection_offers();
            self.input_serials.clear();
        }
        let leave_serial = self.next_configure_serial();
        if let Some(previous) = self.keyboard_focus.take() {
            for keyboard in self
                .keyboard_resources
                .iter()
                .filter(|keyboard| keyboard.client_id == previous.client_id)
            {
                keyboard.resource.leave(leave_serial, &previous.surface);
            }
        }

        if let Some(next) = next {
            if client_changed {
                self.send_selection_to_client(&next.client_id);
            }
            let enter_serial = self.next_configure_serial();
            self.record_input_serial(enter_serial, next.client_id.clone());
            for keyboard in self
                .keyboard_resources
                .iter()
                .filter(|keyboard| keyboard.client_id == next.client_id)
            {
                keyboard
                    .resource
                    .enter(enter_serial, &next.surface, Vec::<u8>::new());
                keyboard.resource.modifiers(enter_serial, 0, 0, 0, 0);
            }
            self.keyboard_focus = Some(next);
        }
    }

    pub(super) fn keyboard_input(&mut self, event: InputEvent) -> bool {
        if event.kind != INPUT_KIND_KEYBOARD {
            return false;
        }

        let Some(focus) = self.keyboard_focus.as_ref() else {
            return false;
        };
        let client_id = focus.client_id.clone();
        let serial = self.next_configure_serial();
        let time = self.event_time_ms();
        self.record_input_serial(serial, client_id.clone());
        let key_state = match event.action {
            INPUT_ACTION_PRESSED | INPUT_ACTION_REPEATED => wl_keyboard::KeyState::Pressed,
            INPUT_ACTION_RELEASED => wl_keyboard::KeyState::Released,
            _ => return false,
        };
        for keyboard in self
            .keyboard_resources
            .iter()
            .filter(|keyboard| keyboard.client_id == client_id)
        {
            keyboard.resource.key(serial, time, event.code, key_state);
            keyboard.resource.modifiers(serial, 0, 0, 0, 0);
        }
        true
    }
}
