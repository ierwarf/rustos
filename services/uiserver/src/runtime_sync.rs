use std::string::String;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::thread;
use std::time::Duration;

use runtime_control::protocol::RUNTIME_WATCH_MAX_WAIT_MS;
use runtime_control::{
    decode_c_string, RuntimeClient, RuntimeRunningProgram, RUNNING_PROGRAMS_DIGEST_UNKNOWN,
};

use crate::app::{HIDDEN_RUNTIME_PROGRAM_TITLES, MAX_RUNNING_PROGRAMS};
use crate::startup::Dependency;
use crate::sys::{spawn_ui_thread, UiThreadRole};

/// Longest this worker asks runtimed to hold a reply while the running set is
/// unchanged. It is a re-arm cadence, not a refresh interval: a launch or an
/// exit answers the watch as soon as runtimed sees it, so the taskbar reacts to
/// a program appearing in about one broker pass rather than within a quarter of
/// a second.
const RUNTIME_WATCH_WAIT: Duration = Duration::from_millis(RUNTIME_WATCH_MAX_WAIT_MS as u64);

/// Back-off after a failed watch, which is almost always runtimed not being
/// reachable yet. Without it a refused connect would spin this thread.
const RUNTIME_SYNC_RETRY_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub(crate) struct RuntimeState {
    pub(crate) running_programs: [RuntimeRunningProgram; MAX_RUNNING_PROGRAMS],
    pub(crate) running_program_count: usize,
    pub(crate) generation: u64,
    pub(crate) dirty: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            running_programs: [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS],
            running_program_count: 0,
            generation: 0,
            dirty: false,
        }
    }
}

