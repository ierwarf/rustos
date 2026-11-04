//! Surface table, snapshots, callbacks, output announcement and configure state.

use super::*;

impl WaylandState {
    pub(super) fn new(display_width: u32, display_height: u32) -> Self {
        Self {
            display_width,
            display_height,
            next_serial: 1,
            next_surface_id: 1,
            cascade_base: 0,
            started_at: Instant::now(),
            surfaces: Vec::new(),
            output_resources: Vec::new(),
            pointer_resources: Vec::new(),
            keyboard_resources: Vec::new(),
            pointer_focus: None,
            pointer_target: None,
            keyboard_focus: None,
            pointer_x: display_width / 2,
            pointer_y: display_height / 2,
            pointer_button_down: false,
            pointer_motion_pending: false,
            next_pointer_motion_flush: Instant::now(),
            dirty: false,
            callback_profile_started: Instant::now(),
            callback_profile_count: 0,
            callback_profile_wait_micros: 0,
            callback_profile_max_wait_micros: 0,
            buffer_copy_profile_count: 0,
            buffer_copy_profile_bytes: 0,
            buffer_copy_profile_micros: 0,
            buffer_copy_profile_max_micros: 0,
        }
    }

    pub(super) fn next_configure_serial(&mut self) -> u32 {
        let serial = self.next_serial.max(1);
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        serial
    }

    pub(super) fn allocate_window_frame(&mut self) -> Rect {
        let (max_w, max_h) = wayland_max_client_size(self.display_width, self.display_height);
        // The hint we send to clients is "use 11/20 by 3/5 of the screen",
        // but a misbehaving client may keep its own size — so the hint here
        // also doubles as the upper bound we render at.
        let suggested_w = ((self.display_width as usize).saturating_mul(11) / 20).min(max_w);
        let suggested_h = ((self.display_height as usize).saturating_mul(3) / 5).min(max_h);

        // Continue the cascade after every window already on screen, console
        // windows included, so the new window leaves the title bars of the
        // earlier ones visible instead of covering them exactly.
        let live_surfaces = self
            .surfaces
            .iter()
            .filter(|surface| surface.shared.lock().is_ok_and(|state| state.alive))
            .count();
        let index = self.cascade_base.saturating_add(live_surfaces);

        let desktop = layout::desktop_bounds(self.display_width, self.display_height);
        let (x, y) = layout::cascade_origin(desktop, index);

        clamp_wayland_frame(
            Rect {
                x,
                y,
                width: suggested_w,
                height: suggested_h,
            },
            self.display_width,
            self.display_height,
        )
    }

    /// Allocate a surface identity that no live surface holds.
    pub(super) fn allocate_surface_id(&mut self) -> u32 {
        loop {
            let id = self.next_surface_id.max(1);
            self.next_surface_id = id.wrapping_add(1).max(1);
            let in_use = self
                .surfaces
                .iter()
                .any(|surface| surface.shared.lock().is_ok_and(|state| state.surface_id == id));
            if !in_use {
                return id;
            }
        }
    }

    pub(super) fn client_surface_count(&self, client_id: &ClientId) -> usize {
        self.surfaces
            .iter()
            .filter(|surface| {
                surface
                    .shared
                    .lock()
                    .is_ok_and(|state| state.client_id.as_ref() == Some(client_id))
            })
            .count()
    }

    /// Pixels retained by every surface except `except`.
    pub(super) fn retained_surface_pixels_excluding(&self, except: &Arc<Mutex<WaylandSurfaceState>>) -> usize {
        self.surfaces
            .iter()
            .filter(|surface| !Arc::ptr_eq(&surface.shared, except))
            .filter_map(|surface| surface.shared.lock().ok().map(|state| state.pixels.len()))
            .fold(0_usize, usize::saturating_add)
    }

    pub(super) fn create_surface_data(&mut self, client_id: &ClientId) -> Option<SurfaceData> {
        self.prune_surfaces();
        if self.surfaces.len() >= MAX_WAYLAND_SURFACES
            || self.client_surface_count(client_id) >= MAX_WAYLAND_SURFACES_PER_CLIENT
        {
            return None;
        }
        let surface_id = self.allocate_surface_id();
        let data = SurfaceData {
            shared: Arc::new(Mutex::new(WaylandSurfaceState {
                surface_id,
                client_id: Some(client_id.clone()),
                title: String::from("Wayland App"),
                frame: self.allocate_window_frame(),
                alive: true,
                ..WaylandSurfaceState::default()
            })),
        };
        self.surfaces.push(data.clone());
        self.dirty = true;
        Some(data)
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty;
        self.dirty = false;
        dirty
    }

