use std::arch::asm;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{
    delegate_noop, ConnectError, Connection, Dispatch, DispatchError, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WIDTH: u32 = 800;
const HEIGHT: u32 = 520;
const HUD_HEIGHT: u32 = 56;
const TARGET_GOAL: u32 = 20;
const BTN_LEFT: u32 = 0x110;
const SHM_BUFFER_COUNT: usize = 2;
// Numbers come from the ABI crate so they cannot drift from the kernel's.
const SYS_RUSTOS_DEBUG_PRINT: usize = rustos_user_abi::syscall::SYS_RUSTOS_DEBUG_PRINT as usize;
const SYS_RUSTOS_PRODUCT_MILESTONE: usize =
    rustos_user_abi::syscall::SYS_RUSTOS_PRODUCT_MILESTONE as usize;
const PRODUCT_MILESTONE_FIRST_FRAME: usize =
    rustos_user_abi::syscall::PRODUCT_MILESTONE_FIRST_FRAME as usize;
const FIRST_FRAME_PRESENTED_MARKER: &str = "wayclick: first frame presented";
const DEFAULT_XDG_RUNTIME_DIR: &str = "/run/user/1000";
const DEFAULT_WAYLAND_DISPLAY: &str = "wayland-0";

fn auto_exit_after_first_frame() -> bool {
    match std::env::var("RUSTOS_WAYCLICK_AUTO_EXIT") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "False" | "FALSE"),
        Err(_) => false,
    }
}

/// Opt in to presenting through `zwp_linux_dmabuf_v1` app DMA-BUFs
/// (`docs/ai/abi/app-dmabuf.md`) instead of `wl_shm`.
fn dmabuf_requested() -> bool {
    matches!(
        std::env::var("RUSTOS_WAYCLICK_DMABUF").as_deref(),
        Ok(value) if !matches!(value, "" | "0" | "false" | "False" | "FALSE")
    ) || app_dmabuf_probe_armed()
}

const APP_DMABUF_PROBE_CONTRACT_PATH: &str = "/system/registry/system/appdmabuf-probe-v1.env";
const APP_DMABUF_PROBE_CONTRACT_BODY: &str = "contract=rustos-appdmabuf-probe-v1\n";
const APP_DMABUF_PROBE_FAIL: &str = "app-dmabuf-probe: FAIL";

/// `kvm-smoke --app-dmabuf-probe` publishes this exact private contract file;
/// without it (the production image) the probe is inert. The probe presents
/// through app DMA-BUFs, proves a forged descriptor is refused by the real
/// compositor, and exits once a buffer has been presented and released.
fn app_dmabuf_probe_armed() -> bool {
    std::fs::read_to_string(APP_DMABUF_PROBE_CONTRACT_PATH)
        .is_ok_and(|body| body == APP_DMABUF_PROBE_CONTRACT_BODY)
}

fn profile_enabled() -> bool {
    match std::env::var("RUSTOS_WAYCLICK_PROFILE") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "False" | "FALSE"),
        Err(_) => false,
    }
}

fn raw_stderr_line(message: &str) {
    use std::io::Write;
    let line = format!("{message}\n");
    let written = debug_prefix_with(line.as_bytes(), raw_debug_write);
    if written < line.len() {
        let _ = std::io::stderr().write_all(&line.as_bytes()[written..]);
    }
}

fn debug_prefix_with(bytes: &[u8], mut write: impl FnMut(&[u8]) -> Result<usize, i32>) -> usize {
    let mut written = 0;
    for _ in 0..64 {
        if written == bytes.len() { break; }
        match write(&bytes[written..]) {
            Ok(n) if n > 0 && n <= bytes.len() - written => written += n,
            Err(libc::EAGAIN) => {},
            _ => break,
        }
        if written < bytes.len() { std::thread::yield_now(); }
    }
    written
}

#[cfg(test)]
#[test]
fn diagnostic_short_writes_retry_only_the_remaining_suffix() {
    let mut replies = [Ok(2), Err(libc::EAGAIN), Ok(4)].into_iter();
    let mut lengths = Vec::new();
    assert_eq!(debug_prefix_with(b"hello\n", |bytes| {
        lengths.push(bytes.len());
        replies.next().unwrap()
    }), 6);
    assert_eq!(lengths, [6, 4, 4]);
}

fn raw_debug_write(buffer: &[u8]) -> Result<usize, i32> {
    let result = unsafe {
        syscall2(
            SYS_RUSTOS_DEBUG_PRINT,
            buffer.as_ptr() as usize,
            buffer.len(),
        )
    };
    syscall_usize(result)
}

fn syscall_usize(result: isize) -> Result<usize, i32> {
    if (-4095..0).contains(&result) {
        return Err((-result) as i32);
    }
    usize::try_from(result).map_err(|_| 22)
}

unsafe fn syscall2(number: usize, arg0: usize, arg1: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

unsafe fn syscall3(number: usize, arg0: usize, arg1: usize, arg2: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") arg0 as isize,
            in("rsi") arg1 as isize,
            in("rdx") arg2 as isize,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        raw_stderr_line(&format!("wayclick panic: {info}"));
    }));
}

