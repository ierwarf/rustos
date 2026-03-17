use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::console;
use crate::debug;
use crate::multitask;
use crate::ring::RingBuffer;
use crate::session::{
    ConsoleSessionId, MAX_CONSOLE_SESSIONS, active_console_session_count, active_console_sessions,
    allocate_console_session, ensure_console_session, release_console_session,
};
use crate::tty;
use crate::user::console_host::{self, ExecutableImage};

const DESKTOP_REQUEST_CAPACITY: usize = 32;

static RUNTIME: Mutex<DesktopRuntimeState> = Mutex::new(DesktopRuntimeState::new());
static RUNTIME_PRESENTATION_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DesktopProgramId(usize);

impl DesktopProgramId {
    pub const fn index(self) -> usize {
        self.0
    }

    pub const fn from_index(index: usize) -> Self {
        Self(index)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopLaunchTarget {
    Session(ConsoleSessionId),
    FirstAvailableSession,
    AllSessions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopTerminateTarget {
    Session(ConsoleSessionId),
    ProcessId(u64),
    AllSessions,
}

#[derive(Clone, Copy)]
pub struct DesktopProgramRegistration {
    pub display_name: &'static str,
    pub image: ExecutableImage,
    pub exec_path: Option<&'static str>,
    pub weight_micros: u64,
    pub logical_admin: bool,
    pub argv: &'static [&'static str],
    pub env: &'static [&'static str],
}

impl DesktopProgramRegistration {
    pub const fn new(
        display_name: &'static str,
        image: ExecutableImage,
        weight_micros: u64,
    ) -> Self {
        Self {
            display_name,
            image,
            exec_path: None,
            weight_micros,
            logical_admin: false,
            argv: &[],
            env: &[],
        }
    }

    pub const fn with_logical_admin(mut self, logical_admin: bool) -> Self {
        self.logical_admin = logical_admin;
        self
    }

    pub const fn with_exec_path(mut self, exec_path: &'static str) -> Self {
        self.exec_path = Some(exec_path);
        self
    }

    pub const fn with_args(mut self, argv: &'static [&'static str]) -> Self {
        self.argv = argv;
        self
    }

    pub const fn with_env(mut self, env: &'static [&'static str]) -> Self {
        self.env = env;
        self
    }
}

#[derive(Clone, Copy)]
pub struct DesktopProgramInfo {
    pub id: DesktopProgramId,
    pub display_name: &'static str,
    pub exec_path: &'static str,
    pub weight_micros: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DesktopRunningProgramInfo {
    pub pid: u64,
    pub program_id: DesktopProgramId,
    pub session: ConsoleSessionId,
    pub display_name: &'static str,
}

#[derive(Debug)]
pub enum DesktopRuntimeError {
    AlreadyBootstrapped,
    InvalidProgramWeight {
        weight_micros: u64,
    },
    Load {
        path: &'static str,
        fallback_path: Option<&'static str>,
        error: fatfs::Error<crate::fat::DiskIoError>,
    },
    ProgramNotFound {
        program_id: DesktopProgramId,
    },
    RequestQueueFull,
    NoAvailableSession,
    SessionBusy {
        session: ConsoleSessionId,
        pid: Option<u64>,
    },
}

impl DesktopRuntimeError {
    pub fn summary(&self) -> &'static str {
        match self {
            Self::AlreadyBootstrapped => "desktop runtime already bootstrapped",
            Self::InvalidProgramWeight { .. } => {
                "desktop program weight is outside scheduler limits"
            }
            Self::Load { .. } => "failed to load desktop program image",
            Self::ProgramNotFound { .. } => "desktop program is not registered",
            Self::RequestQueueFull => "desktop runtime request queue is full",
            Self::NoAvailableSession => "no idle console session is available",
            Self::SessionBusy { .. } => "target console session is already busy",
        }
    }

    pub fn log_debug_details(&self) {
        match self {
            Self::AlreadyBootstrapped => {
                debug::println!("desktop runtime bootstrap requested more than once");
            }
            Self::InvalidProgramWeight { weight_micros } => {
                debug::println!(
                    "desktop runtime rejected program weight outside scheduler limits: {}",
                    weight_micros,
                );
            }
            Self::Load {
                path,
                fallback_path,
                error,
            } => {
                if let Some(fallback_path) = fallback_path {
                    debug::println!(
                        "desktop runtime failed to load {} or {}: {:?}",
                        path,
                        fallback_path,
                        error,
                    );
                } else {
                    debug::println!("desktop runtime failed to load {}: {:?}", path, error);
                }
            }
            Self::ProgramNotFound { program_id } => {
                debug::println!(
                    "desktop runtime program lookup failed: program_id={}",
                    program_id.index(),
                );
            }
            Self::RequestQueueFull => {
                debug::println!("desktop runtime request queue is full");
            }
            Self::NoAvailableSession => {
                debug::println!("desktop runtime launch rejected: no idle session");
            }
            Self::SessionBusy { session, pid } => {
                debug::println!(
                    "desktop runtime launch rejected: session={} pid={:?}",
                    session.name(),
                    pid,
                );
            }
        }
    }
}

