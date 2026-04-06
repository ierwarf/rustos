mod app;
mod canvas;
mod font;
mod input_loop;
mod render;
mod runtime_sync;
mod simd;
mod sys;
mod terminal;
mod wayland;

use std::os::fd::AsRawFd;
use std::time::Instant;

use app::{
    AppState, VisualUpdate, CONSOLE_POLL_SLEEP, CURSOR_BLINK_INTERVAL, IDLE_SLEEP,
    INPUT_EVENT_BATCH, RUNTIME_POLL_SLEEP, TARGET_FRAME_INTERVAL,
};
use render::{render_frame, render_rect};
use runtime_sync::{refresh_runtime_state, RuntimeState};
use sys::{open_runtime, raw_stderr_line, InputEvent};
use wayland::WaylandCompositor;

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        raw_stderr_line(&format!("uiserver panic: {info}"));
    }));
}

fn run() -> Result<(), i32> {
    let mut state = AppState::initialize()?;
    let mut wayland = WaylandCompositor::initialize(state.display.width, state.display.height);
    let mut runtime_state = RuntimeState::default();
    let runtime_fd = open_runtime().map_err(|_| {
        raw_stderr_line("uiserver: open_runtime failed");
        19
    })?;
    let _ = refresh_runtime_state(&runtime_fd, &mut runtime_state);
    let _ = state.apply_runtime_state(&mut runtime_state);
    let _ = state.refresh_console_windows();
    let mut events = [InputEvent::default(); INPUT_EVENT_BATCH];
    let mut next_runtime_poll = Instant::now() + RUNTIME_POLL_SLEEP;
    let mut next_console_poll = Instant::now() + CONSOLE_POLL_SLEEP;
    let mut next_present_at = Instant::now() + TARGET_FRAME_INTERVAL;
    let mut next_cursor_blink = Instant::now() + CURSOR_BLINK_INTERVAL;
    let mut pending_update = VisualUpdate::default();

    render_frame(&mut state);
    state.present()?;

    loop {
        let now = Instant::now();
        if now >= next_runtime_poll {
            let _ = refresh_runtime_state(&runtime_fd, &mut runtime_state);
            next_runtime_poll = now + RUNTIME_POLL_SLEEP;
        }

        if state.apply_runtime_state(&mut runtime_state) {
            pending_update.request_full();
        }
        if let Some(compositor) = wayland.as_mut() {
            if compositor.tick() && state.sync_wayland_windows(compositor.window_snapshots()) {
                pending_update.request_full();
            }
        }
        if now >= next_console_poll {
            if state.refresh_console_windows()? {
                pending_update.request_full();
            }
            next_console_poll = now + CONSOLE_POLL_SLEEP;
        }

        let input = input_loop::process_pending_input(
            &mut state,
            wayland.as_mut(),
            runtime_fd.as_raw_fd(),
            &mut events,
        )?;
        pending_update.absorb(VisualUpdate {
            needs_full_redraw: input.needs_full_redraw,
            partial_redraw_rect: input.partial_redraw_rect,
            secondary_partial_redraw_rect: input.secondary_partial_redraw_rect,
        });

        let now = Instant::now();
        if now >= next_cursor_blink {
            if let Some(rect) = state.toggle_focused_terminal_cursor() {
                pending_update.absorb(VisualUpdate::partial(rect));
            }
            next_cursor_blink = now + CURSOR_BLINK_INTERVAL;
        }
        pending_update.promote_large_partial(state.surface.width, state.surface.height);

        let now = Instant::now();
        if pending_update.is_empty() {
            let sleep_deadline = next_runtime_poll
                .min(next_console_poll)
                .min(now + IDLE_SLEEP);
            input_loop::sleep_until(sleep_deadline);
            continue;
        }

        if now < next_present_at {
            if input.backlog_remaining {
                continue;
            }
            let sleep_deadline = next_present_at
                .min(next_runtime_poll)
                .min(next_console_poll);
            input_loop::sleep_until(sleep_deadline);
            continue;
        }

        if pending_update.needs_full_redraw {
            render_frame(&mut state);
            match state.present() {
                Ok(()) => {}
                Err(err) if state.recover_if_stale_surface_error(err)? => {
                    pending_update.request_full();
                    next_present_at = Instant::now();
                    continue;
                }
                Err(err) => return Err(err),
            }
        } else if !pending_update.partial_redraw_rect.is_empty() {
            render_rect(&mut state, pending_update.partial_redraw_rect);
            match state.present_rect(pending_update.partial_redraw_rect) {
                Ok(()) => {}
                Err(err) if state.recover_if_stale_surface_error(err)? => {
                    pending_update.request_full();
                    next_present_at = Instant::now();
                    continue;
                }
                Err(err) => return Err(err),
            }
            if !pending_update.secondary_partial_redraw_rect.is_empty() {
                render_rect(&mut state, pending_update.secondary_partial_redraw_rect);
                match state.present_rect(pending_update.secondary_partial_redraw_rect) {
                    Ok(()) => {}
                    Err(err) if state.recover_if_stale_surface_error(err)? => {
                        pending_update.request_full();
                        next_present_at = Instant::now();
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        pending_update.clear();
        next_present_at = Instant::now() + TARGET_FRAME_INTERVAL;
    }
}

fn main() {
    raw_stderr_line("uiserver: main enter");
    install_panic_hook();
    raw_stderr_line("uiserver: panic hook installed");
    let exit_code = match run() {
        Ok(()) => 0,
        Err(code) => code,
    };
    if exit_code != 0 {
        raw_stderr_line("uiserver: exiting with nonzero status");
    }
    std::process::exit(exit_code);
}