fn connect_wayland_with_retry() -> Result<Connection, ConnectError> {
    const MAX_CONNECT_ATTEMPTS: usize = 20;
    const CONNECT_RETRY_DELAY_MILLIS: u64 = 100;

    ensure_wayland_env_defaults();
    let mut last_error = None;
    for attempt in 0..MAX_CONNECT_ATTEMPTS {
        match Connection::connect_to_env() {
            Ok(connection) => {
                raw_stderr_line(&format!(
                    "wayclick: connected path={}",
                    configured_wayland_socket_path()
                ));
                return Ok(connection);
            }
            Err(err) => {
                if attempt + 1 == MAX_CONNECT_ATTEMPTS {
                    raw_stderr_line(&format!(
                        "wayclick: wayland connect failed path={}",
                        configured_wayland_socket_path()
                    ));
                    return Err(err);
                }
                last_error = Some(err);
                thread::sleep(Duration::from_millis(CONNECT_RETRY_DELAY_MILLIS));
            }
        }
    }

    Err(last_error.unwrap_or(ConnectError::NoCompositor))
}

fn ensure_wayland_env_defaults() {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        std::env::set_var("XDG_RUNTIME_DIR", DEFAULT_XDG_RUNTIME_DIR);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        std::env::set_var("WAYLAND_DISPLAY", DEFAULT_WAYLAND_DISPLAY);
    }
}

fn configured_wayland_socket_path() -> String {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_XDG_RUNTIME_DIR.to_string());
    let display = std::env::var("WAYLAND_DISPLAY")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_WAYLAND_DISPLAY.to_string());
    if display.starts_with('/') {
        display
    } else {
        format!("{runtime_dir}/{display}")
    }
}

fn main() {
    raw_stderr_line("wayclick: main enter");
    install_panic_hook();
    raw_stderr_line("wayclick: panic hook installed");
    if profile_enabled() {
        raw_stderr_line("wayclick: acceptance profile enabled");
    }
    let conn = match connect_wayland_with_retry() {
        Ok(conn) => conn,
        Err(err) => {
            raw_stderr_line(&format!("wayclick: connect failed: {err:?}"));
            return;
        }
    };
    raw_stderr_line("wayclick: connected");
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    conn.display().get_registry(&qh, ());
    conn.display().sync(&qh, CallbackKind::RegistrySync);
    raw_stderr_line("wayclick: registry requested");
    // The initial registry request has no incoming event to wake a client that
    // enters its first blocking dispatch before the buffered write reaches
    // netd. Publish it explicitly; all later request batches use the matching
    // post-dispatch flush below.
    if let Err(err) = flush_connection(&conn) {
        raw_stderr_line(&format!("wayclick: initial flush failed: {err:?}"));
        return;
    }

    let mut state = GameState::new();
    while state.running {
        match event_queue.blocking_dispatch(&mut state) {
            Ok(_) => {}
            // `blocking_dispatch` flushes first, and a compositor that has not
            // yet drained the socket makes that flush report `WouldBlock`. That
            // is backpressure, not a protocol failure: wait for the socket to
            // become writable and dispatch again.
            Err(DispatchError::Backend(WaylandError::Io(err)))
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                wait_socket_writable(&conn);
                continue;
            }
            Err(err) => {
                raw_stderr_line(&format!("wayclick: dispatch failed: {err:?}"));
                break;
            }
        }
        // Frame callbacks enqueue the next attach/damage/commit batch while
        // dispatching pending events. `blocking_dispatch` returns immediately
        // in that case and would otherwise defer its implicit flush until the
        // next loop iteration. Flush now so the standard Wayland request batch
        // reaches the compositor in the same scheduling turn.
        if let Err(err) = flush_connection(&conn) {
            raw_stderr_line(&format!("wayclick: flush failed: {err:?}"));
            break;
        }
        state
            .profile
            .maybe_emit(state.frame_callback.is_some(), state.redraw_pending);
    }
    if state.probe.armed && !state.probe.finished {
        state.probe.fail("wayclick stopped before every proof step was observed");
    }
    raw_stderr_line("wayclick: main exit");
}

/// Longest a request flush may wait for the compositor to drain the socket.
const FLUSH_WRITABLE_BUDGET: Duration = Duration::from_secs(10);
const FLUSH_WRITABLE_POLL_MS: i32 = 20;

/// Waits (bounded) until the Wayland socket accepts more bytes.
fn wait_socket_writable(conn: &Connection) {
    let mut poll_fd = libc::pollfd {
        fd: conn.as_fd().as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call. An interrupted or
    // timed-out poll only costs another flush attempt.
    unsafe {
        libc::poll(&mut poll_fd, 1, FLUSH_WRITABLE_POLL_MS);
    }
}

/// `wl_display_flush` contract: `EAGAIN` means the compositor has not yet
/// consumed earlier requests, so the client waits for the socket to become
/// writable and retries instead of treating the connection as dead. Treating
/// it as fatal made WayClick exit on a loaded single-CPU boot, where the
/// compositor lagged behind its first request batch.
fn flush_connection(conn: &Connection) -> Result<(), WaylandError> {
    let started = Instant::now();
    loop {
        match conn.flush() {
            Err(WaylandError::Io(err))
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                if started.elapsed() >= FLUSH_WRITABLE_BUDGET {
                    return Err(WaylandError::Io(err));
                }
                wait_socket_writable(conn);
            }
            result => return result,
        }
    }
}