    pub(super) fn window_snapshots(&self) -> Vec<WaylandWindowSnapshot> {
        let mut snapshots = Vec::new();
        for surface in &self.surfaces {
            let Ok(surface) = surface.shared.lock() else {
                continue;
            };
            if !surface.alive
                || surface.pixels.is_empty()
                || surface.width == 0
                || surface.height == 0
                || surface.stride_pixels < surface.width
            {
                continue;
            }
            let Some(required_len) = surface
                .stride_pixels
                .checked_mul(surface.height.saturating_sub(1))
                .and_then(|prefix| prefix.checked_add(surface.width))
            else {
                continue;
            };
            if required_len > surface.pixels.len() {
                continue;
            }
            snapshots.push(WaylandWindowSnapshot {
                surface_id: surface.surface_id,
                title: surface.title.clone(),
                frame: surface.frame,
                minimized: surface.minimized,
                content_version: surface.content_version,
                damage: surface.last_damage,
                pixels: surface.pixels.clone(),
                width: surface.width,
                height: surface.height,
                stride_pixels: surface.stride_pixels,
            });
        }
        snapshots
    }

    pub(super) fn clear_window_damage(&mut self) {
        for surface in &self.surfaces {
            if let Ok(mut state) = surface.shared.lock() {
                state.last_damage = Rect::empty();
            }
        }
    }

    pub(super) fn event_time_ms(&self) -> u32 {
        let millis = self.started_at.elapsed().as_millis();
        u32::try_from(millis.min(u128::from(u32::MAX))).unwrap_or(u32::MAX)
    }

