use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use runtime_control::RuntimeClient;

use crate::app::{
    AppState, InputProcessingResult, VisualUpdate, INPUT_EVENT_BATCH, INPUT_PROCESS_BUDGET,
    MAX_INPUT_READ_BATCHES_PER_TICK,
};
use crate::profile;
use crate::sys::{
    boot_line, diag_line, read_input, spawn_ui_thread, InputEvent, UiThreadRole, INPUT_ACTION_NONE,
    INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION,
};
use crate::wayland::WaylandCompositor;

const SLOW_INPUT_READ_THRESHOLD_MS: u128 = 50;
const INPUT_READER_QUEUE_CAPACITY: usize = INPUT_EVENT_BATCH * 64;
const INPUT_READER_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const INPUT_READER_PANIC_THRESHOLD_MS: u64 = 3_000;
const MAX_INITIAL_INPUT_DROP_LOGS: u64 = 8;
const MAX_WAYLAND_POINTER_FLUSH_EVENTS: u64 = 16;
const INPUT_WAITSET_RETRY_BACKOFF: Duration = Duration::from_millis(10);
const INPUT_WAITSET_STARTUP_BUDGET: Duration =
    Duration::from_millis(rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS);

pub(crate) struct InputReader {
    receiver: Receiver<InputEvent>,
    wake_sender: UiWakeSender,
    wake_generation: Arc<AtomicU64>,
    observed_wake_generation: AtomicU64,
    stats: Arc<InputReaderStats>,
}

/// Coalesced UI-loop readiness publisher.
///
/// The generation is the authoritative readiness edge. The capacity-one
/// generation is the scheduler-independent notification token. The main loop
/// samples it around a bounded compositor deadline; repeated publications may
/// coalesce but can never erase an input or Wayland edge.
#[derive(Clone)]
pub(crate) struct UiWakeSender {
    generation: Arc<AtomicU64>,
    thread: thread::Thread,
}

impl UiWakeSender {
    pub(crate) fn signal(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.thread.unpark();
    }
}

struct InputReaderStats {
    started: Instant,
    wait_active: AtomicBool,
    wait_started_ms: AtomicU64,
    wait_attempts: AtomicU64,
    completed_waits: AtomicU64,
    read_active: AtomicBool,
    read_started_ms: AtomicU64,
    read_attempts: AtomicU64,
    completed_reads: AtomicU64,
    raw_events: AtomicU64,
    delivered_events: AtomicU64,
    last_delivery_ms: AtomicU64,
    queue_drops: AtomicU64,
    slow_reads: AtomicU64,
    errors: AtomicU64,
}

pub(crate) struct InputReaderSnapshot {
    pub(crate) wait_active: bool,
    pub(crate) wait_elapsed_ms: u64,
    pub(crate) wait_attempts: u64,
    pub(crate) completed_waits: u64,
    pub(crate) read_active: bool,
    pub(crate) read_elapsed_ms: u64,
    pub(crate) read_attempts: u64,
    pub(crate) completed_reads: u64,
    pub(crate) raw_events: u64,
    pub(crate) delivered_events: u64,
    pub(crate) last_delivery_age_ms: u64,
    pub(crate) queue_drops: u64,
    pub(crate) slow_reads: u64,
    pub(crate) errors: u64,
}

impl InputReader {
    fn try_recv(&self) -> Result<InputEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    fn wait_for_wake_until(&self, deadline: Instant, mut trace: impl FnMut(InputWaitPhase)) {
        trace(InputWaitPhase::CheckGeneration);
        let published_generation = self.wake_generation.load(Ordering::Acquire);
        let observed_generation = self
            .observed_wake_generation
            .swap(published_generation, Ordering::AcqRel);
        if published_generation != observed_generation {
            return;
        }
        trace(InputWaitPhase::ComputeDeadline);
        let Some(duration) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        if duration.is_zero() {
            return;
        }
        trace(InputWaitPhase::RecheckGeneration);
        let rechecked_generation = self.wake_generation.load(Ordering::Acquire);
        if rechecked_generation != published_generation {
            self.observed_wake_generation
                .store(rechecked_generation, Ordering::Release);
            return;
        }
        // The generation closes the check/arm race; the parker is only a
        // coalescing notification mechanism. Its timed deadline remains an
        // independent recovery authority even when publications coalesce.
        trace(InputWaitPhase::Park);
        thread::park_timeout(duration);
        trace(InputWaitPhase::Returned);
    }

