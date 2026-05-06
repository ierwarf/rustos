use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use std::{
    fs,
    path::{Component, Path},
};

use wayland_protocols::xdg::decoration::zv1::server::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::server::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::backend::{ClientData, ClientId, ObjectId};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat,
    wl_shm, wl_shm_pool, wl_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::canvas::Rect;
use crate::sys::{
    diag_line, map_shared_fd_readable, InputEvent, SharedFdMapping, INPUT_ACTION_PRESSED,
    INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD,
};

const WAYLAND_SOCKET_NAME: &str = "wayland-0";
const WINDOW_CASCADE_X: usize = 42;
const WINDOW_CASCADE_Y: usize = 30;
const WINDOW_CASCADE_SLOTS: usize = 6;
const WINDOW_TITLEBAR_HEIGHT: usize = 36;
const LINUX_BTN_LEFT: u32 = 0x110;
const MAX_WAYLAND_SHM_POOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_WAYLAND_BUFFER_DIMENSION: usize = 8192;
const MAX_WAYLAND_BUFFER_PIXELS: usize = MAX_WAYLAND_SHM_POOL_BYTES / 4;

fn post_protocol_error<I: Resource>(resource: &I, message: String) {
    diag_line(&format!("uiserver: wayland protocol error: {message}"));
    resource.post_error(0_u32, message);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WaylandWindowSnapshot {
    pub(crate) surface_id: u32,
    pub(crate) title: String,
    pub(crate) frame: Rect,
    pub(crate) minimized: bool,
    pub(crate) pixels: Arc<Vec<u32>>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride_pixels: usize,
}

pub(crate) struct WaylandCompositor {
    display: Display<WaylandState>,
    listener: UnixListener,
    state: WaylandState,
}

impl WaylandCompositor {
    pub(crate) fn initialize(display_width: u32, display_height: u32) -> Option<Self> {
        let runtime_dir = current_runtime_dir();
        let socket_path = format!("{runtime_dir}/{WAYLAND_SOCKET_NAME}");
        let display = match Display::new() {
            Ok(display) => display,
            Err(err) => {
                diag_line(&format!("uiserver: wayland display init failed: {err}"));
                return None;
            }
        };
        let listener = match bind_wayland_listener(runtime_dir.as_str(), socket_path.as_str()) {
            Ok(listener) => listener,
            Err(err) => {
                diag_line(&format!(
                    "uiserver: wayland socket bind failed path={} err={err}",
                    socket_path
                ));
                return None;
            }
        };
        if let Err(err) = listener.set_nonblocking(true) {
            diag_line(&format!(
                "uiserver: wayland listener nonblocking failed: {err}"
            ));
            return None;
        }

        let state = WaylandState::new(display_width, display_height);
        {
            let handle = display.handle();
            handle.create_global::<WaylandState, wl_compositor::WlCompositor, _>(6, ());
            handle.create_global::<WaylandState, wl_shm::WlShm, _>(1, ());
            handle.create_global::<WaylandState, wl_output::WlOutput, _>(4, ());
            handle.create_global::<WaylandState, wl_seat::WlSeat, _>(7, ());
            handle.create_global::<WaylandState, xdg_wm_base::XdgWmBase, _>(6, ());
            handle.create_global::<WaylandState, zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _>(
                1,
                (),
            );
        }
        diag_line(&format!(
            "uiserver: wayland compositor ready on {}/{}",
            runtime_dir, WAYLAND_SOCKET_NAME
        ));

        Some(Self {
            display,
            listener,
            state,
        })
    }

    pub(crate) fn tick(&mut self) -> bool {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(err) = stream.set_nonblocking(true) {
                        diag_line(&format!(
                            "uiserver: accepted wayland client nonblocking failed: {err}"
                        ));
                        continue;
                    }
                    if let Err(err) = self
                        .display
                        .handle()
                        .insert_client(stream, Arc::new(WaylandClientState))
                    {
                        diag_line(&format!("uiserver: wayland insert_client failed: {err}"));
                    } else {
                        self.state.dirty = true;
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) => {
                    diag_line(&format!("uiserver: wayland accept failed: {err}"));
                    break;
                }
            }
        }

        if let Err(err) = self.display.dispatch_clients(&mut self.state) {
            diag_line(&format!("uiserver: wayland dispatch failed: {err}"));
        }
        if let Err(err) = self.display.flush_clients() {
            diag_line(&format!("uiserver: wayland flush failed: {err}"));
        }

        self.state.take_dirty()
    }

    pub(crate) fn frame_presented(&mut self) {
        self.state.send_frame_callbacks();
        if let Err(err) = self.display.flush_clients() {
            diag_line(&format!("uiserver: wayland flush failed: {err}"));
        }
    }

    pub(crate) fn window_snapshots(&self) -> Vec<WaylandWindowSnapshot> {
        self.state.window_snapshots()
    }

    pub(crate) fn pointer_motion(&mut self, x: u32, y: u32) {
        self.state.pointer_motion(x, y);
    }

    pub(crate) fn pointer_button(&mut self, button: u32, pressed: bool) -> bool {
        self.state.pointer_button(button, pressed)
    }

    pub(crate) fn keyboard_input(&mut self, event: InputEvent) -> bool {
        self.state.keyboard_input(event)
    }

    pub(crate) fn focus_surface(&mut self, surface_id: u32) -> bool {
        self.state.focus_surface(surface_id)
    }

    pub(crate) fn move_surface(&mut self, surface_id: u32, x: usize, y: usize) -> bool {
        self.state.move_surface(surface_id, x, y)
    }

    pub(crate) fn set_surface_minimized(&mut self, surface_id: u32, minimized: bool) -> bool {
        self.state.set_surface_minimized(surface_id, minimized)
    }

    pub(crate) fn close_surface(&mut self, surface_id: u32) -> bool {
        self.state.close_surface(surface_id)
    }

    pub(crate) fn clear_focus(&mut self) {
        self.state.clear_focus();
    }
}