#[derive(Clone, Default)]
struct SharedRuntimeState {
    running_programs: [RuntimeRunningProgram; MAX_RUNNING_PROGRAMS],
    running_program_count: usize,
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct RuntimeSyncHandle {
    shared: Arc<Mutex<SharedRuntimeState>>,
}

/// Start the runtime link: one worker that owns every interaction with
/// `runtimed` that can block.
///
/// Readiness toward the broker is a *level*, not an edge. `runtimed` only
/// launches the first desktop client after it has been told the UI is ready,
/// so a refused connect, a short write or a lost reply must not be forgotten.
/// The worker drives the `Runtime` dependency (`crate::startup`) until
/// `runtimed` has *acknowledged* `OP_NOTIFY_READY` (runtime-control wire v2: the
/// reply is sent after the broker recorded the edge), under that dependency's
/// bounded backoff, and only then enters the running-set watch. The operation is
/// idempotent on the broker, so a retry after a lost ack is safe.
pub(crate) fn start_runtime_sync() -> RuntimeSyncHandle {
    let shared = Arc::new(Mutex::new(SharedRuntimeState::default()));
    let worker_shared = Arc::clone(&shared);
    spawn_ui_thread(UiThreadRole::Background, "uiserver-runtime-sync", move || {
        let startup = crate::startup::global();
        let runtime = match startup.drive(Dependency::Runtime, notify_error_is_retryable, || {
            let runtime = RuntimeClient::open_default()?;
            runtime.notify_ui_ready()?;
            Ok(runtime)
        }) {
            Ok(runtime) => runtime,
            Err(errno) => {
                crate::sys::diag_line(format!("uiserver: ui ready notify failed errno={errno}"));
                return;
            }
        };
        startup.succeed(Dependency::Runtime);
        crate::sys::diag_line("uiserver: ui ready notified");
        runtime_sync_worker(runtime, worker_shared)
    })
    .unwrap_or_else(|_| std::process::exit(134));
    RuntimeSyncHandle { shared }
}

/// Only a malformed, unsupported, unauthorised or version-mismatched request is
/// permanent (`EPROTO`: a mixed-version build cannot converge by retrying);
/// every transport error (refused connect, `EAGAIN`, `ETIMEDOUT`, a missing ack,
/// a not-yet-bound socket) is the broker not being reachable *yet*.
fn notify_error_is_retryable(errno: i32) -> bool {
    !matches!(
        errno,
        libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM | libc::EACCES | libc::EPROTO
    )
}

pub(crate) fn refresh_runtime_state(
    sync: &RuntimeSyncHandle,
    runtime_state: &mut RuntimeState,
) -> Result<bool, i32> {
    // The shared state is plain data replaced whole under the lock, so a
    // poisoned guard still holds a consistent snapshot; refusing it would
    // freeze the taskbar for the life of the process.
    let snapshot = match sync.shared.try_lock() {
        Ok(state) => state.clone(),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().clone(),
        Err(TryLockError::WouldBlock) => return Ok(false),
    };
    if snapshot.generation == runtime_state.generation {
        return Ok(false);
    }

    runtime_state.running_programs = snapshot.running_programs;
    runtime_state.running_program_count = snapshot.running_program_count;
    runtime_state.generation = snapshot.generation;
    runtime_state.dirty = true;
    Ok(true)
}

fn runtime_sync_worker(runtime: RuntimeClient, shared: Arc<Mutex<SharedRuntimeState>>) {
    // The digest runtimed last answered with. Handing it back is what lets the
    // broker decide there is nothing to say and hold the reply, so this thread
    // spends its life blocked on a change instead of asking for one.
    let mut observed_digest = RUNNING_PROGRAMS_DIGEST_UNKNOWN;
    loop {
        let Ok((snapshot, digest)) =
            runtime.watch_running_programs(observed_digest, RUNTIME_WATCH_WAIT)
        else {
            thread::sleep(RUNTIME_SYNC_RETRY_BACKOFF);
            continue;
        };
        // A re-arm returns the same digest. Publishing nothing then keeps the
        // UI generation an edge count rather than a wake count.
        if digest == observed_digest {
            continue;
        }
        observed_digest = digest;

        let mut running_programs = [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS];
        let running_count = snapshot.len().min(MAX_RUNNING_PROGRAMS);
        running_programs[..running_count].copy_from_slice(&snapshot[..running_count]);

        let mut shared = shared.lock().unwrap_or_else(PoisonError::into_inner);
        // The digest covers runtimed's whole set; this window covers only the
        // part the UI can show. A change past the visible cap is real to the
        // broker and invisible here, so it must not become a repaint.
        if running_count == shared.running_program_count
            && shared.running_programs[..running_count] == running_programs[..running_count]
        {
            continue;
        }
        shared.running_programs = running_programs;
        shared.running_program_count = running_count;
        shared.generation = shared.generation.wrapping_add(1).max(1);
    }
}

pub(crate) fn runtime_program_title(program: &RuntimeRunningProgram) -> String {
    runtime_program_display_name(program).into_owned()
}

pub(crate) fn runtime_program_is_hidden(program: &RuntimeRunningProgram) -> bool {
    let title = runtime_program_display_name(program);
    runtime_title_is_hidden(title.as_ref())
}

pub(crate) fn runtime_title_is_hidden(title: &str) -> bool {
    HIDDEN_RUNTIME_PROGRAM_TITLES.contains(&title)
}

pub(crate) fn runtime_program_display_name(
    program: &RuntimeRunningProgram,
) -> std::borrow::Cow<'_, str> {
    std::borrow::Cow::Owned(decode_c_string(&program.display_name))
}

#[cfg(test)]
mod tests {
    use super::notify_error_is_retryable;

    #[test]
    fn only_permanent_request_errors_stop_the_ready_notification() {
        for transient in [libc::ECONNREFUSED, libc::ENOENT, libc::EAGAIN, libc::ETIMEDOUT, libc::EPIPE] {
            assert!(notify_error_is_retryable(transient), "errno {transient}");
        }
        for permanent in [libc::EINVAL, libc::EOPNOTSUPP, libc::EPERM, libc::EACCES, libc::EPROTO] {
            assert!(!notify_error_is_retryable(permanent), "errno {permanent}");
        }
    }
}