    pub(crate) fn wake_sender(&self) -> UiWakeSender {
        self.wake_sender.clone()
    }

    pub(crate) fn snapshot(&self) -> InputReaderSnapshot {
        self.stats.snapshot()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputWaitPhase {
    CheckGeneration,
    ComputeDeadline,
    RecheckGeneration,
    Park,
    Returned,
}

impl InputReaderStats {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            wait_active: AtomicBool::new(false),
            wait_started_ms: AtomicU64::new(0),
            wait_attempts: AtomicU64::new(0),
            completed_waits: AtomicU64::new(0),
            read_active: AtomicBool::new(false),
            read_started_ms: AtomicU64::new(0),
            read_attempts: AtomicU64::new(0),
            completed_reads: AtomicU64::new(0),
            raw_events: AtomicU64::new(0),
            delivered_events: AtomicU64::new(0),
            last_delivery_ms: AtomicU64::new(0),
            queue_drops: AtomicU64::new(0),
            slow_reads: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
    }

    fn snapshot(&self) -> InputReaderSnapshot {
        let now_ms = self.elapsed_ms();
        let wait_active = self.wait_active.load(Ordering::Acquire);
        let wait_started_ms = self.wait_started_ms.load(Ordering::Acquire);
        let read_active = self.read_active.load(Ordering::Acquire);
        let read_started_ms = self.read_started_ms.load(Ordering::Acquire);
        let last_delivery_ms = self.last_delivery_ms.load(Ordering::Acquire);
        InputReaderSnapshot {
            wait_active,
            wait_elapsed_ms: if wait_active {
                now_ms.saturating_sub(wait_started_ms)
            } else {
                0
            },
            wait_attempts: self.wait_attempts.load(Ordering::Acquire),
            completed_waits: self.completed_waits.load(Ordering::Acquire),
            read_active,
            read_elapsed_ms: if read_active {
                now_ms.saturating_sub(read_started_ms)
            } else {
                0
            },
            read_attempts: self.read_attempts.load(Ordering::Acquire),
            completed_reads: self.completed_reads.load(Ordering::Acquire),
            raw_events: self.raw_events.load(Ordering::Acquire),
            delivered_events: self.delivered_events.load(Ordering::Acquire),
            last_delivery_age_ms: if last_delivery_ms == 0 {
                0
            } else {
                now_ms.saturating_sub(last_delivery_ms)
            },
            queue_drops: self.queue_drops.load(Ordering::Acquire),
            slow_reads: self.slow_reads.load(Ordering::Acquire),
            errors: self.errors.load(Ordering::Acquire),
        }
    }
}

fn create_input_wait_epoll_once(input_fds: &[OwnedFd]) -> Result<OwnedFd, i32> {
    if input_fds.is_empty() {
        return Err(libc::ENODEV);
    }
    let raw_epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if raw_epoll < 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    let epoll = unsafe { OwnedFd::from_raw_fd(raw_epoll) };
    for (index, input_fd) in input_fds.iter().enumerate() {
        let mut event = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLERR | libc::EPOLLHUP) as u32,
            u64: index as u64 + 1,
        };
        let status = unsafe {
            libc::epoll_ctl(
                epoll.as_raw_fd(),
                libc::EPOLL_CTL_ADD,
                input_fd.as_raw_fd(),
                &mut event,
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO));
        }
    }
    Ok(epoll)
}

fn retryable_input_waitset_error(errno: i32) -> bool {
    matches!(errno, libc::ETIMEDOUT | libc::EAGAIN | libc::EINTR)
}

