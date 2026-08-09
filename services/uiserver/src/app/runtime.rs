use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use std::vec::Vec;

use super::{
    AppState, ConsoleWindow, DragTarget, VisualUpdate, CONSOLE_POLL_SLEEP, MAX_RUNNING_PROGRAMS,
};
use crate::canvas;
use crate::input_loop::UiWakeSender;
use crate::render::{self, default_console_window_rect, taskbar_slot_rect};
use crate::runtime_sync::{runtime_program_is_hidden, runtime_program_title, RuntimeState};
use crate::sys::{
    console_get_state, console_send_input_event, console_set_focus,
    console_snapshot_session_output, console_snapshot_sessions, open_console, spawn_ui_thread,
    ConsoleSessionHandle, ConsoleSessionInfo, ConsoleStateInfo, InputEvent, UiThreadRole,
    MAX_CONSOLE_SNAPSHOT_BYTES,
};
use crate::wayland::WaylandCompositor;
use runtime_control::RuntimeRunningProgram;

const CONSOLE_COMMAND_QUEUE_CAPACITY: usize = 512;
const SLOW_CONSOLE_INPUT_COMMAND_THRESHOLD: Duration = Duration::from_millis(50);

/// Publishes the one readiness edge the console transport cannot deliver.
///
/// Console output is observable only by polling `output_generation`, so the
/// echo of a keystroke can never arrive sooner than the poll that looks for
/// it. On a fixed cadence that costs every typed character up to a full
/// interval here, and then up to another one in the main loop, before a single
/// pixel changes - a delay the user reads as a slow shell rather than as a
/// slow poll. The keystroke, however, is known exactly: the dispatcher has
/// just handed it to the session. Publishing that edge lets the idle wait end
/// on the keystroke and keeps the interval as the fallback it was meant to be.
///
/// The generation is compared under the lock, so an edge published between two
/// waits is observed by the next one instead of being lost.
struct ConsoleEchoSignal {
    generation: Mutex<u64>,
    published: Condvar,
}

impl ConsoleEchoSignal {
    fn new() -> Self {
        Self {
            generation: Mutex::new(0),
            published: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        // A panicking peer must not silence the echo path; the counter is a
        // plain generation and carries no invariant a panic could break.
        self.generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self) {
        *self.lock() += 1;
        self.published.notify_one();
    }

    fn wait_for_edge(&self, observed: &mut u64, timeout: Duration) {
        let guard = self.lock();
        if *guard != *observed {
            *observed = *guard;
            return;
        }
        let (guard, _) = self
            .published
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *observed = *guard;
    }
}

pub(crate) struct ConsoleRefresh {
    state: ConsoleStateInfo,
    sessions: Vec<ConsoleSessionInfo>,
    outputs: Vec<ConsoleSessionOutput>,
}

/// Console input and focus updates cross the `uiserver` -> `devmgrd` ->
/// `runtimed` policy boundary. That IPC is allowed to wait for a recovering
/// service, but the UI input/present loop is not. A bounded worker owns the
/// wait so congestion is explicit telemetry rather than a frozen frame.
pub(crate) struct ConsoleCommandDispatcher {
    sender: SyncSender<ConsoleCommand>,
    stats: Arc<ConsoleCommandStats>,
}

enum ConsoleCommand {
    Input {
        session_handle: ConsoleSessionHandle,
        event: InputEvent,
    },
    SetFocus {
        session_handle: ConsoleSessionHandle,
    },
}

struct ConsoleCommandStats {
    started: Instant,
    accepted: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
    slow_commands: AtomicU64,
    active: AtomicBool,
    active_started_ms: AtomicU64,
}

pub(crate) struct ConsoleCommandSnapshot {
    pub(crate) pending: u64,
    pub(crate) accepted: u64,
    pub(crate) completed: u64,
    pub(crate) rejected: u64,
    pub(crate) errors: u64,
    pub(crate) slow_commands: u64,
    pub(crate) active: bool,
    pub(crate) active_elapsed_ms: u64,
}

impl ConsoleCommandStats {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            accepted: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            slow_commands: AtomicU64::new(0),
            active: AtomicBool::new(false),
            active_started_ms: AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn snapshot(&self) -> ConsoleCommandSnapshot {
        let accepted = self.accepted.load(Ordering::Relaxed);
        let completed = self.completed.load(Ordering::Relaxed);
        let active = self.active.load(Ordering::Acquire);
        let active_elapsed_ms = if active {
            self.elapsed_ms()
                .saturating_sub(self.active_started_ms.load(Ordering::Acquire))
        } else {
            0
        };
        ConsoleCommandSnapshot {
            pending: accepted.saturating_sub(completed),
            accepted,
            completed,
            rejected: self.rejected.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            slow_commands: self.slow_commands.load(Ordering::Relaxed),
            active,
            active_elapsed_ms,
        }
    }
}

impl ConsoleCommandDispatcher {
    pub(crate) fn submit_input(&self, session_handle: ConsoleSessionHandle, event: InputEvent) {
        if session_handle == 0 {
            return;
        }
        self.submit(ConsoleCommand::Input {
            session_handle,
            event,
        });
    }

