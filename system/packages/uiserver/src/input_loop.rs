use std::os::fd::{AsRawFd, RawFd};
use std::thread;
use std::time::Instant;

use crate::app::{
    AppState, InputProcessingResult, INPUT_EVENT_BATCH, INPUT_PROCESS_BUDGET,
    MAX_INPUT_READ_BATCHES_PER_TICK,
};
use crate::canvas;
use crate::sys::{read_input, InputEvent};

pub(crate) fn process_pending_input(
    state: &mut AppState,
    runtime_fd: RawFd,
    events: &mut [InputEvent; INPUT_EVENT_BATCH],
) -> Result<InputProcessingResult, i32> {
    let previous_cursor_x = state.cursor_x;
    let previous_cursor_y = state.cursor_y;
    let mut needs_full_redraw = false;
    let mut partial_redraw_rect = canvas::Rect::empty();
    let mut secondary_partial_redraw_rect = canvas::Rect::empty();
    let mut backlog_remaining = false;
    let started_at = Instant::now();

    for batch_index in 0..MAX_INPUT_READ_BATCHES_PER_TICK {
        let read_count = read_input(state.input_fd.as_raw_fd(), events)?;
        if read_count == 0 {
            break;
        }

        for event in &events[..read_count] {
            let redraw = state.handle_input_event(runtime_fd, event)?;
            needs_full_redraw |= redraw.needs_full_redraw;
            partial_redraw_rect = partial_redraw_rect.union(redraw.partial_redraw_rect);
        }

        let hit_batch_limit = batch_index + 1 == MAX_INPUT_READ_BATCHES_PER_TICK;
        let hit_time_budget = started_at.elapsed() >= INPUT_PROCESS_BUDGET;
        if hit_batch_limit || hit_time_budget {
            backlog_remaining = true;
            break;
        }
    }

    if state.cursor_x != previous_cursor_x || state.cursor_y != previous_cursor_y {
        let previous_rect = canvas::cursor_dirty_rect(
            previous_cursor_x,
            previous_cursor_y,
            state.surface.width,
            state.surface.height,
        );
        let current_rect = canvas::cursor_dirty_rect(
            state.cursor_x,
            state.cursor_y,
            state.surface.width,
            state.surface.height,
        );
        if partial_redraw_rect.is_empty() {
            partial_redraw_rect = previous_rect;
            secondary_partial_redraw_rect = current_rect;
        } else {
            partial_redraw_rect = partial_redraw_rect.union(previous_rect).union(current_rect);
        }
    }

    Ok(InputProcessingResult {
        needs_full_redraw,
        partial_redraw_rect,
        secondary_partial_redraw_rect,
        backlog_remaining,
    })
}

pub(crate) fn sleep_until(deadline: Instant) {
    if let Some(duration) = deadline.checked_duration_since(Instant::now()) {
        if !duration.is_zero() {
            thread::sleep(duration);
        }
    }
}