fn create_input_wait_epoll(input_fds: &[OwnedFd]) -> Result<OwnedFd, i32> {
    let deadline = Instant::now() + INPUT_WAITSET_STARTUP_BUDGET;
    loop {
        match create_input_wait_epoll_once(input_fds) {
            Ok(epoll) => return Ok(epoll),
            Err(errno) if retryable_input_waitset_error(errno) && Instant::now() < deadline => {
                // Each persistent mutation retains its independent 100 ms
                // reconciliation rail in compat. Boot owns a larger bounded
                // recovery window so a single-threaded userspace server that
                // is completing an earlier request cannot turn transient head-
                // of-line delay into permanent loss of the display service.
                thread::sleep(INPUT_WAITSET_RETRY_BACKOFF);
            }
            Err(errno) => return Err(errno),
        }
    }
}

pub(crate) fn start_input_reader(input_fds: Vec<OwnedFd>) -> Result<InputReader, i32> {
    let input_wait_epoll = create_input_wait_epoll(&input_fds).map_err(|errno| {
        // This is a synchronous boot-terminal diagnostic. The normal
        // observability worker may not run again when persistent epoll
        // creation or registration exhausts its bounded service deadline.
        crate::sys::debug_line(&format!(
            "uiserver: startup failed stage=input-waitset-create errno={errno}"
        ));
        errno
    })?;
    let (sender, receiver) = mpsc::sync_channel::<InputEvent>(INPUT_READER_QUEUE_CAPACITY);
    let wake_generation = Arc::new(AtomicU64::new(0));
    let shared_wake_sender = UiWakeSender {
        generation: Arc::clone(&wake_generation),
        thread: thread::current(),
    };
    let reader_wake_sender = shared_wake_sender.clone();
    let stats = Arc::new(InputReaderStats::new());
    let reader_stats = Arc::clone(&stats);
    spawn_ui_thread(UiThreadRole::Protocol, "uiserver-input-reader", move || {
        boot_line("uiserver: input reader worker enter");
        let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
        let mut readiness = [libc::epoll_event { events: 0, u64: 0 }; 8];
        let mut first_read = true;
        loop {
            reader_stats
                .wait_started_ms
                .store(reader_stats.elapsed_ms(), Ordering::Release);
            reader_stats.wait_active.store(true, Ordering::Release);
            reader_stats.wait_attempts.fetch_add(1, Ordering::Relaxed);
            let ready = loop {
                let ready = unsafe {
                    libc::epoll_wait(
                        input_wait_epoll.as_raw_fd(),
                        readiness.as_mut_ptr(),
                        readiness.len() as i32,
                        -1,
                    )
                };
                if ready >= 0 {
                    break ready as usize;
                }
                let errno = std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO);
                if errno != libc::EINTR {
                    diag_line(format!(
                        "uiserver input watchdog panic: readiness wait failed errno={errno}"
                    ));
                    std::process::exit(134);
                }
            };
            reader_stats.wait_active.store(false, Ordering::Release);
            reader_stats.completed_waits.fetch_add(1, Ordering::Relaxed);
            if readiness[..ready]
                .iter()
                .any(|event| event.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0)
            {
                diag_line("uiserver input watchdog panic: input provider revoked");
                std::process::exit(134);
            }
            if first_read {
                boot_line("uiserver: input reader first nonblocking read begin");
            }
            reader_stats
                .read_started_ms
                .store(reader_stats.elapsed_ms(), Ordering::Release);
            reader_stats.read_active.store(true, Ordering::Release);
            reader_stats.read_attempts.fetch_add(1, Ordering::Relaxed);
            let read_started = Instant::now();
            let result = read_input(&input_fds, &mut events);
            if first_read {
                boot_line("uiserver: input reader first nonblocking read returned");
                first_read = false;
            }
            reader_stats.read_active.store(false, Ordering::Release);
            reader_stats.completed_reads.fetch_add(1, Ordering::Relaxed);
            let read_count = match result {
                Ok(count) => count,
                Err(errno) => {
                    reader_stats.errors.fetch_add(1, Ordering::Relaxed);
                    diag_line(format!("uiserver: input reader failed errno={errno}"));
                    continue;
                }
            };
            let read_elapsed = read_started.elapsed();
            if read_elapsed.as_millis() >= SLOW_INPUT_READ_THRESHOLD_MS {
                reader_stats.slow_reads.fetch_add(1, Ordering::Relaxed);
                diag_line(format!(
                    "uiserver: input read slow elapsed_ms={} events={}",
                    read_elapsed.as_millis(),
                    read_count,
                ));
            }
            if read_count == 0 {
                continue;
            }
            reader_stats
                .raw_events
                .fetch_add(read_count as u64, Ordering::Relaxed);
            let mut batch = InputReaderBatchCoalescer::default();
            for event in events[..read_count].iter().copied() {
                batch.push(event);
            }

            let mut sent = 0_u64;
            let mut coalesced = [InputEvent::default(); INPUT_EVENT_BATCH];
            for event in batch.drain_into(&mut coalesced).iter().copied() {
                if sender.try_send(event).is_err() {
                    let drop_count = reader_stats
                        .queue_drops
                        .fetch_add(1, Ordering::Relaxed)
                        .saturating_add(1);
                    if drop_count <= MAX_INITIAL_INPUT_DROP_LOGS || drop_count.is_power_of_two() {
                        diag_line(format!(
                            "uiserver: input reader queue full; dropping event drops={drop_count}"
                        ));
                    }
                    break;
                }
                sent = sent.saturating_add(1);
            }
            reader_stats
                .delivered_events
                .fetch_add(sent, Ordering::Relaxed);
            if sent > 0 {
                reader_stats
                    .last_delivery_ms
                    .store(reader_stats.elapsed_ms(), Ordering::Release);
                reader_wake_sender.signal();
            }
        }
    })
    .unwrap_or_else(|_| {
        diag_line("uiserver input watchdog panic: failed to spawn input reader");
        std::process::exit(134);
    });

    let watchdog_stats = Arc::clone(&stats);
    spawn_ui_thread(UiThreadRole::Watchdog, "uiserver-input-watchdog", move || {
            loop {
                thread::sleep(INPUT_READER_WATCHDOG_INTERVAL);
                let snapshot = watchdog_stats.snapshot();
                // An input reader blocked in epoll_wait is healthy: provider
                // readiness and revoke own its wakeup. Only a post-readiness
                // read may consume this watchdog's bounded progress budget.
                if snapshot.read_active
                    && snapshot.read_elapsed_ms >= INPUT_READER_PANIC_THRESHOLD_MS
                {
                    diag_line(format!(
                        "uiserver input watchdog panic: read_input blocked elapsed_ms={} read_attempts={} completed_reads={} raw_events={} delivered_events={} queue_drops={} slow_reads={} errors={}",
                        snapshot.read_elapsed_ms,
                        snapshot.read_attempts,
                        snapshot.completed_reads,
                        snapshot.raw_events,
                        snapshot.delivered_events,
                        snapshot.queue_drops,
                        snapshot.slow_reads,
                        snapshot.errors,
                    ));
                    std::process::exit(134);
                }
            }
        })
        .unwrap_or_else(|_| {
            diag_line("uiserver input watchdog panic: failed to spawn input watchdog");
            std::process::exit(134);
        });

    Ok(InputReader {
        receiver,
        wake_sender: shared_wake_sender,
        wake_generation,
        observed_wake_generation: AtomicU64::new(0),
        stats,
    })
}

