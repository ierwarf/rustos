//! Rootd-only direct bootstrap spawn substrate before loaderd is available.

use super::*;

pub fn syscall_linux_loader_spawn_exec(args_ptr: u64) -> u64 {
    if !current_process_can_bootstrap_spawn() {
        return linux_errno(LINUX_EACCES);
    }
    let args = match usermem::read_current_user_struct::<rustos_user_abi::syscall::RustosBootstrapSpawnArgs>(args_ptr) {
        Ok(args) => args,
        Err(err) => return linux_errno(address_space_error_to_linux_errno(err)),
    };
    if !rustos_user_abi::syscall::bootstrap_spawn_shape_valid(&args) {
        return linux_errno(LINUX_EINVAL);
    }
    let exec_path = match copy_current_user_path(args.path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let logical_admin = args.flags & rustos_user_abi::syscall::BOOTSTRAP_SPAWN_FLAG_LOGICAL_ADMIN != 0;
    if !can_bootstrap_spawn_direct(exec_path.as_str(), logical_admin) {
        return linux_errno(LINUX_EACCES);
    }
    let Some(requester_pid) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_EPERM);
    };
    let job = match proc_broker_ops::resolve_spawn_job_descriptor(requester_pid, args.job_handle) {
        Ok(job) => job,
        Err(errno) => return linux_errno(errno),
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
        u64::from(args.flags),
        args.console_session,
        args.weight_micros,
        scheduling_policy,
        job,
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

fn can_bootstrap_spawn_direct(exec_path: &str, logical_admin: bool) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    // `rootd` may start an unprivileged copy of its own image, which is how the
    // boot self-check proves a job memory limit refuses pages from a process
    // that can do nothing else. The copy is never admitted as logical admin, so
    // it cannot itself spawn or hold supervisor authority.
    if path == "services/rootd/rootd.elf" {
        return !logical_admin;
    }
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/pagerd/pagerd.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
            | "services/procd/procd.elf"
    )
}

#[cfg(test)]
mod tests {
    use super::can_bootstrap_spawn_direct;

    #[test]
    fn the_core_services_may_be_spawned_directly_with_or_without_logical_admin() {
        for path in [
            "services/syscalld/syscalld.elf",
            "services/pagerd/pagerd.elf",
            "/services/vfsd/vfsd.elf",
            "services/loaderd/loaderd.elf",
            "services/procd/procd.elf",
        ] {
            assert!(can_bootstrap_spawn_direct(path, true), "{path}");
        }
        assert!(!can_bootstrap_spawn_direct("services/initd/initd.elf", true));
        assert!(!can_bootstrap_spawn_direct("apps/shell/shell.elf", false));
    }

    #[test]
    fn a_copy_of_rootd_may_be_spawned_only_without_logical_admin() {
        assert!(can_bootstrap_spawn_direct("services/rootd/rootd.elf", false));
        assert!(can_bootstrap_spawn_direct("/services/rootd/rootd.elf", false));
        assert!(
            !can_bootstrap_spawn_direct("services/rootd/rootd.elf", true),
            "a privileged second rootd is never admitted"
        );
    }
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
    scheduling_policy: rustos_user_abi::syscall::RustosSchedulingContextPolicy,
    job: multitask::JobId,
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
    .with_job(job)
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