    pub(crate) fn submit_focus(&self, session_handle: ConsoleSessionHandle) {
        if session_handle == 0 {
            return;
        }
        self.submit(ConsoleCommand::SetFocus { session_handle });
    }

    fn submit(&self, command: ConsoleCommand) {
        match self.sender.try_send(command) {
            Ok(()) => {
                self.stats.accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.stats.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(crate) fn snapshot(&self) -> ConsoleCommandSnapshot {
        self.stats.snapshot()
    }
}

pub(crate) fn start_console_command_dispatcher(console_fd: OwnedFd) -> ConsoleCommandDispatcher {
    let (sender, receiver) = mpsc::sync_channel::<ConsoleCommand>(CONSOLE_COMMAND_QUEUE_CAPACITY);
    let stats = Arc::new(ConsoleCommandStats::new());
    let worker_stats = Arc::clone(&stats);
    let echo = Arc::clone(console_echo_signal());
    spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-console-command",
        move || {
            while let Ok(command) = receiver.recv() {
                let delivers_input = matches!(command, ConsoleCommand::Input { .. });
                worker_stats
                    .active_started_ms
                    .store(worker_stats.elapsed_ms(), Ordering::Release);
                worker_stats.active.store(true, Ordering::Release);
                let started = Instant::now();
                let result = match command {
                    ConsoleCommand::Input {
                        session_handle,
                        event,
                    } => console_send_input_event(console_fd.as_raw_fd(), session_handle, event),
                    ConsoleCommand::SetFocus { session_handle } => {
                        console_set_focus(console_fd.as_raw_fd(), session_handle)
                    }
                };
                let elapsed = started.elapsed();
                if elapsed >= SLOW_CONSOLE_INPUT_COMMAND_THRESHOLD {
                    worker_stats.slow_commands.fetch_add(1, Ordering::Relaxed);
                }
                if result.is_err() {
                    worker_stats.errors.fetch_add(1, Ordering::Relaxed);
                }
                worker_stats.completed.fetch_add(1, Ordering::Relaxed);
                worker_stats.active.store(false, Ordering::Release);
                // The session now holds the keystroke, so its echo is the next
                // thing the refresh poll can observe. Publish after the
                // delivery, never before: an edge published ahead of it would
                // send the poll looking for output that has not been produced.
                if delivers_input {
                    echo.publish();
                }
            }
        },
    )
    .unwrap_or_else(|_| std::process::exit(134));
    ConsoleCommandDispatcher { sender, stats }
}

/// Applies every console refresh the worker has published, reporting whether
/// any arrived. The caller uses that to tell an edge-driven drain - a shell
/// that has just echoed a keystroke - from an ordinary interval tick.
pub(crate) fn drain_console_refreshes(
    state: &mut AppState,
    refreshes: &Receiver<ConsoleRefresh>,
    update: &mut VisualUpdate,
) -> Result<bool, i32> {
    let mut drained = false;
    while let Ok(refresh) = refreshes.try_recv() {
        drained = true;
        update.absorb(state.apply_console_refresh(refresh)?);
    }
    Ok(drained)
}

/// One process-wide edge shared by the dispatcher and the refresh worker. They
/// are started independently and neither owns the other, so the signal outlives
/// both rather than being threaded through the startup order.
fn console_echo_signal() -> &'static Arc<ConsoleEchoSignal> {
    static SIGNAL: std::sync::OnceLock<Arc<ConsoleEchoSignal>> = std::sync::OnceLock::new();
    SIGNAL.get_or_init(|| Arc::new(ConsoleEchoSignal::new()))
}

struct ConsoleSessionOutput {
    session_handle: ConsoleSessionHandle,
    output_generation: u64,
    bytes: Vec<u8>,
}

pub(crate) fn start_console_refresh_worker(wake: UiWakeSender) -> Receiver<ConsoleRefresh> {
    let (sender, receiver) = mpsc::sync_channel(2);
    let echo = Arc::clone(console_echo_signal());
    spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-console-refresh",
        move || {
            let Ok(console_fd) = open_console() else {
                return;
            };
            let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
            let mut output_generations = BTreeMap::<ConsoleSessionHandle, u64>::new();
            let mut observed_echo = 0_u64;
            loop {
                let mut produced_output = false;
                if let Ok(refresh) = collect_console_refresh(
                    console_fd.as_raw_fd(),
                    &mut snapshot,
                    &mut output_generations,
                ) {
                    produced_output = !refresh.outputs.is_empty();
                    if sender.try_send(refresh).is_err() {
                        output_generations.clear();
                    } else if produced_output {
                        // Only new session output can change a pixel. Waking
                        // the main loop for an unchanged snapshot would trade
                        // this thread's fixed cadence for a busy render loop.
                        wake.signal();
                    }
                }
                if produced_output {
                    // A shell answers a keystroke with more than one write.
                    // Look again before idling so the rest of the line is not
                    // held back by an interval it did not need to wait for.
                    continue;
                }
                echo.wait_for_edge(&mut observed_echo, CONSOLE_POLL_SLEEP);
            }
        },
    )
    .unwrap_or_else(|_| std::process::exit(134));
    receiver
}

fn collect_console_refresh(
    console_fd: i32,
    snapshot: &mut [u8; MAX_CONSOLE_SNAPSHOT_BYTES],
    output_generations: &mut BTreeMap<ConsoleSessionHandle, u64>,
) -> Result<ConsoleRefresh, i32> {
    let state = console_get_state(console_fd)?;
    let mut sessions = [ConsoleSessionInfo::default(); crate::sys::CONSOLE_SESSION_CAPACITY];
    let session_count = console_snapshot_sessions(console_fd, &mut sessions)?
        .min(crate::sys::CONSOLE_SESSION_CAPACITY);
    let sessions = sessions[..session_count].to_vec();
    output_generations.retain(|session_handle, _| {
        sessions
            .iter()
            .any(|session| session.session_handle == *session_handle)
    });
    let mut outputs = Vec::new();
    for session in &sessions {
        let previous_generation = output_generations
            .get(&session.session_handle)
            .copied()
            .unwrap_or(0);
        if previous_generation == session.output_generation {
            continue;
        }
        match console_snapshot_session_output(console_fd, session.session_handle, snapshot) {
            Ok(count) => outputs.push(ConsoleSessionOutput {
                session_handle: session.session_handle,
                output_generation: session.output_generation,
                bytes: snapshot[..count].to_vec(),
            }),
            Err(crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE) => {}
            Err(err) => return Err(err),
        }
        output_generations.insert(session.session_handle, session.output_generation);
    }

    Ok(ConsoleRefresh {
        state,
        sessions,
        outputs,
    })
}

impl AppState {
    pub(crate) fn apply_runtime_state(&mut self, runtime_state: &mut RuntimeState) -> VisualUpdate {
        if !runtime_state.dirty {
            return VisualUpdate::default();
        }

        let mut update = self.sync_windows_from_runtime(
            &runtime_state.running_programs[..runtime_state
                .running_program_count
                .min(MAX_RUNNING_PROGRAMS)],
        );
        runtime_state.dirty = false;
        update.add_partial_rect(self.focused_window_reorder_dirty_rect());
        update
    }