#[derive(Clone, Copy, Debug)]
struct InputReaderBatchCoalescer {
    pending_pointer: Option<InputEvent>,
    events: [InputEvent; INPUT_EVENT_BATCH],
    len: usize,
}

impl Default for InputReaderBatchCoalescer {
    fn default() -> Self {
        Self {
            pending_pointer: None,
            events: [InputEvent::default(); INPUT_EVENT_BATCH],
            len: 0,
        }
    }
}

impl InputReaderBatchCoalescer {
    fn push(&mut self, event: InputEvent) {
        match event.kind {
            INPUT_KIND_POINTER_MOTION => {
                self.push_pointer(event);
            }
            INPUT_KIND_POINTER_POSITION => {
                self.push_pointer(event);
            }
            _ => {
                self.flush_pointer();
                self.push_non_pointer(event);
            }
        }
    }

    fn drain_into(mut self, out: &mut [InputEvent; INPUT_EVENT_BATCH]) -> &[InputEvent] {
        self.flush_pointer();
        let count = self.len.min(out.len());
        out[..count].copy_from_slice(&self.events[..count]);
        &out[..count]
    }

    fn push_pointer(&mut self, event: InputEvent) {
        if let Some(pending) = self.pending_pointer.as_mut() {
            match (pending.kind, event.kind) {
                (INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_MOTION) => {
                    pending.value0 = saturating_i32_add(pending.value0, event.value0);
                    pending.value1 = saturating_i32_add(pending.value1, event.value1);
                    return;
                }
                (INPUT_KIND_POINTER_POSITION, INPUT_KIND_POINTER_POSITION) => {
                    pending.value0 = event.value0;
                    pending.value1 = event.value1;
                    return;
                }
                _ => {
                    self.flush_pointer();
                }
            }
        }
        self.pending_pointer = Some(event);
    }

