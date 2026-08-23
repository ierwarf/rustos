//! Rootd-only direct bootstrap spawn substrate before loaderd is available.

use super::*;

pub fn syscall_linux_loader_spawn_exec(
    path_ptr: u64,
    _argv_ptr: u64,
    _envp_ptr: u64,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> u64 {
    const BOOTSTRAP_SPAWN_ALLOWED_FLAGS: u64 = 0x1;
    let exec_path = match copy_current_user_path(path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if flags & !BOOTSTRAP_SPAWN_ALLOWED_FLAGS != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if !current_process_can_bootstrap_spawn() || !can_bootstrap_spawn_direct(exec_path.as_str()) {
        return linux_errno(LINUX_EACCES);
    }
    let Some(requester_pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let scheduling_policy = match proc_broker_ops::consume_direct_bootstrap_scheduling_context(
        requester_pid,
        exec_path.as_str(),
    ) {
        Ok(policy) => policy,
        Err(errno) => return linux_errno(errno),
    };
    match spawn_bootstrap_exec_direct(
        exec_path.as_str(),
        flags,
        console_session,
        weight_micros,
        scheduling_policy,
    ) {
        Ok(pid) => pid,
        Err(errno) => linux_errno(errno),
    }
}

fn current_process_can_bootstrap_spawn() -> bool {
    const ROOTD_EXEC_PATH: &str = "services/rootd/rootd.elf";
    multitask::with_current_process_state(|_, process_state| {
        process_state.security().is_logical_admin()
            && process_state.exec_path().trim_start_matches('/') == ROOTD_EXEC_PATH
    })
    .unwrap_or(false)
}

fn can_bootstrap_spawn_direct(exec_path: &str) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
            | "services/procd/procd.elf"
    )
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
    scheduling_policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
) -> Result<u64, i64> {
    let loaded = crate::user::console_host::load_executable_image_by_path(exec_path)
        .map_err(console_host_error_to_linux_errno)?;
    let session = if console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .ok_or(LINUX_EINVAL)?
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(console_session)
    };
    let logical_admin = flags & 0x1 != 0;
    let program = crate::user::console_host::ConsoleProgramSpec::new(
        &loaded.bytes,
        loaded.path,
        weight_micros,
    )
    .with_logical_admin(logical_admin)
    .with_scheduling_context(scheduling_policy);
    crate::user::console_host::spawn_program_in_session(session, program)
        .map(|spawned| spawned.pid)
        .map_err(console_host_error_to_linux_errno)
}

fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { .. } => LINUX_ENOEXEC,
    }
}
