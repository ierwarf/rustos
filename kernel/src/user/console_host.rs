use alloc::vec::Vec;

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::storage::fat;
use crate::user::linux::LinuxProcessLaunch;
use crate::user::process::{self, ProcessLaunchOptions, ProcessLoadError, SpawnedProcess};

#[derive(Clone, Copy)]
pub struct ExecutableImage {
    pub primary_path: &'static str,
    pub fallback_path: Option<&'static str>,
}

impl ExecutableImage {
    pub const fn new(primary_path: &'static str) -> Self {
        Self {
            primary_path,
            fallback_path: None,
        }
    }

    pub const fn preferred(primary_path: &'static str, fallback_path: &'static str) -> Self {
        Self {
            primary_path,
            fallback_path: Some(fallback_path),
        }
    }
}

#[derive(Clone, Copy)]
pub struct ConsoleProgramSpec<'a> {
    pub image: &'a [u8],
    pub exec_path: &'a str,
    pub weight_micros: u64,
    pub logical_admin: bool,
    pub argv: &'a [&'a str],
    pub env: &'a [&'a str],
}

impl<'a> ConsoleProgramSpec<'a> {
    pub const fn new(image: &'a [u8], exec_path: &'a str, weight_micros: u64) -> Self {
        Self {
            image,
            exec_path,
            weight_micros,
            logical_admin: false,
            argv: &[],
            env: &[],
        }
    }

    pub const fn with_args(mut self, argv: &'a [&'a str], env: &'a [&'a str]) -> Self {
        self.argv = argv;
        self.env = env;
        self
    }

    pub const fn with_logical_admin(mut self, logical_admin: bool) -> Self {
        self.logical_admin = logical_admin;
        self
    }
}

#[derive(Debug)]
pub enum ConsoleHostError {
    Load {
        path: &'static str,
        fallback_path: Option<&'static str>,
        error: fatfs::Error<fat::DiskIoError>,
    },
    Spawn {
        session: ConsoleSessionHandle,
        error: ProcessLoadError,
    },
}

impl ConsoleHostError {
    pub fn log_debug_details(&self) {
        match self {
            Self::Load {
                path,
                fallback_path,
                error,
            } => {
                if let Some(fallback_path) = fallback_path {
                    debug::println!(
                        "failed to load boot program image from {} or {}: {:?}",
                        path,
                        fallback_path,
                        error,
                    );
                } else {
                    debug::println!(
                        "failed to load boot program image from {}: {:?}",
                        path,
                        error
                    );
                }
            }
            Self::Spawn { error, .. } => error.log_debug_details(),
        }
    }
}

pub fn spawn_program_in_session(
    session: ConsoleSessionHandle,
    program: ConsoleProgramSpec<'_>,
) -> Result<SpawnedProcess, ConsoleHostError> {
    let default_argv = [program.exec_path];
    let argv = if program.argv.is_empty() {
        &default_argv[..]
    } else {
        program.argv
    };

    let launch = ProcessLaunchOptions {
        linux: LinuxProcessLaunch {
            exec_path: program.exec_path,
            argv,
            env: program.env,
        },
        console_session: session,
        logical_admin: program.logical_admin,
        ..ProcessLaunchOptions::default()
    };

    process::spawn_process_with_launch(program.image, program.weight_micros, launch)
        .map_err(|error| ConsoleHostError::Spawn { session, error })
}

pub fn load_executable_image(
    image: ExecutableImage,
) -> Result<(&'static str, Vec<u8>), ConsoleHostError> {
    match fat::read_file_to_vec(image.primary_path) {
        Ok(bytes) => Ok((image.primary_path, bytes)),
        Err(primary_error) => {
            let Some(fallback_path) = image.fallback_path else {
                return Err(ConsoleHostError::Load {
                    path: image.primary_path,
                    fallback_path: None,
                    error: primary_error,
                });
            };

            fat::read_file_to_vec(fallback_path)
                .map(|bytes| (fallback_path, bytes))
                .map_err(|fallback_error| ConsoleHostError::Load {
                    path: image.primary_path,
                    fallback_path: Some(fallback_path),
                    error: fallback_error,
                })
        }
    }
}
