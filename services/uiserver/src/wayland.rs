//! Wayland client object, buffer, callback, and disconnect state machine.
//!
//! - **Owner:** `uiserver` owns compositor-side Wayland policy and object
//!   lifetime; clients own only their admitted protocol objects and SHM bytes.
//! - **Boundary:** Socket frames, object IDs, opcodes, SHM layouts, damage,
//!   commits, callbacks, and disconnect timing are untrusted.
//! - **Lifecycle:** Accept client, create typed objects, attach/damage/commit,
//!   present, callback/release, destroy, and revoke every object on disconnect.
//! - **Concurrency:** Client fd readiness composes with input/runtime deadlines;
//!   callbacks and releases bind exact surface/buffer generations.
//! - **Failure:** Malformed request, capacity, stale ID, short frame, client
//!   death, provider revoke, and queue pressure isolate the exact client.
//! - **Forbidden:** No client pointer, unbounded object table, callback without
//!   matching release, periodic scan as readiness, or whole-compositor failure.
//! - **Evidence:** `wayland-client-ingress`, `ui-main-loop-wakeup`, and
//!   `gpu-frame-lifecycle`.
use std::ffi::CString;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
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
use crate::input_loop::UiWakeSender;
use crate::layout::{
    self, clamp_wayland_frame, wayland_client_size_for_buffer, wayland_max_client_size,
    WINDOW_BORDER, WINDOW_TITLE_HEIGHT,
};
use crate::sys::{
    diag_line, spawn_ui_thread, ui_profile_enabled, InputEvent, UiThreadRole, INPUT_ACTION_PRESSED,
    INPUT_ACTION_RELEASED, INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD,
};
use crate::wayland_accept::{start_wayland_acceptor, WaylandAcceptor};

mod buffer;
mod commit;
mod compositor;
mod input;
mod output;
mod seat;
mod shm;
mod snapshot;
mod state;
mod surface;
#[cfg(test)]
mod tests;
mod windows;
mod xdg;

use self::buffer::*;
use self::commit::CommitSlot;
use self::snapshot::{SnapshotLayout, SnapshotPixels, SnapshotService, SnapshotSpec};

