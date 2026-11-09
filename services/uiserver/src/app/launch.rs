//! Launching programs from the home screen.
//!
//! A click starts one background request. The request path is bounded and
//! never ends the compositor: a burst of clicks cannot exhaust threads, a
//! program that is already being requested is not requested again, and a
//! thread that cannot be spawned drops that one request.

use std::sync::{Mutex, PoisonError};

use runtime_control::RuntimeClient;

use crate::sys::{self, diag_line};

/// Launch requests that may be in flight at once.
const MAX_LAUNCHES_IN_FLIGHT: usize = 4;

/// Tracks which programs have a launch request in flight.
pub(super) struct LaunchGate {
    in_flight: Mutex<Vec<String>>,
    capacity: usize,
}

impl LaunchGate {
    pub(super) const fn new() -> Self {
        Self::with_capacity(MAX_LAUNCHES_IN_FLIGHT)
    }

    pub(super) const fn with_capacity(capacity: usize) -> Self {
        Self {
            in_flight: Mutex::new(Vec::new()),
            capacity,
        }
    }

    /// Reserve a slot for `program`. Fails when the program is already being
    /// launched or all slots are taken.
    pub(super) fn try_begin(&self, program: &str) -> bool {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if in_flight.len() >= self.capacity || in_flight.iter().any(|id| id == program) {
            return false;
        }
        in_flight.push(program.to_owned());
        true
    }

    /// Release the slot taken by `try_begin`.
    pub(super) fn finish(&self, program: &str) {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(index) = in_flight.iter().position(|id| id == program) {
            in_flight.swap_remove(index);
        }
    }
}

static LAUNCH_GATE: LaunchGate = LaunchGate::new();

pub(super) fn launch_program_async(desktop_file_id: String) {
    if !LAUNCH_GATE.try_begin(&desktop_file_id) {
        diag_line("uiserver: launch request ignored: already launching or too many in flight");
        return;
    }
    let for_thread = desktop_file_id.clone();
    let spawned = sys::spawn_ui_thread(
        sys::UiThreadRole::Background,
        "uiserver-program-launch",
        move || {
            if let Ok(runtime) = RuntimeClient::open_default() {
                let _ = runtime.request_launch_program_new_session(for_thread.as_str());
            }
            LAUNCH_GATE.finish(for_thread.as_str());
        },
    );
    if spawned.is_err() {
        // Losing one launch is recoverable; ending the compositor is not.
        LAUNCH_GATE.finish(&desktop_file_id);
        diag_line("uiserver: launch thread could not be spawned; request dropped");
    }
}

/// Terminate requests that may be in flight at once. Console sessions are
/// capped far below this, so a request is only refused for a handle that is
/// already being terminated.
const MAX_TERMINATES_IN_FLIGHT: usize = 16;
const TERMINATE_ATTEMPTS: u32 = 3;
const TERMINATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

static TERMINATE_GATE: LaunchGate = LaunchGate::with_capacity(MAX_TERMINATES_IN_FLIGHT);

/// Ask runtimed to end a console session without blocking the UI thread. The
/// RPC may wait out its full I/O deadline, which exceeds the UI phase
/// watchdog; the window is already gone from the UI by the time this runs.
pub(super) fn terminate_session_async(session_handle: u64) {
    let key = format!("terminate:{session_handle}");
    if !TERMINATE_GATE.try_begin(&key) {
        diag_line("uiserver: terminate request ignored: already terminating or too many in flight");
        return;
    }
    let for_thread = key.clone();
    let spawned = sys::spawn_ui_thread(
        sys::UiThreadRole::Background,
        "uiserver-session-terminate",
        move || {
            let mut result = Err(libc::EIO);
            for attempt in 0..TERMINATE_ATTEMPTS {
                if attempt != 0 {
                    std::thread::sleep(TERMINATE_RETRY_DELAY);
                }
                result = RuntimeClient::open_default()
                    .and_then(|runtime| runtime.request_terminate_session(session_handle));
                if result.is_ok() {
                    break;
                }
            }
            if let Err(errno) = result {
                diag_line(format!(
                    "uiserver: terminate session {session_handle} failed errno={errno}"
                ));
            }
            TERMINATE_GATE.finish(for_thread.as_str());
        },
    );
    if spawned.is_err() {
        TERMINATE_GATE.finish(&key);
        diag_line("uiserver: terminate thread could not be spawned; request dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::{LaunchGate, MAX_LAUNCHES_IN_FLIGHT};

    #[test]
    fn a_program_already_launching_is_not_launched_again() {
        let gate = LaunchGate::new();
        assert!(gate.try_begin("shell.desktop"));
        assert!(!gate.try_begin("shell.desktop"));
        gate.finish("shell.desktop");
        assert!(
            gate.try_begin("shell.desktop"),
            "a finished launch frees its slot"
        );
    }

    #[test]
    fn launches_in_flight_are_bounded() {
        let gate = LaunchGate::new();
        for index in 0..MAX_LAUNCHES_IN_FLIGHT {
            assert!(gate.try_begin(&format!("program-{index}.desktop")));
        }
        assert!(!gate.try_begin("one-too-many.desktop"));
        gate.finish("program-0.desktop");
        assert!(gate.try_begin("one-too-many.desktop"));
    }

    #[test]
    fn finishing_an_unknown_program_is_harmless() {
        let gate = LaunchGate::new();
        gate.finish("never-started.desktop");
        assert!(gate.try_begin("a.desktop"));
    }
}