    pub(crate) fn apply_console_refresh(
        &mut self,
        refresh: ConsoleRefresh,
    ) -> Result<VisualUpdate, i32> {
        let sessions = refresh.sessions.as_slice();
        let before_dirty = self
            .console_stack_dirty_rect()
            .union(self.taskbar_dirty_rect());
        let mut update = VisualUpdate::default();
        let pruned = self.prune_windows(|session_handle| {
            sessions
                .iter()
                .any(|session| session.session_handle == session_handle)
        });
        let added_update =
            self.add_missing_console_windows(sessions, refresh.state.focused_session_handle);
        if pruned {
            update.add_partial_rect(before_dirty);
            update.add_partial_rect(self.console_stack_dirty_rect());
            update.add_partial_rect(self.taskbar_dirty_rect());
        } else if !added_update.is_empty() {
            update.absorb(added_update);
            update.add_partial_rect(self.taskbar_dirty_rect());
        }
        self.clamp_console_snapshot_index();
        update
            .add_partial_rect(self.reconcile_console_focus(refresh.state.focused_session_handle)?);

        let taskbar_dirty_rect = self.taskbar_dirty_rect();
        for window in self.console_windows.iter_mut() {
            let Some(session) = sessions
                .iter()
                .find(|session| session.session_handle == window.session_handle)
            else {
                continue;
            };
            let session_title = console_session_title(session);
            if !session_title.is_empty() && window.title != session_title {
                window.title = session_title;
                window.invalidate_surface();
                // Defer rebuild to the render pass; draw_console_window will
                // call rebuild_console_window_surface unconditionally before
                // blitting the cached pixels. Priming here would just duplicate
                // that work and stretch the runtime refresh (the only time we
                // hold the main loop) into a visible stall.
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
                update.add_partial_rect(taskbar_dirty_rect);
            }
            if session.output_generation == window.output_generation {
                continue;
            }
            let output = refresh
                .outputs
                .iter()
                .find(|output| output.session_handle == window.session_handle);
            let Some(output) = output else {
                continue;
            };
            if window.output_cache.as_slice() != output.bytes.as_slice() {
                window.output_cache.clear();
                window
                    .output_cache
                    .extend_from_slice(output.bytes.as_slice());
                window.terminal_dirty = true;
                window.invalidate_surface();
                // Defer rebuild to the render pass; draw_console_window will
                // call rebuild_console_window_surface unconditionally before
                // blitting the cached pixels. Priming here would just duplicate
                // that work and stretch the runtime refresh (the only time we
                // hold the main loop) into a visible stall.
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
            }
            window.output_generation = output.output_generation;
        }

        Ok(update)
    }