const WAYLAND_SOCKET_NAME: &str = "wayland-0";
const LINUX_BTN_LEFT: u32 = 0x110;
const MAX_WAYLAND_SHM_POOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_WAYLAND_BUFFER_DIMENSION: usize = 8192;
const MAX_WAYLAND_BUFFER_PIXELS: usize = MAX_WAYLAND_SHM_POOL_BYTES / 4;
const MAX_WAYLAND_CLIENT_ACCEPTS_PER_TICK: usize = 8;
const MAX_WAYLAND_CLIENT_DISPATCHES_PER_TICK: usize = 1;
const MAX_WAYLAND_SURFACES: usize = 64;
/// One client may not consume the whole compositor-wide surface table.
const MAX_WAYLAND_SURFACES_PER_CLIENT: usize = 16;
/// Compositor-wide ceiling on retained surface snapshots (4-byte pixels).
/// Each surface may hold up to `MAX_WAYLAND_BUFFER_PIXELS`; without a shared
/// ceiling 64 surfaces could pin 4 GiB of heap.
const MAX_WAYLAND_TOTAL_SURFACE_PIXELS: usize = 4 * MAX_WAYLAND_BUFFER_PIXELS;
const MAX_WAYLAND_CLIENTS: usize = 64;
/// Live `wl_shm_pool` backing descriptors one client may pin. A pool's fd stays
/// open until its last `wl_buffer` is gone, so the count tracks the fd, not the
/// protocol object.
const MAX_WAYLAND_SHM_POOLS_PER_CLIENT: usize = 32;
/// Compositor-wide ceiling on pinned shm descriptors; the process descriptor
/// table is shared with input, GPU and service IPC.
const MAX_WAYLAND_SHM_POOLS_TOTAL: usize = 256;
const MAX_WAYLAND_BUFFERS_PER_CLIENT: usize = 256;
const MAX_WAYLAND_REGIONS_PER_CLIENT: usize = 64;
const MAX_WAYLAND_DECORATIONS_PER_CLIENT: usize = 16;
const MAX_WAYLAND_OUTSTANDING_CONFIGURES: usize = 16;
const MAX_WAYLAND_TITLE_BYTES: usize = 256;
/// Tracked output, pointer or keyboard objects one client may hold at once.
const MAX_WAYLAND_RESOURCES_PER_CLIENT: usize = 8;
const MAX_WAYLAND_OUTPUT_RESOURCES: usize = 64;
const MAX_WAYLAND_POINTER_RESOURCES: usize = 64;
const MAX_WAYLAND_KEYBOARD_RESOURCES: usize = 64;
const MAX_WAYLAND_FRAME_CALLBACKS_PER_SURFACE: usize = 8;
const MAX_WAYLAND_DISPATCH_LOGS: usize = 16;
/// Per-callback identity lines are bounded like every other diagnostic here.
///
/// They were unbounded, so a client running at 60 Hz put 60 lines per second on
/// the debug transport for `done` alone. Every debugcon byte is a port write
/// that exits to the host, under one global lock held with interrupts disabled,
/// and the acceptance proof reads the same transport - so the frame evidence
/// was crowding out the window records the proof counts. The identity of an
/// individual callback mattered once, for the `Invalid new_id: 15` failure the
/// comment in `send_frame_callbacks` records; a bounded prefix keeps that
/// evidence for the case it was built for.
const MAX_WAYLAND_CALLBACK_ID_LOGS: usize = 32;
const MAX_WAYLAND_SLOW_TICK_LOGS: usize = 8;
const SLOW_WAYLAND_TICK_MS: u128 = 16;
const WAYLAND_POINTER_FRAME_INTERVAL: Duration = Duration::from_millis(15);
const WAYLAND_READINESS_TRANSIENT_FAILURE_LIMIT: usize = 8;
const WAYLAND_READINESS_RETRY_DELAY: Duration = Duration::from_millis(1);

static WAYLAND_DISPATCH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAYLAND_CALLBACK_ID_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAYLAND_SLOW_TICK_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAYLAND_READINESS_WAKE_LOGGED: AtomicBool = AtomicBool::new(false);

fn claim_wayland_rearm(needs_rearm: &AtomicBool) -> bool {
    needs_rearm.swap(false, Ordering::AcqRel)
}

fn transient_wayland_readiness_error(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::BrokenPipe | ErrorKind::TimedOut)
}

fn post_protocol_error<I: Resource>(resource: &I, message: String) {
    diag_line(format!("uiserver: wayland protocol error: {message}"));
    resource.post_error(0_u32, message);
}

fn post_pool_resize_error(resource: &wl_shm_pool::WlShmPool, message: String) {
    diag_line(format!("uiserver: wayland protocol error: {message}"));
    // Match libwayland's WL_SHM_ERROR_INVALID_FD for pool resize.
    resource.post_error(wl_shm::Error::InvalidFd, message);
}

#[derive(Clone, Debug)]
pub(crate) struct WaylandWindowSnapshot {
    pub(crate) surface_id: u32,
    pub(crate) title: String,
    pub(crate) frame: Rect,
    pub(crate) minimized: bool,
    pub(crate) content_version: u64,
    pub(crate) damage: Rect,
    pub(crate) pixels: Arc<Vec<u32>>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride_pixels: usize,
}

impl PartialEq for WaylandWindowSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.surface_id == other.surface_id
            && self.title == other.title
            && self.frame == other.frame
            && self.minimized == other.minimized
            && self.content_version == other.content_version
            && self.width == other.width
            && self.height == other.height
            && self.stride_pixels == other.stride_pixels
    }
}