/// Progress of the armed app DMA-BUF probe (`app_dmabuf_probe_armed`).
struct AppDmabufProbe {
    armed: bool,
    forged_refused: bool,
    frame_presented: bool,
    buffer_released: bool,
    finished: bool,
}

impl AppDmabufProbe {
    fn new() -> Self {
        Self {
            armed: app_dmabuf_probe_armed(),
            forged_refused: false,
            frame_presented: false,
            buffer_released: false,
            finished: false,
        }
    }

    fn fail(&mut self, why: &str) {
        if self.armed {
            raw_stderr_line(&format!("{APP_DMABUF_PROBE_FAIL} {why}"));
        }
    }

    /// True once every proof step has been observed (reported exactly once).
    fn complete(&mut self) -> bool {
        if self.armed
            && !self.finished
            && self.forged_refused
            && self.frame_presented
            && self.buffer_released
        {
            self.finished = true;
            raw_stderr_line("wayclick: app-dmabuf probe passed");
            return true;
        }
        false
    }
}

/// Negative proof: a descriptor that is not an app DMA-BUF (a pipe) must be
/// refused by the real compositor with `failed`, never imported.
fn send_forged_dmabuf(
    dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<GameState>,
) -> io::Result<()> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element out array. A socketpair is any
    // non-DeviceBuffer fd the compositor must refuse; this system has no pipe(2).
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created and are owned here.
    let (read_end, write_end) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    drop(write_end);
    let params = dmabuf.create_params(qh, ());
    params.add(
        read_end.as_fd(),
        0,
        0,
        WIDTH * 4,
        (rustos_app_dmabuf::MODIFIER_LINEAR >> 32) as u32,
        rustos_app_dmabuf::MODIFIER_LINEAR as u32,
    );
    params.create(
        WIDTH as i32,
        HEIGHT as i32,
        rustos_app_dmabuf::Format::Argb8888.fourcc(),
        zwp_linux_buffer_params_v1::Flags::empty(),
    );
    raw_stderr_line("wayclick: app-dmabuf probe forged fd sent");
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackKind {
    RegistrySync,
    Frame,
}

struct GameState {
    running: bool,
    dmabuf: bool,
    probe: AppDmabufProbe,
    auto_exit_after_first_frame: bool,
    first_frame_presented: bool,
    registry_synced: bool,
    configured: bool,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    buffers: Vec<ShmBuffer>,
    buffer_available: Vec<bool>,
    frame_callback: Option<wl_callback::WlCallback>,
    redraw_pending: bool,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_x: f64,
    cursor_y: f64,
    pointer_inside: bool,
    score: u32,
    misses: u32,
    streak: u32,
    won: bool,
    rng: u64,
    target_x: i32,
    target_y: i32,
    target_radius: i32,
    pending_damage: DamageRegion,
    profile: FrameProfile,
}

impl GameState {
    fn new() -> Self {
        let mut state = Self {
            running: true,
            dmabuf: dmabuf_requested(),
            probe: AppDmabufProbe::new(),
            auto_exit_after_first_frame: auto_exit_after_first_frame(),
            first_frame_presented: false,
            registry_synced: false,
            configured: false,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            wm_base: None,
            decoration_manager: None,
            decoration: None,
            buffers: Vec::new(),
            buffer_available: Vec::new(),
            frame_callback: None,
            redraw_pending: false,
            pointer: None,
            cursor_x: 0.0,
            cursor_y: 0.0,
            pointer_inside: false,
            score: 0,
            misses: 0,
            streak: 0,
            won: false,
            rng: 0x5eed_cafe_d15c_a11e,
            target_x: 0,
            target_y: 0,
            target_radius: 34,
            pending_damage: DamageRegion::full(),
            profile: FrameProfile::new(),
        };
        state.reseed_target();
        state
    }