    fn flush_pointer(&mut self) {
        let Some(event) = self.pending_pointer.take() else {
            return;
        };
        self.push_non_pointer(event);
    }

    fn push_non_pointer(&mut self, event: InputEvent) {
        if self.len < self.events.len() {
            self.events[self.len] = event;
            self.len += 1;
        }
    }
}

fn saturating_i32_add(left: i32, right: i32) -> i32 {
    let value = i64::from(left) + i64::from(right);
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[derive(Clone, Copy, Debug)]
struct PendingPointerPosition {
    x: u32,
    y: u32,
    dirty: bool,
}

impl PendingPointerPosition {
    fn new(x: u32, y: u32) -> Self {
        Self { x, y, dirty: false }
    }

    fn apply_motion(&mut self, dx: i32, dy: i32, max_x: u32, max_y: u32) {
        self.x = clamp_pointer_coordinate(self.x as i64 + i64::from(dx), max_x);
        self.y = clamp_pointer_coordinate(self.y as i64 + i64::from(dy), max_y);
        self.dirty = true;
    }

    fn apply_position(&mut self, x: i32, y: i32, max_x: u32, max_y: u32) {
        self.x = clamp_pointer_coordinate(i64::from(x), max_x);
        self.y = clamp_pointer_coordinate(i64::from(y), max_y);
        self.dirty = true;
    }

    fn take_event(&mut self) -> Option<InputEvent> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(InputEvent {
            kind: INPUT_KIND_POINTER_POSITION,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0: self.x as i32,
            value1: self.y as i32,
            modifiers: 0,
            text: 0,
        })
    }
}

pub(crate) fn process_pending_input(
    state: &mut AppState,
    wayland: Option<&mut WaylandCompositor>,
    runtime: &RuntimeClient,
    events: &InputReader,
) -> Result<InputProcessingResult, i32> {
    let previous_cursor_x = state.cursor_x;
    let previous_cursor_y = state.cursor_y;
    let mut visual_update = VisualUpdate::default();
    let mut backlog_remaining = false;
    let started_at = Instant::now();
    let mut input_events = 0_u64;
    let mut pointer_motion_events = 0_u64;
    let mut pointer_position_events = 0_u64;
    let mut other_events = 0_u64;
    let mut wayland_motion_calls = 0_u64;
    let max_x = state.display.width.saturating_sub(1);
    let max_y = state.display.height.saturating_sub(1);
    let mut pending_pointer = PendingPointerPosition::new(state.cursor_x, state.cursor_y);

    let mut wayland = wayland;
    let max_events_per_tick = INPUT_EVENT_BATCH.saturating_mul(MAX_INPUT_READ_BATCHES_PER_TICK);
    for event_index in 0..max_events_per_tick {
        if event_index != 0 && started_at.elapsed() >= INPUT_PROCESS_BUDGET {
            backlog_remaining = true;
            break;
        }
        let event = match events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        };

        input_events = input_events.saturating_add(1);
        match event.kind {
            crate::sys::INPUT_KIND_POINTER_MOTION => {
                pointer_motion_events = pointer_motion_events.saturating_add(1);
                pending_pointer.apply_motion(event.value0, event.value1, max_x, max_y);
                continue;
            }
            INPUT_KIND_POINTER_POSITION => {
                pointer_position_events = pointer_position_events.saturating_add(1);
                pending_pointer.apply_position(event.value0, event.value1, max_x, max_y);
                continue;
            }
            _ => {
                other_events = other_events.saturating_add(1);
            }
        }

        let allow_wayland_pointer = false;
        let pointer_wayland = if allow_wayland_pointer {
            wayland.as_deref_mut()
        } else {
            None
        };
        if flush_pending_pointer(
            state,
            runtime,
            pointer_wayland,
            &mut pending_pointer,
            &mut visual_update,
        )? {
            wayland_motion_calls = wayland_motion_calls.saturating_add(1);
        }

        let event_wayland = if event_index == 0 && started_at.elapsed() < INPUT_PROCESS_BUDGET {
            wayland.as_deref_mut()
        } else {
            None
        };
        let redraw = state.handle_input_event(runtime, &event, event_wayland)?;
        visual_update.absorb(redraw);
        if event_index + 1 == max_events_per_tick {
            backlog_remaining = true;
        }
    }

    let allow_wayland_pointer = !backlog_remaining
        && input_events <= MAX_WAYLAND_POINTER_FLUSH_EVENTS
        && started_at.elapsed() < INPUT_PROCESS_BUDGET;
    let pointer_wayland = if allow_wayland_pointer { wayland } else { None };
    if flush_pending_pointer(
        state,
        runtime,
        pointer_wayland,
        &mut pending_pointer,
        &mut visual_update,
    )? {
        wayland_motion_calls = wayland_motion_calls.saturating_add(1);
    }

    let cursor_moved = state.cursor_x != previous_cursor_x || state.cursor_y != previous_cursor_y;
    profile::record_input_loop(
        started_at.elapsed(),
        input_events,
        pointer_motion_events,
        pointer_position_events,
        other_events,
        backlog_remaining,
        cursor_moved,
    );

    Ok(InputProcessingResult {
        visual_update,
        backlog_remaining,
        input_events,
        pointer_motion_events,
        pointer_position_events,
        wayland_motion_calls,
    })
}

