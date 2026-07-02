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
    diag_line, read_input, InputEvent, INPUT_ACTION_NONE, INPUT_KIND_POINTER_POSITION,
};
use crate::wayland::WaylandCompositor;

const SLOW_INPUT_READ_THRESHOLD_MS: u128 = 50;
const INPUT_READER_IDLE_SLEEP: std::time::Duration = std::time::Duration::from_millis(1);
const INPUT_READER_QUEUE_CAPACITY: usize = INPUT_EVENT_BATCH * 64;
const INPUT_READER_WATCHDOG_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const INPUT_READER_PANIC_THRESHOLD_MS: u64 = 3_000;
const MAX_INITIAL_INPUT_DROP_LOGS: u64 = 8;

pub(crate) struct InputReader {
    receiver: Receiver<InputEvent>,
    stats: Arc<InputReaderStats>,
}

struct InputReaderStats {
    started: Instant,
    read_active: AtomicBool,
    read_started_ms: AtomicU64,
    read_attempts: AtomicU64,
    completed_reads: AtomicU64,
    delivered_events: AtomicU64,
    queue_drops: AtomicU64,
    slow_reads: AtomicU64,
    errors: AtomicU64,
}

pub(crate) struct InputReaderSnapshot {
    pub(crate) read_active: bool,
    pub(crate) read_elapsed_ms: u64,
    pub(crate) read_attempts: u64,
    pub(crate) completed_reads: u64,
    pub(crate) delivered_events: u64,
    pub(crate) queue_drops: u64,
    pub(crate) slow_reads: u64,
    pub(crate) errors: u64,
}

impl InputReader {
    fn try_recv(&self) -> Result<InputEvent, TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) fn snapshot(&self) -> InputReaderSnapshot {
        self.stats.snapshot()
    }
}

impl InputReaderStats {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            read_active: AtomicBool::new(false),
            read_started_ms: AtomicU64::new(0),
            read_attempts: AtomicU64::new(0),
            completed_reads: AtomicU64::new(0),
            delivered_events: AtomicU64::new(0),
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
        let read_active = self.read_active.load(Ordering::Acquire);
        let read_started_ms = self.read_started_ms.load(Ordering::Acquire);
        InputReaderSnapshot {
            read_active,
            read_elapsed_ms: if read_active {
                now_ms.saturating_sub(read_started_ms)
            } else {
                0
            },
            read_attempts: self.read_attempts.load(Ordering::Acquire),
            completed_reads: self.completed_reads.load(Ordering::Acquire),
            delivered_events: self.delivered_events.load(Ordering::Acquire),
            queue_drops: self.queue_drops.load(Ordering::Acquire),
            slow_reads: self.slow_reads.load(Ordering::Acquire),
            errors: self.errors.load(Ordering::Acquire),
        }
    }
}

pub(crate) fn start_input_reader(input_fds: Vec<OwnedFd>) -> InputReader {
    let (sender, receiver) = mpsc::sync_channel::<InputEvent>(INPUT_READER_QUEUE_CAPACITY);
    let stats = Arc::new(InputReaderStats::new());
    let reader_stats = Arc::clone(&stats);
    thread::Builder::new()
        .name(String::from("uiserver-input-reader"))
        .spawn(move || {
            let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
            loop {
                reader_stats
                    .read_started_ms
                    .store(reader_stats.elapsed_ms(), Ordering::Release);
                reader_stats.read_active.store(true, Ordering::Release);
                reader_stats.read_attempts.fetch_add(1, Ordering::Relaxed);
                let read_started = Instant::now();
                let result = read_input(&input_fds, &mut events);
                reader_stats.read_active.store(false, Ordering::Release);
                reader_stats.completed_reads.fetch_add(1, Ordering::Relaxed);
                let read_count = match result {
                    Ok(count) => count,
                    Err(errno) => {
                        reader_stats.errors.fetch_add(1, Ordering::Relaxed);
                        diag_line(format!("uiserver: input reader failed errno={errno}"));
                        thread::sleep(INPUT_READER_IDLE_SLEEP);
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
                    thread::sleep(INPUT_READER_IDLE_SLEEP);
                    continue;
                }
                let mut sent = 0_u64;
                for event in events[..read_count].iter().copied() {
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
            }
        })
        .unwrap_or_else(|_| {
            diag_line("uiserver input watchdog panic: failed to spawn input reader");
            std::process::exit(134);
        });

    let watchdog_stats = Arc::clone(&stats);
    thread::Builder::new()
        .name(String::from("uiserver-input-watchdog"))
        .spawn(move || loop {
            thread::sleep(INPUT_READER_WATCHDOG_INTERVAL);
            let snapshot = watchdog_stats.snapshot();
            if snapshot.read_active && snapshot.read_elapsed_ms >= INPUT_READER_PANIC_THRESHOLD_MS {
                diag_line(format!(
                    "uiserver input watchdog panic: read_input blocked elapsed_ms={} read_attempts={} completed_reads={} delivered_events={} queue_drops={} slow_reads={} errors={}",
                    snapshot.read_elapsed_ms,
                    snapshot.read_attempts,
                    snapshot.completed_reads,
                    snapshot.delivered_events,
                    snapshot.queue_drops,
                    snapshot.slow_reads,
                    snapshot.errors,
                ));
                std::process::exit(134);
            }
        })
        .unwrap_or_else(|_| {
            diag_line("uiserver input watchdog panic: failed to spawn input watchdog");
            std::process::exit(134);
        });

    InputReader { receiver, stats }
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
    let max_x = state.display.width.saturating_sub(1);
    let max_y = state.display.height.saturating_sub(1);
    let mut pending_pointer = PendingPointerPosition::new(state.cursor_x, state.cursor_y);

    let mut wayland = wayland;
    let mut batch = [InputEvent::default(); INPUT_EVENT_BATCH];
    for batch_index in 0..MAX_INPUT_READ_BATCHES_PER_TICK {
        let mut read_count = 0usize;
        while read_count < batch.len() {
            match events.try_recv() {
                Ok(event) => {
                    batch[read_count] = event;
                    read_count += 1;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        if read_count == 0 {
            break;
        }
        let read_filled_batch = read_count == batch.len();

        for event in &batch[..read_count] {
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

            flush_pending_pointer(
                state,
                runtime,
                wayland.as_deref_mut(),
                &mut pending_pointer,
                &mut visual_update,
            )?;

            let redraw = state.handle_input_event(runtime, event, wayland.as_deref_mut())?;
            visual_update.absorb(redraw);
        }

        let hit_batch_limit = batch_index + 1 == MAX_INPUT_READ_BATCHES_PER_TICK;
        let hit_time_budget = started_at.elapsed() >= INPUT_PROCESS_BUDGET;
        if hit_batch_limit || hit_time_budget {
            backlog_remaining = true;
            break;
        }
        if !read_filled_batch {
            break;
        }
    }

    flush_pending_pointer(
        state,
        runtime,
        wayland.as_deref_mut(),
        &mut pending_pointer,
        &mut visual_update,
    )?;

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
    })
}

fn flush_pending_pointer(
    state: &mut AppState,
    runtime: &RuntimeClient,
    wayland: Option<&mut WaylandCompositor>,
    pending_pointer: &mut PendingPointerPosition,
    visual_update: &mut VisualUpdate,
) -> Result<(), i32> {
    let Some(event) = pending_pointer.take_event() else {
        return Ok(());
    };

    let redraw = state.handle_input_event(runtime, &event, wayland)?;
    visual_update.absorb(redraw);
    Ok(())
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
