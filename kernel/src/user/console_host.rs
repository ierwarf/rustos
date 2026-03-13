use crate::debug;
use crate::process::{self, ProcessLoadError, SpawnedProcess};
use crate::session::ConsoleSessionId;

#[derive(Clone, Copy)]
pub struct ConsoleProgramSpec<'a> {
    pub image: &'a [u8],
    pub exec_path: &'a str,
    pub weight_micros: u64,
    pub argv: &'a [&'a str],
    pub env: &'a [&'a str],
}

impl<'a> ConsoleProgramSpec<'a> {
    pub const fn new(image: &'a [u8], exec_path: &'a str, weight_micros: u64) -> Self {
        Self {
            image,
            exec_path,
            weight_micros,
            argv: &[],
            env: &[],
        }
    }

    #[allow(dead_code)]
    pub const fn with_args(mut self, argv: &'a [&'a str], env: &'a [&'a str]) -> Self {
        self.argv = argv;
        self.env = env;
        self
    }
}

#[derive(Debug)]
pub enum ConsoleHostError {
    Spawn {
        session: ConsoleSessionId,
        error: ProcessLoadError,
    },
}

impl ConsoleHostError {
    pub fn summary(&self) -> &'static str {
        match self {
            Self::Spawn { error, .. } => error.summary(),
        }
    }

    pub fn session(&self) -> ConsoleSessionId {
        match self {
            Self::Spawn { session, .. } => *session,
        }
    }

    pub fn log_debug_details(&self) {
        match self {
            Self::Spawn { error, .. } => error.log_debug_details(),
        }
    }
}

pub fn spawn_program_in_session(
    session: ConsoleSessionId,
    program: ConsoleProgramSpec<'_>,
) -> Result<SpawnedProcess, ConsoleHostError> {
    let default_argv = [program.exec_path];
    let argv = if program.argv.is_empty() {
        &default_argv[..]
    } else {
        program.argv
    };

    process::spawn_linux_process_with_args_in_session(
        program.image,
        program.weight_micros,
        program.exec_path,
        argv,
        program.env,
        session,
    )
    .map_err(|error| ConsoleHostError::Spawn { session, error })
}

pub fn spawn_program_on_all_sessions(
    program: ConsoleProgramSpec<'_>,
) -> Result<(), ConsoleHostError> {
    for session in ConsoleSessionId::all() {
        let spawned = spawn_program_in_session(session, program)?;
        log_spawn(session, program, &spawned);
    }
    Ok(())
}

fn log_spawn(session: ConsoleSessionId, program: ConsoleProgramSpec<'_>, spawned: &SpawnedProcess) {
    debug::println!(
        "Console session spawned: session={} pid={} entry={:#x} weight={}us path={} abi={}",
        session.name(),
        spawned.pid,
        spawned.entry.as_u64(),
        program.weight_micros,
        program.exec_path,
        spawned.abi.name(),
    );
}