fn flush_pending_pointer(
    state: &mut AppState,
    runtime: &RuntimeClient,
    wayland: Option<&mut WaylandCompositor>,
    pending_pointer: &mut PendingPointerPosition,
    visual_update: &mut VisualUpdate,
) -> Result<bool, i32> {
    let Some(event) = pending_pointer.take_event() else {
        return Ok(false);
    };

    let redraw = state.handle_input_event(runtime, &event, wayland)?;
    visual_update.absorb(redraw);
    Ok(true)
}

fn clamp_pointer_coordinate(value: i64, max: u32) -> u32 {
    if value <= 0 {
        return 0;
    }
    let value = value as u64;
    let max = u64::from(max);
    value.min(max) as u32
}

pub(crate) fn sleep_until(deadline: Instant) {
    if let Some(duration) = deadline.checked_duration_since(Instant::now()) {
        if !duration.is_zero() {
            thread::sleep(duration);
        }
    }
}

pub(crate) fn sleep_until_input_or(
    events: &InputReader,
    deadline: Instant,
    trace: impl FnMut(InputWaitPhase),
) {
    events.wait_for_wake_until(deadline, trace);
}

#[cfg(test)]
mod tests {
    use super::{
        create_input_wait_epoll, InputReader, InputReaderBatchCoalescer, InputReaderStats,
        UiWakeSender, INPUT_EVENT_BATCH,
    };
    use crate::sys::{
        InputEvent, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_KIND_KEYBOARD,
        INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn input_waitset_startup_retries_only_transient_control_failures() {
        assert!(super::retryable_input_waitset_error(libc::ETIMEDOUT));
        assert!(super::retryable_input_waitset_error(libc::EAGAIN));
        assert!(super::retryable_input_waitset_error(libc::EINTR));
        assert!(!super::retryable_input_waitset_error(libc::EINVAL));
        assert!(!super::retryable_input_waitset_error(libc::ENODEV));
        assert_eq!(
            super::INPUT_WAITSET_STARTUP_BUDGET,
            Duration::from_millis(rustos_user_abi::performance::IPC_BOOT_CONTROL_HARD_LIMIT_MS)
        );
        assert!(super::INPUT_WAITSET_RETRY_BACKOFF < super::INPUT_WAITSET_STARTUP_BUDGET);
    }

    fn event(kind: u16, value0: i32, value1: i32) -> InputEvent {
        InputEvent {
            kind,
            action: INPUT_ACTION_NONE,
            code: 0,
            value0,
            value1,
            modifiers: 0,
            text: 0,
        }
    }

    fn keyboard_event() -> InputEvent {
        InputEvent {
            kind: INPUT_KIND_KEYBOARD,
            action: INPUT_ACTION_PRESSED,
            code: 30,
            value0: 0,
            value1: 0,
            modifiers: 0,
            text: b'a' as u32,
        }
    }

    #[test]
    fn prequeued_wake_never_commits_a_timeout_sleep() {
        let (_event_sender, event_receiver) = mpsc::sync_channel(1);
        let wake_generation = Arc::new(AtomicU64::new(0));
        let wake_sender = UiWakeSender {
            generation: Arc::clone(&wake_generation),
            thread: thread::current(),
        };
        wake_sender.signal();
        let reader = InputReader {
            receiver: event_receiver,
            wake_sender,
            wake_generation,
            observed_wake_generation: AtomicU64::new(0),
            stats: Arc::new(InputReaderStats::new()),
        };

        let started = Instant::now();
        reader.wait_for_wake_until(started + Duration::from_secs(1), |_| {});
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn coalesced_notification_tokens_still_advance_readiness_generation() {
        let generation = Arc::new(AtomicU64::new(0));
        let wake = UiWakeSender {
            generation: Arc::clone(&generation),
            thread: thread::current(),
        };

        wake.signal();
        wake.signal();

        assert_eq!(generation.load(Ordering::Acquire), 2);
    }

    #[test]
    fn input_reader_batch_coalesces_relative_motion() {
        let mut batch = InputReaderBatchCoalescer::default();
        batch.push(event(INPUT_KIND_POINTER_MOTION, 3, -2));
        batch.push(event(INPUT_KIND_POINTER_MOTION, 4, 6));

        let mut out = [InputEvent::default(); INPUT_EVENT_BATCH];
        let events = batch.drain_into(&mut out);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[0].value0, 7);
        assert_eq!(events[0].value1, 4);
    }

    #[test]
    fn input_reader_batch_keeps_latest_absolute_position() {
        let mut batch = InputReaderBatchCoalescer::default();
        batch.push(event(INPUT_KIND_POINTER_POSITION, 10, 20));
        batch.push(event(INPUT_KIND_POINTER_POSITION, 30, 40));

        let mut out = [InputEvent::default(); INPUT_EVENT_BATCH];
        let events = batch.drain_into(&mut out);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_POSITION);
        assert_eq!(events[0].value0, 30);
        assert_eq!(events[0].value1, 40);
    }

