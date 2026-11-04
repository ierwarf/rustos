//! Pointer hit testing, pointer focus and keyboard delivery.

use super::*;

impl WaylandState {
    pub(super) fn hit_test_pointer(&self, x: u32, y: u32) -> Option<PointerHit> {
        for surface in self.surfaces.iter().rev() {
            let Ok(surface) = surface.shared.lock() else {
                continue;
            };
            if !surface.alive || surface.minimized || surface.width == 0 || surface.height == 0 {
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
                x: surface.frame.x + WINDOW_BORDER,
                y: surface.frame.y + WINDOW_TITLE_HEIGHT + WINDOW_BORDER,
                width: visible_w,
                height: visible_h,
            };
            if !client_rect.contains(x, y) {
                continue;
            }
            return Some(PointerHit {
                surface: resource,
                client_id,
                surface_x: f64::from(x.saturating_sub(client_rect.x as u32)),
                surface_y: f64::from(y.saturating_sub(client_rect.y as u32)),
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
            let enter_serial = self.next_configure_serial();
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