    fn init_shell(&mut self, qh: &QueueHandle<Self>) {
        if !self.registry_synced || self.xdg_surface.is_some() {
            return;
        }
        let Some(wm_base) = self.wm_base.as_ref() else {
            return;
        };
        let Some(surface) = self.surface.as_ref() else {
            return;
        };

        let xdg_surface = wm_base.get_xdg_surface(surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        if let Some(decoration_manager) = self.decoration_manager.as_ref() {
            let decoration = decoration_manager.get_toplevel_decoration(&toplevel, qh, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            self.decoration = Some(decoration);
        }
        self.update_title(&toplevel);
        surface.commit();
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
    }

    fn update_title(&self, toplevel: &xdg_toplevel::XdgToplevel) {
        let title = if self.won {
            format!(
                "WayClick Clear - {}/{} hits, {} misses",
                self.score, TARGET_GOAL, self.misses
            )
        } else {
            format!(
                "WayClick - hits {}/{}  misses {}  streak {}",
                self.score, TARGET_GOAL, self.misses, self.streak
            )
        };
        toplevel.set_title(title);
    }

    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        self.profile.redraw_requests = self.profile.redraw_requests.saturating_add(1);
        self.redraw_pending = true;
        self.try_redraw(qh);
    }

    fn try_redraw(&mut self, qh: &QueueHandle<Self>) {
        if !self.redraw_pending || !self.configured || self.frame_callback.is_some() {
            return;
        }

        let Some(surface) = self.surface.clone() else {
            return;
        };
        let Some(buffer_index) = self
            .buffer_available
            .iter()
            .position(|available| *available)
        else {
            return;
        };
        let redraw_started = Instant::now();

        let score = self.score;
        let streak = self.streak;
        let won = self.won;
        let pointer_inside = self.pointer_inside;
        let cursor_x = self.cursor_x;
        let cursor_y = self.cursor_y;
        let target_x = self.target_x;
        let target_y = self.target_y;
        let target_radius = self.target_radius;

        let Some(buffer) = self.buffers.get_mut(buffer_index) else {
            return;
        };
        buffer.clear(0xFF081F38);
        buffer.fill_rect(0, 0, WIDTH as i32, HUD_HEIGHT as i32, 0xFF0E3A64);
        buffer.fill_rect(0, HUD_HEIGHT as i32 - 1, WIDTH as i32, 1, 0xFF5EA8D3);

        let progress_w = ((WIDTH - 32) * score.min(TARGET_GOAL)) / TARGET_GOAL;
        buffer.fill_rect(16, 16, (WIDTH - 32) as i32, 12, 0xFF174D78);
        buffer.fill_rect(16, 16, progress_w as i32, 12, 0xFF75D9FF);
        buffer.fill_rect(16, 36, (WIDTH - 32) as i32, 6, 0xFF103657);
        buffer.fill_rect(
            16,
            36,
            ((WIDTH - 32) * streak.min(10) / 10) as i32,
            6,
            0xFFB8ECFF,
        );

        if won {
            for stripe in 0..12 {
                let y = HUD_HEIGHT as i32 + stripe * 32;
                let color = if stripe % 2 == 0 {
                    0xFF0F3E67
                } else {
                    0xFF0A2C4D
                };
                buffer.fill_rect(0, y, WIDTH as i32, 18, color);
            }
            draw_target(buffer, target_x, target_y, target_radius, true);
        } else {
            draw_target(buffer, target_x, target_y, target_radius, false);
        }

        if pointer_inside {
            let px = cursor_x as i32;
            let py = cursor_y as i32;
            buffer.fill_rect(px - 10, py, 21, 1, 0xFFCFF8FF);
            buffer.fill_rect(px, py - 10, 1, 21, 0xFFCFF8FF);
        }

        let Some(wl_buffer) = buffer.wl_buffer.as_ref().cloned() else {
            return;
        };
        self.buffer_available[buffer_index] = false;
        self.redraw_pending = false;
        self.frame_callback = Some(surface.frame(qh, CallbackKind::Frame));
        surface.attach(Some(&wl_buffer), 0, 0);
        if let Some((x, y, width, height)) = self.pending_damage.take() {
            surface.damage(x, y, width, height);
        }
        surface.commit();
        self.profile.commits = self.profile.commits.saturating_add(1);
        self.profile.record_redraw(redraw_started.elapsed());
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct DamageRegion {
    bounds: Option<(i32, i32, i32, i32)>,
}

impl DamageRegion {
    const fn full() -> Self {
        Self {
            bounds: Some((0, 0, WIDTH as i32, HEIGHT as i32)),
        }
    }

    fn include_cursor(&mut self, x: f64, y: f64) {
        self.include_rect(x as i32 - 10, y as i32 - 10, 21, 21);
    }

    fn include_rect(&mut self, x: i32, y: i32, width: i32, height: i32) {
        let left = x.clamp(0, WIDTH as i32);
        let top = y.clamp(0, HEIGHT as i32);
        let right = x.saturating_add(width).clamp(0, WIDTH as i32);
        let bottom = y.saturating_add(height).clamp(0, HEIGHT as i32);
        if right <= left || bottom <= top {
            return;
        }
        self.bounds = Some(match self.bounds {
            None => (left, top, right - left, bottom - top),
            Some((old_x, old_y, old_width, old_height)) => {
                let union_left = old_x.min(left);
                let union_top = old_y.min(top);
                let union_right = old_x.saturating_add(old_width).max(right);
                let union_bottom = old_y.saturating_add(old_height).max(bottom);
                (
                    union_left,
                    union_top,
                    union_right - union_left,
                    union_bottom - union_top,
                )
            }
        });
    }

    fn mark_full(&mut self) {
        *self = Self::full();
    }

    fn take(&mut self) -> Option<(i32, i32, i32, i32)> {
        self.bounds.take()
    }
}

#[cfg(test)]
mod damage_tests {
    use super::*;

    #[test]
    fn cursor_damage_unions_old_and_new_positions_without_full_surface_copy() {
        let mut damage = DamageRegion::default();
        damage.include_cursor(100.0, 120.0);
        damage.include_cursor(112.0, 128.0);
        assert_eq!(damage.take(), Some((90, 110, 33, 29)));
        assert_eq!(damage.take(), None);
    }

    #[test]
    fn cursor_damage_is_clipped_and_state_changes_force_full_damage() {
        let mut damage = DamageRegion::default();
        damage.include_cursor(2.0, 3.0);
        assert_eq!(damage.take(), Some((0, 0, 13, 14)));
        damage.include_cursor(WIDTH as f64 - 1.0, HEIGHT as f64 - 1.0);
        assert_eq!(damage.take(), Some((789, 509, 11, 11)));
        damage.mark_full();
        assert_eq!(damage.take(), Some((0, 0, WIDTH as i32, HEIGHT as i32)));
    }

    #[test]
    fn first_frame_marker_is_the_user_visible_boot_terminal() {
        assert_eq!(
            FIRST_FRAME_PRESENTED_MARKER,
            "wayclick: first frame presented"
        );
    }
}

struct FrameProfile {
    enabled: bool,
    /// Monotonic index of the window being reported.
    ///
    /// The acceptance proof needs 60 consecutive one-second windows and saw 40
    /// in about 85 seconds of wall time, with a 40 ms worst callback gap across
    /// all of them. Those two facts do not fit together: a client that never
    /// paused longer than 40 ms cannot have lost half the run. Either the
    /// windows were emitted and the lines did not survive the log the proof
    /// parses, or the client really did stop. A sequence number separates them -
    /// contiguous indices with a short run means the client stopped, gaps mean
    /// the transport dropped the evidence.
    window: u64,
    started_at: Instant,
    last_callback_at: Option<Instant>,
    redraw_requests: u64,
    pointer_updates: u64,
    commits: u64,
    callbacks: u64,
    buffer_releases: u64,
    max_callback_gap_ms: u64,
    redraw_micros: u64,
    max_redraw_micros: u64,
}

impl FrameProfile {
    fn new() -> Self {
        Self {
            enabled: profile_enabled(),
            window: 0,
            started_at: Instant::now(),
            last_callback_at: None,
            redraw_requests: 0,
            pointer_updates: 0,
            commits: 0,
            callbacks: 0,
            buffer_releases: 0,
            max_callback_gap_ms: 0,
            redraw_micros: 0,
            max_redraw_micros: 0,
        }
    }

    fn record_callback(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.last_callback_at.replace(now) {
            self.max_callback_gap_ms = self
                .max_callback_gap_ms
                .max(now.duration_since(previous).as_millis() as u64);
        }
        self.callbacks = self.callbacks.saturating_add(1);
    }

    fn record_redraw(&mut self, elapsed: Duration) {
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.redraw_micros = self.redraw_micros.saturating_add(micros);
        self.max_redraw_micros = self.max_redraw_micros.max(micros);
    }

    fn maybe_emit(&mut self, callback_in_flight: bool, redraw_pending: bool) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.started_at);
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let elapsed_micros = u64::try_from(elapsed.as_micros())
            .unwrap_or(u64::MAX)
            .max(1);
        let commit_hz_milli = self
            .commits
            .saturating_mul(1_000_000_000)
            .saturating_div(elapsed_micros);
        let callback_hz_milli = self
            .callbacks
            .saturating_mul(1_000_000_000)
            .saturating_div(elapsed_micros);
        raw_stderr_line(&format!(
            "wayclick profile: window={} elapsed_ms={} commit_hz_milli={} callback_hz_milli={} redraw_requests={} pointer_updates={} commits={} callbacks={} buffer_releases={} max_callback_gap_ms={} redraw_ms={} max_redraw_ms={} callback_in_flight={} redraw_pending={}",
            self.window,
            elapsed_micros / 1_000,
            commit_hz_milli,
            callback_hz_milli,
            self.redraw_requests,
            self.pointer_updates,
            self.commits,
            self.callbacks,
            self.buffer_releases,
            self.max_callback_gap_ms,
            self.redraw_micros / 1_000,
            self.max_redraw_micros / 1_000,
            u8::from(callback_in_flight),
            u8::from(redraw_pending),
        ));
        self.window = self.window.saturating_add(1);
        self.started_at = now;
        self.redraw_requests = 0;
        self.pointer_updates = 0;
        self.commits = 0;
        self.callbacks = 0;
        self.buffer_releases = 0;
        self.max_callback_gap_ms = 0;
        self.redraw_micros = 0;
        self.max_redraw_micros = 0;
    }
}

fn draw_target(
    buffer: &mut ShmBuffer,
    target_x: i32,
    target_y: i32,
    target_radius: i32,
    cleared: bool,
) {
    let outer = if cleared { 44 } else { target_radius };
    let inner = if cleared { 18 } else { target_radius / 2 };
    let glow = if cleared { 0xFFB8ECFF } else { 0xFF69D5FF };
    let core = if cleared { 0xFFF3FBFF } else { 0xFFEAF8FF };
    buffer.fill_circle(target_x, target_y, outer + 10, 0x332E82B4);
    buffer.fill_circle(target_x, target_y, outer, glow);
    buffer.fill_circle(target_x, target_y, inner, core);
    buffer.fill_rect(
        target_x - outer - 12,
        target_y,
        outer * 2 + 25,
        1,
        0xFF08233C,
    );
    buffer.fill_rect(
        target_x,
        target_y - outer - 12,
        1,
        outer * 2 + 25,
        0xFF08233C,
    );
}

impl GameState {
    fn next_random(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 16) as u32
    }