#[derive(Clone)]
struct RegisteredDesktopProgram {
    display_name: &'static str,
    image: ExecutableImage,
    exec_path: &'static str,
    weight_micros: u64,
    logical_admin: bool,
    argv: &'static [&'static str],
    env: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunningDesktopProgram {
    pid: u64,
    program_id: DesktopProgramId,
    session: ConsoleSessionId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopRequest {
    Launch {
        program_id: DesktopProgramId,
        session: ConsoleSessionId,
    },
    TerminateSession {
        session: ConsoleSessionId,
    },
    TerminatePid {
        pid: u64,
    },
}

struct DesktopRuntimeState {
    bootstrapped: bool,
    programs: Vec<RegisteredDesktopProgram>,
    running: [Option<RunningDesktopProgram>; MAX_CONSOLE_SESSIONS],
    queued_launches: [u8; MAX_CONSOLE_SESSIONS],
    requests: RingBuffer<DesktopRequest, DESKTOP_REQUEST_CAPACITY>,
}

impl DesktopRuntimeState {
    const fn new() -> Self {
        Self {
            bootstrapped: false,
            programs: Vec::new(),
            running: [None; MAX_CONSOLE_SESSIONS],
            queued_launches: [0; MAX_CONSOLE_SESSIONS],
            requests: RingBuffer::new(),
        }
    }
}

struct LaunchRequestProgram {
    image: ExecutableImage,
    exec_path: &'static str,
    weight_micros: u64,
    logical_admin: bool,
    argv: &'static [&'static str],
    env: &'static [&'static str],
}

pub fn bootstrap() -> Result<(), DesktopRuntimeError> {
    with_runtime_state(|state| {
        if state.bootstrapped {
            return Err(DesktopRuntimeError::AlreadyBootstrapped);
        }
        state.bootstrapped = true;
        Ok(())
    })?;

    Ok(())
}

pub fn register_program(
    registration: DesktopProgramRegistration,
) -> Result<DesktopProgramId, DesktopRuntimeError> {
    if !multitask::thread_weight_is_valid(registration.weight_micros) {
        return Err(DesktopRuntimeError::InvalidProgramWeight {
            weight_micros: registration.weight_micros,
        });
    }

    let exec_path = registration
        .exec_path
        .unwrap_or(registration.image.primary_path);

    let program_id = with_runtime_state(|state| {
        let program_id = DesktopProgramId(state.programs.len());
        state.programs.push(RegisteredDesktopProgram {
            display_name: registration.display_name,
            image: registration.image,
            exec_path,
            weight_micros: registration.weight_micros,
            logical_admin: registration.logical_admin,
            argv: registration.argv,
            env: registration.env,
        });
        program_id
    });

    debug::println!(
        "desktop runtime registered: program_id={} name={} path={}",
        program_id.index(),
        registration.display_name,
        exec_path,
    );

    Ok(program_id)
}

pub fn snapshot_programs(dest: &mut [DesktopProgramInfo]) -> usize {
    with_runtime_state(|state| {
        let count = dest.len().min(state.programs.len());
        for (index, program) in state.programs.iter().take(count).enumerate() {
            dest[index] = DesktopProgramInfo {
                id: DesktopProgramId(index),
                display_name: program.display_name,
                exec_path: program.exec_path,
                weight_micros: program.weight_micros,
            };
        }
        count
    })
}

pub fn snapshot_running_programs(dest: &mut [DesktopRunningProgramInfo]) -> usize {
    with_runtime_state(|state| {
        let mut count = 0;
        for running in state.running.iter().flatten() {
            if count == dest.len() {
                break;
            }

            let program = &state.programs[running.program_id.index()];
            dest[count] = DesktopRunningProgramInfo {
                pid: running.pid,
                program_id: running.program_id,
                session: running.session,
                display_name: program.display_name,
            };
            count += 1;
        }
        count
    })
}

pub fn presentation_generation() -> u64 {
    RUNTIME_PRESENTATION_GENERATION.load(Ordering::Acquire)
}