    fn add_missing_console_windows(
        &mut self,
        sessions: &[ConsoleSessionInfo],
        kernel_focused_session: ConsoleSessionHandle,
    ) -> VisualUpdate {
        let mut known_sessions = self
            .console_windows
            .iter()
            .map(|window| window.session_handle)
            .collect::<Vec<_>>();
        let mut update = VisualUpdate::default();

        for session in sessions {
            let session_handle = session.session_handle;
            if session_handle == 0
                || self.is_console_session_closing(session_handle)
                || known_sessions.contains(&session_handle)
            {
                continue;
            }

            let frame = default_console_window_rect(
                self.display.width,
                self.display.height,
                self.console_windows.len(),
            );
            self.console_windows.push(ConsoleWindow::new(
                session_handle,
                console_session_title(session),
                frame,
                0,
            ));
            known_sessions.push(session_handle);
            if session.focused != 0 || session_handle == kernel_focused_session {
                self.focused_wayland_surface_id = None;
                self.focused_session_handle = session_handle;
                self.pending_console_focus = Some(session_handle);
            }
            update.add_partial_rect(crate::render::console_window_dirty_rect(frame));
        }

        update
    }

    fn sync_windows_from_runtime(&mut self, programs: &[RuntimeRunningProgram]) -> VisualUpdate {
        let before_dirty = self
            .console_stack_dirty_rect()
            .union(self.taskbar_dirty_rect());
        let taskbar_dirty_rect = self.taskbar_dirty_rect();
        self.closing_console_sessions.retain(|session_handle| {
            programs
                .iter()
                .any(|program| program.session_handle == *session_handle)
        });
        let existing = std::mem::take(&mut self.console_windows);
        let mut next = Vec::with_capacity(programs.len());
        let mut update = VisualUpdate::default();
        let mut topology_changed = false;
        let mut kept_sessions = Vec::with_capacity(programs.len());

        for mut window in existing {
            if self.is_console_session_closing(window.session_handle) {
                topology_changed = true;
                continue;
            }
            let Some(program) = programs
                .iter()
                .find(|program| program.session_handle == window.session_handle)
            else {
                topology_changed = true;
                continue;
            };
            if runtime_program_is_hidden(program) || program.session_handle == 0 {
                topology_changed = true;
                continue;
            }

            let new_title = runtime_program_title(program);
            if window.title != new_title {
                window.title = new_title;
                window.invalidate_surface();
                update.add_partial_rect(crate::render::console_window_dirty_rect(window.frame));
                update.add_partial_rect(taskbar_dirty_rect);
            }
            kept_sessions.push(window.session_handle);
            next.push(window);
        }

        for program in programs {
            if runtime_program_is_hidden(program)
                || program.session_handle == 0
                || self.is_console_session_closing(program.session_handle)
                || kept_sessions.contains(&program.session_handle)
            {
                continue;
            }

            let frame =
                default_console_window_rect(self.display.width, self.display.height, next.len());
            next.push(ConsoleWindow::new(
                program.session_handle,
                runtime_program_title(program),
                frame,
                0,
            ));
            update.add_partial_rect(crate::render::console_window_dirty_rect(frame));
            update.add_partial_rect(taskbar_dirty_rect);
        }

        if let Some(DragTarget::Console(session_handle)) = self.dragging_window {
            if next
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window = None;
                topology_changed = true;
            }
        }

