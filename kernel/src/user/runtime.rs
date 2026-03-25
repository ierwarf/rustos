use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
#[cfg(not(test))]
use x86_64::instructions::interrupts;

use crate::debug;
use crate::io::console;
use crate::io::session::{
    self, ConsoleSessionHandle, ConsoleSessionState, CreateConsoleSessionError,
};
use crate::io::tty;
use crate::multitask;
use crate::user::console_host::{self, ExecutableImage};
use crate::util::ring::RingBuffer;

const DESKTOP_REQUEST_CAPACITY: usize = 64;
const MAX_LAUNCHES_PER_SERVICE: usize = 2;

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
    Session(ConsoleSessionHandle),
    NewSession,
    AllSessions,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopTerminateTarget {
    Session(ConsoleSessionHandle),
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
    pub console_hosted: bool,
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
            console_hosted: true,
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

    pub const fn with_console_hosted(mut self, console_hosted: bool) -> Self {
        self.console_hosted = console_hosted;
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
    pub session_handle: ConsoleSessionHandle,
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
        error: fatfs::Error<crate::storage::fat::DiskIoError>,
    },
    ProgramNotFound {
        program_id: DesktopProgramId,
    },
    RequestQueueFull,
    NoAvailableSession,
    SessionBusy {
        session: Option<ConsoleSessionHandle>,
        pid: Option<u64>,
    },
    SessionNotFound {
        session: ConsoleSessionHandle,
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
            Self::NoAvailableSession => "no console session capacity is available",
            Self::SessionBusy { .. } => "target console session is already busy",
            Self::SessionNotFound { .. } => "target console session does not exist",
        }
    }

    pub fn log_debug_details(&self) {
        match self {
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
            Self::SessionBusy { session, pid } => {
                debug::println!(
                    "desktop runtime session busy: session={:?} pid={:?}",
                    session.map(ConsoleSessionHandle::raw),
                    pid,
                );
            }
            Self::SessionNotFound { session } => {
                debug::println!(
                    "desktop runtime session not found: handle={:#x}",
                    session.raw()
                );
            }
            _ => debug::println!("desktop runtime error: {}", self.summary()),
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
    console_hosted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunningDesktopProgram {
    pid: u64,
    program_id: DesktopProgramId,
    session_handle: ConsoleSessionHandle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopRequest {
    Launch {
        program_id: DesktopProgramId,
        target: DesktopLaunchTarget,
    },
    TerminateSession {
        session: ConsoleSessionHandle,
    },
    TerminatePid {
        pid: u64,
    },
}

struct DesktopRuntimeState {
    bootstrapped: bool,
    programs: Vec<RegisteredDesktopProgram>,
    running: Vec<RunningDesktopProgram>,
    requests: RingBuffer<DesktopRequest, DESKTOP_REQUEST_CAPACITY>,
}

impl DesktopRuntimeState {
    const fn new() -> Self {
        Self {
            bootstrapped: false,
            programs: Vec::new(),
            running: Vec::new(),
            requests: RingBuffer::new(),
        }
    }
}

struct LaunchRequestProgram {
    display_name: &'static str,
    image: ExecutableImage,
    exec_path: &'static str,
    weight_micros: u64,
    logical_admin: bool,
    argv: &'static [&'static str],
    env: &'static [&'static str],
    console_hosted: bool,
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
            console_hosted: registration.console_hosted,
        });
        program_id
    });

    debug::println!(
        "desktop runtime registered: program_id={} name={} path={} console_hosted={}",
        program_id.index(),
        registration.display_name,
        exec_path,
        registration.console_hosted,
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
        let count = dest.len().min(state.running.len());
        for (dest_slot, running) in dest.iter_mut().zip(state.running.iter().take(count)) {
            let program = &state.programs[running.program_id.index()];
            *dest_slot = DesktopRunningProgramInfo {
                pid: running.pid,
                program_id: running.program_id,
                session_handle: running.session_handle,
                display_name: program.display_name,
            };
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
        let Some(program) = state.programs.get(program_id.index()) else {
            return Err(DesktopRuntimeError::ProgramNotFound { program_id });
        };

        match target {
            DesktopLaunchTarget::Session(session) if program.console_hosted => {
                if !session::is_console_session_active(session) {
                    return Err(DesktopRuntimeError::SessionNotFound { session });
                }
                if state.requests.remaining_capacity() < 1 {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }
                if state
                    .running
                    .iter()
                    .any(|running| running.session_handle == session)
                {
                    let pid = state
                        .running
                        .iter()
                        .find(|running| running.session_handle == session)
                        .map(|running| running.pid);
                    return Err(DesktopRuntimeError::SessionBusy {
                        session: Some(session),
                        pid,
                    });
                }
                let queued = state
                    .requests
                    .push(DesktopRequest::Launch { program_id, target });
                debug_assert!(queued);
                Ok(())
            }
            DesktopLaunchTarget::NewSession => {
                if state.requests.remaining_capacity() < 1 {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }
                let queued = state
                    .requests
                    .push(DesktopRequest::Launch { program_id, target });
                debug_assert!(queued);
                Ok(())
            }
            DesktopLaunchTarget::AllSessions if program.console_hosted => {
                let mut sessions =
                    [ConsoleSessionHandle::SYSTEM; session::MAX_LIVE_CONSOLE_SESSIONS];
                let count = session::snapshot_console_session_handles(&mut sessions);
                if count == 0 {
                    return Err(DesktopRuntimeError::NoAvailableSession);
                }
                if state.requests.remaining_capacity() < count {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }
                for session in sessions.into_iter().take(count) {
                    if state
                        .running
                        .iter()
                        .any(|running| running.session_handle == session)
                    {
                        return Err(DesktopRuntimeError::SessionBusy {
                            session: Some(session),
                            pid: state
                                .running
                                .iter()
                                .find(|running| running.session_handle == session)
                                .map(|running| running.pid),
                        });
                    }
                }
                for session in sessions.into_iter().take(count) {
                    let queued = state.requests.push(DesktopRequest::Launch {
                        program_id,
                        target: DesktopLaunchTarget::Session(session),
                    });
                    debug_assert!(queued);
                }
                Ok(())
            }
            DesktopLaunchTarget::AllSessions => {
                if state.requests.remaining_capacity() < 1 {
                    return Err(DesktopRuntimeError::RequestQueueFull);
                }
                let queued = state.requests.push(DesktopRequest::Launch {
                    program_id,
                    target: DesktopLaunchTarget::NewSession,
                });
                debug_assert!(queued);
                Ok(())
            }
            DesktopLaunchTarget::Session(session) => {
                Err(DesktopRuntimeError::SessionNotFound { session })
            }
        }
    })
}

