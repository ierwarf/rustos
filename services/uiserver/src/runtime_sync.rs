use std::string::String;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use runtime_control::{decode_c_string, RuntimeClient, RuntimeRunningProgram};

use crate::app::{HIDDEN_RUNTIME_PROGRAM_TITLES, MAX_RUNNING_PROGRAMS};

const RUNTIME_SYNC_INTERVAL: Duration = Duration::from_millis(250);

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
    thread::spawn(move || runtime_sync_worker(runtime, worker_shared));
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
    loop {
        if let Ok(snapshot) = runtime.snapshot_running_programs() {
            let mut running_programs = [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS];
            let running_count = snapshot.len().min(MAX_RUNNING_PROGRAMS);
            running_programs[..running_count].copy_from_slice(&snapshot[..running_count]);

            let mut shared = shared.lock().unwrap();
            let changed = running_count != shared.running_program_count
                || shared.running_programs[..running_count] != running_programs[..running_count];
            if changed {
                shared.running_programs = running_programs;
                shared.running_program_count = running_count;
                shared.generation = shared.generation.wrapping_add(1).max(1);
            }
        }

        thread::sleep(RUNTIME_SYNC_INTERVAL);
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
