use std::os::fd::{AsRawFd, OwnedFd};
use std::string::String;

use crate::app::{HIDDEN_RUNTIME_PROGRAM_TITLES, MAX_REGISTERED_PROGRAMS, MAX_RUNNING_PROGRAMS};
use crate::sys::{
    runtime_generation, runtime_snapshot_programs, runtime_snapshot_running_programs,
    RuntimeProgram, RuntimeRunningProgram,
};

#[derive(Clone)]
pub(crate) struct RuntimeState {
    pub(crate) generation: u64,
    pub(crate) running_programs: [RuntimeRunningProgram; MAX_RUNNING_PROGRAMS],
    pub(crate) running_program_count: usize,
    pub(crate) registered_programs: [RuntimeProgram; MAX_REGISTERED_PROGRAMS],
    pub(crate) registered_program_count: usize,
    pub(crate) dirty: bool,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            generation: 0,
            running_programs: [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS],
            running_program_count: 0,
            registered_programs: [RuntimeProgram::default(); MAX_REGISTERED_PROGRAMS],
            registered_program_count: 0,
            dirty: false,
        }
    }
}

pub(crate) fn refresh_runtime_state(
    runtime_fd: &OwnedFd,
    runtime_state: &mut RuntimeState,
) -> Result<bool, i32> {
    let generation = runtime_generation(runtime_fd.as_raw_fd())?;
    let mut registered_programs = [RuntimeProgram::default(); MAX_REGISTERED_PROGRAMS];
    let registered_count =
        runtime_snapshot_programs(runtime_fd.as_raw_fd(), &mut registered_programs)?
            .min(MAX_REGISTERED_PROGRAMS);
    let mut running_programs = [RuntimeRunningProgram::default(); MAX_RUNNING_PROGRAMS];
    let running_count =
        runtime_snapshot_running_programs(runtime_fd.as_raw_fd(), &mut running_programs)?
            .min(MAX_RUNNING_PROGRAMS);
    let running_changed = generation != runtime_state.generation
        || running_count != runtime_state.running_program_count
        || runtime_state.running_programs[..running_count] != running_programs[..running_count];
    let registered_changed = registered_count != runtime_state.registered_program_count
        || runtime_state.registered_programs[..registered_count]
            != registered_programs[..registered_count];
    if !running_changed && !registered_changed {
        return Ok(false);
    }

    runtime_state.generation = generation;
    runtime_state.running_programs = running_programs;
    runtime_state.running_program_count = running_count;
    runtime_state.registered_programs = registered_programs;
    runtime_state.registered_program_count = registered_count;
    runtime_state.dirty = true;
    Ok(true)
}

pub(crate) fn runtime_program_title(program: &RuntimeRunningProgram) -> String {
    runtime_program_display_name(program).into_owned()
}

pub(crate) fn runtime_program_is_hidden(program: &RuntimeRunningProgram) -> bool {
    let title = runtime_program_display_name(program);
    runtime_title_is_hidden(title.as_ref())
}

pub(crate) fn runtime_title_is_hidden(title: &str) -> bool {
    HIDDEN_RUNTIME_PROGRAM_TITLES
        .iter()
        .any(|hidden| *hidden == title)
}

pub(crate) fn runtime_program_display_name(
    program: &RuntimeRunningProgram,
) -> std::borrow::Cow<'_, str> {
    let end = program
        .display_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(program.display_name.len());
    let bytes = &program.display_name[..end];
    String::from_utf8_lossy(bytes)
}

pub(crate) fn runtime_program_name(program: &RuntimeProgram) -> std::borrow::Cow<'_, str> {
    let end = program
        .display_name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(program.display_name.len());
    let bytes = &program.display_name[..end];
    String::from_utf8_lossy(bytes)
}
