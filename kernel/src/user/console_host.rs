use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::user::linux::LinuxProcessLaunch;
use crate::user::process::{self, ProcessLaunchOptions, ProcessLoadError, SpawnedProcess};
use crate::user::windows::WindowsProcessLaunch;
use crate::vfs;

const CONSOLE_HOST_TRACE_BUDGET: usize = 8;

static CONSOLE_HOST_TRACE_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
pub struct LoadedExecutableImage {
    pub path: &'static str,
    pub bytes: Vec<u8>,
}

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

    // Preferred/fallback pairs remain part of the launch contract for staged images.
    #[allow(dead_code)]
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
        error: vfs::VfsError,
    },
    Spawn {
        error: ProcessLoadError,
    },
}

impl ConsoleHostError {
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code, unused_variables))]
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
    let trace = reserve_console_host_trace();
    if trace {
        debug::println!(
            "console host: spawn begin session={} exec={} argv={} env={} logical_admin={}",
            session.raw(),
            program.exec_path,
            program.argv.len(),
            program.env.len(),
            program.logical_admin,
        );
    }
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
        windows: WindowsProcessLaunch {
            exec_path: program.exec_path,
            argv,
            env: program.env,
        },
        console_session: session,
        logical_admin: program.logical_admin,
        ..ProcessLaunchOptions::default()
    };

    process::spawn_process_with_launch(program.image, program.weight_micros, launch)
        .map(|spawned| {
            if trace {
                debug::println!(
                    "console host: spawn done session={} exec={} pid={}",
                    session.raw(),
                    program.exec_path,
                    spawned.pid,
                );
            }
            spawned
        })
        .map_err(|error| {
            let _ = session;
            ConsoleHostError::Spawn { error }
        })
}

pub fn load_executable_image(
    image: ExecutableImage,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    let trace = reserve_console_host_trace();
    if trace {
        debug::println!(
            "console host: load image begin primary={} fallback={}",
            image.primary_path,
            image.fallback_path.unwrap_or("-"),
        );
    }
    match load_executable_image_uncached(image) {
        Ok(loaded) => {
            if trace {
                debug::println!(
                    "console host: load image done path={} bytes={}",
                    loaded.path,
                    loaded.bytes.len(),
                );
            }
            Ok(loaded)
        }
        Err(err) => Err(err),
    }
}

pub fn prime_executable_image(image: ExecutableImage) -> Result<(), ConsoleHostError> {
    let loaded = load_executable_image_uncached(image)?;
    if reserve_console_host_trace() {
        debug::println!(
            "console host: verified image path={} bytes={} magic={:02x} {:02x} {:02x} {:02x}",
            loaded.path,
            loaded.bytes.len(),
            loaded.bytes.first().copied().unwrap_or(0),
            loaded.bytes.get(1).copied().unwrap_or(0),
            loaded.bytes.get(2).copied().unwrap_or(0),
            loaded.bytes.get(3).copied().unwrap_or(0),
        );
    }
    Ok(())
}

fn reserve_console_host_trace() -> bool {
    CONSOLE_HOST_TRACE_COUNT.fetch_add(1, Ordering::Relaxed) < CONSOLE_HOST_TRACE_BUDGET
}

fn load_executable_image_uncached(
    image: ExecutableImage,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    match vfs::read_path_to_vec_for_kernel(image.primary_path) {
        Ok(bytes) => Ok(LoadedExecutableImage {
            path: image.primary_path,
            bytes,
        }),
        Err(primary_error) => {
            let Some(fallback_path) = image.fallback_path else {
                return Err(ConsoleHostError::Load {
                    path: image.primary_path,
                    fallback_path: None,
                    error: primary_error,
                });
            };

            vfs::read_path_to_vec_for_kernel(fallback_path)
                .map(|bytes| LoadedExecutableImage {
                    path: fallback_path,
                    bytes,
                })
                .map_err(|fallback_error| ConsoleHostError::Load {
                    path: image.primary_path,
                    fallback_path: Some(fallback_path),
                    error: fallback_error,
                })
        }
    }
}