    fn reseed_target(&mut self) {
        let radius = 34_i32.saturating_sub((self.score as i32 / 3) * 2).max(18);
        self.target_radius = radius;
        let min_x = radius + 24;
        let max_x = WIDTH as i32 - radius - 24;
        let min_y = HUD_HEIGHT as i32 + radius + 24;
        let max_y = HEIGHT as i32 - radius - 24;
        let span_x = (max_x - min_x).max(1) as u32;
        let span_y = (max_y - min_y).max(1) as u32;
        self.target_x = min_x + (self.next_random() % span_x) as i32;
        self.target_y = min_y + (self.next_random() % span_y) as i32;
    }

    fn handle_click(&mut self, qh: &QueueHandle<Self>) {
        if self.won || !self.pointer_inside {
            return;
        }
        let dx = self.cursor_x as i32 - self.target_x;
        let dy = self.cursor_y as i32 - self.target_y;
        let hit = dx * dx + dy * dy <= self.target_radius * self.target_radius;
        if hit {
            self.score += 1;
            self.streak += 1;
            if self.score >= TARGET_GOAL {
                self.won = true;
            } else {
                self.reseed_target();
            }
        } else {
            self.misses += 1;
            self.streak = 0;
        }
        if let Some(toplevel) = self.toplevel.as_ref() {
            self.update_title(toplevel);
        }
        self.pending_damage.mark_full();
        self.request_redraw(qh);
    }