pub fn request_launch(
    program_id: DesktopProgramId,
    target: DesktopLaunchTarget,
) -> Result<(), DesktopRuntimeError> {
    with_runtime_state(|state| {
        if state.programs.get(program_id.index()).is_none() {
            return Err(DesktopRuntimeError::ProgramNotFound { program_id });
        }

        match target {
            DesktopLaunchTarget::Session(session) => {
                if state.requests.remaining_capacity() < 1 {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }
                if !ensure_console_session(session) {
                    return Err(DesktopRuntimeError::NoAvailableSession);
                }
                ensure_session_launchable(state, session)?;
                enqueue_launch_request(state, program_id, session);
                Ok(())
            }
            DesktopLaunchTarget::FirstAvailableSession => {
                if state.requests.remaining_capacity() < 1 {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }

                let session = active_console_sessions()
                    .iter()
                    .find(|session| session_is_launchable(state, *session))
                    .or_else(allocate_console_session)
                    .ok_or(DesktopRuntimeError::NoAvailableSession)?;
                ensure_session_launchable(state, session)?;
                enqueue_launch_request(state, program_id, session);
                Ok(())
            }
            DesktopLaunchTarget::AllSessions => {
                let sessions = active_console_sessions();
                let request_count = sessions.count();
                if state.requests.remaining_capacity() < request_count {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }

                for session in sessions.iter() {
                    ensure_session_launchable(state, session)?;
                }
                for session in sessions.iter() {
                    enqueue_launch_request(state, program_id, session);
                }
                Ok(())
            }
        }
    })
}

pub fn request_terminate(target: DesktopTerminateTarget) -> Result<(), DesktopRuntimeError> {
    with_runtime_state(|state| {
        let request_count = match target {
            DesktopTerminateTarget::AllSessions => active_console_session_count(),
            DesktopTerminateTarget::Session(_) | DesktopTerminateTarget::ProcessId(_) => 1,
        };
        if state.requests.remaining_capacity() < request_count {
            return Err(DesktopRuntimeError::RequestQueueFull);
        }

        match target {
            DesktopTerminateTarget::Session(session) => {
                let queued = state
                    .requests
                    .push(DesktopRequest::TerminateSession { session });
                debug_assert!(queued);
            }
            DesktopTerminateTarget::ProcessId(pid) => {
                let queued = state.requests.push(DesktopRequest::TerminatePid { pid });
                debug_assert!(queued);
            }
            DesktopTerminateTarget::AllSessions => {
                for session in active_console_sessions().iter() {
                    let queued = state
                        .requests
                        .push(DesktopRequest::TerminateSession { session });
                    debug_assert!(queued);
                }
            }
        }

        Ok(())
    })
}

pub fn service_pending_requests() -> usize {
    service_once()
}

fn service_once() -> usize {
    reap_exited_programs() + process_next_request()
}

fn reap_exited_programs() -> usize {
    let running = with_runtime_state(|state| state.running);
    let mut reaped = 0;

    for running_program in running.into_iter().flatten() {
        if multitask::is_user_task_alive(running_program.pid) {
            continue;
        }

        let cleared = with_runtime_state(|state| {
            let slot = &mut state.running[running_program.session.index()];
            if *slot != Some(running_program) {
                return false;
            }
            *slot = None;
            true
        });
        if !cleared {
            continue;
        }

        mark_presentation_changed();
        release_session_if_unused(running_program.session);
        debug::println!(
            "desktop runtime exited: session={} pid={} program_id={}",
            running_program.session.name(),
            running_program.pid,
            running_program.program_id.index(),
        );
        console::write(b"Desktop runtime: program exited.\r\n");
        reaped += 1;
    }

    reaped
}

fn process_next_request() -> usize {
    let request = with_runtime_state(|state| {
        let request = state.requests.pop();
        if let Some(DesktopRequest::Launch { session, .. }) = request {
            let queued = &mut state.queued_launches[session.index()];
            if *queued != 0 {
                *queued -= 1;
            }
        }
        request
    });

    let Some(request) = request else {
        return 0;
    };

    match request {
        DesktopRequest::Launch {
            program_id,
            session,
        } => handle_launch_request(program_id, session),
        DesktopRequest::TerminateSession { session } => handle_terminate_session(session),
        DesktopRequest::TerminatePid { pid } => handle_terminate_pid(pid),
    }

    1
}