impl Eq for WaylandWindowSnapshot {}

pub(crate) struct WaylandCompositor {
    display: Display<WaylandState>,
    acceptor: WaylandAcceptor,
    clients: Vec<ClientId>,
    state: WaylandState,
    readiness: Option<WaylandReadiness>,
}

struct WaylandReadiness {
    rearm_sender: SyncSender<()>,
    needs_rearm: Arc<AtomicBool>,
}

impl WaylandCompositor {
    pub(crate) fn initialize(
        display_width: u32,
        display_height: u32,
        ui_wake_sender: UiWakeSender,
    ) -> Option<Self> {
        let runtime_dir = current_runtime_dir();
        let socket_path = format!("{runtime_dir}/{WAYLAND_SOCKET_NAME}");
        let display = match Display::new() {
            Ok(display) => display,
            Err(err) => {
                diag_line(format!("uiserver: wayland display init failed: {err}"));
                return None;
            }
        };
        let listener = match bind_wayland_listener(runtime_dir.as_str(), socket_path.as_str()) {
            Ok(listener) => listener,
            Err(err) => {
                diag_line(format!(
                    "uiserver: wayland socket bind failed path={} err={err}",
                    socket_path
                ));
                return None;
            }
        };
        if let Err(err) = set_fd_nonblocking(listener.as_raw_fd()) {
            diag_line(format!(
                "uiserver: wayland listener nonblocking failed path={} err={err}",
                socket_path
            ));
            return None;
        }
        let snapshot_waker = ui_wake_sender.clone();
        let acceptor = match start_wayland_acceptor(listener, ui_wake_sender) {
            Ok(acceptor) => acceptor,
            Err(err) => {
                diag_line(format!(
                    "uiserver: wayland accept worker start failed path={} err={err}",
                    socket_path
                ));
                return None;
            }
        };
        let state = WaylandState::new(display_width, display_height);
        state.snapshots.set_waker(snapshot_waker);
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
        diag_line(format!(
            "uiserver: wayland compositor ready on {}/{}",
            runtime_dir, WAYLAND_SOCKET_NAME
        ));
        crate::sys::debug_line("uiserver: wayland compositor ready");

        Some(Self {
            display,
            acceptor,
            clients: Vec::new(),
            state,
            readiness: None,
        })
    }