        if next
            .iter()
            .all(|window| window.session_handle != self.focused_session_handle)
            && self.focused_session_handle != 0
        {
            self.focused_session_handle = 0;
            topology_changed = true;
        }

        self.console_windows = next;
        if topology_changed {
            let mut update = VisualUpdate::partial(before_dirty);
            update.add_partial_rect(self.console_stack_dirty_rect());
            update.add_partial_rect(self.taskbar_dirty_rect());
            update
        } else {
            update
        }
    }

    fn reconcile_console_focus(
        &mut self,
        kernel_focused_session: ConsoleSessionHandle,
    ) -> Result<canvas::Rect, i32> {
        let previous_focused = self.focused_session_handle;
        let pending_console_focus = self.pending_console_focus.take().filter(|pending_session| {
            self.console_windows
                .iter()
                .any(|window| window.session_handle == *pending_session && !window.minimized)
        });
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if self.console_windows.is_empty() {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
        }

        if wayland_focused && pending_console_focus.is_none() {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
        }

        self.focused_session_handle = if let Some(pending_session) = pending_console_focus {
            self.focused_wayland_surface_id = None;
            pending_session
        } else if self
            .console_windows
            .iter()
            .any(|window| window.session_handle == kernel_focused_session && !window.minimized)
        {
            kernel_focused_session
        } else {
            0
        };

        let mut dirty_rect = if previous_focused == self.focused_session_handle {
            canvas::Rect::empty()
        } else if previous_focused == 0 {
            self.window_rect_for_session(self.focused_session_handle)
                .union(self.taskbar_slot_rect_for_session(self.focused_session_handle))
        } else {
            self.window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused))
                .union(self.taskbar_slot_rect_for_session(self.focused_session_handle))
                .union(self.window_rect_for_session(self.focused_session_handle))
        };
        while self.focused_session_handle == 0 && !self.console_windows.is_empty() {
            let Some(fallback_session) = self
                .console_windows
                .iter()
                .rev()
                .find(|window| !window.minimized)
                .map(|window| window.session_handle)
            else {
                break;
            };
            self.console_commands.submit_focus(fallback_session);
            self.pending_console_focus = Some(fallback_session);
            self.focused_session_handle = fallback_session;
            dirty_rect = dirty_rect.union(self.window_rect_for_session(fallback_session));
            dirty_rect = dirty_rect.union(self.taskbar_slot_rect_for_session(fallback_session));
        }

        if self.focused_session_handle != 0 && self.focused_session_handle != previous_focused {
            dirty_rect = dirty_rect.union(self.focused_window_reorder_dirty_rect());
        }
        Ok(dirty_rect)
    }

    pub(crate) fn recover_focus_after_wayland_change(
        &mut self,
        wayland: Option<&mut WaylandCompositor>,
    ) -> Result<canvas::Rect, i32> {
        let previous_wayland_focus = self.focused_wayland_surface_id;
        let wayland_focused = self.focused_wayland_surface_id.is_some()
            && self.wayland_windows.iter().any(|window| {
                !window.minimized && Some(window.surface_id) == self.focused_wayland_surface_id
            });
        if wayland_focused {
            return Ok(canvas::Rect::empty());
        }

        self.focused_wayland_surface_id = None;

        let console_focused = self.focused_session_handle != 0
            && self.console_windows.iter().any(|window| {
                !window.minimized && window.session_handle == self.focused_session_handle
            });
        if console_focused {
            return Ok(if previous_wayland_focus.is_some() {
                self.wayland_stack_dirty_rect()
                    .union(self.wayland_taskbar_dirty_rect())
            } else {
                canvas::Rect::empty()
            });
        }

        // If no console explicitly owns focus, prefer the most-recently-stacked
        // visible Wayland window before falling back to a console. Closing one
        // Wayland app on top of another should hand focus to the next Wayland
        // window, not jump back to a terminal in the background.
        if let Some(fallback_surface) = self
            .wayland_windows
            .iter()
            .rev()
            .find(|window| !window.minimized)
            .map(|window| window.surface_id)
        {
            if let Some(wayland) = wayland {
                wayland.focus_surface(fallback_surface);
            }
            self.focused_wayland_surface_id = Some(fallback_surface);
            self.focused_session_handle = 0;
            return Ok(self
                .wayland_stack_dirty_rect()
                .union(self.wayland_taskbar_dirty_rect()));
        }

        let console_refocused = self.refocus_visible_console_window()?;
        Ok(if previous_wayland_focus.is_some() {
            self.wayland_stack_dirty_rect()
                .union(self.wayland_taskbar_dirty_rect())
                .union(console_refocused)
        } else {
            console_refocused
        })
    }

    pub(super) fn mark_console_session_closing(&mut self, session_handle: ConsoleSessionHandle) {
        if session_handle == 0 || self.is_console_session_closing(session_handle) {
            return;
        }
        self.closing_console_sessions.push(session_handle);
        if self.closing_console_sessions.len() > MAX_RUNNING_PROGRAMS {
            let overflow = self.closing_console_sessions.len() - MAX_RUNNING_PROGRAMS;
            self.closing_console_sessions.drain(..overflow);
        }
    }

    fn is_console_session_closing(&self, session_handle: ConsoleSessionHandle) -> bool {
        self.closing_console_sessions.contains(&session_handle)
    }

    fn prune_windows(&mut self, mut keep: impl FnMut(ConsoleSessionHandle) -> bool) -> bool {
        let previous_len = self.console_windows.len();
        let previous_focused = self.focused_session_handle;
        let previous_dragging = self.dragging_window;
        self.console_windows
            .retain(|window| keep(window.session_handle));
        self.clamp_console_snapshot_index();

        if self
            .console_windows
            .iter()
            .all(|window| window.session_handle != self.focused_session_handle)
        {
            self.focused_session_handle = 0;
        }

        if let Some(DragTarget::Console(session_handle)) = self.dragging_window {
            if self
                .console_windows
                .iter()
                .all(|window| window.session_handle != session_handle)
            {
                self.dragging_window = None;
            }
        }

        previous_len != self.console_windows.len()
            || previous_focused != self.focused_session_handle
            || previous_dragging != self.dragging_window
    }

    fn clamp_console_snapshot_index(&mut self) {
        if self.console_windows.is_empty() {
            self.next_console_snapshot_index = 0;
        } else if self.next_console_snapshot_index >= self.console_windows.len() {
            self.next_console_snapshot_index %= self.console_windows.len();
        }
    }

    pub(super) fn focus_window(
        &mut self,
        session_handle: ConsoleSessionHandle,
    ) -> Result<canvas::Rect, i32> {
        if let Some(window) = self
            .console_windows
            .iter_mut()
            .find(|window| window.session_handle == session_handle)
        {
            window.minimized = false;
        }
        self.focused_wayland_surface_id = None;
        let previous_focused = self.focused_session_handle;
        let mut dirty_rect = self.window_rect_for_session(previous_focused);
        dirty_rect = dirty_rect.union(self.window_rect_for_session(session_handle));
        if self.focused_session_handle != session_handle {
            self.console_commands.submit_focus(session_handle);
            self.pending_console_focus = Some(session_handle);
            self.focused_session_handle = session_handle;
        }

        if self.bring_window_to_front(session_handle) {
            dirty_rect = dirty_rect
                .union(self.window_stack_dirty_rect())
                .union(self.taskbar_dirty_rect());
        } else {
            dirty_rect = dirty_rect
                .union(self.taskbar_slot_rect_for_session(previous_focused))
                .union(self.taskbar_slot_rect_for_session(session_handle));
        }

        Ok(dirty_rect)
    }

    pub(super) fn bring_window_to_front(&mut self, session_handle: ConsoleSessionHandle) -> bool {
        let Some(index) = self
            .console_windows
            .iter()
            .position(|window| window.session_handle == session_handle)
        else {
            return false;
        };
        if index + 1 == self.console_windows.len() {
            return false;
        }

        let window = self.console_windows.remove(index);
        self.console_windows.push(window);
        true
    }

    pub(super) fn refocus_visible_console_window(&mut self) -> Result<canvas::Rect, i32> {
        let previous_focused = self.focused_session_handle;
        let Some(session_handle) = self
            .console_windows
            .iter()
            .rev()
            .find(|window| !window.minimized)
            .map(|window| window.session_handle)
        else {
            self.focused_session_handle = 0;
            return Ok(self
                .window_rect_for_session(previous_focused)
                .union(self.taskbar_slot_rect_for_session(previous_focused)));
        };

        let dirty_rect = self
            .window_rect_for_session(previous_focused)
            .union(self.window_rect_for_session(session_handle))
            .union(self.taskbar_slot_rect_for_session(previous_focused))
            .union(self.taskbar_slot_rect_for_session(session_handle))
            .union(self.focused_window_reorder_dirty_rect());
        if self.focused_session_handle != session_handle {
            self.console_commands.submit_focus(session_handle);
            self.pending_console_focus = Some(session_handle);
            self.focused_session_handle = session_handle;
        }

        Ok(dirty_rect)
    }

    fn window_rect_for_session(&self, session_handle: ConsoleSessionHandle) -> canvas::Rect {
        self.console_windows
            .iter()
            .find(|window| window.session_handle == session_handle && !window.minimized)
            .map(|window| window.frame)
            .unwrap_or_default()
    }

    fn taskbar_slot_rect_for_session(&self, session_handle: ConsoleSessionHandle) -> canvas::Rect {
        self.console_windows
            .iter()
            .enumerate()
            .find(|(_, window)| window.session_handle == session_handle)
            .map(|(index, _)| taskbar_slot_rect(self.display.width, self.display.height, index))
            .unwrap_or_default()
    }

    fn window_stack_dirty_rect(&self) -> canvas::Rect {
        self.console_windows
            .iter()
            .fold(canvas::Rect::empty(), |dirty, window| {
                if window.minimized {
                    dirty
                } else {
                    dirty.union(crate::render::console_window_dirty_rect(window.frame))
                }
            })
    }

    fn taskbar_dirty_rect(&self) -> canvas::Rect {
        render::taskbar_dirty_rect(self.display.width, self.display.height)
    }

    fn console_stack_dirty_rect(&self) -> canvas::Rect {
        self.window_stack_dirty_rect()
    }

    fn focused_window_reorder_dirty_rect(&mut self) -> canvas::Rect {
        let session_handle = self.focused_session_handle;
        if session_handle == 0 {
            return canvas::Rect::empty();
        }
        if self.bring_window_to_front(session_handle) {
            self.window_stack_dirty_rect()
                .union(self.taskbar_dirty_rect())
        } else {
            self.window_rect_for_session(session_handle)
        }
    }
}

