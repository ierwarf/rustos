use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use runtime_control::RuntimeClient;

use crate::app::{
    AppState, InputProcessingResult, VisualUpdate, INPUT_EVENT_BATCH, INPUT_PROCESS_BUDGET,
    MAX_INPUT_READ_BATCHES_PER_TICK,
};
use crate::profile;
use crate::sys::{
    boot_line, diag_line, read_input, require_background_thread_class, InputEvent,
    INPUT_ACTION_NONE, INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION,
};
use crate::wayland::WaylandCompositor;

const SLOW_INPUT_READ_THRESHOLD_MS: u128 = 50;
// The common wait set now exports inputd's service-owned readiness generation.
// Keep this bounded compatibility cadence until the dedicated reader itself is
// moved onto that ABI; its shared wake channel also receives the Wayland
// backend's aggregate-epoll wake and therefore lets the UI loop wait on input,
// client traffic, and its runtime deadline without polling clients every 16 ms.
const INPUT_READER_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(4);
const INPUT_READER_QUEUE_CAPACITY: usize = INPUT_EVENT_BATCH * 64;
const INPUT_READER_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const INPUT_READER_PANIC_THRESHOLD_MS: u64 = 3_000;
const MAX_INITIAL_INPUT_DROP_LOGS: u64 = 8;
const MAX_WAYLAND_POINTER_FLUSH_EVENTS: u64 = 16;

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

fn next_input_probe_deadline(previous: Instant, now: Instant) -> Instant {
    let scheduled = previous
        .checked_add(INPUT_READER_PROBE_INTERVAL)
        .unwrap_or(now);
    if scheduled > now {
        scheduled
    } else {
        now.checked_add(INPUT_READER_PROBE_INTERVAL).unwrap_or(now)
    }
}

fn wait_for_next_input_probe(stats: &InputReaderStats, deadline: &mut Instant) {
    let now = Instant::now();
    *deadline = next_input_probe_deadline(*deadline, now);
    stats
        .wait_started_ms
        .store(stats.elapsed_ms(), Ordering::Release);
    stats.wait_active.store(true, Ordering::Release);
    stats.wait_attempts.fetch_add(1, Ordering::Relaxed);
    if let Some(duration) = deadline.checked_duration_since(now) {
        if !duration.is_zero() {
            thread::sleep(duration);
        }
    }
    stats.wait_active.store(false, Ordering::Release);
    stats.completed_waits.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn start_input_reader(input_fds: Vec<OwnedFd>) -> InputReader {
    let (sender, receiver) = mpsc::sync_channel::<InputEvent>(INPUT_READER_QUEUE_CAPACITY);
    let wake_generation = Arc::new(AtomicU64::new(0));
    let shared_wake_sender = UiWakeSender {
        generation: Arc::clone(&wake_generation),
        thread: thread::current(),
    };
    let reader_wake_sender = shared_wake_sender.clone();
    let stats = Arc::new(InputReaderStats::new());
    let reader_stats = Arc::clone(&stats);
    thread::Builder::new()
        .name(String::from("uiserver-input-reader"))
        .spawn(move || {
            boot_line("uiserver: input reader worker enter");
            let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
            let mut first_read = true;
            let mut next_probe_deadline = Instant::now();
            loop {
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
                        wait_for_next_input_probe(&reader_stats, &mut next_probe_deadline);
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
                    wait_for_next_input_probe(&reader_stats, &mut next_probe_deadline);
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
                        if drop_count <= MAX_INITIAL_INPUT_DROP_LOGS
                            || drop_count.is_power_of_two()
                        {
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
    thread::Builder::new()
        .name(String::from("uiserver-input-watchdog"))
        .spawn(move || {
            require_background_thread_class();
            loop {
            thread::sleep(INPUT_READER_WATCHDOG_INTERVAL);
            let snapshot = watchdog_stats.snapshot();
            if snapshot.wait_active && snapshot.wait_elapsed_ms >= INPUT_READER_PANIC_THRESHOLD_MS {
                diag_line(format!(
                    "uiserver input watchdog panic: bounded probe sleep stalled elapsed_ms={} wait_attempts={} completed_waits={} read_attempts={} completed_reads={} raw_events={} delivered_events={} queue_drops={} slow_reads={} errors={}",
                    snapshot.wait_elapsed_ms,
                    snapshot.wait_attempts,
                    snapshot.completed_waits,
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
            if snapshot.read_active && snapshot.read_elapsed_ms >= INPUT_READER_PANIC_THRESHOLD_MS {
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

    InputReader {
        receiver,
        wake_sender: shared_wake_sender,
        wake_generation,
        observed_wake_generation: AtomicU64::new(0),
        stats,
    }
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
        next_input_probe_deadline, InputReader, InputReaderBatchCoalescer, InputReaderStats,
        UiWakeSender, INPUT_EVENT_BATCH, INPUT_READER_PROBE_INTERVAL,
    };
    use crate::sys::{
        InputEvent, INPUT_ACTION_NONE, INPUT_ACTION_PRESSED, INPUT_KIND_KEYBOARD,
        INPUT_KIND_POINTER_MOTION, INPUT_KIND_POINTER_POSITION,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

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
    fn input_reader_probe_deadline_does_not_accumulate_missed_slots() {
        let base = std::time::Instant::now();
        let missed = base + INPUT_READER_PROBE_INTERVAL * 4;

        assert_eq!(
            next_input_probe_deadline(base, missed),
            missed + INPUT_READER_PROBE_INTERVAL
        );
    }

    #[test]
    fn input_reader_probe_deadline_preserves_the_next_future_slot() {
        let base = std::time::Instant::now();
        let now = base + INPUT_READER_PROBE_INTERVAL / 2;

        assert_eq!(
            next_input_probe_deadline(base, now),
            base + INPUT_READER_PROBE_INTERVAL
        );
    }
}