fn current_runtime_dir() -> String {
    const FALLBACK_RUNTIME_DIR: &str = "/run";
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| safe_runtime_dir(value.as_str()))
        .unwrap_or_else(|| String::from(FALLBACK_RUNTIME_DIR))
}

fn bind_wayland_listener(runtime_dir: &str, socket_path: &str) -> std::io::Result<UnixListener> {
    fs::create_dir_all(runtime_dir)?;
    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            if stale_wayland_socket(runtime_dir, socket_path)? {
                fs::remove_file(socket_path)?;
                UnixListener::bind(socket_path)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn stale_wayland_socket(runtime_dir: &str, socket_path: &str) -> std::io::Result<bool> {
    let socket = Path::new(socket_path);
    let runtime = Path::new(runtime_dir);
    if !socket.starts_with(runtime) {
        return Ok(false);
    }
    if !fs::symlink_metadata(socket_path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
    {
        return Ok(false);
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Ok(false),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

fn safe_runtime_dir(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 108 - WAYLAND_SOCKET_NAME.len() - 1
        && path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::RootDir | Component::Normal(_) | Component::Prefix(_)
            )
        })
}

fn checked_wayland_pixel_count(width: usize, height: usize) -> Option<usize> {
    if width == 0
        || height == 0
        || width > MAX_WAYLAND_BUFFER_DIMENSION
        || height > MAX_WAYLAND_BUFFER_DIMENSION
    {
        return None;
    }
    let pixels = width.checked_mul(height)?;
    if pixels > MAX_WAYLAND_BUFFER_PIXELS {
        return None;
    }
    Some(pixels)
}

fn validate_wayland_buffer_layout(
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    pool_len: usize,
) -> Option<usize> {
    let _ = checked_wayland_pixel_count(width, height)?;
    let width_bytes = width.checked_mul(4)?;
    if stride < width_bytes || stride > MAX_WAYLAND_SHM_POOL_BYTES {
        return None;
    }
    let required = stride.checked_mul(height)?;
    if required > MAX_WAYLAND_SHM_POOL_BYTES {
        return None;
    }
    let end = offset.checked_add(required)?;
    if end > pool_len {
        return None;
    }
    Some(required)
}

fn surface_damage_rect(x: i32, y: i32, width: i32, height: i32) -> Option<Rect> {
    if width <= 0 || height <= 0 {
        return None;
    }

    let x0 = x.max(0) as usize;
    let y0 = y.max(0) as usize;
    let x1 = i64::from(x)
        .saturating_add(i64::from(width))
        .max(0)
        .min(MAX_WAYLAND_BUFFER_DIMENSION as i64) as usize;
    let y1 = i64::from(y)
        .saturating_add(i64::from(height))
        .max(0)
        .min(MAX_WAYLAND_BUFFER_DIMENSION as i64) as usize;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }

    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        checked_wayland_pixel_count, validate_wayland_buffer_layout, MAX_WAYLAND_BUFFER_DIMENSION,
        MAX_WAYLAND_SHM_POOL_BYTES,
    };

    #[test]
    fn wayland_buffer_limits_reject_oversized_dimensions() {
        assert!(checked_wayland_pixel_count(MAX_WAYLAND_BUFFER_DIMENSION + 1, 1).is_none());
        assert!(checked_wayland_pixel_count(1, MAX_WAYLAND_BUFFER_DIMENSION + 1).is_none());
        assert!(checked_wayland_pixel_count(1920, 1080).is_some());
    }

    #[test]
    fn wayland_buffer_layout_rejects_out_of_bounds_and_bad_stride() {
        assert!(validate_wayland_buffer_layout(0, 128, 128, 128 * 4, 128 * 128 * 4).is_some());
        assert!(validate_wayland_buffer_layout(0, 128, 128, 127 * 4, 128 * 128 * 4).is_none());
        assert!(validate_wayland_buffer_layout(
            MAX_WAYLAND_SHM_POOL_BYTES,
            128,
            128,
            128 * 4,
            MAX_WAYLAND_SHM_POOL_BYTES,
        )
        .is_none());
    }
}

