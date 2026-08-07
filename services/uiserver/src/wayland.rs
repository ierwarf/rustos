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
    diag_line, map_shared_fd_readable, spawn_ui_thread, ui_profile_enabled, InputEvent,
    SharedFdMapping, UiThreadRole, INPUT_ACTION_PRESSED, INPUT_ACTION_RELEASED,
    INPUT_ACTION_REPEATED, INPUT_KIND_KEYBOARD,
};
use crate::wayland_accept::{start_wayland_acceptor, WaylandAcceptor};

const WAYLAND_SOCKET_NAME: &str = "wayland-0";
const WINDOW_CASCADE_X: usize = 42;
const WINDOW_CASCADE_Y: usize = 30;
const WINDOW_CASCADE_SLOTS: usize = 6;
const LINUX_BTN_LEFT: u32 = 0x110;
const MAX_WAYLAND_SHM_POOL_BYTES: usize = 64 * 1024 * 1024;
const MAX_WAYLAND_BUFFER_DIMENSION: usize = 8192;
const MAX_WAYLAND_BUFFER_PIXELS: usize = MAX_WAYLAND_SHM_POOL_BYTES / 4;
const MAX_WAYLAND_CLIENT_ACCEPTS_PER_TICK: usize = 8;
const MAX_WAYLAND_CLIENT_DISPATCHES_PER_TICK: usize = 1;
const MAX_WAYLAND_SURFACES: usize = 64;
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
                    match self
                        .display
                        .handle()
                        .insert_client(stream, Arc::new(WaylandClientState))
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

    pub(crate) fn clear_window_damage(&mut self) {
        self.state.clear_window_damage();
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
        can_retain_undamaged_buffer, checked_wayland_pixel_count, claim_wayland_rearm,
        copy_wayland_bgra_row, transient_wayland_readiness_error, validate_wayland_buffer_layout,
        wayland_nonnegative_i32, WaylandWindowSnapshot, MAX_WAYLAND_BUFFER_DIMENSION,
        MAX_WAYLAND_SHM_POOL_BYTES,
    };
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn wayland_readiness_requires_one_dispatch_before_rearm() {
        let needs_rearm = AtomicBool::new(true);
        assert!(claim_wayland_rearm(&needs_rearm));
        assert!(!claim_wayland_rearm(&needs_rearm));
        needs_rearm.store(true, Ordering::Release);
        assert!(claim_wayland_rearm(&needs_rearm));
    }

    #[test]
    fn wayland_readiness_retries_only_transient_transport_failures() {
        assert!(transient_wayland_readiness_error(ErrorKind::BrokenPipe));
        assert!(transient_wayland_readiness_error(ErrorKind::TimedOut));
        assert!(!transient_wayland_readiness_error(ErrorKind::InvalidInput));
    }

    use crate::canvas::Rect;
    use std::sync::Arc;

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

    #[test]
    fn wayland_integer_args_reject_negative_values() {
        assert_eq!(wayland_nonnegative_i32(0), Some(0));
        assert_eq!(wayland_nonnegative_i32(42), Some(42));
        assert_eq!(wayland_nonnegative_i32(-1), None);
    }

    #[test]
    fn wayland_argb_row_preserves_alpha() {
        let source = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut pixels = [0_u32; 2];
        assert!(copy_wayland_bgra_row(&source, &mut pixels, true));
        assert_eq!(pixels, [0x0403_0201, 0x0807_0605]);
    }

    #[test]
    fn wayland_xrgb_row_forces_opaque_alpha_across_chunk_boundary() {
        let mut source = [0_u8; 20];
        for (index, byte) in source.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let mut pixels = [0_u32; 5];
        assert!(copy_wayland_bgra_row(&source, &mut pixels, false));
        assert_eq!(
            pixels,
            [
                0xff02_0100,
                0xff06_0504,
                0xff0a_0908,
                0xff0e_0d0c,
                0xff12_1110
            ]
        );
    }

    #[test]
    fn undamaged_buffer_reuse_requires_an_exact_existing_layout() {
        assert!(can_retain_undamaged_buffer(
            true, 800, 520, 800, 800, 520, 800
        ));
        assert!(!can_retain_undamaged_buffer(
            false, 800, 520, 800, 800, 520, 800
        ));
        assert!(!can_retain_undamaged_buffer(
            true, 800, 520, 800, 801, 520, 801
        ));
    }

    #[test]
    fn wayland_snapshot_equality_uses_content_version_not_pixels() {
        let mut first = WaylandWindowSnapshot {
            surface_id: 1,
            title: String::from("app"),
            frame: Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            minimized: false,
            content_version: 7,
            damage: Rect::empty(),
            pixels: Arc::new(vec![1, 2, 3]),
            width: 3,
            height: 1,
            stride_pixels: 3,
        };
        let mut second = first.clone();
        second.pixels = Arc::new(vec![9, 9, 9]);
        assert_eq!(first, second);
        first.content_version = 8;
        assert_ne!(first, second);
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

    fn next_configure_serial(&mut self) -> u32 {
        let serial = self.next_serial.max(1);
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        serial
    }

    fn allocate_window_frame(&mut self) -> Rect {
        let (max_w, max_h) = wayland_max_client_size(self.display_width, self.display_height);
        // The hint we send to clients is "use 11/20 by 3/5 of the screen",
        // but a misbehaving client may keep its own size — so the hint here
        // also doubles as the upper bound we render at.
        let suggested_w = ((self.display_width as usize).saturating_mul(11) / 20).min(max_w);
        let suggested_h = ((self.display_height as usize).saturating_mul(3) / 5).min(max_h);

        let index = self.next_surface_index;
        self.next_surface_index = self.next_surface_index.saturating_add(1);
        let step = index % WINDOW_CASCADE_SLOTS;

        let desktop = layout::desktop_bounds(self.display_width, self.display_height);
        let x = desktop.x + step * WINDOW_CASCADE_X;
        let y = desktop.y + step * WINDOW_CASCADE_Y;

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

    fn create_surface_data(&mut self) -> Option<SurfaceData> {
        self.prune_surfaces();
        if self.surfaces.len() >= MAX_WAYLAND_SURFACES {
            return None;
        }
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
        Some(data)
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

    fn clear_window_damage(&mut self) {
        for surface in &self.surfaces {
            if let Ok(mut state) = surface.shared.lock() {
                state.last_damage = Rect::empty();
            }
        }
    }

    fn event_time_ms(&self) -> u32 {
        let millis = self.started_at.elapsed().as_millis();
        u32::try_from(millis.min(u128::from(u32::MAX))).unwrap_or(u32::MAX)
    }

    fn record_buffer_copy(&mut self, elapsed: Duration, bytes: usize) {
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

    fn send_frame_callbacks(&mut self) {
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

    fn prune_surfaces(&mut self) {
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

    fn prune_surface_callbacks(surface: &mut WaylandSurfaceState) {
        surface
            .pending_callbacks
            .retain(|pending| pending.callback.is_alive());
    }

    fn surface_callback_count(surface: &mut WaylandSurfaceState) -> usize {
        Self::prune_surface_callbacks(surface);
        surface.pending_callbacks.len()
    }

    fn output_resource_count(&mut self) -> usize {
        self.prune_output_resources();
        self.output_resources.len()
    }

    fn pointer_resource_count(&mut self) -> usize {
        self.prune_pointer_resources();
        self.pointer_resources.len()
    }

    fn keyboard_resource_count(&mut self) -> usize {
        self.prune_keyboard_resources();
        self.keyboard_resources.len()
    }

    fn can_track_output_resource(&mut self) -> bool {
        self.output_resource_count() < MAX_WAYLAND_OUTPUT_RESOURCES
    }

    fn can_track_pointer_resource(&mut self) -> bool {
        self.pointer_resource_count() < MAX_WAYLAND_POINTER_RESOURCES
    }

    fn can_track_keyboard_resource(&mut self) -> bool {
        self.keyboard_resource_count() < MAX_WAYLAND_KEYBOARD_RESOURCES
    }

    fn pending_frame_callback_rect(&self) -> Rect {
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
            state.acknowledged_serial = 0;
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

    fn flush_pointer_motion(&mut self, force: bool) {
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
        diag_line(format!(
            "uiserver: retire_surface begin surface={surface_id}"
        ));
        self.clear_focus_for_surface(surface_id);
        self.pointer_button_down = false;
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
        state.width = 0;
        state.height = 0;
        state.stride_pixels = 0;
        state.current_buffer = None;
        state.content_version = next_content_version(state.content_version);
        state.pending_buffer = None;
        let pending_releases = state.pending_buffer_releases.drain(..).collect::<Vec<_>>();
        state.pending_callbacks.clear();
        state.on_output = false;
        state.needs_initial_configure = false;
        drop(state);
        for buffer in pending_releases {
            buffer.release();
        }
        self.mark_dirty();
        diag_line(format!("uiserver: retire_surface end surface={surface_id}"));
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

    fn toggle_surface_maximized(&mut self, surface_id: u32) -> bool {
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
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

    fn close_surface(&mut self, surface_id: u32) -> bool {
        diag_line(format!(
            "uiserver: close_surface begin surface={surface_id}"
        ));
        let Some(shared) = self.find_surface_by_protocol_id(surface_id) else {
            diag_line(format!(
                "uiserver: close_surface missing surface={surface_id}"
            ));
            return false;
        };
        let toplevel = shared.lock().ok().and_then(|state| state.toplevel.clone());
        let retired = self.retire_surface(&shared, surface_id);
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
    pixels: Arc<Vec<u32>>,
    width: usize,
    height: usize,
    stride_pixels: usize,
    content_version: u64,
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

struct PendingFrameCallback {
    callback: wl_callback::WlCallback,
    requested_at: Instant,
}

impl BufferData {
    fn copy_damage_into(
        &self,
        dst: &mut Arc<Vec<u32>>,
        dst_width: usize,
        dst_height: usize,
        dst_stride_pixels: usize,
        damage: Rect,
    ) -> bool {
        if dst_width != self.shared.width
            || dst_height != self.shared.height
            || dst_stride_pixels != self.shared.width
            || dst.is_empty()
        {
            return false;
        }
        let damage = damage.intersect(Rect {
            x: 0,
            y: 0,
            width: self.shared.width,
            height: self.shared.height,
        });
        if damage.is_empty() {
            return true;
        }
        let Ok(pool) = self.shared.pool.lock() else {
            return false;
        };
        let bytes = pool.mapping.bytes();
        let start = self.shared.offset;
        if validate_wayland_buffer_layout(
            start,
            self.shared.width,
            self.shared.height,
            self.shared.stride,
            pool.len.min(bytes.len()),
        )
        .is_none()
        {
            return false;
        }
        let Some(width_bytes) = damage.width.checked_mul(4) else {
            return false;
        };
        let pixels = Arc::make_mut(dst);
        for row in 0..damage.height {
            let Some(src_row) = start
                .checked_add((damage.y + row).saturating_mul(self.shared.stride))
                .and_then(|row_start| row_start.checked_add(damage.x.saturating_mul(4)))
            else {
                return false;
            };
            let Some(src_end) = src_row.checked_add(width_bytes) else {
                return false;
            };
            let Some(row_bytes) = bytes.get(src_row..src_end) else {
                return false;
            };
            let Some(dst_row) = (damage.y + row)
                .checked_mul(dst_stride_pixels)
                .and_then(|row_start| row_start.checked_add(damage.x))
            else {
                return false;
            };
            let Some(dst_end) = dst_row.checked_add(damage.width) else {
                return false;
            };
            let Some(row_pixels) = pixels.get_mut(dst_row..dst_end) else {
                return false;
            };
            if !copy_wayland_bgra_row(row_bytes, row_pixels, self.shared.has_alpha) {
                return false;
            }
        }
        true
    }

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
        pixels.resize(pixel_count, 0);
        for row in 0..self.shared.height {
            let row_start = start.checked_add(row.checked_mul(self.shared.stride)?)?;
            let row_end = row_start.checked_add(width_bytes)?;
            let row_bytes = bytes.get(row_start..row_end)?;
            let dst_start = row.checked_mul(self.shared.width)?;
            let dst_end = dst_start.checked_add(self.shared.width)?;
            if !copy_wayland_bgra_row(
                row_bytes,
                pixels.get_mut(dst_start..dst_end)?,
                self.shared.has_alpha,
            ) {
                return None;
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

fn can_retain_undamaged_buffer(
    has_snapshot: bool,
    current_width: usize,
    current_height: usize,
    current_stride_pixels: usize,
    next_width: usize,
    next_height: usize,
    next_stride_pixels: usize,
) -> bool {
    has_snapshot
        && current_width == next_width
        && current_height == next_height
        && current_stride_pixels == next_stride_pixels
}

fn next_content_version(current: u64) -> u64 {
    current.wrapping_add(1).max(1)
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
        resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let Some(surface) = state.create_surface_data() else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_compositor.create_surface: surface limit {} reached",
                            MAX_WAYLAND_SURFACES
                        ),
                    );
                    return;
                };
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
        let can_track = state.can_track_output_resource();
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
            if !can_track {
                diag_line(format!(
                    "uiserver: wayland output resource tracking limit {} reached",
                    MAX_WAYLAND_OUTPUT_RESOURCES
                ));
                return;
            }
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
        state: &mut Self,
        _client: &Client,
        resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &SeatData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                if !state.can_track_pointer_resource() {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_seat.get_pointer: pointer resource limit {} reached",
                            MAX_WAYLAND_POINTER_RESOURCES
                        ),
                    );
                    return;
                }
                let pointer = data_init.init(id, PointerData);
                if let Some(client_id) = pointer.client().map(|client| client.id()) {
                    state.pointer_resources.push(PointerResource {
                        resource: pointer,
                        client_id,
                    });
                }
            }
            wl_seat::Request::GetKeyboard { id } => {
                if !state.can_track_keyboard_resource() {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_seat.get_keyboard: keyboard resource limit {} reached",
                            MAX_WAYLAND_KEYBOARD_RESOURCES
                        ),
                    );
                    return;
                }
                let keyboard = data_init.init(id, KeyboardData);
                if let Some(client_id) = keyboard.client().map(|client| client.id()) {
                    state.keyboard_resources.push(KeyboardResource {
                        resource: keyboard,
                        client_id,
                    });
                }
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
            let Some(size) = wayland_nonnegative_i32(size) else {
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
                let Some(offset) = wayland_nonnegative_i32(offset) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid offset".into(),
                    );
                    return;
                };
                let Some(width) = wayland_nonnegative_i32(width) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid width".into(),
                    );
                    return;
                };
                let Some(height) = wayland_nonnegative_i32(height) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid height".into(),
                    );
                    return;
                };
                let Some(stride) = wayland_nonnegative_i32(stride) else {
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
                let layout_valid = {
                    let Ok(pool) = data.shared.lock() else {
                        post_protocol_error(
                            resource,
                            "wl_shm_pool.create_buffer: pool state unavailable".into(),
                        );
                        return;
                    };
                    validate_wayland_buffer_layout(
                        offset,
                        width,
                        height,
                        stride,
                        pool.len.min(pool.mapping.len_bytes()),
                    )
                    .is_some()
                };
                if !layout_valid {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid or out-of-bounds layout".into(),
                    );
                    return;
                };
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
                let Some(size) = wayland_nonnegative_i32(size) else {
                    post_protocol_error(resource, "wl_shm_pool.resize: invalid size".into());
                    return;
                };
                if size == 0 {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.resize: pool size must be non-zero".into(),
                    );
                    return;
                }
                let resize_result = {
                    if let Ok(mut pool) = data.shared.lock() {
                        if size > MAX_WAYLAND_SHM_POOL_BYTES || size > pool.mapping.len_bytes() {
                            Err(format!(
                                "wl_shm_pool.resize: size {} exceeds mapped safety limit {}",
                                size,
                                pool.mapping.len_bytes().min(MAX_WAYLAND_SHM_POOL_BYTES)
                            ))
                        } else {
                            pool.len = size;
                            Ok(())
                        }
                    } else {
                        Err("wl_shm_pool.resize: pool state unavailable".into())
                    }
                };
                if let Err(message) = resize_result {
                    post_protocol_error(resource, message);
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
        resource: &wl_surface::WlSurface,
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
                // The `new_id` is consumed unconditionally, before any decision.
                //
                // The client has already allocated this id and will not reuse
                // it until the server destroys the object. Returning early
                // without calling `data_init.init` leaves the id in a state the
                // backend and the client disagree about, and the next
                // `wl_surface.frame` fails with `wl_display` error
                // `Invalid new_id`. WayClick hit exactly that after presenting
                // its first frame, and it is fatal to the client rather than to
                // the request that caused it.
                let callback = data_init.init(callback, CallbackData);
                // Both ends of the callback object's life are logged, because
                // the `Invalid new_id` failure that motivated them was an
                // id-lifetime disagreement that no other record named.
                //
                // Bounded, like its `done` counterpart. Unbounded it emitted one
                // line per frame - 277 per second at the rate this compositor
                // now reaches - and every debugcon byte is a port write that
                // exits to the host under one global lock. The acceptance proof
                // counts one line per second from the client on that same
                // transport, and this record was crowding it out: a run with
                // WayClick alive and committing throughout still stopped
                // producing window records after the ninth.
                if WAYLAND_CALLBACK_ID_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                    < MAX_WAYLAND_CALLBACK_ID_LOGS
                {
                    crate::sys::debug_line(&format!(
                        "uiserver: wayland frame callback created id={:?}",
                        callback.id()
                    ));
                }

                let Ok(mut surface) = data.shared.lock() else {
                    post_protocol_error(resource, "wl_surface.frame: surface unavailable".into());
                    return;
                };
                if WaylandState::surface_callback_count(&mut surface)
                    >= MAX_WAYLAND_FRAME_CALLBACKS_PER_SURFACE
                {
                    // Answer the callback instead of dropping it. A client that
                    // is ahead of the compositor is throttling itself correctly;
                    // failing its connection turns a backlog into an exit.
                    drop(surface);
                    callback.done(state.event_time_ms());
                    return;
                }
                surface.pending_callbacks.push(PendingFrameCallback {
                    callback,
                    requested_at: Instant::now(),
                });
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
                let (pending_buffer, committed_damage_rect) = {
                    let Ok(mut surface) = data.shared.lock() else {
                        return;
                    };
                    let committed_damage = surface.pending_damage;
                    surface.pending_damage = Rect::empty();
                    (surface.pending_buffer.take(), committed_damage)
                };
                let has_damage = !committed_damage_rect.is_empty();

                let commits_buffer = matches!(pending_buffer, Some(Some(_))) || has_damage;
                if commits_buffer {
                    let commit_ready = {
                        match data.shared.lock() {
                            Ok(surface)
                                if surface.toplevel.is_some()
                                    && surface.current_buffer.is_none()
                                    && (surface.configured_serial == 0
                                        || surface.acknowledged_serial
                                            != surface.configured_serial) =>
                            {
                                Err(
                                    "wl_surface.commit: xdg surface buffer committed before configure ack"
                                        .into(),
                                )
                            }
                            Ok(_) => Ok(()),
                            Err(_) => Err("wl_surface.commit: surface unavailable".into()),
                        }
                    };
                    if let Err(message) = commit_ready {
                        post_protocol_error(resource, message);
                        return;
                    }
                }

                let mut mapped = false;
                let mut trigger_initial_configure = false;
                let mut dirty = false;
                let mut copy_source = None::<BufferData>;
                let mut release_buffer = None::<wl_buffer::WlBuffer>;

                if let Some(pending) = pending_buffer {
                    match pending {
                        Some(buffer) => {
                            let BufferAttachment { resource, data } = buffer;
                            copy_source = Some(data);
                            release_buffer = Some(resource);
                        }
                        None => {
                            if let Ok(mut surface) = data.shared.lock() {
                                surface.current_buffer = None;
                                surface.pixels = Arc::default();
                                surface.width = 0;
                                surface.height = 0;
                                surface.stride_pixels = 0;
                                surface.last_damage = Rect::empty();
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                surface.configured_serial = 0;
                                surface.acknowledged_serial = 0;
                                surface.needs_initial_configure = surface.toplevel.is_some();
                                trigger_initial_configure =
                                    surface.needs_initial_configure && surface.toplevel.is_some();
                                dirty = true;
                            }
                        }
                    }
                } else if has_damage {
                    copy_source = data
                        .shared
                        .lock()
                        .ok()
                        .and_then(|surface| surface.current_buffer.clone());
                } else if let Ok(surface) = data.shared.lock() {
                    mapped =
                        !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0;
                    trigger_initial_configure =
                        surface.needs_initial_configure && !mapped && surface.toplevel.is_some();
                }

                if let Some(copy_source) = copy_source {
                    let display_width = state.display_width;
                    let display_height = state.display_height;
                    // Attaching a same-layout buffer with no declared damage
                    // changes buffer ownership, not visible pixels. Preserve
                    // the compositor snapshot and avoid a full SHM read/copy.
                    // The first buffer or a layout change still requires a
                    // complete copy so no undefined pixels become visible.
                    let retained_undamaged = if !has_damage {
                        if let Ok(mut surface) = data.shared.lock() {
                            let can_retain = can_retain_undamaged_buffer(
                                !surface.pixels.is_empty(),
                                surface.width,
                                surface.height,
                                surface.stride_pixels,
                                copy_source.shared.width,
                                copy_source.shared.height,
                                copy_source.shared.width,
                            );
                            if can_retain {
                                surface.current_buffer = Some(copy_source.clone());
                                surface.last_damage = Rect::empty();
                                mapped = true;
                            }
                            can_retain
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if has_damage {
                        let mut completed_copy = None;
                        if let Ok(mut surface) = data.shared.lock() {
                            let width = surface.width;
                            let height = surface.height;
                            let stride_pixels = surface.stride_pixels;
                            let copy_started = Instant::now();
                            let copied = copy_source.copy_damage_into(
                                &mut surface.pixels,
                                width,
                                height,
                                stride_pixels,
                                committed_damage_rect,
                            );
                            let copy_elapsed = copy_started.elapsed();
                            if copied {
                                let copied_bytes = committed_damage_rect
                                    .width
                                    .saturating_mul(committed_damage_rect.height)
                                    .saturating_mul(std::mem::size_of::<u32>());
                                completed_copy = Some((copy_elapsed, copied_bytes));
                                surface.current_buffer = Some(copy_source.clone());
                                surface.last_damage = committed_damage_rect;
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                mapped = true;
                                dirty = true;
                            }
                            trigger_initial_configure = surface.needs_initial_configure
                                && !mapped
                                && surface.toplevel.is_some();
                        }
                        if let Some((elapsed, bytes)) = completed_copy {
                            state.record_buffer_copy(elapsed, bytes);
                        }
                    }
                    if !dirty && !retained_undamaged {
                        let copy_started = Instant::now();
                        let copied = copy_source.copy_pixels();
                        let copy_elapsed = copy_started.elapsed();
                        if let Ok(mut surface) = data.shared.lock() {
                            if let Some((pixels, width, height, stride_pixels)) = copied {
                                state.record_buffer_copy(
                                    copy_elapsed,
                                    pixels.len().saturating_mul(std::mem::size_of::<u32>()),
                                );
                                surface.current_buffer = Some(copy_source);
                                surface.pixels = pixels;
                                surface.width = width;
                                surface.height = height;
                                surface.stride_pixels = stride_pixels;
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                // Clamp the rendered chrome to the desktop region.
                                // The client buffer may be bigger than this (e.g.
                                // wayclick hardcodes 800×520 and ignores our
                                // configure); in that case the buffer is cropped
                                // to what the chrome can show.
                                let (visible_w, visible_h) = wayland_client_size_for_buffer(
                                    width,
                                    height,
                                    display_width,
                                    display_height,
                                );
                                let clamped = clamp_wayland_frame(
                                    Rect {
                                        x: surface.frame.x,
                                        y: surface.frame.y,
                                        width: visible_w,
                                        height: visible_h,
                                    },
                                    display_width,
                                    display_height,
                                );
                                surface.frame = clamped;
                                surface.last_damage = if has_damage {
                                    committed_damage_rect
                                } else {
                                    Rect {
                                        x: 0,
                                        y: 0,
                                        width,
                                        height,
                                    }
                                };
                                mapped = true;
                                dirty = true;
                            } else {
                                diag_line(
                                    "uiserver: rejecting wl_shm buffer during commit due to invalid layout",
                                );
                                mapped = !surface.pixels.is_empty()
                                    && surface.width != 0
                                    && surface.height != 0;
                            }
                            trigger_initial_configure = surface.needs_initial_configure
                                && !mapped
                                && surface.toplevel.is_some();
                        }
                    }
                }

                if let Some(buffer) = release_buffer {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.pending_buffer_releases.push(buffer);
                    } else {
                        buffer.release();
                    }
                }

                if has_damage && !dirty {
                    if let Ok(surface) = data.shared.lock() {
                        if !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0 {
                            mapped = true;
                        }
                    }
                }

                if !dirty {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.last_damage = Rect::empty();
                    }
                }
                if dirty {
                    state.mark_dirty();
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
        diag_line(format!(
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
                let role_result = {
                    if let Ok(mut wl_surface_state) = surface_data.shared.lock() {
                        if wl_surface_state.role_assigned {
                            Err("xdg_wm_base.get_xdg_surface: surface already has a role".into())
                        } else {
                            wl_surface_state.role_assigned = true;
                            Ok(())
                        }
                    } else {
                        Err("xdg_wm_base.get_xdg_surface: target surface is unavailable".into())
                    }
                };
                if let Err(message) = role_result {
                    post_protocol_error(resource, message);
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
        resource: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel_result = {
                    if let Ok(surface) = data.shared.lock() {
                        if surface.toplevel.is_some() {
                            Err("xdg_surface.get_toplevel: toplevel already exists".into())
                        } else {
                            Ok(())
                        }
                    } else {
                        Err("xdg_surface.get_toplevel: surface unavailable".into())
                    }
                };
                if let Err(message) = toplevel_result {
                    post_protocol_error(resource, message);
                    return;
                }
                let surface = data.clone();
                let toplevel = data_init.init(id, surface);
                if let Ok(mut surface) = data.shared.lock() {
                    surface.toplevel = Some(toplevel);
                    surface.needs_initial_configure = true;
                }
                state.send_toplevel_configure(&data.shared);
                state.mark_dirty();
            }
            xdg_surface::Request::AckConfigure { serial } => {
                let ack_result = {
                    if let Ok(mut surface) = data.shared.lock() {
                        if surface.configured_serial == 0 || serial != surface.configured_serial {
                            Err("xdg_surface.ack_configure: unknown configure serial".into())
                        } else {
                            surface.acknowledged_serial = serial;
                            Ok(())
                        }
                    } else {
                        Err("xdg_surface.ack_configure: surface unavailable".into())
                    }
                };
                if let Err(message) = ack_result {
                    post_protocol_error(resource, message);
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
                diag_line(format!(
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