    fn update_pointer(&mut self, inside: bool, x: f64, y: f64, qh: &QueueHandle<Self>) {
        if self.pointer_inside {
            self.pending_damage
                .include_cursor(self.cursor_x, self.cursor_y);
        }
        self.pointer_inside = inside;
        self.cursor_x = x;
        self.cursor_y = y;
        if self.pointer_inside {
            self.pending_damage
                .include_cursor(self.cursor_x, self.cursor_y);
        }
        self.request_redraw(qh);
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for GameState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let compositor =
                        registry.bind::<wl_compositor::WlCompositor, _, _>(name, 1, qh, ());
                    state.surface = Some(compositor.create_surface(qh, ()));
                    state.init_shell(qh);
                }
                "wl_shm" if !state.dmabuf => {
                    let shm = registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ());
                    state.buffers.clear();
                    state.buffer_available.clear();
                    for index in 0..SHM_BUFFER_COUNT {
                        match ShmBuffer::new(&shm, WIDTH, HEIGHT, qh, index) {
                            Ok(buffer) => {
                                state.buffers.push(buffer);
                                state.buffer_available.push(true);
                            }
                            Err(error) => {
                                // Out of memory is an environmental failure, not a bug:
                                // report it and exit the event loop instead of panicking.
                                raw_stderr_line(&format!("wayclick: shm buffer failed: {error}"));
                                state.buffers.clear();
                                state.buffer_available.clear();
                                state.running = false;
                                return;
                            }
                        }
                    }
                    if state.configured {
                        state.pending_damage.mark_full();
                        state.request_redraw(qh);
                    }
                }
                "zwp_linux_dmabuf_v1" if state.dmabuf => {
                    // Version 3 is the first with the modifier event; create_immed
                    // needs version 2. Buffers are linear, so version 3 is bound
                    // only to match what the compositor advertises.
                    let dmabuf = registry.bind::<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _, _>(
                        name, 3, qh, (),
                    );
                    state.buffers.clear();
                    state.buffer_available.clear();
                    for index in 0..SHM_BUFFER_COUNT {
                        match ShmBuffer::new_dmabuf(&dmabuf, WIDTH, HEIGHT, qh, index) {
                            Ok(buffer) => {
                                state.buffers.push(buffer);
                                state.buffer_available.push(true);
                            }
                            Err(error) => {
                                // Requested explicitly: do not silently present
                                // through another transport.
                                raw_stderr_line(&format!(
                                    "wayclick: dmabuf buffer failed: {error}"
                                ));
                                state.probe.fail(&format!("allocation or import failed: {error}"));
                                state.buffers.clear();
                                state.buffer_available.clear();
                                state.running = false;
                                return;
                            }
                        }
                    }
                    if state.probe.armed {
                        if let Err(error) = send_forged_dmabuf(&dmabuf, qh) {
                            state.probe.fail(&format!("forged fd setup failed: {error}"));
                            state.running = false;
                            return;
                        }
                    }
                    if state.configured {
                        state.pending_damage.mark_full();
                        state.request_redraw(qh);
                    }
                }
                "wl_seat" => {
                    registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ());
                }
                "xdg_wm_base" => {
                    state.wm_base =
                        Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(name, 1, qh, ()));
                    state.init_shell(qh);
                }
                "zxdg_decoration_manager_v1" => {
                    state.decoration_manager = Some(registry.bind::<
                        zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
                        _,
                        _,
                    >(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(GameState: ignore wl_compositor::WlCompositor);
delegate_noop!(GameState: ignore wl_surface::WlSurface);
delegate_noop!(GameState: ignore wl_shm::WlShm);
delegate_noop!(GameState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(GameState: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(GameState: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
// Format/modifier advertisements are not consulted: only linear ARGB8888 is
// ever allocated, and the compositor refuses anything else at create_immed.
delegate_noop!(GameState: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
// create_immed never produces created/failed (a failed immediate import is a
// protocol error). Only the armed probe's non-immediate forged `create` yields
// `failed`, which is the expected refusal.
impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, ()> for GameState {
    fn event(
        state: &mut Self,
        _: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !state.probe.armed {
            return;
        }
        match event {
            zwp_linux_buffer_params_v1::Event::Failed => {
                state.probe.forged_refused = true;
                raw_stderr_line("wayclick: app-dmabuf probe forged-fd refused");
                if state.probe.complete() {
                    state.running = false;
                }
            }
            zwp_linux_buffer_params_v1::Event::Created { .. } => {
                state.probe.fail("forged fd was imported by the compositor");
                state.running = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for GameState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for GameState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.configured = true;
            state.pending_damage.mark_full();
            state.request_redraw(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for GameState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            raw_stderr_line("wayclick: received close");
            state.running = false;
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for GameState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for GameState {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.profile.pointer_updates = state.profile.pointer_updates.saturating_add(1);
                state.update_pointer(true, surface_x, surface_y, qh);
            }
            wl_pointer::Event::Leave { .. } => {
                state.update_pointer(false, state.cursor_x, state.cursor_y, qh);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.profile.pointer_updates = state.profile.pointer_updates.saturating_add(1);
                state.update_pointer(true, surface_x, surface_y, qh);
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } if button == BTN_LEFT => {
                state.handle_click(qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, CallbackKind> for GameState {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        kind: &CallbackKind,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if *kind == CallbackKind::RegistrySync {
                state.registry_synced = true;
                state.init_shell(qh);
                return;
            }
            let in_flight = state
                .frame_callback
                .as_ref()
                .is_some_and(|current| current.id() == callback.id());
            if in_flight {
                state.profile.record_callback();
                state.frame_callback = None;
                if !state.first_frame_presented {
                    state.first_frame_presented = true;
                    let _ = unsafe {
                        syscall3(
                            SYS_RUSTOS_PRODUCT_MILESTONE,
                            PRODUCT_MILESTONE_FIRST_FRAME,
                            0,
                            0,
                        )
                    };
                    raw_stderr_line(FIRST_FRAME_PRESENTED_MARKER);
                    state.probe.frame_presented = true;
                    if state.probe.armed {
                        // A second commit attaches the other buffer, which is
                        // what makes the compositor release the first.
                        state.pending_damage.mark_full();
                        state.redraw_pending = true;
                    }
                    if state.auto_exit_after_first_frame && !state.probe.armed {
                        raw_stderr_line("wayclick: auto-exit after first frame");
                        state.running = false;
                        return;
                    }
                }
                if state.profile.enabled || state.probe.armed {
                    state.redraw_pending = true;
                }
                state.try_redraw(qh);
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, usize> for GameState {
    fn event(
        state: &mut Self,
        _: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        index: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            state.profile.buffer_releases = state.profile.buffer_releases.saturating_add(1);
            if state.probe.armed && state.probe.frame_presented {
                state.probe.buffer_released = true;
                if state.probe.complete() {
                    state.running = false;
                    return;
                }
            }
            if let Some(available) = state.buffer_available.get_mut(*index) {
                *available = true;
            }
            state.try_redraw(qh);
        }
    }
}

struct ShmBuffer {
    wl_buffer: Option<wl_buffer::WlBuffer>,
    fd: OwnedFd,
    ptr: *mut u32,
    len_pixels: usize,
    width: u32,
    height: u32,
}

impl ShmBuffer {
    fn new(
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
        qh: &QueueHandle<GameState>,
        buffer_index: usize,
    ) -> io::Result<Self> {
        let len_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|len| *len != 0)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let fd = create_memfd("wayclick-buffer")?;
        let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), len_bytes as libc::off_t) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len_bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let pool = shm.create_pool(fd.as_fd(), len_bytes as i32, qh, ());
        let wl_buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            buffer_index,
        );
        Ok(Self {
            wl_buffer: Some(wl_buffer),
            fd,
            ptr: mapped.cast::<u32>(),
            len_pixels: len_bytes / 4,
            width,
            height,
        })
    }

    /// Allocates an app DMA-BUF, maps it and wraps it as a linear ARGB8888
    /// `wl_buffer` through `zwp_linux_buffer_params_v1.create_immed`. The fd is
    /// sent over SCM_RIGHTS by the Wayland backend and kept for the buffer's life.
    fn new_dmabuf(
        dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        width: u32,
        height: u32,
        qh: &QueueHandle<GameState>,
        buffer_index: usize,
    ) -> io::Result<Self> {
        let buf = rustos_app_dmabuf::AppDmabuf::allocate(
            &rustos_app_dmabuf::KernelSource,
            width,
            height,
            rustos_app_dmabuf::Format::Argb8888,
        )?;
        raw_stderr_line(&format!(
            "wayclick: app-dmabuf allocated index={buffer_index} width={width} height={height} stride={} bytes={}",
            buf.stride(),
            buf.len()
        ));
        let params = dmabuf.create_params(qh, ());
        params.add(
            buf.as_fd(),
            0,
            0,
            buf.stride(),
            (rustos_app_dmabuf::MODIFIER_LINEAR >> 32) as u32,
            rustos_app_dmabuf::MODIFIER_LINEAR as u32,
        );
        let wl_buffer = params.create_immed(
            width as i32,
            height as i32,
            buf.format().fourcc(),
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            buffer_index,
        );
        params.destroy();
        let (fd, ptr, len) = buf.into_raw_parts();
        Ok(Self {
            wl_buffer: Some(wl_buffer),
            fd,
            ptr,
            len_pixels: len / 4,
            width,
            height,
        })
    }

    fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len_pixels) }
    }

    fn clear(&mut self, color: u32) {
        self.pixels_mut().fill(color);
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        let start_x = x.max(0).min(self.width as i32);
        let start_y = y.max(0).min(self.height as i32);
        let end_x = x.saturating_add(w).max(0).min(self.width as i32);
        let end_y = y.saturating_add(h).max(0).min(self.height as i32);
        if start_x >= end_x || start_y >= end_y {
            return;
        }
        let width = self.width as usize;
        let pixels = self.pixels_mut();
        for row in start_y as usize..end_y as usize {
            let row_start = row * width;
            for col in start_x as usize..end_x as usize {
                pixels[row_start + col] = color;
            }
        }
    }

    fn fill_circle(&mut self, cx: i32, cy: i32, radius: i32, color: u32) {
        if radius < 0 {
            return;
        }
        let width = self.width as usize;
        let height = self.height as usize;
        fill_circle_in_slice(
            self.pixels_mut(),
            width,
            height,
            cx,
            cy,
            radius,
            color,
        );
    }
}

/// Alpha-blend a straight-ARGB source color over a premultiplied/opaque
/// destination pixel so translucent glows keep the window surface opaque.
fn blend_argb(dst: u32, src: u32) -> u32 {
    let a = (src >> 24) & 0xff;
    if a == 0 {
        return dst;
    }
    if a == 0xff {
        return src;
    }
    let inv = 255 - a;
    let dst_a = (dst >> 24) & 0xff;
    let out_a = (a * 255 + dst_a * inv + 127) / 255;
    let blend_chan = |shift: u32| {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        (s * a + d * inv + 127) / 255
    };
    (out_a << 24) | (blend_chan(16) << 16) | (blend_chan(8) << 8) | blend_chan(0)
}

fn fill_circle_in_slice(
    pixels: &mut [u32],
    width: usize,
    height: usize,
    cx: i32,
    cy: i32,
    radius: i32,
    color: u32,
) {
    if radius < 0 || width == 0 || height == 0 {
        return;
    }
    let a = (color >> 24) & 0xff;
    if a == 0 {
        return;
    }
    let r2 = i64::from(radius) * i64::from(radius);
    let start_y = cy.saturating_sub(radius).max(0);
    let end_y = cy.saturating_add(radius).min(height as i32 - 1);
    for y in start_y..=end_y {
        let dy = i64::from(y - cy);
        let rem = (r2 - dy * dy).max(0) as u64;
        let half = rem.isqrt() as i32;
        let start_x = cx.saturating_sub(half).max(0);
        let end_x = cx.saturating_add(half).min(width as i32 - 1);
        if start_x > end_x {
            continue;
        }
        let row_offset = y as usize * width;
        let span = &mut pixels[row_offset + start_x as usize..=row_offset + end_x as usize];
        if a == 0xff {
            span.fill(color);
        } else {
            for pixel in span {
                *pixel = blend_argb(*pixel, color);
            }
        }
    }
}

#[cfg(test)]
mod circle_blend_tests {
    use super::*;

    #[test]
    fn translucent_target_halo_blends_over_opaque_canvas_without_leaking_alpha() {
        let mut pixels = vec![0xFF08_1F38_u32; 32 * 32];
        fill_circle_in_slice(&mut pixels, 32, 32, 16, 16, 8, 0x332E_82B4);
        let center = pixels[16 * 32 + 16];
        assert_eq!(center >> 24, 0xff);
        assert_ne!(center, 0xFF08_1F38);
        assert_eq!(pixels[0], 0xFF08_1F38);
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                libc::munmap(
                    self.ptr.cast::<libc::c_void>(),
                    self.len_pixels * std::mem::size_of::<u32>(),
                );
            }
        }
        let _ = self.wl_buffer.take();
        let _ = &self.fd;
    }
}

fn create_memfd(name: &str) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// Native ABI revision stamp (hint, not authority); see docs/architecture/evolution-rules.md.
rustos_user_abi::native_abi_note!();