#[derive(Debug)]
struct WaylandClientState;

impl ClientData for WaylandClientState {}

struct WaylandState {
    display_width: u32,
    display_height: u32,
    next_serial: u32,
    next_surface_index: usize,
    started_at: Instant,
    surfaces: Vec<SurfaceData>,
    output_resources: Vec<OutputResource>,
    pointer_resources: Vec<PointerResource>,
    keyboard_resources: Vec<KeyboardResource>,
    pointer_focus: Option<PointerFocus>,
    keyboard_focus: Option<KeyboardFocus>,
    pointer_x: u32,
    pointer_y: u32,
    pointer_button_down: bool,
    dirty: bool,
}

impl WaylandState {
    fn new(display_width: u32, display_height: u32) -> Self {
        Self {
            display_width,
            display_height,
            next_serial: 1,
            next_surface_index: 0,
            started_at: Instant::now(),
            surfaces: Vec::new(),
            output_resources: Vec::new(),
            pointer_resources: Vec::new(),
            keyboard_resources: Vec::new(),
            pointer_focus: None,
            keyboard_focus: None,
            pointer_x: display_width / 2,
            pointer_y: display_height / 2,
            pointer_button_down: false,
            dirty: false,
        }
    }

    fn next_configure_serial(&mut self) -> u32 {
        let serial = self.next_serial.max(1);
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        serial
    }

    fn allocate_window_frame(&mut self) -> Rect {
        let width = (self.display_width as usize).saturating_mul(11) / 20;
        let height = (self.display_height as usize).saturating_mul(3) / 5;
        let index = self.next_surface_index;
        self.next_surface_index = self.next_surface_index.saturating_add(1);
        let step = index % WINDOW_CASCADE_SLOTS;
        Rect {
            x: 72 + step * WINDOW_CASCADE_X,
            y: 84 + step * WINDOW_CASCADE_Y,
            width,
            height,
        }
    }

