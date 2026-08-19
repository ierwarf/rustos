use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
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
    console_snapshot_session_output, console_snapshot_sessions, console_wait_graph, open_console,
    spawn_ui_thread, ConsoleSessionHandle, ConsoleSessionInfo, ConsoleStateInfo, InputEvent,
    UiThreadRole, MAX_CONSOLE_SNAPSHOT_BYTES,
};
use crate::wayland::WaylandCompositor;
use runtime_control::RuntimeRunningProgram;

const CONSOLE_COMMAND_QUEUE_CAPACITY: usize = 512;
const SLOW_CONSOLE_INPUT_COMMAND_THRESHOLD: Duration = Duration::from_millis(50);

/// Longest the refresh worker asks the console broker to hold a graph wait
/// while nothing changes. This is a re-arm interval, not a latency: the broker
/// answers on the edge, so shell output reaches the screen as fast as the
/// scheduler carries it.
const CONSOLE_GRAPH_WAIT: Duration =
    Duration::from_millis(rustos_user_abi::syscall::SESSIOND_CONSOLE_GRAPH_WAIT_MAX_MS);

pub(crate) struct ConsoleRefresh {
    state: ConsoleStateInfo,
    sessions: Vec<ConsoleSessionInfo>,
    outputs: Vec<ConsoleSessionOutput>,
    /// Set when a session's output could not be fetched for a reason that is
    /// not "this session has nothing to fetch". Everything collected before it
    /// is still carried - the failure costs one session's output, not the whole
    /// refresh - but it means this snapshot does not fully account for the
    /// generation that announced it, so the caller must not consume that edge.
    failed_session: Option<i32>,
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
        crate::keytrace::mark_submit();
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
    spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-console-command",
        move || {
            while let Ok(command) = receiver.recv() {
                worker_stats
                    .active_started_ms
                    .store(worker_stats.elapsed_ms(), Ordering::Release);
                worker_stats.active.store(true, Ordering::Release);
                let started = Instant::now();
                let result = match command {
                    ConsoleCommand::Input {
                        session_handle,
                        event,
                    } => {
                        crate::keytrace::mark_send();
                        let sent =
                            console_send_input_event(console_fd.as_raw_fd(), session_handle, event);
                        crate::keytrace::mark_delivered();
                        sent
                    }
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

struct ConsoleSessionOutput {
    session_handle: ConsoleSessionHandle,
    output_generation: u64,
    bytes: Vec<u8>,
}

/// Longest the refresh worker will retry a collect that keeps failing before it
/// stops holding the graph token back.
///
/// A failed collect deliberately does not consume the edge, so the next wait
/// returns at once and the collect is retried immediately. That is the right
/// answer for a transient failure and the wrong one for a persistent one: it
/// would spin this worker against the console broker at full speed. After this
/// many consecutive failures the worker sleeps before retrying, which bounds
/// the damage without ever converting the failure into silence.
const MAX_UNPACED_COLLECT_RETRIES: u32 = 4;

pub(crate) fn start_console_refresh_worker(wake: UiWakeSender) -> Receiver<ConsoleRefresh> {
    let (sender, receiver) = mpsc::sync_channel(2);
    spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-console-refresh",
        move || {
            let Ok(console_fd) = open_console() else {
                return;
            };
            let mut snapshot = [0_u8; MAX_CONSOLE_SNAPSHOT_BYTES];
            let mut output_generations = BTreeMap::<ConsoleSessionHandle, u64>::new();
            // The console graph's change token, handed back on every wait so
            // the broker can decide there is nothing to say and hold the
            // reply. Zero means "I have seen nothing", which is answered at
            // once.
            //
            // # Why the announced generation is staged rather than stored
            // This is an edge-triggered subscription: the broker reports a
            // generation once and then holds its reply until that generation
            // moves again. Storing the announcement here - before the snapshot
            // it refers to has actually been fetched - tells the broker "I have
            // seen this" on behalf of a collect that may still fail. If it
            // does, the edge is spent: the payload is still waiting, but the
            // next wait parks for the whole re-arm interval because, as far as
            // the broker can tell, this caller is up to date. It is the classic
            // edge-triggered mistake - consuming the notification without
            // draining what it announced - and it costs one full park budget
            // per failure, which is what a burst of them looks like on screen.
            //
            // So the announcement is staged, and only promoted to `observed`
            // once a collect that ran against it has succeeded. A failed
            // collect leaves the token behind, the next wait is answered
            // immediately, and the fetch is retried against the edge that is
            // still outstanding.
            let mut observed_graph = 0_u64;
            let mut announced_graph = 0_u64;
            let mut consecutive_collect_failures = 0_u32;
            let mut reported_permanent_failure = false;
            let started = Instant::now();
            loop {
                let mut produced_output = false;
                crate::keytrace::mark_refresh();
                crate::keytrace::count_collect_round();
                let collect_started = Instant::now();
                let collected = collect_console_refresh(
                    console_fd.as_raw_fd(),
                    &mut snapshot,
                    &mut output_generations,
                );
                crate::keytrace::add_collecting(collect_started.elapsed());
                match collected {
                    Ok(refresh) => {
                        // A partially collected refresh is still worth
                        // delivering - the sessions that did answer have output
                        // the screen is waiting for - but it has not accounted
                        // for the generation that announced it, so it must not
                        // be allowed to consume that edge.
                        let failed_session = refresh.failed_session;
                        match failed_session {
                            None => consecutive_collect_failures = 0,
                            Some(errno) => {
                                consecutive_collect_failures =
                                    consecutive_collect_failures.saturating_add(1);
                                crate::keytrace::count_failed_collect(errno);
                                report_failed_console_collect(errno, consecutive_collect_failures);
                            }
                        }
                        produced_output = !refresh.outputs.is_empty();
                        if produced_output {
                            crate::keytrace::mark_snapshot();
                        } else {
                            crate::keytrace::count_empty_collect();
                        }
                        // A refresh the main loop could not accept has not
                        // reached anything, so it accounts for nothing.
                        let delivered = match sender.try_send(refresh) {
                            Ok(()) => true,
                            Err(_) => {
                                output_generations.clear();
                                produced_output = true;
                                false
                            }
                        };
                        if collect_accounted_for_its_edge(failed_session, delivered) {
                            // The payload announced by this generation is in
                            // hand, so the subscription may now move past it.
                            observed_graph = announced_graph;
                        } else if consecutive_collect_failures >= MAX_UNPACED_COLLECT_RETRIES {
                            thread::sleep(CONSOLE_POLL_SLEEP);
                        }
                        if delivered && produced_output {
                            // Only new session output can change a pixel.
                            // Waking the main loop for an unchanged snapshot
                            // would trade this thread's fixed cadence for a
                            // busy render loop.
                            wake.signal();
                        }
                    }
                    Err(errno) => {
                        consecutive_collect_failures =
                            consecutive_collect_failures.saturating_add(1);
                        crate::keytrace::count_failed_collect(errno);
                        report_failed_console_collect(errno, consecutive_collect_failures);
                        if consecutive_collect_failures >= MAX_UNPACED_COLLECT_RETRIES {
                            thread::sleep(CONSOLE_POLL_SLEEP);
                        }
                        // The edge that announced this snapshot was never
                        // drained, so it is deliberately not consumed. Falling
                        // through to the wait below is what retries it: the
                        // token is still behind the broker's generation, so the
                        // wait is answered at once rather than parking, and a
                        // broker that is genuinely unreachable fails there too
                        // and is paced by that arm instead of spinning here.
                    }
                }
                if produced_output {
                    // A shell answers a keystroke with more than one write.
                    // Look again before idling so the rest of the line is not
                    // held back by an interval it did not need to wait for.
                    continue;
                }
                // Block on the console's own edge rather than on a timer. This
                // used to be a fixed interval with a keystroke-delivery signal
                // layered over it, which woke this thread *before* the shell
                // had produced anything and then left the real output waiting
                // out the interval. The broker now holds this call until the
                // graph actually moves.
                crate::keytrace::mark_graph_wait();
                let wait_started = Instant::now();
                let waited =
                    console_wait_graph(console_fd.as_raw_fd(), observed_graph, CONSOLE_GRAPH_WAIT);
                crate::keytrace::add_parked(wait_started.elapsed());
                match waited {
                    Ok(generation) => {
                        crate::keytrace::mark_graph();
                        if generation == observed_graph {
                            // The park ran out its budget without the graph
                            // moving. Nothing to fetch, and nothing to stage.
                            crate::keytrace::count_graph_stall();
                        }
                        // Staged, not stored: this is only what the broker says
                        // exists. It becomes `observed_graph` once the collect
                        // that fetches it has succeeded.
                        announced_graph = generation;
                    }
                    Err(errno) => {
                        crate::keytrace::count_graph_wait_error(errno);
                        // A refusal means this caller may never use the rail,
                        // so every later call fails identically and the
                        // compositor silently becomes a `CONSOLE_POLL_SLEEP`
                        // poller. That is exactly what happened for three
                        // releases: a guard admitted only a handle the
                        // compositor cannot hold, every wait returned
                        // `ENOTTY`, and the fallback made a dead readiness
                        // edge look like an idle console.
                        //
                        // Startup is the one time a refusal is not final: the
                        // authority this rail requires is published when the
                        // service registers, and this worker can outrun that.
                        // Same budget and reasoning as the input wait set.
                        let refused_for_good = !console_graph_wait_is_retryable(errno)
                            && started.elapsed() >= CONSOLE_GRAPH_WAIT_STARTUP_BUDGET;
                        if refused_for_good && !reported_permanent_failure {
                            reported_permanent_failure = true;
                            crate::sys::debug_line(&format!(
                                "uiserver: console graph wait refused errno={errno}; \
                                 polling instead and losing keystroke echo latency"
                            ));
                        }
                        thread::sleep(CONSOLE_POLL_SLEEP);
                    }
                }
            }
        },
    )
    .unwrap_or_else(|_| std::process::exit(134));
    receiver
}

/// How long a refusal is still allowed to be a startup race rather than a
/// verdict. The authority the graph wait requires is published when the
/// service registers, which this worker can start before.
const CONSOLE_GRAPH_WAIT_STARTUP_BUDGET: Duration =
    Duration::from_millis(rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS);

/// Whether a failed console graph wait is worth retrying.
///
/// A budget that expired or a broker that is not up yet will answer the next
/// call. A refusal - the caller lacks the handle or the authority the rail
/// requires, or the rail is not implemented - will refuse identically forever,
/// so treating it as transient converts a broken readiness edge into a silent
/// polling loop.
fn console_graph_wait_is_retryable(errno: i32) -> bool {
    matches!(
        errno,
        libc::ETIMEDOUT | libc::EAGAIN | libc::EINTR | libc::ENODEV | libc::ECONNRESET
    )
}

/// Whether a collect has fully accounted for the console generation that
/// announced it, and may therefore let the subscription move past that edge.
///
/// # Why this is the only place the token may advance
/// The console graph is an edge-triggered subject: the broker names a
/// generation once and then holds every later reply until it moves again.
/// Telling it "I have seen this" is therefore a promise that the snapshot it
/// announced has been fetched *and* handed on. A collect that failed part way,
/// or one the main loop refused, has done neither - and if the token advances
/// anyway the outstanding output is not late, it is unreachable, until some
/// unrelated change moves the generation again. Every such mistake costs a full
/// re-arm interval, which is why they show up on screen as output arriving in
/// bursts rather than as uniformly slower echo.
fn collect_accounted_for_its_edge(failed_session: Option<i32>, delivered: bool) -> bool {
    failed_session.is_none() && delivered
}

/// Reports what one collect saw, against what it already had.
///
/// Rate limited to the first samples plus a sparse tail: this runs on every
/// round of a loop that is meant to be idle most of the time.
fn report_collect_generations(
    sessions: &[ConsoleSessionInfo],
    output_generations: &BTreeMap<ConsoleSessionHandle, u64>,
) {
    const EARLY_SAMPLES: u64 = 24;
    static COLLECTS: AtomicU64 = AtomicU64::new(0);
    let total = COLLECTS.fetch_add(1, Ordering::Relaxed) + 1;
    if total > EARLY_SAMPLES && total % 32 != 0 {
        return;
    }
    let Some(session) = sessions.first() else {
        crate::sys::debug_line(&format!("uiserver: collect sessions=0 seq={total}"));
        return;
    };
    let held = output_generations
        .get(&session.session_handle)
        .copied()
        .unwrap_or(0);
    crate::sys::debug_line(&format!(
        "uiserver: collect sessions={} handle={} reported={} held={} fresh={} seq={total}",
        sessions.len(),
        session.session_handle,
        session.output_generation,
        held,
        u8::from(session.output_generation != held),
    ));
}

/// Reports a console collect that failed.
///
/// This used to be `if let Ok(refresh)`, which discarded the error and every
/// consequence of it. The worker then re-parked as though it had nothing to
/// fetch, so a failing snapshot rail was indistinguishable from an idle
/// console - the same silence that hid a dead readiness edge for three
/// releases.
///
/// Rate limited to the first samples plus a sparse tail: a rail that fails
/// every round must not become the thing that keeps this worker busy.
fn report_failed_console_collect(errno: i32, consecutive: u32) {
    const EARLY_SAMPLES: u64 = 8;
    static FAILURES: AtomicU64 = AtomicU64::new(0);
    let total = FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if total > EARLY_SAMPLES && total % 64 != 0 {
        return;
    }
    crate::sys::debug_line(&format!(
        "uiserver: console collect failed errno={errno} consecutive={consecutive} seq={total}"
    ));
}

fn collect_console_refresh(
    console_fd: i32,
    snapshot: &mut [u8; MAX_CONSOLE_SNAPSHOT_BYTES],
    output_generations: &mut BTreeMap<ConsoleSessionHandle, u64>,
) -> Result<ConsoleRefresh, i32> {
    crate::keytrace::mark_state_start();
    let state = console_get_state(console_fd)?;
    crate::keytrace::mark_state_done();
    let mut sessions = [ConsoleSessionInfo::default(); crate::sys::CONSOLE_SESSION_CAPACITY];
    crate::keytrace::mark_sessions_start();
    let session_count = console_snapshot_sessions(console_fd, &mut sessions)?
        .min(crate::sys::CONSOLE_SESSION_CAPACITY);
    crate::keytrace::mark_sessions_done();
    let sessions = sessions[..session_count].to_vec();
    output_generations.retain(|session_handle, _| {
        sessions
            .iter()
            .any(|session| session.session_handle == *session_handle)
    });
    let mut outputs = Vec::new();
    let mut failed_session = None;
    // The last link with no measurement on it: what the broker reports as each
    // session's generation, against what this worker already holds. A collect
    // that is woken by an edge and then finds these equal is being told about a
    // change it cannot see, and that is the only remaining way `snapshot_us`
    // can span several park rounds while every ioctl inside it costs about a
    // millisecond.
    report_collect_generations(&sessions, output_generations);
    for session in &sessions {
        let previous_generation = output_generations
            .get(&session.session_handle)
            .copied()
            .unwrap_or(0);
        if previous_generation == session.output_generation {
            continue;
        }
        crate::keytrace::mark_output_start();
        match console_snapshot_session_output(console_fd, session.session_handle, snapshot) {
            Ok(count) => {
                crate::keytrace::mark_output_done();
                outputs.push(ConsoleSessionOutput {
                    session_handle: session.session_handle,
                    output_generation: session.output_generation,
                    bytes: snapshot[..count].to_vec(),
                });
                output_generations.insert(session.session_handle, session.output_generation);
            }
            // This session has no output to fetch and never will at this
            // generation: it is gone, or it is not one this rail serves. Record
            // the generation so the next round does not ask again, and keep
            // collecting - one departed session must not cost the others their
            // refresh.
            Err(crate::sys::ENOENT | crate::sys::EINVAL | crate::sys::ESTALE) => {
                output_generations.insert(session.session_handle, session.output_generation);
            }
            // A real failure. It used to `return Err` here, which threw away
            // every session already fetched in this pass along with the
            // generations recorded for them - so one failing session erased
            // output that had been read successfully and was already on its way
            // to the screen. Report what was collected and leave this session's
            // generation unrecorded, which is what makes the next round retry
            // exactly the session that failed.
            Err(errno) => {
                failed_session = Some(errno);
                break;
            }
        }
    }

    Ok(ConsoleRefresh {
        state,
        sessions,
        outputs,
        failed_session,
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
    use super::CONSOLE_GRAPH_WAIT;
    use rustos_user_abi::performance::IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS;
    use std::time::Duration;

    /// The broker holds this wait inside a call kernel compat will abandon at
    /// its class deadline. A re-arm interval at or above that deadline would
    /// turn every idle wait into a timeout, and the worker would be back to
    /// looking on a timer - with an error path for a cadence.
    #[test]
    fn the_graph_wait_re_arms_inside_the_class_deadline_that_carries_it() {
        assert!(CONSOLE_GRAPH_WAIT < Duration::from_millis(IPC_INTERACTIVE_CONTROL_HARD_LIMIT_MS));
        // And it has to be a real park, or the interval poll it replaced is
        // still being paid under a new name.
        assert!(CONSOLE_GRAPH_WAIT >= super::CONSOLE_POLL_SLEEP);
    }

    /// An edge-triggered subscription may only be advanced by an observation
    /// that actually completed.
    ///
    /// The broker answers a graph wait once per generation and then holds the
    /// reply until the generation moves again. So the token this worker hands
    /// back is a claim to have seen everything up to it. Advancing it after a
    /// collect that failed, or after a refresh the main loop could not accept,
    /// makes that claim falsely - and the output it skipped then waits for an
    /// unrelated change to move the generation, one full re-arm interval at a
    /// time. `snapshot_us` was measured at 803 ms against ioctls that each took
    /// about a millisecond, which is what a run of those looks like.
    #[test]
    fn only_a_collect_that_fetched_and_delivered_may_consume_the_graph_edge() {
        assert!(super::collect_accounted_for_its_edge(None, true));
        assert!(
            !super::collect_accounted_for_its_edge(Some(libc::EAGAIN), true),
            "a session whose output could not be fetched leaves the edge outstanding"
        );
        assert!(
            !super::collect_accounted_for_its_edge(None, false),
            "a refresh the main loop refused has reached nothing and accounts for nothing"
        );
        assert!(!super::collect_accounted_for_its_edge(
            Some(libc::EAGAIN),
            false
        ));
    }

    /// The retry that a withheld token produces must be paced.
    ///
    /// Not consuming the edge is what makes the next wait return immediately,
    /// which is exactly right for a transient failure and would be a full-speed
    /// spin against the console broker for a persistent one.
    #[test]
    fn a_persistently_failing_collect_is_paced_rather_than_spun() {
        assert!(super::MAX_UNPACED_COLLECT_RETRIES >= 1);
        assert!(
            super::CONSOLE_POLL_SLEEP > Duration::ZERO,
            "the paced retry has to actually yield"
        );
    }

    /// A refusal and a hiccup must not share a branch.
    ///
    /// They did, and it cost three releases: the kernel guard admitted only a
    /// handle the compositor cannot hold, so every wait returned `ENOTTY`, and
    /// the retry branch turned that permanent refusal into a `CONSOLE_POLL_SLEEP`
    /// cadence that looked exactly like an idle console. Keystroke echo was
    /// measured at a 21 ms median against a rail that had never once answered.
    #[test]
    fn a_refused_graph_wait_is_never_mistaken_for_a_broker_hiccup() {
        for transient in [
            libc::ETIMEDOUT,
            libc::EAGAIN,
            libc::EINTR,
            libc::ENODEV,
            libc::ECONNRESET,
        ] {
            assert!(
                super::console_graph_wait_is_retryable(transient),
                "errno {transient} resolves itself and must be retried"
            );
        }
        for refusal in [libc::ENOTTY, libc::EPERM, libc::EACCES, libc::EINVAL] {
            assert!(
                !super::console_graph_wait_is_retryable(refusal),
                "errno {refusal} will refuse identically forever and must be reported, \
                 not absorbed into a polling cadence"
            );
        }
    }
}
