// RING3-MIGRATION-REFERENCE START: loaderd/sessiond should own console-host
// executable discovery policy. Ring0 keeps this bootstrap-local
// console image materialization path for fixed pre-loaderd service bootstrap.
use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

use crate::debug;
use crate::io::session::ConsoleSessionHandle;
use crate::user::linux::LinuxProcessLaunch;
use crate::user::process::{
    self, ProcessLaunchOptions, ProcessLoadError, ProcessStartRegisters, SpawnedProcess,
};
use crate::vfs;

fn emit_console(level: debug::LogLevel, event_id: u16, object_id: u64, message: String) {
    debug::record_milestone(
        debug::LogCategory::Console,
        "console-host",
        event_id as u64,
        object_id,
    );
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
pub struct ConsoleProgramSpec<'a> {
    pub image: &'a [u8],
    pub exec_path: &'a str,
    pub weight_micros: u64,
    pub logical_admin: bool,
    pub argv: &'a [&'a str],
    pub env: &'a [&'a str],
    scheduling_context: Option<rustos_user_abi::syscall::RustosSchedulingContextPolicy>,
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
            scheduling_context: None,
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

    pub const fn with_scheduling_context(
        mut self,
        policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
    ) -> Self {
        self.scheduling_context = Some(policy);
        self
    }

    pub const fn with_bootstrap_scheduling_context(
        self,
        domain: u64,
        budget_ns: u64,
        period_ns: u64,
        criticality: u8,
    ) -> Self {
        self.with_scheduling_context(
            rustos_user_abi::syscall::RustosSchedulingContextPolicy::new(
                u64::MAX,
                budget_ns,
                period_ns,
                8,
                criticality,
                domain,
                1,
            ),
        )
    }
}

#[derive(Debug)]
pub enum ConsoleHostError {
    BootstrapBlocked,
    Load {
        path: &'static str,
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

    // DIAGNOSTIC: Production releases compile out verbose console details while
    // debug builds retain the same typed failure path.
    #[cfg_attr(not(rustos_debug_print_enabled), allow(dead_code, unused_variables))]
    pub fn log_debug_details(&self) {
        match self {
            Self::BootstrapBlocked => emit_console(
                debug::LogLevel::Warn,
                0,
                0,
                String::from("userspace startup blocked before UserspaceReady"),
            ),
            Self::Load { path, error } => {
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
        registers: ProcessStartRegisters::new(),
        linux: LinuxProcessLaunch {
            exec_path: program.exec_path,
            argv,
            env: program.env,
        },
        console_session: session,
        logical_admin: program.logical_admin,
    };

    let policy = program.scheduling_context.ok_or(ConsoleHostError::Spawn {
        error: ProcessLoadError::MissingSchedulingContext,
    })?;
    let spawned = process::spawn_bootstrap_linux_process_with_launch_and_scheduling_context(
        program.image,
        program.weight_micros,
        launch,
        policy,
    );
    spawned
        .inspect(|spawned| {
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
        })
        .map_err(|error| {
            let _ = session;
            ConsoleHostError::Spawn { error }
        })
}

pub fn load_executable_image_by_path(
    primary_path: &str,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    debug::record_milestone(debug::LogCategory::Console, "console-load-enter", 0, 0);
    if !crate::storage::boot_volume::userspace_runtime_active() {
        debug::record_milestone(debug::LogCategory::Console, "console-load-blocked", 0, 0);
        return Err(ConsoleHostError::BootstrapBlocked);
    }
    debug::record_milestone(debug::LogCategory::Console, "console-load-allowed", 0, 0);
    let trace = reserve_console_host_trace();
    if trace {
        emit_console(
            debug::LogLevel::Debug,
            5,
            0,
            alloc::format!("console host: load image begin path={}", primary_path),
        );
    }
    match load_executable_image_path_uncached(primary_path) {
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

fn reserve_console_host_trace() -> bool {
    debug::enabled!(console, debug)
}

fn load_executable_image_path_uncached(
    primary_path: &str,
) -> Result<LoadedExecutableImage, ConsoleHostError> {
    crate::debug::debug!(console, "console host: read begin path={}", primary_path,);
    debug::record_milestone(debug::LogCategory::Console, "console-read-begin", 0, 0);
    match vfs::read_path_to_vec_for_kernel(primary_path) {
        Ok(bytes) => {
            debug::record_milestone(
                debug::LogCategory::Console,
                "console-read-done",
                bytes.len() as u64,
                0,
            );
            Ok(LoadedExecutableImage {
                path: Box::leak(primary_path.to_string().into_boxed_str()),
                bytes,
            })
        }
        Err(primary_error) => {
            debug::record_milestone(debug::LogCategory::Console, "console-read-failed", 0, 0);
            crate::debug::warn!(
                console,
                "console host: read failed path={} err={:?}",
                primary_path,
                primary_error,
            );
            Err(ConsoleHostError::Load {
                path: Box::leak(primary_path.to_string().into_boxed_str()),
                error: primary_error,
            })
        }
    }
}
// RING3-MIGRATION-REFERENCE END: loaderd/sessiond-owned bootstrap console-host substrate exception.