fn console_session_title(session: &ConsoleSessionInfo) -> String {
    let end = session
        .title
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(session.title.len());
    String::from_utf8_lossy(&session.title[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{ConsoleEchoSignal, CONSOLE_POLL_SLEEP};
    use std::time::{Duration, Instant};

    #[test]
    fn a_keystroke_published_before_the_wait_never_commits_an_interval_sleep() {
        let signal = ConsoleEchoSignal::new();
        let mut observed = 0_u64;
        signal.publish();

        let started = Instant::now();
        signal.wait_for_edge(&mut observed, Duration::from_secs(30));

        // The edge landed between two waits. Comparing the generation under
        // the lock is what keeps it from being lost to the parker's state.
        assert!(started.elapsed() < CONSOLE_POLL_SLEEP);
        assert_eq!(observed, 1);
    }

    #[test]
    fn an_idle_console_falls_back_to_the_poll_interval_instead_of_spinning() {
        let signal = ConsoleEchoSignal::new();
        let mut observed = 0_u64;

        let started = Instant::now();
        signal.wait_for_edge(&mut observed, Duration::from_millis(20));

        assert!(started.elapsed() >= Duration::from_millis(15));
        assert_eq!(observed, 0);
    }

    #[test]
    fn consecutive_keystrokes_are_one_edge_rather_than_a_backlog_of_polls() {
        let signal = ConsoleEchoSignal::new();
        let mut observed = 0_u64;
        signal.publish();
        signal.publish();
        signal.publish();

        // Three keystrokes still owe the poll one look, not three: the
        // generation records that something changed, never how much.
        let started = Instant::now();
        signal.wait_for_edge(&mut observed, Duration::from_secs(30));
        assert!(started.elapsed() < CONSOLE_POLL_SLEEP);
        assert_eq!(observed, 3);

        let started = Instant::now();
        signal.wait_for_edge(&mut observed, Duration::from_millis(20));
        assert!(started.elapsed() >= Duration::from_millis(15));
    }
}