pub fn request_terminate(target: DesktopTerminateTarget) -> Result<(), DesktopRuntimeError> {
    with_runtime_state(|state| {
        let needed = match target {
            DesktopTerminateTarget::AllSessions => state.running.len(),
            DesktopTerminateTarget::Session(_) | DesktopTerminateTarget::ProcessId(_) => 1,
        };
        if state.requests.remaining_capacity() < needed {
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
                for running in state.running.iter().copied() {
                    let queued = state.requests.push(DesktopRequest::TerminateSession {
                        session: running.session_handle,
                    });
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
    reap_exited_programs() + process_requests()
}

fn reap_exited_programs() -> usize {
    let running = with_runtime_state(|state| state.running.clone());
    let mut reaped = 0;

    for running_program in running {
        if multitask::is_user_task_alive(running_program.pid) {
            continue;
        }

        let removed = with_runtime_state(|state| {
            let previous_len = state.running.len();
            state.running.retain(|entry| *entry != running_program);
            previous_len != state.running.len()
        });
        if !removed {
            continue;
        }

        if !running_program.session_handle.is_system() {
            close_console_session(running_program.session_handle);
        }
        mark_presentation_changed();
        reaped += 1;
    }

    reaped
}

fn process_requests() -> usize {
    let mut deferred_launches = Vec::new();
    let mut processed = 0;
    let mut launches_processed = 0;
    let mut pulled = Vec::new();

    with_runtime_state(|state| {
        while let Some(request) = state.requests.pop() {
            pulled.push(request);
        }
    });

    for request in pulled {
        match request {
            DesktopRequest::Launch { .. } if launches_processed >= MAX_LAUNCHES_PER_SERVICE => {
                deferred_launches.push(request);
            }
            DesktopRequest::Launch { program_id, target } => {
                handle_launch_request(program_id, target);
                launches_processed += 1;
                processed += 1;
            }
            DesktopRequest::TerminateSession { session } => {
                handle_terminate_session(session);
                processed += 1;
            }
            DesktopRequest::TerminatePid { pid } => {
                handle_terminate_pid(pid);
                processed += 1;
            }
        }
    }

    if !deferred_launches.is_empty() {
        with_runtime_state(|state| {
            for request in deferred_launches {
                let queued = state.requests.push(request);
                debug_assert!(queued);
            }
        });
    }

    processed
}

fn handle_launch_request(program_id: DesktopProgramId, target: DesktopLaunchTarget) {
    let program = with_runtime_state(|state| {
        let program = state.programs.get(program_id.index())?;
        Some(LaunchRequestProgram {
            display_name: program.display_name,
            image: program.image,
            exec_path: program.exec_path,
            weight_micros: program.weight_micros,
            logical_admin: program.logical_admin,
            argv: program.argv,
            env: program.env,
            console_hosted: program.console_hosted,
        })
    });
    let Some(program) = program else {
        return;
    };

    let session_handle = if program.console_hosted {
        match prepare_console_session_for_launch(
            program_id,
            program.display_name,
            program.exec_path,
            target,
        ) {
            Ok(session) => Some(session),
            Err(err) => {
                err.log_debug_details();
                return;
            }
        }
    } else {
        None
    };

    let image = match console_host::load_executable_image(program.image) {
        Ok((_path, image)) => image,
        Err(err) => {
            if let Some(session_handle) = session_handle {
                close_console_session(session_handle);
            }
            let runtime_err = match err {
                console_host::ConsoleHostError::Load {
                    path,
                    fallback_path,
                    error,
                } => DesktopRuntimeError::Load {
                    path,
                    fallback_path,
                    error,
                },
                console_host::ConsoleHostError::Spawn { .. } => unreachable!(),
            };
            runtime_err.log_debug_details();
            return;
        }
    };

    if let Some(session_handle) = session_handle {
        let _ = session::transition_console_session_state(
            session_handle,
            ConsoleSessionState::Spawning,
        );
    }

    let launch =
        console_host::ConsoleProgramSpec::new(&image, program.exec_path, program.weight_micros)
            .with_args(program.argv, program.env)
            .with_logical_admin(program.logical_admin);
    let console_session = session_handle.unwrap_or(ConsoleSessionHandle::SYSTEM);
    match console_host::spawn_program_in_session(console_session, launch) {
        Ok(spawned) => {
            with_runtime_state(|state| {
                state.running.push(RunningDesktopProgram {
                    pid: spawned.pid,
                    program_id,
                    session_handle: console_session,
                });
            });
            if let Some(session_handle) = session_handle {
                let _ = session::assign_console_session_pid(session_handle, Some(spawned.pid));
                let _ = session::transition_console_session_state(
                    session_handle,
                    ConsoleSessionState::Running,
                );
                let _ = session::set_focused_console_session(session_handle);
            }
            mark_presentation_changed();
        }
        Err(err) => {
            if let Some(session_handle) = session_handle {
                close_console_session(session_handle);
            }
            if let console_host::ConsoleHostError::Spawn { error, .. } = err {
                error.log_debug_details();
            }
        }
    }
}

fn prepare_console_session_for_launch(
    program_id: DesktopProgramId,
    display_name: &str,
    exec_path: &str,
    target: DesktopLaunchTarget,
) -> Result<ConsoleSessionHandle, DesktopRuntimeError> {
    match target {
        DesktopLaunchTarget::Session(session) => {
            if !session::is_console_session_active(session) {
                return Err(DesktopRuntimeError::SessionNotFound { session });
            }
            let _ = session::transition_console_session_state(
                session,
                ConsoleSessionState::LoadingImage,
            );
            Ok(session)
        }
        DesktopLaunchTarget::NewSession | DesktopLaunchTarget::AllSessions => {
            let session =
                session::create_console_session(program_id.index() as u32, display_name, exec_path)
                    .map_err(|err| match err {
                        CreateConsoleSessionError::NoCapacity => {
                            DesktopRuntimeError::NoAvailableSession
                        }
                    })?;
            let _ = session::transition_console_session_state(
                session,
                ConsoleSessionState::LoadingImage,
            );
            Ok(session)
        }
    }
}

fn handle_terminate_session(session: ConsoleSessionHandle) {
    let running = with_runtime_state(|state| {
        state
            .running
            .iter()
            .find(|running| running.session_handle == session)
            .copied()
    });
    if let Some(running) = running {
        let _ = multitask::terminate_user_task(running.pid);
        return;
    }

    if session::is_console_session_active(session) {
        close_console_session(session);
        mark_presentation_changed();
    }
}

fn handle_terminate_pid(pid: u64) {
    let _ = multitask::terminate_user_task(pid);
}

fn close_console_session(session_handle: ConsoleSessionHandle) {
    let _ = session::transition_console_session_state(session_handle, ConsoleSessionState::Closing);
    let _ = session::assign_console_session_pid(session_handle, None);
    console::reset_session(session_handle);
    tty::reset_session(session_handle);
    let _ = session::remove_console_session(session_handle);
}

fn mark_presentation_changed() {
    let _ = RUNTIME_PRESENTATION_GENERATION.fetch_add(1, Ordering::AcqRel);
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
