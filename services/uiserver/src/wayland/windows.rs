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
        if self
            .resizing_surface
            .is_some_and(|grab| grab.surface_id == surface_id)
        {
            self.resizing_surface = None;
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
                self.pointer_grab_serial = None;
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

    /// Make a surface retired by `xdg_toplevel.destroy` usable for a new
    /// toplevel. Returns false when the surface tables have no room for it.
    /// A surface that is still alive needs nothing and returns true.
    pub(super) fn revive_retired_surface(&mut self, data: &SurfaceData) -> bool {
        let resource_alive = match data.shared.lock() {
            Ok(state) if state.alive => return true,
            Ok(state) => state
                .resource
                .as_ref()
                .is_some_and(|resource| resource.is_alive()),
            Err(_) => return false,
        };
        if !resource_alive {
            return false;
        }
        // The retired entry itself is dropped by the prune; it is re-added below.
        self.prune_surfaces();
        let client_id = data
            .shared
            .lock()
            .ok()
            .and_then(|state| state.client_id.clone());
        let Some(client_id) = client_id else {
            return false;
        };
        if self.surfaces.len() >= MAX_WAYLAND_SURFACES
            || self.client_surface_count(&client_id) >= MAX_WAYLAND_SURFACES_PER_CLIENT
        {
            return false;
        }
        let Ok(mut state) = data.shared.lock() else {
            return false;
        };
        state.alive = true;
        state.minimized = false;
        state.maximized = false;
        state.fullscreen = false;
        state.resizing = false;
        // The wl_surface survives, but this is a new toplevel. Keep the
        // placement, not the old role's saved maximize state or preferences.
        state.pre_maximize_frame = None;
        state.pre_fullscreen_frame = None;
        state.min_size = (0, 0);
        state.max_size = (0, 0);
        state.pending_configure_geometry_serial = None;
        state.resize_anchor = None;
        state.title.clear();
        state.title.push_str("Wayland App");
        state.decoration_requested_server = true;
        state.pending_surface_damage = Rect::empty();
        state.pending_buffer_damage = Rect::empty();
        state.needs_initial_configure = true;
        state.configured_serial = 0;
        state.acknowledged_serial = 0;
        state.outstanding_configures.clear();
        state.decoration_configures.clear();
        state.decoration_ack_server = false;
        state.decoration_server = false;
        drop(state);
        if !self
            .surfaces
            .iter()
            .any(|surface| Arc::ptr_eq(&surface.shared, &data.shared))
        {
            self.surfaces.push(data.clone());
        }
        self.mark_dirty();
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

    pub(super) fn effective_server_decoration(state: &WaylandSurfaceState) -> bool {
        !state.fullscreen
            && (state.decoration_server
                || (state.decoration.is_some() && state.decoration_requested_server))
    }

    pub(super) fn clamp_frame_to_limits(
        state: &WaylandSurfaceState,
        frame: Rect,
        display_width: u32,
        display_height: u32,
    ) -> Rect {
        let server_decoration = Self::effective_server_decoration(state);
        let (max_disp_w, max_disp_h) =
            wayland_max_client_size_with_decoration(display_width, display_height, server_decoration);
        let min_w = state.min_size.0.min(max_disp_w).max(1);
        let min_h = state.min_size.1.min(max_disp_h).max(1);
        let max_w = if state.max_size.0 > 0 {
            state.max_size.0.min(max_disp_w).max(min_w)
        } else {
            max_disp_w
        };
        let max_h = if state.max_size.1 > 0 {
            state.max_size.1.min(max_disp_h).max(min_h)
        } else {
            max_disp_h
        };
        clamp_wayland_frame_with_decoration(
            Rect {
                x: frame.x,
                y: frame.y,
                width: frame.width.clamp(min_w, max_w),
                height: frame.height.clamp(min_h, max_h),
            },
            display_width,
            display_height,
            server_decoration,
        )
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
        let server_decoration = Self::effective_server_decoration(&state);
        let clamped = clamp_wayland_frame_with_decoration(
            Rect {
                x,
                y,
                width: state.frame.width,
                height: state.frame.height,
            },
            display_width,
            display_height,
            server_decoration,
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

    pub(super) fn set_surface_maximized_shared(
        &mut self,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        maximized: bool,
    ) -> bool {
        let display_width = self.display_width;
        let display_height = self.display_height;
        let changed = {
            let Ok(mut state) = shared.lock() else {
                return false;
            };
            if !state.alive {
                return false;
            }
            let was_maximized = state.maximized || state.pre_maximize_frame.is_some();
            if was_maximized == maximized && !state.needs_initial_configure {
                return false;
            }
            state.fullscreen = false;
            state.pre_fullscreen_frame = None;
            state.resize_anchor = None;
            let server_decoration = Self::effective_server_decoration(&state);
            let next_frame = if maximized {
                if state.pre_maximize_frame.is_none() {
                    state.pre_maximize_frame = Some(state.frame);
                }
                state.maximized = true;
                maximized_wayland_client_rect(display_width, display_height, server_decoration)
            } else {
                state.maximized = false;
                let saved = state.pre_maximize_frame.take().unwrap_or(state.frame);
                Self::clamp_frame_to_limits(&state, saved, display_width, display_height)
            };
            let frame_changed = state.frame != next_frame;
            state.frame = next_frame;
            frame_changed || was_maximized != maximized
        };
        if let Some(serial) = self.send_toplevel_configure_serial(shared) {
            if let Ok(mut state) = shared.lock() {
                state.pending_configure_geometry_serial = Some(serial);
            }
        }
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub(super) fn set_surface_fullscreen_shared(
        &mut self,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        fullscreen: bool,
    ) -> bool {
        let display_width = self.display_width;
        let display_height = self.display_height;
        let changed = {
            let Ok(mut state) = shared.lock() else {
                return false;
            };
            if !state.alive {
                return false;
            }
            let was_fullscreen = state.fullscreen;
            if was_fullscreen == fullscreen && !state.needs_initial_configure {
                return false;
            }
            state.resize_anchor = None;
            let next_frame = if fullscreen {
                if state.pre_fullscreen_frame.is_none() {
                    state.pre_fullscreen_frame = Some(state.frame);
                }
                state.fullscreen = true;
                Rect {
                    x: 0,
                    y: 0,
                    width: (display_width as usize).max(1),
                    height: (display_height as usize).max(1),
                }
            } else {
                state.fullscreen = false;
                if let Some(saved) = state.pre_fullscreen_frame.take() {
                    Self::clamp_frame_to_limits(&state, saved, display_width, display_height)
                } else if state.maximized {
                    maximized_wayland_client_rect(
                        display_width,
                        display_height,
                        Self::effective_server_decoration(&state),
                    )
                } else {
                    Self::clamp_frame_to_limits(&state, state.frame, display_width, display_height)
                }
            };
            let frame_changed = state.frame != next_frame;
            state.frame = next_frame;
            frame_changed || was_fullscreen != fullscreen
        };
        if let Some(serial) = self.send_toplevel_configure_serial(shared) {
            if let Ok(mut state) = shared.lock() {
                state.pending_configure_geometry_serial = Some(serial);
            }
        }
        if changed {
            self.mark_dirty();
        }
        changed
    }

    pub(super) fn toggle_surface_maximized(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let next_maximized = shared
            .lock()
            .ok()
            .is_some_and(|state| !(state.maximized || state.pre_maximize_frame.is_some()));
        self.set_surface_maximized_shared(&shared, next_maximized)
    }

    pub(super) fn begin_surface_resize(&mut self, surface_id: u32, _edge: ResizeEdge) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let changed = {
            let Ok(mut state) = shared.lock() else {
                return false;
            };
            if state.resizing {
                return false;
            }
            state.resizing = true;
            true
        };
        self.send_toplevel_configure(&shared);
        changed
    }

    pub(super) fn end_surface_resize(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let changed = {
            let Ok(mut state) = shared.lock() else {
                return false;
            };
            let was_resizing = state.resizing || state.resize_anchor.is_some();
            state.resizing = false;
            state.resize_anchor = None;
            was_resizing
        };
        if changed {
            self.send_toplevel_configure(&shared);
            self.mark_dirty();
        }
        changed
    }

    pub(super) fn resize_surface(
        &mut self,
        surface_id: u32,
        frame: Rect,
        edge: Option<ResizeEdge>,
    ) -> bool {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return false;
        };
        let display_width = self.display_width;
        let display_height = self.display_height;
        let changed = {
            let Ok(mut state) = shared.lock() else {
                return false;
            };
            let clamped = Self::clamp_frame_to_limits(&state, frame, display_width, display_height);
            let server_decoration = Self::effective_server_decoration(&state);
            let next_anchor = edge.filter(|e| e.left || e.top).map(|e| {
                let outer_w = clamped.width + if server_decoration { window_border() * 2 } else { 0 };
                let outer_h = clamped.height
                    + if server_decoration {
                        window_border() * 2 + window_title_height()
                    } else {
                        0
                    };
                ResizeAnchor {
                    anchor_right: e.left,
                    anchor_bottom: e.top,
                    right_edge: clamped.x + outer_w,
                    bottom_edge: clamped.y + outer_h,
                }
            });
            if state.frame == clamped && !state.maximized && state.pre_maximize_frame.is_none() {
                state.resize_anchor = next_anchor;
                return false;
            }
            state.maximized = false;
            state.fullscreen = false;
            state.pre_maximize_frame = None;
            state.pre_fullscreen_frame = None;
            state.resizing = true;
            state.resize_anchor = next_anchor;
            state.frame = clamped;
            true
        };
        if changed {
            if let Some(serial) = self.send_toplevel_configure_serial(&shared) {
                if let Ok(mut state) = shared.lock() {
                    state.pending_configure_geometry_serial = Some(serial);
                }
            }
            self.mark_dirty();
        }
        changed
    }

    pub(super) fn surface_size_limits(&self, surface_id: u32) -> ((usize, usize), (usize, usize)) {
        let Some(shared) = self.find_surface_by_id(surface_id) else {
            return ((0, 0), (0, 0));
        };
        shared
            .lock()
            .map_or(((0, 0), (0, 0)), |state| (state.min_size, state.max_size))
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
