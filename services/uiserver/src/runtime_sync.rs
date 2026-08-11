use std::string::String;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use runtime_control::protocol::RUNTIME_WATCH_MAX_WAIT_MS;
use runtime_control::{
    decode_c_string, RuntimeClient, RuntimeRunningProgram, RUNNING_PROGRAMS_DIGEST_UNKNOWN,
};

use crate::app::{HIDDEN_RUNTIME_PROGRAM_TITLES, MAX_RUNNING_PROGRAMS};
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

pub(crate) fn start_runtime_sync(runtime: RuntimeClient) -> RuntimeSyncHandle {
    let shared = Arc::new(Mutex::new(SharedRuntimeState::default()));
    let worker_shared = Arc::clone(&shared);
    spawn_ui_thread(
        UiThreadRole::Background,
        "uiserver-runtime-sync",
        move || runtime_sync_worker(runtime, worker_shared),
    )
    .unwrap_or_else(|_| std::process::exit(134));
    RuntimeSyncHandle { shared }
}

pub(crate) fn refresh_runtime_state(
    sync: &RuntimeSyncHandle,
    runtime_state: &mut RuntimeState,
) -> Result<bool, i32> {
    let Ok(snapshot) = sync.shared.try_lock().map(|state| state.clone()) else {
        return Ok(false);
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

        let mut shared = shared.lock().unwrap();
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