    fn create_surface_data(&mut self) -> SurfaceData {
        let data = SurfaceData {
            shared: Arc::new(Mutex::new(WaylandSurfaceState {
                title: String::from("Wayland App"),
                frame: self.allocate_window_frame(),
                alive: true,
                ..WaylandSurfaceState::default()
            })),
        };
        self.surfaces.push(data.clone());
        self.dirty = true;
        data
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn take_dirty(&mut self) -> bool {
        let dirty = self.dirty;
        self.dirty = false;
        dirty
    }

    fn window_snapshots(&self) -> Vec<WaylandWindowSnapshot> {
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
                surface_id: surface
                    .resource
                    .as_ref()
                    .map(|resource| resource.id().protocol_id())
                    .unwrap_or(0),
                title: surface.title.clone(),
                frame: surface.frame,
                minimized: surface.minimized,
                pixels: surface.pixels.clone(),
                width: surface.width,
                height: surface.height,
                stride_pixels: surface.stride_pixels,
            });
        }
        snapshots
    }

    fn event_time_ms(&self) -> u32 {
        let millis = self.started_at.elapsed().as_millis();
        u32::try_from(millis.min(u128::from(u32::MAX))).unwrap_or(u32::MAX)
    }

    fn send_frame_callbacks(&mut self) {
        let time = self.event_time_ms();
        for surface in &self.surfaces {
            let Ok(mut surface) = surface.shared.lock() else {
                continue;
            };
            if !surface.alive || surface.minimized || surface.pixels.is_empty() {
                continue;
            }
            for callback in surface.pending_callbacks.drain(..) {
                callback.done(time);
            }
        }
    }

    fn prune_pointer_resources(&mut self) {
        self.pointer_resources
            .retain(|pointer| pointer.resource.is_alive());
    }

    fn prune_output_resources(&mut self) {
        self.output_resources
            .retain(|output| output.resource.is_alive());
    }

    fn prune_keyboard_resources(&mut self) {
        self.keyboard_resources
            .retain(|keyboard| keyboard.resource.is_alive());
    }

    fn announce_bound_output_to_surfaces(
        &mut self,
        client_id: ClientId,
        output: &wl_output::WlOutput,
    ) {
        for surface in &self.surfaces {
            let Ok(mut state) = surface.shared.lock() else {
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
            resource.enter(output);
            state.on_output = true;
        }
    }

    fn sync_surface_output(&mut self, surface: &Arc<Mutex<WaylandSurfaceState>>, mapped: bool) {
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

    fn send_toplevel_configure(&mut self, surface: &Arc<Mutex<WaylandSurfaceState>>) {
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
            state.needs_initial_configure = false;
        }
    }

    fn hit_test_pointer(&self, x: u32, y: u32) -> Option<PointerHit> {
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
            let client_rect = Rect {
                x: surface.frame.x + 1,
                y: surface.frame.y + WINDOW_TITLEBAR_HEIGHT + 1,
                width: surface.width,
                height: surface.height,
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

    fn update_pointer_focus(&mut self, hit: Option<PointerHit>) {
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

    fn pointer_motion(&mut self, x: u32, y: u32) {
        self.pointer_x = x.min(self.display_width.saturating_sub(1));
        self.pointer_y = y.min(self.display_height.saturating_sub(1));
        let hit = self.hit_test_pointer(self.pointer_x, self.pointer_y);
        self.update_pointer_focus(hit.clone());
        let Some(focus) = self.pointer_focus.as_ref() else {
            return;
        };
        let time = self.event_time_ms();
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

    fn pointer_button(&mut self, button: u32, pressed: bool) -> bool {
        if button != 1 {
            return false;
        }

        if pressed {
            let Some(hit) = self.hit_test_pointer(self.pointer_x, self.pointer_y) else {
                return false;
            };
            self.bring_surface_to_front(hit.surface.id().clone());
            self.set_keyboard_focus(Some(KeyboardFocus {
                surface: hit.surface.clone(),
                client_id: hit.client_id.clone(),
            }));
            self.update_pointer_focus(Some(hit));
            self.pointer_button_down = true;
            self.mark_dirty();
        } else if !self.pointer_button_down {
            return false;
        } else {
            self.pointer_button_down = false;
        }

        let Some(focus) = self.pointer_focus.as_ref() else {
            return false;
        };
        let client_id = focus.client_id.clone();
        let serial = self.next_configure_serial();
        let time = self.event_time_ms();
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        for pointer in self
            .pointer_resources
            .iter()
            .filter(|pointer| pointer.client_id == client_id)
        {
            pointer.resource.button(serial, time, LINUX_BTN_LEFT, state);
            if pointer.resource.version() >= 5 {
                pointer.resource.frame();
            }
        }
        true
    }

    fn set_keyboard_focus(&mut self, next: Option<KeyboardFocus>) {
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

    fn keyboard_input(&mut self, event: InputEvent) -> bool {
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

    fn bring_surface_to_front(&mut self, object_id: ObjectId) {
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

    fn find_surface_by_protocol_id(
        &self,
        surface_id: u32,
    ) -> Option<Arc<Mutex<WaylandSurfaceState>>> {
        self.surfaces.iter().find_map(|surface| {
            let state = surface.shared.lock().ok()?;
            let resource = state.resource.as_ref()?;
            if resource.id().protocol_id() == surface_id {
                Some(surface.shared.clone())
            } else {
                None
            }
        })
    }

    fn clear_focus_for_surface(&mut self, surface_id: u32) {
        let pointer_matches = self
            .pointer_focus
            .as_ref()
            .map(|focus| focus.surface.id().protocol_id() == surface_id)
            .unwrap_or(false);
        if pointer_matches {
            self.update_pointer_focus(None);
        }

        let keyboard_matches = self
            .keyboard_focus
            .as_ref()
            .map(|focus| focus.surface.id().protocol_id() == surface_id)
            .unwrap_or(false);
        if keyboard_matches {
            self.set_keyboard_focus(None);
        }
    }

    fn retire_surface(
        &mut self,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        surface_id: u32,
    ) -> bool {
        diag_line(&format!(
            "uiserver: retire_surface begin surface={surface_id}"
        ));
        self.clear_focus_for_surface(surface_id);
        self.pointer_button_down = false;
        self.sync_surface_output(shared, false);
        let Ok(mut state) = shared.lock() else {
            diag_line(&format!(
                "uiserver: retire_surface lock failed surface={surface_id}"
            ));
            return false;
        };
        diag_line(&format!(
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
        state.width = 0;
        state.height = 0;
        state.stride_pixels = 0;
        state.pending_buffer = None;
        state.pending_callbacks.clear();
        state.on_output = false;
        state.needs_initial_configure = false;
        drop(state);
        self.mark_dirty();
        diag_line(&format!(
            "uiserver: retire_surface end surface={surface_id}"
        ));
        true
    }

    fn focus_surface(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
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

    fn move_surface(&mut self, surface_id: u32, x: usize, y: usize) -> bool {
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
            return false;
        };
        let Ok(mut state) = shared.lock() else {
            return false;
        };
        if state.frame.x == x && state.frame.y == y {
            return false;
        }
        state.frame.x = x;
        state.frame.y = y;
        drop(state);
        self.mark_dirty();
        true
    }

    fn set_surface_minimized(&mut self, surface_id: u32, minimized: bool) -> bool {
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
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
        if minimized {
            self.clear_focus_for_surface(surface_id);
        } else if let Some(object_id) = object_id {
            self.bring_surface_to_front(object_id);
        }
        self.mark_dirty();
        true
    }

    fn close_surface(&mut self, surface_id: u32) -> bool {
        diag_line(&format!(
            "uiserver: close_surface begin surface={surface_id}"
        ));
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
            diag_line(&format!(
                "uiserver: close_surface missing surface={surface_id}"
            ));
            return false;
        };
        let toplevel = shared.lock().ok().and_then(|state| state.toplevel.clone());
        let retired = self.retire_surface(&shared, surface_id);
        diag_line(&format!(
            "uiserver: close_surface retired surface={} retired={} has_toplevel={}",
            surface_id,
            retired,
            toplevel.is_some()
        ));
        let Some(toplevel) = toplevel else {
            return retired;
        };
        toplevel.close();
        diag_line(&format!(
            "uiserver: close_surface close_sent surface={surface_id}"
        ));
        retired
    }

    fn clear_focus(&mut self) {
        self.update_pointer_focus(None);
        self.set_keyboard_focus(None);
        self.pointer_button_down = false;
    }
}

#[derive(Clone)]
struct SurfaceData {
    shared: Arc<Mutex<WaylandSurfaceState>>,
}

#[derive(Clone)]
struct PointerResource {
    resource: wl_pointer::WlPointer,
    client_id: ClientId,
}

#[derive(Clone)]
struct OutputResource {
    resource: wl_output::WlOutput,
    client_id: ClientId,
}

#[derive(Clone)]
struct KeyboardResource {
    resource: wl_keyboard::WlKeyboard,
    client_id: ClientId,
}

#[derive(Clone)]
struct PointerHit {
    surface: wl_surface::WlSurface,
    client_id: ClientId,
    surface_x: f64,
    surface_y: f64,
}

#[derive(Clone)]
struct PointerFocus {
    surface: wl_surface::WlSurface,
    client_id: ClientId,
    surface_x: f64,
    surface_y: f64,
}

#[derive(Clone)]
struct KeyboardFocus {
    surface: wl_surface::WlSurface,
    client_id: ClientId,
}

impl PointerFocus {
    fn from_hit(hit: PointerHit) -> Self {
        Self {
            surface: hit.surface,
            client_id: hit.client_id,
            surface_x: hit.surface_x,
            surface_y: hit.surface_y,
        }
    }
}

#[derive(Default)]
struct WaylandSurfaceState {
    title: String,
    frame: Rect,
    minimized: bool,
    resource: Option<wl_surface::WlSurface>,
    client_id: Option<ClientId>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    pending_buffer: Option<Option<BufferAttachment>>,
    pending_damage: Rect,
    pending_callbacks: Vec<wl_callback::WlCallback>,
    pixels: Arc<Vec<u32>>,
    width: usize,
    height: usize,
    stride_pixels: usize,
    role_assigned: bool,
    needs_initial_configure: bool,
    configured_serial: u32,
    acknowledged_serial: u32,
    on_output: bool,
    alive: bool,
}

#[derive(Clone)]
struct BufferAttachment {
    resource: wl_buffer::WlBuffer,
    data: BufferData,
}

#[derive(Clone)]
struct BufferData {
    shared: Arc<WaylandBufferState>,
}

struct WaylandBufferState {
    pool: Arc<Mutex<WaylandShmPoolState>>,
    offset: usize,
    width: usize,
    height: usize,
    stride: usize,
    has_alpha: bool,
}

#[derive(Clone)]
struct ShmPoolData {
    shared: Arc<Mutex<WaylandShmPoolState>>,
}

struct WaylandShmPoolState {
    mapping: SharedFdMapping,
    len: usize,
}

#[derive(Default)]
struct RegionData;

#[derive(Default)]
struct OutputData;

#[derive(Default)]
struct SeatData;

#[derive(Default)]
struct PointerData;

#[derive(Default)]
struct KeyboardData;

#[derive(Default)]
struct CallbackData;

impl BufferData {
    fn copy_pixels(&self) -> Option<(Arc<Vec<u32>>, usize, usize, usize)> {
        let pool = self.shared.pool.lock().ok()?;
        let bytes = pool.mapping.bytes();
        let start = self.shared.offset;
        let pixel_count = checked_wayland_pixel_count(self.shared.width, self.shared.height)?;
        let width_bytes = self.shared.width.checked_mul(4)?;
        let required = validate_wayland_buffer_layout(
            start,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool.len.min(bytes.len()),
        )?;
        if required > MAX_WAYLAND_SHM_POOL_BYTES {
            return None;
        }

        let mut pixels = Vec::new();
        pixels.try_reserve_exact(pixel_count).ok()?;
        for row in 0..self.shared.height {
            let row_start = start.checked_add(row.checked_mul(self.shared.stride)?)?;
            let row_end = row_start.checked_add(width_bytes)?;
            let row_bytes = bytes.get(row_start..row_end)?;
            for pixel in row_bytes.chunks_exact(4) {
                let alpha = if self.shared.has_alpha {
                    pixel[3]
                } else {
                    0xff
                };
                pixels.push(u32::from_le_bytes([pixel[0], pixel[1], pixel[2], alpha]));
            }
        }
        Some((
            Arc::new(pixels),
            self.shared.width,
            self.shared.height,
            self.shared.width,
        ))
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let surface = state.create_surface_data();
                let resource = data_init.init(id, surface.clone());
                let shared = surface.shared.clone();
                {
                    let lock = shared.lock();
                    if let Ok(mut shared) = lock {
                        shared.resource = Some(resource.clone());
                        shared.client_id = resource.client().map(|client| client.id());
                    }
                }
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, RegionData);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, RegionData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        _request: wl_region::Request,
        _data: &RegionData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl GlobalDispatch<wl_output::WlOutput, ()> for WaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_output::WlOutput>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let output = data_init.init(resource, OutputData);
        output.geometry(
            0,
            0,
            i32::try_from((state.display_width / 4).max(1)).unwrap_or(1),
            i32::try_from((state.display_height / 4).max(1)).unwrap_or(1),
            wl_output::Subpixel::Unknown,
            String::from("RustOS"),
            String::from("Display-0"),
            wl_output::Transform::Normal,
        );
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            i32::try_from(state.display_width).unwrap_or(1920),
            i32::try_from(state.display_height).unwrap_or(1080),
            60_000,
        );
        output.scale(1);
        if output.version() >= 4 {
            output.name(String::from("RustOS-0"));
            output.description(format!(
                "RustOS Display {}x{}",
                state.display_width, state.display_height
            ));
        }
        if output.version() >= 2 {
            output.done();
        }
        if let Some(client_id) = output.client().map(|client| client.id()) {
            state.output_resources.push(OutputResource {
                resource: output.clone(),
                client_id: client_id.clone(),
            });
            state.announce_bound_output_to_surfaces(client_id, &output);
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_output::WlOutput,
        _request: wl_output::Request,
        _data: &OutputData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state
            .output_resources
            .retain(|output| output.resource.id() != resource.id() || output.resource.is_alive());
    }
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, SeatData);
        seat.capabilities(wl_seat::Capability::Pointer);
        if seat.version() >= 2 {
            seat.name(String::from("seat0"));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, SeatData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &SeatData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                let pointer = data_init.init(id, PointerData);
                if let Some(client_id) = pointer.client().map(|client| client.id()) {
                    _state.pointer_resources.push(PointerResource {
                        resource: pointer,
                        client_id,
                    });
                }
            }
            wl_seat::Request::GetKeyboard { id } => {
                let keyboard = data_init.init(id, KeyboardData);
                let _ = keyboard;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, PointerData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_pointer::WlPointer,
        _request: wl_pointer::Request,
        _data: &PointerData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state.pointer_resources.retain(|pointer| {
            pointer.resource.id() != resource.id() || pointer.resource.is_alive()
        });
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, KeyboardData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_keyboard::WlKeyboard,
        _request: wl_keyboard::Request,
        _data: &KeyboardData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state.keyboard_resources.retain(|keyboard| {
            keyboard.resource.id() != resource.id() || keyboard.resource.is_alive()
        });
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_shm::WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            let Ok(size) = usize::try_from(size.max(0)) else {
                post_protocol_error(resource, "wl_shm.create_pool: invalid pool size".into());
                return;
            };
            if size == 0 {
                post_protocol_error(
                    resource,
                    "wl_shm.create_pool: pool size must be non-zero".into(),
                );
                return;
            }
            if size > MAX_WAYLAND_SHM_POOL_BYTES {
                post_protocol_error(
                    resource,
                    format!(
                        "wl_shm.create_pool: pool size {} exceeds safety limit {}",
                        size, MAX_WAYLAND_SHM_POOL_BYTES
                    ),
                );
                return;
            }
            let Ok(mapping) = map_shared_fd_readable(fd.as_raw_fd(), size) else {
                post_protocol_error(
                    resource,
                    format!("wl_shm.create_pool: failed to map fd for size={size}"),
                );
                return;
            };
            data_init.init(
                id,
                ShmPoolData {
                    shared: Arc::new(Mutex::new(WaylandShmPoolState { mapping, len: size })),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ShmPoolData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &ShmPoolData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let Ok(offset) = usize::try_from(offset.max(0)) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid offset".into(),
                    );
                    return;
                };
                let Ok(width) = usize::try_from(width.max(0)) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid width".into(),
                    );
                    return;
                };
                let Ok(height) = usize::try_from(height.max(0)) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid height".into(),
                    );
                    return;
                };
                let Ok(stride) = usize::try_from(stride.max(0)) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid stride".into(),
                    );
                    return;
                };
                if width == 0 || height == 0 || stride == 0 {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: dimensions and stride must be non-zero".into(),
                    );
                    return;
                }
                let format = match format {
                    WEnum::Value(value) => value,
                    WEnum::Unknown(_) => {
                        post_protocol_error(
                            resource,
                            "wl_shm_pool.create_buffer: unsupported pixel format".into(),
                        );
                        return;
                    }
                };
                let has_alpha = matches!(format, wl_shm::Format::Argb8888);
                if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: only ARGB8888/XRGB8888 are supported".into(),
                    );
                    return;
                }
                let Some(_) = checked_wayland_pixel_count(width, height) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: dimensions exceed compositor safety limits"
                            .into(),
                    );
                    return;
                };
                let Ok(pool) = data.shared.lock() else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: pool state unavailable".into(),
                    );
                    return;
                };
                let Some(_required) = validate_wayland_buffer_layout(
                    offset,
                    width,
                    height,
                    stride,
                    pool.len.min(pool.mapping.len_bytes()),
                ) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid or out-of-bounds layout".into(),
                    );
                    return;
                };
                drop(pool);
                data_init.init(
                    id,
                    BufferData {
                        shared: Arc::new(WaylandBufferState {
                            pool: data.shared.clone(),
                            offset,
                            width,
                            height,
                            stride,
                            has_alpha,
                        }),
                    },
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                let Ok(size) = usize::try_from(size.max(0)) else {
                    post_protocol_error(resource, "wl_shm_pool.resize: invalid size".into());
                    return;
                };
                if let Ok(mut pool) = data.shared.lock() {
                    pool.len = size.min(pool.mapping.len_bytes());
                } else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.resize: pool state unavailable".into(),
                    );
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, CallbackData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_callback::WlCallback,
        _request: wl_callback::Request,
        _data: &CallbackData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                let pending = buffer.and_then(|buffer| {
                    let data = buffer.data::<BufferData>().cloned()?;
                    Some(BufferAttachment {
                        resource: buffer,
                        data,
                    })
                });
                if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_buffer = Some(pending);
                }
            }
            wl_surface::Request::Frame { callback } => {
                let callback = data_init.init(callback, CallbackData);
                if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_callbacks.push(callback);
                }
            }
            wl_surface::Request::Damage {
                x,
                y,
                width,
                height,
            }
            | wl_surface::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if let Some(rect) = surface_damage_rect(x, y, width, height) {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.pending_damage = surface.pending_damage.union(rect);
                    }
                }
            }
            wl_surface::Request::Commit => {
                let mut mapped = false;
                let mut trigger_initial_configure = false;
                if let Ok(mut surface) = data.shared.lock() {
                    let committed_damage = !surface.pending_damage.is_empty();
                    surface.pending_damage = Rect::empty();
                    if let Some(pending) = surface.pending_buffer.take() {
                        match pending {
                            Some(buffer) => {
                                if let Some((pixels, width, height, stride_pixels)) =
                                    buffer.data.copy_pixels()
                                {
                                    surface.pixels = pixels;
                                    surface.width = width;
                                    surface.height = height;
                                    surface.stride_pixels = stride_pixels;
                                    surface.frame.width = width.max(surface.frame.width.min(width));
                                    surface.frame.height =
                                        height.max(surface.frame.height.min(height));
                                    mapped = true;
                                } else {
                                    diag_line(
                                        "uiserver: rejecting wl_shm buffer during commit due to invalid layout",
                                    );
                                }
                                buffer.resource.release();
                            }
                            None => {
                                surface.pixels = Arc::default();
                                surface.width = 0;
                                surface.height = 0;
                                surface.stride_pixels = 0;
                                surface.configured_serial = 0;
                                surface.acknowledged_serial = 0;
                                surface.needs_initial_configure = surface.toplevel.is_some();
                            }
                        }
                        state.mark_dirty();
                    }
                    if committed_damage
                        && !surface.pixels.is_empty()
                        && surface.width != 0
                        && surface.height != 0
                    {
                        state.mark_dirty();
                    }
                    mapped = mapped
                        || (!surface.pixels.is_empty()
                            && surface.width != 0
                            && surface.height != 0);
                    trigger_initial_configure =
                        surface.needs_initial_configure && !mapped && surface.toplevel.is_some();
                }
                state.sync_surface_output(&data.shared, mapped);
                if trigger_initial_configure {
                    state.send_toplevel_configure(&data.shared);
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wl_surface::WlSurface,
        data: &SurfaceData,
    ) {
        diag_line(&format!(
            "uiserver: wl_surface destroyed surface={}",
            resource.id().protocol_id()
        ));
        let _ = state.retire_surface(&data.shared, resource.id().protocol_id());
    }
}

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<xdg_wm_base::XdgWmBase>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
                    post_protocol_error(
                        resource,
                        "xdg_wm_base.get_xdg_surface: target surface has no compositor state"
                            .into(),
                    );
                    return;
                };
                if let Ok(mut wl_surface_state) = surface_data.shared.lock() {
                    if wl_surface_state.role_assigned {
                        post_protocol_error(
                            resource,
                            "xdg_wm_base.get_xdg_surface: surface already has a role".into(),
                        );
                        return;
                    }
                    wl_surface_state.role_assigned = true;
                } else {
                    post_protocol_error(
                        resource,
                        "xdg_wm_base.get_xdg_surface: target surface is unavailable".into(),
                    );
                    return;
                }
                let xdg_surface = data_init.init(id, surface_data.clone());
                if let Ok(mut wl_surface_state) = surface_data.shared.lock() {
                    wl_surface_state.xdg_surface = Some(xdg_surface);
                }
                state.mark_dirty();
            }
            xdg_wm_base::Request::Pong { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let surface = data.clone();
                let toplevel = data_init.init(id, surface);
                if let Ok(mut surface) = data.shared.lock() {
                    surface.toplevel = Some(toplevel);
                    surface.needs_initial_configure = true;
                }
                state.mark_dirty();
            }
            xdg_surface::Request::AckConfigure { serial } => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.acknowledged_serial = serial;
                }
            }
            xdg_surface::Request::Destroy => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.xdg_surface = None;
                    if surface.toplevel.is_none() {
                        surface.role_assigned = false;
                    }
                }
                state.mark_dirty();
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::SetTitle { title } => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.title = title;
                }
                state.mark_dirty();
            }
            xdg_toplevel::Request::SetMaximized
            | xdg_toplevel::Request::UnsetMaximized
            | xdg_toplevel::Request::SetFullscreen { .. }
            | xdg_toplevel::Request::UnsetFullscreen => {
                state.send_toplevel_configure(&data.shared);
            }
            xdg_toplevel::Request::Destroy => {
                let surface_id = data.shared.lock().ok().and_then(|surface| {
                    surface
                        .resource
                        .as_ref()
                        .map(|resource| resource.id().protocol_id())
                });
                diag_line(&format!(
                    "uiserver: xdg_toplevel destroy surface={:?}",
                    surface_id
                ));
                if let Ok(mut surface) = data.shared.lock() {
                    surface.toplevel = None;
                }
                if let Some(surface_id) = surface_id {
                    let _ = state.retire_surface(&data.shared, surface_id);
                } else {
                    state.mark_dirty();
                }
            }
            _ => {}
        }
    }
}

impl GlobalDispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        request: zxdg_decoration_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, toplevel } = request
        {
            let Some(surface_data) = toplevel.data::<SurfaceData>().cloned() else {
                post_protocol_error(
                    resource,
                    "zxdg_decoration_manager_v1.get_toplevel_decoration: toplevel is unavailable"
                        .into(),
                );
                return;
            };
            let decoration = data_init.init(id, surface_data);
            decoration.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, SurfaceData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        _data: &SurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zxdg_toplevel_decoration_v1::Request::SetMode { .. }
            | zxdg_toplevel_decoration_v1::Request::UnsetMode => {
                resource.configure(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            }
            zxdg_toplevel_decoration_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