    #[test]
    fn input_reader_batch_flushes_pointer_before_keyboard() {
        let mut batch = InputReaderBatchCoalescer::default();
        batch.push(event(INPUT_KIND_POINTER_MOTION, 1, 2));
        batch.push(keyboard_event());
        batch.push(event(INPUT_KIND_POINTER_MOTION, 3, 4));

        let mut out = [InputEvent::default(); INPUT_EVENT_BATCH];
        let events = batch.drain_into(&mut out);

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, INPUT_KIND_POINTER_MOTION);
        assert_eq!(events[1].kind, INPUT_KIND_KEYBOARD);
        assert_eq!(events[2].kind, INPUT_KIND_POINTER_MOTION);
    }

    #[test]
    fn input_reader_uses_blocking_epoll_readiness_not_probe_cadence() {
        let mut pipe_fds = [-1_i32; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        let read_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };
        let input_fds = [read_fd];
        let epoll = create_input_wait_epoll(&input_fds).expect("epoll input wait");

        let byte = [1_u8];
        assert_eq!(
            unsafe {
                libc::write(
                    write_fd.as_raw_fd(),
                    byte.as_ptr().cast::<libc::c_void>(),
                    byte.len(),
                )
            },
            1
        );
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        assert_eq!(
            unsafe { libc::epoll_wait(epoll.as_raw_fd(), &mut event, 1, 0) },
            1
        );
        let event_data = event.u64;
        let event_mask = event.events;
        assert_eq!(event_data, 1);
        assert_ne!(event_mask & libc::EPOLLIN as u32, 0);
    }
}
