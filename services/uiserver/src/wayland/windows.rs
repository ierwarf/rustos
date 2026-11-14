//! Window management: z-order, focus, retire, move, minimize, maximize, close.

use super::*;

impl WaylandState {
    pub(super) fn bring_surface_to_front(&mut self, object_id: ObjectId) {
        let Some(index) = self.surfaces.iter().position(|surface| {
            surface
                .shared
                .lock()
                .ok()
                .and_then(|state| {
                    state
                        .resource
                        .as_ref()
                        .map(|resource| resource.id() == object_id)
                })
                .unwrap_or(false)
        }) else {
            return;
        };

        if index + 1 == self.surfaces.len() {
            return;
        }

        let surface = self.surfaces.remove(index);
        self.surfaces.push(surface);
    }

    pub(super) fn find_surface_by_id(
        &self,
        surface_id: u32,
    ) -> Option<Arc<Mutex<WaylandSurfaceState>>> {
        if surface_id == 0 {
            return None;
        }
        self.surfaces.iter().find_map(|surface| {
            let state = surface.shared.lock().ok()?;
            (state.alive && state.surface_id == surface_id).then(|| surface.shared.clone())
        })
    }

    pub(super) fn clear_focus_for_surface(&mut self, object_id: &ObjectId) {
        let pointer_matches = self
            .pointer_focus
            .as_ref()
            .is_some_and(|focus| focus.surface.id() == *object_id);
        if pointer_matches {
            self.update_pointer_focus(None);
        }

        let keyboard_matches = self
            .keyboard_focus
            .as_ref()
            .is_some_and(|focus| focus.surface.id() == *object_id);
        if keyboard_matches {
            self.set_keyboard_focus(None);
        }
    }

    pub(super) fn retire_surface(&mut self, shared: &Arc<Mutex<WaylandSurfaceState>>) -> bool {
        let (surface_id, object_id) = match shared.lock() {
            Ok(state) => (
                state.surface_id,
                state.resource.as_ref().map(|resource| resource.id()),
            ),
            Err(_) => {
                diag_line("uiserver: retire_surface lock failed");
                return false;
            }
        };
        diag_line(format!(
            "uiserver: retire_surface begin surface={surface_id}"
        ));
        if self
            .moving_surface
            .is_some_and(|(id, _, _)| id == surface_id)
        {
            self.moving_surface = None;
            self.pointer_button_down = false;
            self.pointer_grab_serial = None;
        }
        if let Some(object_id) = object_id.as_ref() {
            self.drag_surface_destroyed(object_id);
            // Only the surface that owns the pointer grab ends it; retiring an
            // unrelated surface must not drop another client's drag.
            let owned_grab = self
                .pointer_focus
                .as_ref()
                .is_some_and(|focus| focus.surface.id() == *object_id);
            self.clear_focus_for_surface(object_id);
            if owned_grab {
                self.pointer_button_down = false;
            }
        }
        self.sync_surface_output(shared, false);
        let Ok(mut state) = shared.lock() else {
            diag_line(format!(
                "uiserver: retire_surface lock failed surface={surface_id}"
            ));
            return false;
        };
        diag_line(format!(
            "uiserver: retire_surface state surface={} title={} alive={} minimized={} size={}x{} callbacks={}",
            surface_id,
            state.title,
            state.alive,
            state.minimized,
            state.width,
            state.height,
            state.pending_callbacks.len()
        ));
        state.alive = false;
        state.minimized = true;
        state.pixels = Arc::default();
        state.original_pixels = Arc::default();
        state.original_width = 0;
        state.original_height = 0;
        state.width = 0;
        state.height = 0;
        state.stride_pixels = 0;
        state.current_buffer = None;
        state.content_version = next_content_version(state.content_version);
        state.pending_buffer = None;
        self.abandon_commits(&mut state);
        let pending_releases = state.pending_buffer_releases.drain(..).collect::<Vec<_>>();
        state.pending_callbacks.clear();
        state.on_output = false;
        state.needs_initial_configure = false;
        drop(state);
        for buffer in pending_releases {
            super::dmabuf::note_release(&buffer);
            buffer.release();
        }
        self.mark_dirty();
        diag_line(format!("uiserver: retire_surface end surface={surface_id}"));
        true
    }

