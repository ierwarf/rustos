use std::thread;
use std::time::Instant;

use runtime_control::RuntimeClient;

use crate::app::{
    AppState, InputProcessingResult, VisualUpdate, INPUT_EVENT_BATCH, INPUT_PROCESS_BUDGET,
    MAX_INPUT_READ_BATCHES_PER_TICK,
};
use crate::profile;
use crate::sys::{read_input, InputEvent, INPUT_ACTION_NONE, INPUT_KIND_POINTER_POSITION};
use crate::wayland::WaylandCompositor;

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
    events: &mut [InputEvent; INPUT_EVENT_BATCH],
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
    for batch_index in 0..MAX_INPUT_READ_BATCHES_PER_TICK {
        let read_count = read_input(&state.input_fds, events)?;
        if read_count == 0 {
            break;
        }

        for event in &events[..read_count] {
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