    pub(super) fn record_buffer_copy(&mut self, elapsed: Duration, bytes: usize) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.buffer_copy_profile_count = self.buffer_copy_profile_count.saturating_add(1);
        self.buffer_copy_profile_bytes = self
            .buffer_copy_profile_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        self.buffer_copy_profile_micros = self.buffer_copy_profile_micros.saturating_add(micros);
        self.buffer_copy_profile_max_micros = self.buffer_copy_profile_max_micros.max(micros);
    }

    /// Emit `BufferStorageReleased` for every buffer whose contents the
    /// compositor has already copied out.
    ///
    /// `V5-GPU-UI-OWNER-014` §4.2 states the two completions separately and
    /// says release "may precede presentation". It could not: the release
    /// drain shared `send_frame_callbacks`, so a buffer the compositor had
    /// provably finished reading at commit time stayed held until a frame
    /// permit arrived. The client then waited a full presentation for storage
    /// it could already have refilled. Storage custody ends when the copy
    /// ends, and that is a different fact from a frame having been shown.
    pub(crate) fn flush_buffer_releases(&mut self) -> u64 {
        let mut released = 0_u64;
        for surface in &self.surfaces {
            let releases = {
                let Ok(mut surface) = surface.shared.lock() else {
                    continue;
                };
                surface
                    .pending_buffer_releases
                    .drain(..)
                    .collect::<Vec<_>>()
            };
            for buffer in releases {
                released = released.saturating_add(1);
                buffer.release();
            }
        }
        released
    }

    pub(super) fn send_frame_callbacks(&mut self) {
        let time = self.event_time_ms();
        // `V5-UI-PIPELINE-011`: both completions carry the frame identity that
        // produced them. Release may precede presentation, so they are counted
        // separately rather than assumed to move together.
        let frame_seq = crate::loop_timing::current_frame_seq();
        let mut completed = 0_u64;
        let mut max_wait_micros = 0_u64;
        for surface in &self.surfaces {
            let callbacks = {
                let Ok(mut surface) = surface.shared.lock() else {
                    continue;
                };
                // Protocol progress must not be gated on render state.
                //
                // This used to also require a populated pixel cache and a
                // non-zero size. A frame requested before the first buffer copy
                // then had its callback withheld indefinitely, and the client
                // reused the protocol id for its next request while the server
                // still held the old object — which is how WayClick died with
                // `wl_display` `Invalid new_id: 15` right after presenting its
                // first frame. The instrumented run shows it plainly: every
                // callback carries protocol id 15, and the two that never
                // received `done` are the ones that stranded it.
                //
                // Visibility is a legitimate reason to withhold a callback; the
                // compositor's own buffer bookkeeping is not, and the audit's
                // `V5-WAYLAND-HOL-013` is exactly this coupling.
                if surface.alive && !surface.minimized {
                    surface.pending_callbacks.drain(..).collect::<Vec<_>>()
                } else {
                    Vec::new()
                }
            };
            for pending in callbacks {
                let wait_micros =
                    u64::try_from(pending.requested_at.elapsed().as_micros()).unwrap_or(u64::MAX);
                self.callback_profile_count = self.callback_profile_count.saturating_add(1);
                self.callback_profile_wait_micros = self
                    .callback_profile_wait_micros
                    .saturating_add(wait_micros);
                self.callback_profile_max_wait_micros =
                    self.callback_profile_max_wait_micros.max(wait_micros);
                completed = completed.saturating_add(1);
                max_wait_micros = max_wait_micros.max(wait_micros);
                if WAYLAND_CALLBACK_ID_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                    < MAX_WAYLAND_CALLBACK_ID_LOGS
                {
                    crate::sys::debug_line(&format!(
                        "uiserver: wayland frame callback done frame_seq={frame_seq} id={:?}",
                        pending.callback.id()
                    ));
                }
                pending.callback.done(time);
            }
        }
        if completed != 0 && crate::loop_timing::frame_seq_is_sampled(frame_seq) {
            crate::sys::debug_line(&format!(
                "uiserver: frame completion frame_seq={frame_seq} callbacks={completed} max_wait_us={max_wait_micros}"
            ));
        }
        let profile_elapsed = self.callback_profile_started.elapsed();
        if ui_profile_enabled() && profile_elapsed >= Duration::from_secs(1) {
            let elapsed_micros = u64::try_from(profile_elapsed.as_micros())
                .unwrap_or(u64::MAX)
                .max(1);
            crate::sys::profile_line(&format!(
                "uiserver wayland callback profile: elapsed_ms={} callback_hz_milli={} avg_wait_ms={} max_wait_ms={} shm_copies={} shm_copy_bytes={} shm_copy_avg_us={} shm_copy_max_us={}",
                elapsed_micros / 1_000,
                self.callback_profile_count
                    .saturating_mul(1_000_000_000)
                    .saturating_div(elapsed_micros),
                self.callback_profile_wait_micros / self.callback_profile_count.max(1) / 1_000,
                self.callback_profile_max_wait_micros / 1_000,
                self.buffer_copy_profile_count,
                self.buffer_copy_profile_bytes,
                self.buffer_copy_profile_micros / self.buffer_copy_profile_count.max(1),
                self.buffer_copy_profile_max_micros,
            ));
            self.callback_profile_started = Instant::now();
            self.callback_profile_count = 0;
            self.callback_profile_wait_micros = 0;
            self.callback_profile_max_wait_micros = 0;
            self.buffer_copy_profile_count = 0;
            self.buffer_copy_profile_bytes = 0;
            self.buffer_copy_profile_micros = 0;
            self.buffer_copy_profile_max_micros = 0;
        }
    }

    pub(super) fn prune_surfaces(&mut self) {
        self.surfaces.retain(|surface| {
            surface
                .shared
                .lock()
                .map(|state| {
                    state.alive
                        && state
                            .resource
                            .as_ref()
                            .map(|resource| resource.is_alive())
                            .unwrap_or(false)
                })
                .unwrap_or(false)
        });
    }

    pub(super) fn prune_surface_callbacks(surface: &mut WaylandSurfaceState) {
        surface
            .pending_callbacks
            .retain(|pending| pending.callback.is_alive());
    }

    pub(super) fn surface_callback_count(surface: &mut WaylandSurfaceState) -> usize {
        Self::prune_surface_callbacks(surface);
        surface.pending_callbacks.len()
    }

    pub(super) fn output_resource_count(&mut self) -> usize {
        self.prune_output_resources();
        self.output_resources.len()
    }

    pub(super) fn pointer_resource_count(&mut self) -> usize {
        self.prune_pointer_resources();
        self.pointer_resources.len()
    }

    pub(super) fn keyboard_resource_count(&mut self) -> usize {
        self.prune_keyboard_resources();
        self.keyboard_resources.len()
    }

    /// Whether `client_id` may add another tracked output. The compositor-wide
    /// ceiling alone let one client fill the table and make every other
    /// client's bind fail (and, for seat devices, die with a protocol error).
    pub(super) fn can_track_output_resource(&mut self, client_id: &ClientId) -> bool {
        self.output_resource_count() < MAX_WAYLAND_OUTPUT_RESOURCES
            && self
                .output_resources
                .iter()
                .filter(|output| &output.client_id == client_id)
                .count()
                < MAX_WAYLAND_RESOURCES_PER_CLIENT
    }

    pub(super) fn can_track_pointer_resource(&mut self, client_id: &ClientId) -> bool {
        self.pointer_resource_count() < MAX_WAYLAND_POINTER_RESOURCES
            && self
                .pointer_resources
                .iter()
                .filter(|pointer| &pointer.client_id == client_id)
                .count()
                < MAX_WAYLAND_RESOURCES_PER_CLIENT
    }

    pub(super) fn can_track_keyboard_resource(&mut self, client_id: &ClientId) -> bool {
        self.keyboard_resource_count() < MAX_WAYLAND_KEYBOARD_RESOURCES
            && self
                .keyboard_resources
                .iter()
                .filter(|keyboard| &keyboard.client_id == client_id)
                .count()
                < MAX_WAYLAND_RESOURCES_PER_CLIENT
    }

    pub(super) fn pending_frame_callback_rect(&self) -> Rect {
        let mut rect = Rect::empty();
        for surface in &self.surfaces {
            let Ok(surface) = surface.shared.lock() else {
                continue;
            };
            if !surface.alive
                || surface.minimized
                || surface.pending_callbacks.is_empty()
                || surface.pixels.is_empty()
                || surface.width == 0
                || surface.height == 0
            {
                continue;
            }
            rect = rect.union(surface.frame);
        }
        rect
    }

    pub(super) fn prune_pointer_resources(&mut self) {
        self.pointer_resources
            .retain(|pointer| pointer.resource.is_alive());
    }

    pub(super) fn prune_output_resources(&mut self) {
        self.output_resources
            .retain(|output| output.resource.is_alive());
    }

    pub(super) fn prune_keyboard_resources(&mut self) {
        self.keyboard_resources
            .retain(|keyboard| keyboard.resource.is_alive());
    }

    pub(super) fn announce_bound_output_to_surfaces(
        &mut self,
        client_id: ClientId,
        output: &wl_output::WlOutput,
    ) {
        let mut enter_targets = Vec::new();
        for surface in &self.surfaces {
            let Ok(state) = surface.shared.lock() else {
                continue;
            };
            if !state.alive || state.width == 0 || state.height == 0 || state.pixels.is_empty() {
                continue;
            }
            if state.client_id.as_ref() != Some(&client_id) {
                continue;
            }
            let Some(resource) = state.resource.as_ref() else {
                continue;
            };
            enter_targets.push((surface.shared.clone(), resource.clone()));
        }

        for (shared, resource) in enter_targets {
            resource.enter(output);
            if let Ok(mut state) = shared.lock() {
                if state
                    .resource
                    .as_ref()
                    .map(|current| current.id() == resource.id())
                    .unwrap_or(false)
                {
                    state.on_output = true;
                }
            }
        }
    }

    pub(super) fn sync_surface_output(&mut self, surface: &Arc<Mutex<WaylandSurfaceState>>, mapped: bool) {
        self.prune_output_resources();

        let (resource, client_id, on_output) = {
            let Ok(state) = surface.lock() else {
                return;
            };
            let Some(resource) = state.resource.clone() else {
                return;
            };
            let Some(client_id) = state.client_id.clone() else {
                return;
            };
            (resource, client_id, state.on_output)
        };

        if mapped == on_output {
            return;
        }

        let outputs: Vec<_> = self
            .output_resources
            .iter()
            .filter(|output| output.client_id == client_id)
            .map(|output| output.resource.clone())
            .collect();

        if outputs.is_empty() {
            if let Ok(mut state) = surface.lock() {
                state.on_output = false;
            }
            return;
        }

        for output in outputs {
            if mapped {
                resource.enter(&output);
            } else {
                resource.leave(&output);
            }
        }

        if let Ok(mut state) = surface.lock() {
            state.on_output = mapped;
        }
    }

    pub(super) fn send_toplevel_configure(&mut self, surface: &Arc<Mutex<WaylandSurfaceState>>) {
        let (xdg_surface, toplevel, width, height) = {
            let Ok(state) = surface.lock() else {
                return;
            };
            let Some(xdg_surface) = state.xdg_surface.clone() else {
                return;
            };
            let Some(toplevel) = state.toplevel.clone() else {
                return;
            };
            (
                xdg_surface,
                toplevel,
                i32::try_from(state.frame.width).unwrap_or(0),
                i32::try_from(state.frame.height).unwrap_or(0),
            )
        };

        toplevel.configure(width, height, Vec::<u8>::new());
        let serial = self.next_configure_serial();
        xdg_surface.configure(serial);

        if let Ok(mut state) = surface.lock() {
            state.configured_serial = serial;
            state.acknowledged_serial = 0;
            if state.outstanding_configures.len() >= MAX_WAYLAND_OUTSTANDING_CONFIGURES {
                state.outstanding_configures.remove(0);
            }
            state.outstanding_configures.push(serial);
            state.needs_initial_configure = false;
        }
    }
}
