use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::user::linux::LinuxProcessLaunch;
use crate::user::process::{self, ProcessLaunchOptions, ProcessLoadError, SpawnedProcess};
use crate::user::windows::WindowsProcessLaunch;
use crate::vfs;

fn emit_console(level: debug::LogLevel, event_id: u16, object_id: u64, message: String) {
    let _ = (event_id, object_id);
    match level {
        debug::LogLevel::Trace => debug::trace!(console, "{}", message),
        debug::LogLevel::Debug => debug::debug!(console, "{}", message),
        debug::LogLevel::Info => debug::info!(console, "{}", message),
        debug::LogLevel::Warn => debug::warn!(console, "{}", message),
        debug::LogLevel::Error | debug::LogLevel::Fatal => debug::error!(console, "{}", message),
    }
}

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
    BootstrapBlocked,
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
    pub fn summary(&self) -> &'static str {
        match self {
            Self::BootstrapBlocked => "userspace startup blocked until kernel bootstrap completes",
            Self::Load { .. } => "failed to load executable image",
            Self::Spawn { error } => error.summary(),
        }
    }

    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code, unused_variables))]
    pub fn log_debug_details(&self) {
        match self {
            Self::BootstrapBlocked => emit_console(
                debug::LogLevel::Warn,
                0,
                0,
                String::from("userspace startup blocked before UserspaceReady"),
            ),
            Self::Load {
                path,
                fallback_path,
                error,
            } => {
                if let Some(fallback_path) = fallback_path {
                    emit_console(
                        debug::LogLevel::Warn,
                        0,
                        0,
                        alloc::format!(
                            "failed to load boot program image from {} or {}: {:?}",
                            path,
                            fallback_path,
                            error,
                        ),
                    );
                } else {
                    emit_console(
                        debug::LogLevel::Warn,
                        0,
                        0,
                        alloc::format!(
                            "failed to load boot program image from {}: {:?}",
                            path,
                            error
                        ),
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
    if !crate::storage::boot_volume::userspace_runtime_active() {
        return Err(ConsoleHostError::BootstrapBlocked);
    }
    let trace = reserve_console_host_trace();
    if trace {
        emit_console(
            debug::LogLevel::Debug,
            1,
            session.raw(),
            alloc::format!(
                "console host: spawn begin session={} exec={} argv={} env={} logical_admin={}",
                session.raw(),
                program.exec_path,
                program.argv.len(),
                program.env.len(),
                program.logical_admin,
            ),
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
                emit_console(
                    debug::LogLevel::Debug,
                    2,
                    spawned.pid,
                    alloc::format!(
                        "console host: spawn done session={} exec={} pid={}",
                        session.raw(),
                        program.exec_path,
                        spawned.pid,
                    ),
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
        emit_console(
            debug::LogLevel::Debug,
            3,
            0,
            alloc::format!(
                "console host: load image begin primary={} fallback={}",
                image.primary_path,
                image.fallback_path.unwrap_or("-"),
            ),
        );
    }
    match load_executable_image_uncached(image) {
        Ok(loaded) => {
            if trace {
                emit_console(
                    debug::LogLevel::Debug,
                    4,
                    0,
                    alloc::format!(
                        "console host: load image done path={} bytes={}",
                        loaded.path,
                        loaded.bytes.len(),
                    ),
                );
            }
            Ok(loaded)
        }
        Err(err) => Err(err),
    }
}

pub fn load_executable_image_by_path(
    primary_path: &str,
    fallback_path: Option<&str>,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    if !crate::storage::boot_volume::userspace_runtime_active() {
        return Err(ConsoleHostError::BootstrapBlocked);
    }
    let trace = reserve_console_host_trace();
    if trace {
        emit_console(
            debug::LogLevel::Debug,
            5,
            0,
            alloc::format!(
                "console host: load image begin primary={} fallback={}",
                primary_path,
                fallback_path.unwrap_or("-"),
            ),
        );
    }
    match load_executable_image_path_uncached(primary_path, fallback_path) {
        Ok(loaded) => {
            if trace {
                emit_console(
                    debug::LogLevel::Debug,
                    6,
                    0,
                    alloc::format!(
                        "console host: load image done path={} bytes={}",
                        loaded.path,
                        loaded.bytes.len(),
                    ),
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
        emit_console(
            debug::LogLevel::Debug,
            7,
            0,
            alloc::format!(
                "console host: verified image path={} bytes={} magic={:02x} {:02x} {:02x} {:02x}",
                loaded.path,
                loaded.bytes.len(),
                loaded.bytes.first().copied().unwrap_or(0),
                loaded.bytes.get(1).copied().unwrap_or(0),
                loaded.bytes.get(2).copied().unwrap_or(0),
                loaded.bytes.get(3).copied().unwrap_or(0),
            ),
        );
    }
    Ok(())
}

fn reserve_console_host_trace() -> bool {
    debug::enabled!(console, debug)
}

fn load_executable_image_uncached(
    image: ExecutableImage,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    load_executable_image_path_uncached(image.primary_path, image.fallback_path)
}

fn load_executable_image_path_uncached(
    primary_path: &str,
    fallback_path: Option<&str>,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    crate::debug::debug!(
        console,
        "console host: primary read begin path={} fallback={}",
        primary_path,
        fallback_path.unwrap_or("-"),
    );
    match vfs::read_path_to_vec_for_kernel(primary_path) {
        Ok(bytes) => Ok(LoadedExecutableImage {
            path: Box::leak(primary_path.to_string().into_boxed_str()),
            bytes,
        }),
        Err(primary_error) => {
            crate::debug::warn!(
                console,
                "console host: primary read failed path={} err={:?}",
                primary_path,
                primary_error,
            );
            let Some(fallback_path) = fallback_path else {
                return Err(ConsoleHostError::Load {
                    path: Box::leak(primary_path.to_string().into_boxed_str()),
                    fallback_path: None,
                    error: primary_error,
                });
            };

            crate::debug::debug!(
                console,
                "console host: fallback read begin path={}",
                fallback_path
            );
            vfs::read_path_to_vec_for_kernel(fallback_path)
                .map(|bytes| LoadedExecutableImage {
                    path: Box::leak(fallback_path.to_string().into_boxed_str()),
                    bytes,
                })
                .map_err(|fallback_error| ConsoleHostError::Load {
                    path: Box::leak(primary_path.to_string().into_boxed_str()),
                    fallback_path: Some(Box::leak(fallback_path.to_string().into_boxed_str())),
                    error: fallback_error,
                })
        }
    }
}