    pub(crate) fn attach_readiness_waker(
        &mut self,
        ui_wake_sender: UiWakeSender,
    ) -> std::io::Result<()> {
        if self.readiness.is_some() {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "Wayland readiness waiter already attached",
            ));
        }
        let poll_fd = self.display.backend().poll_fd().as_raw_fd();
        let duplicated = unsafe { libc::fcntl(poll_fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicated < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let owned_fd = unsafe { OwnedFd::from_raw_fd(duplicated) };
        let (rearm_sender, rearm_receiver) = sync_channel::<()>(1);
        let needs_rearm = Arc::new(AtomicBool::new(false));
        let worker_needs_rearm = Arc::clone(&needs_rearm);
        spawn_ui_thread(UiThreadRole::Protocol, "wayland-readiness", move || {
            let mut event = libc::epoll_event { events: 0, u64: 0 };
            let mut consecutive_transient_failures = 0_usize;
            loop {
                let ready = unsafe { libc::epoll_wait(owned_fd.as_raw_fd(), &mut event, 1, -1) };
                if ready < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    if transient_wayland_readiness_error(err.kind())
                        && consecutive_transient_failures
                            < WAYLAND_READINESS_TRANSIENT_FAILURE_LIMIT
                    {
                        consecutive_transient_failures =
                            consecutive_transient_failures.saturating_add(1);
                        if consecutive_transient_failures == 1 {
                            diag_line(format!(
                                "uiserver: Wayland readiness transient wait failure: {err}"
                            ));
                        }
                        thread::sleep(WAYLAND_READINESS_RETRY_DELAY);
                        continue;
                    }
                    diag_line(format!("uiserver: Wayland readiness wait failed: {err}"));
                    std::process::exit(134);
                }
                consecutive_transient_failures = 0;
                if ready == 0 {
                    continue;
                }
                if !WAYLAND_READINESS_WAKE_LOGGED.swap(true, Ordering::AcqRel) {
                    diag_line("uiserver: Wayland readiness wake observed");
                }
                worker_needs_rearm.store(true, Ordering::Release);
                ui_wake_sender.signal();
                if rearm_receiver.recv().is_err() {
                    break;
                }
            }
        })?;
        self.readiness = Some(WaylandReadiness {
            rearm_sender,
            needs_rearm,
        });
        diag_line("uiserver: Wayland readiness waiter ready");
        Ok(())
    }

    pub(crate) fn rearm_readiness(&self) {
        let Some(readiness) = self.readiness.as_ref() else {
            return;
        };
        if claim_wayland_rearm(&readiness.needs_rearm)
            && readiness.rearm_sender.try_send(()).is_err()
        {
            diag_line("uiserver: Wayland readiness rearm failed");
            std::process::exit(134);
        }
    }

    /// Whether an external edge authorizes a potentially consuming protocol
    /// turn. `wayland-server` documents `poll_fd()` as the dispatch authority;
    /// probing every UI turn would turn an empty nonblocking socket read into
    /// cross-service VFS/NETD traffic and scheduler churn.
    pub(crate) fn has_pending_protocol_input(&self) -> bool {
        self.acceptor.has_pending()
            || self.state.snapshots_need_service()
            || self
                .readiness
                .as_ref()
                .is_some_and(|readiness| readiness.needs_rearm.load(Ordering::Acquire))
    }

    pub(crate) fn tick(&mut self) -> bool {
        let tick_started = Instant::now();
        let accept_started = tick_started;
        crate::note_wayland_step(crate::WAYLAND_STEP_ACCEPT);
        let mut accepted = 0_usize;
        while accepted < MAX_WAYLAND_CLIENT_ACCEPTS_PER_TICK {
            match self.acceptor.try_recv() {
                Ok(stream) => {
                    accepted = accepted.saturating_add(1);
                    if self.clients.len() >= MAX_WAYLAND_CLIENTS {
                        // Each client pins a descriptor and protocol state;
                        // shed the newcomer instead of exhausting the table
                        // that input, GPU and service IPC also draw from.
                        diag_line("uiserver: wayland client limit reached; connection closed");
                        continue;
                    }
                    match self
                        .display
                        .handle()
                        .insert_client(stream, Arc::new(WaylandClientState::default()))
                    {
                        Ok(client) => {
                            self.clients.push(client.id());
                            diag_line("uiserver: wayland client accepted");
                            crate::sys::debug_line("uiserver: wayland client accepted");
                            self.state.dirty = true;
                        }
                        Err(err) => {
                            diag_line(format!("uiserver: wayland insert_client failed: {err}"));
                        }
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let accept_elapsed = accept_started.elapsed();
        // Apply finished client-buffer reads and fail the overdue ones before
        // dispatching new requests, so a completed commit's release and
        // configure precede the next request from the same client.
        self.state.pump_snapshots();

        let dispatch_started = Instant::now();
        crate::note_wayland_step(crate::WAYLAND_STEP_DISPATCH);
        let clients = core::mem::take(&mut self.clients);
        let mut deferred_clients = Vec::new();
        let mut dispatch_clients = Vec::new();
        for client_id in clients {
            if dispatch_clients.len() < MAX_WAYLAND_CLIENT_DISPATCHES_PER_TICK {
                dispatch_clients.push(client_id);
            } else {
                deferred_clients.push(client_id);
            }
        }
        self.clients = deferred_clients;

        let mut dispatched_requests = 0_usize;
        for client_id in dispatch_clients {
            match self
                .display
                .backend()
                .dispatch_single_client(&mut self.state, client_id.clone())
            {
                Ok(dispatched) => {
                    dispatched_requests = dispatched_requests.saturating_add(dispatched);
                    self.clients.push(client_id);
                    if ui_profile_enabled()
                        && dispatched > 0
                        && WAYLAND_DISPATCH_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                            < MAX_WAYLAND_DISPATCH_LOGS
                    {
                        diag_line(format!("uiserver: wayland dispatched count={dispatched}"));
                        crate::sys::debug_line("uiserver: wayland dispatched");
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    self.clients.push(client_id);
                }
                Err(err) => {
                    if WAYLAND_DISPATCH_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                        < MAX_WAYLAND_DISPATCH_LOGS
                    {
                        diag_line(format!("uiserver: wayland client dropped: {err}"));
                    }
                }
            }
        }
        let dispatch_elapsed = dispatch_started.elapsed();
        // Storage custody is settled by the dispatch that just copied it, so
        // this runs every tick rather than on a frame permit.
        let released = self.state.flush_buffer_releases();
        if released != 0 {
            let frame_seq = crate::loop_timing::current_frame_seq();
            if crate::loop_timing::frame_seq_is_sampled(frame_seq) {
                crate::sys::debug_line(&format!(
                    "uiserver: buffer storage released frame_seq={frame_seq} releases={released}"
                ));
            }
        }
        crate::note_wayland_step(crate::WAYLAND_STEP_POINTER_FLUSH);
        self.state.flush_pointer_motion(false);
        let flush_started = Instant::now();
        crate::note_wayland_step(crate::WAYLAND_STEP_FLUSH);
        self.flush_clients();
        crate::note_wayland_step(crate::WAYLAND_STEP_NONE);
        let flush_elapsed = flush_started.elapsed();
        let tick_elapsed = tick_started.elapsed();
        if ui_profile_enabled()
            && tick_elapsed.as_millis() >= SLOW_WAYLAND_TICK_MS
            && WAYLAND_SLOW_TICK_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                < MAX_WAYLAND_SLOW_TICK_LOGS
        {
            diag_line(format!(
                "uiserver: wayland slow tick total_ms={} accept_ms={} accepted={} dispatch_ms={} dispatched={} flush_ms={} clients={}",
                tick_elapsed.as_millis(),
                accept_elapsed.as_millis(),
                accepted,
                dispatch_elapsed.as_millis(),
                dispatched_requests,
                flush_elapsed.as_millis(),
                self.clients.len(),
            ));
        }
        self.state.take_dirty()
    }

    /// Consume one compositor-issued frame permit by sending the pending
    /// one-shot callbacks.
    pub(crate) fn consume_frame_callback_permit(&mut self) {
        crate::note_wayland_step(crate::WAYLAND_STEP_SEND_CALLBACKS);
        self.state.send_frame_callbacks();
        crate::note_wayland_step(crate::WAYLAND_STEP_CALLBACK_FLUSH);
        self.flush_clients();
        crate::note_wayland_step(crate::WAYLAND_STEP_NONE);
    }

    pub(crate) fn flush_clients(&mut self) {
        if let Err(err) = self.display.flush_clients() {
            diag_line(format!("uiserver: wayland flush failed: {err}"));
        }
    }

    pub(crate) fn pending_frame_callback_rect(&self) -> Rect {
        self.state.pending_frame_callback_rect()
    }

    pub(crate) fn window_snapshots(&self) -> Vec<WaylandWindowSnapshot> {
        self.state.window_snapshots()
    }

    /// Tell the compositor how many non-Wayland windows are already placed, so
    /// new Wayland windows continue the shared cascade.
    pub(crate) fn set_window_cascade_base(&mut self, windows: usize) {
        self.state.cascade_base = windows;
    }

    pub(crate) fn clear_window_damage(&mut self) {
        self.state.clear_window_damage();
    }

    /// UI policy resolves console occlusion and chrome before Wayland routing.
    pub(crate) fn set_pointer_target(&mut self, surface_id: Option<u32>) {
        self.state.pointer_target = surface_id;
        if !self.state.pointer_button_down {
            let hit = self
                .state
                .hit_test_pointer(self.state.pointer_x, self.state.pointer_y);
            self.state.update_pointer_focus(hit);
        }
    }

    pub(crate) fn pointer_motion(&mut self, x: u32, y: u32) {
        self.state.pointer_motion(x, y);
    }

    pub(crate) fn pointer_button(&mut self, button: u32, pressed: bool) -> bool {
        self.state.pointer_button(button, pressed)
    }

    pub(crate) fn set_keyboard_target(&mut self, surface_id: Option<u32>) {
        let focus = surface_id.and_then(|id| {
            let shared = self.state.find_surface_by_id(id)?;
            let surface = shared.lock().ok()?;
            if surface.minimized || surface.width == 0 || surface.height == 0 {
                return None;
            }
            Some(KeyboardFocus {
                surface: surface.resource.clone()?,
                client_id: surface.client_id.clone()?,
            })
        });
        self.state.set_keyboard_focus(focus);
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

    /// Toggle the surface's maximized state. The first call snapshots the
    /// current frame and resizes it to fill the desktop region; the second
    /// call restores the snapshot.
    pub(crate) fn toggle_surface_maximized(&mut self, surface_id: u32) -> bool {
        self.state.toggle_surface_maximized(surface_id)
    }

    pub(crate) fn close_surface(&mut self, surface_id: u32) -> bool {
        self.state.close_surface(surface_id)
    }

    pub(crate) fn clear_focus(&mut self) {
        self.state.clear_focus();
    }
}

/// Decode one validated Wayland shm row. RustOS' supported architecture is
/// little-endian, so ARGB8888 bytes already have the native `u32` layout.
/// XRGB8888 needs only the alpha byte forced opaque; process four pixels per
/// iteration without an allocation or per-channel reconstruction.
fn copy_wayland_bgra_row(src: &[u8], dst: &mut [u32], has_alpha: bool) -> bool {
    if src.len() != dst.len().saturating_mul(4) {
        return false;
    }
    if has_alpha {
        for (target, bytes) in dst.iter_mut().zip(src.chunks_exact(4)) {
            *target = u32::from_le_bytes(bytes.try_into().expect("four-byte pixel"));
        }
        return true;
    }

    const OPAQUE_4: u128 = 0xff00_0000_ff00_0000_ff00_0000_ff00_0000;
    let mut chunks = src.chunks_exact(16);
    let mut out = dst.chunks_exact_mut(4);
    for (source, target) in chunks.by_ref().zip(out.by_ref()) {
        let packed = u128::from_le_bytes(source.try_into().expect("four pixels")) | OPAQUE_4;
        for (pixel, value) in target.iter_mut().zip(packed.to_le_bytes().chunks_exact(4)) {
            *pixel = u32::from_le_bytes(value.try_into().expect("four-byte pixel"));
        }
    }
    let remainder = chunks.remainder();
    let output_remainder = out.into_remainder();
    for (target, bytes) in output_remainder.iter_mut().zip(remainder.chunks_exact(4)) {
        *target = u32::from_le_bytes(bytes.try_into().expect("four-byte pixel")) | 0xff00_0000;
    }
    true
}

fn set_fd_nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn current_runtime_dir() -> String {
    const FALLBACK_RUNTIME_DIR: &str = "/run/user/1000";
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| safe_runtime_dir(value.as_str()))
        .unwrap_or_else(|| String::from(FALLBACK_RUNTIME_DIR))
}

fn bind_wayland_listener(runtime_dir: &str, socket_path: &str) -> std::io::Result<UnixListener> {
    fs::create_dir_all(runtime_dir)?;
    match bind_nonblocking_unix_listener(socket_path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            if stale_wayland_socket(runtime_dir, socket_path)? {
                fs::remove_file(socket_path)?;
                bind_nonblocking_unix_listener(socket_path)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn bind_nonblocking_unix_listener(socket_path: &str) -> std::io::Result<UnixListener> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        let path = CString::new(socket_path)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let path_bytes = path.as_bytes_with_nul();
        let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        if path_bytes.len() > addr.sun_path.len() {
            return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        for (index, byte) in path_bytes.iter().enumerate() {
            addr.sun_path[index] = *byte as libc::c_char;
        }

        let bind_rc = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if bind_rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::listen(fd, 16) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();

    if let Err(err) = result {
        let _ = unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
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
        && value.len() < 108 - WAYLAND_SOCKET_NAME.len()
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

fn wayland_nonnegative_i32(value: i32) -> Option<usize> {
    if value < 0 {
        return None;
    }
    usize::try_from(value).ok()
}

/// Whether a surface may retain `incoming_pixels` given what every other
/// surface already holds.
fn surface_pixel_budget_allows(other_surfaces_pixels: usize, incoming_pixels: usize) -> bool {
    other_surfaces_pixels
        .checked_add(incoming_pixels)
        .is_some_and(|total| total <= MAX_WAYLAND_TOTAL_SURFACE_PIXELS)
}

fn valid_wayland_pool_resize(current_size: usize, new_size: usize) -> bool {
    new_size >= current_size && new_size <= MAX_WAYLAND_SHM_POOL_BYTES
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

impl WaylandState {
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
        // Preserve Wayland input ordering: the latest coalesced coordinates
        // must be visible to the client before the button transition.
        self.flush_pointer_motion(true);

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
}

/// Per-client live-object counters. The protocol backend caps neither object
/// count nor pinned descriptors, so a client that never destroys what it
/// creates would otherwise grow compositor memory and the shared fd table
/// without bound.
#[derive(Debug, Default)]
struct WaylandClientState {
    shm_pools: Arc<AtomicUsize>,
    buffers: Arc<AtomicUsize>,
    regions: Arc<AtomicUsize>,
    decorations: Arc<AtomicUsize>,
}

impl ClientData for WaylandClientState {}

/// Process-wide count of pinned shm pool descriptors (see
/// `MAX_WAYLAND_SHM_POOLS_TOTAL`).
static LIVE_SHM_POOLS: AtomicUsize = AtomicUsize::new(0);

/// RAII admission ticket for one counted client object. The counters are
/// released when the object's user data (or the state it is embedded in) is
/// dropped, so destroy, client disconnect and error paths release exactly once.
struct LiveObjectTicket {
    per_client: Arc<AtomicUsize>,
    global: Option<&'static AtomicUsize>,
}

impl LiveObjectTicket {
    /// Admit one object against `per_client_limit` and, when given, a
    /// process-wide `global` counter. Returns `None` when a limit is reached.
    fn admit(
        per_client: &Arc<AtomicUsize>,
        per_client_limit: usize,
        global: Option<(&'static AtomicUsize, usize)>,
    ) -> Option<Self> {
        // ORDERING: Relaxed - the counters are independent admission counts;
        // they publish no other memory and every update is a single RMW.
        if per_client.fetch_add(1, Ordering::Relaxed) >= per_client_limit {
            per_client.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        if let Some((total, limit)) = global {
            if total.fetch_add(1, Ordering::Relaxed) >= limit {
                total.fetch_sub(1, Ordering::Relaxed);
                per_client.fetch_sub(1, Ordering::Relaxed);
                return None;
            }
        }
        Some(Self {
            per_client: Arc::clone(per_client),
            global: global.map(|(total, _)| total),
        })
    }
}

impl Drop for LiveObjectTicket {
    fn drop(&mut self) {
        // ORDERING: Relaxed, paired with the admission increments above.
        self.per_client.fetch_sub(1, Ordering::Relaxed);
        if let Some(total) = self.global {
            total.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

struct WaylandState {
    display_width: u32,
    display_height: u32,
    next_serial: u32,
    /// Next compositor-unique surface identity. Protocol object ids are
    /// per-client, so two clients routinely own the same `wl_surface` id.
    next_surface_id: u32,
    /// Windows that belong to other owners (console sessions) and occupy the
    /// first cascade slots.
    cascade_base: usize,
    started_at: Instant,
    surfaces: Vec<SurfaceData>,
    output_resources: Vec<OutputResource>,
    pointer_resources: Vec<PointerResource>,
    keyboard_resources: Vec<KeyboardResource>,
    pointer_focus: Option<PointerFocus>,
    pointer_target: Option<u32>,
    keyboard_focus: Option<KeyboardFocus>,
    pointer_x: u32,
    pointer_y: u32,
    pointer_button_down: bool,
    pointer_motion_pending: bool,
    next_pointer_motion_flush: Instant,
    dirty: bool,
    callback_profile_started: Instant,
    callback_profile_count: u64,
    callback_profile_wait_micros: u64,
    callback_profile_max_wait_micros: u64,
    buffer_copy_profile_count: u64,
    buffer_copy_profile_bytes: u64,
    buffer_copy_profile_micros: u64,
    buffer_copy_profile_max_micros: u64,
    /// Off-thread, budgeted client-buffer reads (`wayland/snapshot.rs`).
    snapshots: SnapshotService,
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
    /// Compositor-unique identity; the key every external caller (focus,
    /// move, close) uses. Never a protocol object id, which is per-client.
    surface_id: u32,
    title: String,
    frame: Rect,
    minimized: bool,
    /// `Some(frame)` while the surface is currently maximized; storing the
    /// pre-maximize frame lets the next maximize click restore it.
    pre_maximize_frame: Option<Rect>,
    resource: Option<wl_surface::WlSurface>,
    client_id: Option<ClientId>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    pending_buffer: Option<Option<BufferAttachment>>,
    current_buffer: Option<BufferData>,
    pending_damage: Rect,
    last_damage: Rect,
    pending_callbacks: Vec<PendingFrameCallback>,
    pending_buffer_releases: Vec<wl_buffer::WlBuffer>,
    /// The commit whose pixels are being read off-thread, and the one queued
    /// behind it (`wayland/commit.rs`).
    commit: CommitSlot,
    pixels: Arc<Vec<u32>>,
    width: usize,
    height: usize,
    stride_pixels: usize,
    content_version: u64,
    role_assigned: bool,
    needs_initial_configure: bool,
    configured_serial: u32,
    acknowledged_serial: u32,
    /// Recently sent, not yet superseded configure serials. A client may
    /// legitimately acknowledge any of them (it can ack an earlier configure
    /// after a later one is already on the wire); only unknown serials are a
    /// protocol error.
    outstanding_configures: Vec<u32>,
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
    /// Released when the last reference to this buffer goes away.
    _ticket: LiveObjectTicket,
}

#[derive(Clone)]
struct ShmPoolData {
    shared: Arc<Mutex<WaylandShmPoolState>>,
}

struct WaylandShmPoolState {
    len: usize,
    /// Shared by buffers after pool destruction; never mapped into uiserver.
    /// Reference-counted so a snapshot job that is mid-`pread` keeps the
    /// descriptor open: a closed-and-reused fd number must never be read.
    fd: Arc<OwnedFd>,
    /// Held while the descriptor is open, which outlives the pool object while
    /// any buffer still references it.
    _ticket: LiveObjectTicket,
}

struct RegionData {
    _ticket: LiveObjectTicket,
}

struct DecorationData {
    _ticket: LiveObjectTicket,
}

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

struct PendingFrameCallback {
    callback: wl_callback::WlCallback,
    requested_at: Instant,
}