    pub(super) fn focus_surface(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let (resource, client_id) = {
            let Ok(state) = shared.lock() else {
                return false;
            };
            let Some(resource) = state.resource.clone() else {
                return false;
            };
            let Some(client_id) = state.client_id.clone() else {
                return false;
            };
            (resource, client_id)
        };
        self.bring_surface_to_front(resource.id().clone());
        self.set_keyboard_focus(Some(KeyboardFocus {
            surface: resource.clone(),
            client_id,
        }));
        self.mark_dirty();
        true
    }

    pub(super) fn move_surface(&mut self, surface_id: u32, x: usize, y: usize) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let display_width = self.display_width;
        let display_height = self.display_height;
        let Ok(mut state) = shared.lock() else {
            return false;
        };
        let clamped = clamp_wayland_frame(
            Rect {
                x,
                y,
                width: state.frame.width,
                height: state.frame.height,
            },
            display_width,
            display_height,
        );
        if state.frame.x == clamped.x && state.frame.y == clamped.y {
            return false;
        }
        state.frame.x = clamped.x;
        state.frame.y = clamped.y;
        drop(state);
        self.mark_dirty();
        true
    }

    pub(super) fn set_surface_minimized(&mut self, surface_id: u32, minimized: bool) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let Ok(mut state) = shared.lock() else {
            return false;
        };
        if state.minimized == minimized {
            return false;
        }
        state.minimized = minimized;
        let object_id = state
            .resource
            .as_ref()
            .map(|resource| resource.id().clone());
        drop(state);
        if let Some(object_id) = object_id {
            if minimized {
                self.clear_focus_for_surface(&object_id);
            } else {
                self.bring_surface_to_front(object_id);
            }
        }
        self.mark_dirty();
        true
    }

    pub(super) fn toggle_surface_maximized(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let display_width = self.display_width;
        let display_height = self.display_height;
        let Ok(mut state) = shared.lock() else {
            return false;
        };
        let next_frame = if let Some(saved) = state.pre_maximize_frame.take() {
            // Restore: send the saved client size back to the client so it
            // re-renders at the original dimensions if it respects configure.
            clamp_wayland_frame(saved, display_width, display_height)
        } else {
            state.pre_maximize_frame = Some(state.frame);
            let bounds = layout::desktop_bounds(display_width, display_height);
            clamp_wayland_frame(
                Rect {
                    x: bounds.x,
                    y: bounds.y,
                    width: bounds.width.saturating_sub(WINDOW_BORDER * 2),
                    height: bounds
                        .height
                        .saturating_sub(WINDOW_BORDER * 2 + WINDOW_TITLE_HEIGHT),
                },
                display_width,
                display_height,
            )
        };
        if state.frame == next_frame {
            return false;
        }
        state.frame = next_frame;
        drop(state);
        self.send_toplevel_configure(&shared);
        self.mark_dirty();
        true
    }

    pub(super) fn close_surface(&mut self, surface_id: u32) -> bool {
        diag_line(format!(
            "uiserver: close_surface begin surface={surface_id}"
        ));
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            diag_line(format!(
                "uiserver: close_surface missing surface={surface_id}"
            ));
            return false;
        };
        let toplevel = shared.lock().ok().and_then(|state| state.toplevel.clone());
        let retired = self.retire_surface(&shared);
        diag_line(format!(
            "uiserver: close_surface retired surface={} retired={} has_toplevel={}",
            surface_id,
            retired,
            toplevel.is_some()
        ));
        let Some(toplevel) = toplevel else {
            return retired;
        };
        toplevel.close();
        diag_line(format!(
            "uiserver: close_surface close_sent surface={surface_id}"
        ));
        retired
    }

    pub(super) fn clear_focus(&mut self) {
        self.update_pointer_focus(None);
        self.set_keyboard_focus(None);
        self.pointer_button_down = false;
    }
}