fn handle_launch_request(program_id: DesktopProgramId, session: ConsoleSessionId) {
    let program = with_runtime_state(|state| {
        if state.running[session.index()].is_some() {
            return None;
        }

        let program = state.programs.get(program_id.index())?;
        Some(LaunchRequestProgram {
            image: program.image,
            exec_path: program.exec_path,
            weight_micros: program.weight_micros,
            logical_admin: program.logical_admin,
            argv: program.argv,
            env: program.env,
        })
    });

    let Some(program) = program else {
        debug::println!(
            "desktop runtime launch skipped: program_id={} session={}",
            program_id.index(),
            session.name(),
        );
        return;
    };

    debug::println!(
        "desktop runtime launching program_id={} session={} path={}",
        program_id.index(),
        session.name(),
        program.exec_path,
    );
    let image = match console_host::load_executable_image(program.image) {
        Ok((_path, image)) => image,
        Err(err) => {
            err.log_debug_details();
            console::write(b"Desktop runtime: image load failed.\r\n");
            release_session_if_unused(session);
            return;
        }
    };

    let launch =
        console_host::ConsoleProgramSpec::new(&image, program.exec_path, program.weight_micros)
            .with_args(program.argv, program.env)
            .with_logical_admin(program.logical_admin);

    match console_host::spawn_program_in_session(session, launch) {
        Ok(spawned) => {
            let recorded = with_runtime_state(|state| {
                let slot = &mut state.running[session.index()];
                if slot.is_some() {
                    return false;
                }
                *slot = Some(RunningDesktopProgram {
                    pid: spawned.pid,
                    program_id,
                    session,
                });
                true
            });

            if !recorded {
                let _ = multitask::terminate_user_task(spawned.pid);
                debug::println!(
                    "desktop runtime launch rolled back: session={} pid={}",
                    session.name(),
                    spawned.pid,
                );
                return;
            }

            mark_presentation_changed();
            debug::println!(
                "desktop runtime launched: session={} pid={} program_id={} entry={:#x} path={} abi={}",
                session.name(),
                spawned.pid,
                program_id.index(),
                spawned.entry.as_u64(),
                program.exec_path,
                spawned.abi.name(),
            );
            crate::multitask::yield_now();
        }
        Err(err) => {
            err.log_debug_details();
            console::write(b"Desktop runtime: process launch failed.\r\n");
            release_session_if_unused(session);
        }
    }
}

fn handle_terminate_session(session: ConsoleSessionId) {
    let running = with_runtime_state(|state| state.running[session.index()]);
    let Some(running) = running else {
        return;
    };

    let terminated = multitask::terminate_user_task(running.pid);
    debug::println!(
        "desktop runtime terminate session={} pid={} accepted={}",
        session.name(),
        running.pid,
        terminated,
    );
}

fn handle_terminate_pid(pid: u64) {
    let running = with_runtime_state(|state| {
        for index in 0..state.running.len() {
            let Some(running) = state.running[index] else {
                continue;
            };
            if running.pid == pid {
                return Some(running);
            }
        }
        None
    });

    let Some(running) = running else {
        return;
    };

    let terminated = multitask::terminate_user_task(pid);
    debug::println!(
        "desktop runtime terminate pid={} session={} accepted={}",
        pid,
        running.session.name(),
        terminated,
    );
}

fn enqueue_launch_request(
    state: &mut DesktopRuntimeState,
    program_id: DesktopProgramId,
    session: ConsoleSessionId,
) {
    let queued = state.requests.push(DesktopRequest::Launch {
        program_id,
        session,
    });
    debug_assert!(queued);
    let counter = &mut state.queued_launches[session.index()];
    *counter = counter.saturating_add(1);
}

fn release_session_if_unused(session: ConsoleSessionId) {
    if session == ConsoleSessionId::PRIMARY {
        return;
    }

    let releasable = with_runtime_state(|state| {
        state.running[session.index()].is_none() && state.queued_launches[session.index()] == 0
    });
    if !releasable {
        return;
    }

    console::reset_session(session);
    tty::reset_session(session);
    let _ = release_console_session(session);
}

fn mark_presentation_changed() {
    let _ = RUNTIME_PRESENTATION_GENERATION.fetch_add(1, Ordering::AcqRel);
}

fn ensure_session_launchable(
    state: &DesktopRuntimeState,
    session: ConsoleSessionId,
) -> Result<(), DesktopRuntimeError> {
    if let Some(running) = state.running[session.index()] {
        return Err(DesktopRuntimeError::SessionBusy {
            session,
            pid: Some(running.pid),
        });
    }
    if state.queued_launches[session.index()] != 0 {
        return Err(DesktopRuntimeError::SessionBusy { session, pid: None });
    }
    Ok(())
}

fn session_is_launchable(state: &DesktopRuntimeState, session: ConsoleSessionId) -> bool {
    state.running[session.index()].is_none() && state.queued_launches[session.index()] == 0
}

fn with_runtime_state<R>(f: impl FnOnce(&mut DesktopRuntimeState) -> R) -> R {
    #[cfg(test)]
    {
        f(&mut RUNTIME.lock())
    }

    #[cfg(not(test))]
    {
        interrupts::without_interrupts(|| f(&mut RUNTIME.lock()))
    }
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    *RUNTIME.lock() = DesktopRuntimeState::new();
}
