use super::*;
use crate::user::process_state::ProcessSecurityContext;

pub(crate) fn fork(frame: LinuxCloneFrame) -> Result<u64, LinuxSysopError> {
    let retained =
        multitask::retain_current_user_process_state().ok_or(LinuxSysopError::Unsupported)?;
    if retained.abi() != UserAbi::Linux {
        return Err(LinuxSysopError::Unsupported);
    }

    let parent_pid = retained.process_id();
    let parent_state = retained.process_state();
    let child_address_space = parent_state
        .address_space()
        .clone_user_space()
        .map_err(LinuxSysopError::AddressSpace)?;
    let child_thread_state = clone_fork_thread_state(
        multitask::current_linux_thread_state().ok_or(LinuxSysopError::Unsupported)?,
    );
    let console_session = multitask::current_console_session();
    let user_stack = multitask::current_user_stack_state();

    let mut bootstrap = multitask::UserTaskBootstrap::new(
        UserAbi::Linux,
        VirtAddr::new(frame.user_rip),
        VirtAddr::new(frame.user_rsp),
    );
    bootstrap.registers = frame.registers;
    bootstrap.registers.rax = 0;
    bootstrap.registers.rcx = frame.user_rip;
    bootstrap.registers.r11 = frame.user_rflags;
    bootstrap.user_stack = user_stack;
    bootstrap.console_session = console_session;
    bootstrap.logical_admin = parent_state.security().is_logical_admin();
    bootstrap.linux_process_state = parent_state.linux_process_state().copied();
    bootstrap.linux_memory_map = parent_state.linux_memory_map().cloned();
    bootstrap.linux_runtime_profile = parent_state.linux_runtime_profile().cloned();
    bootstrap.linux_thread_state = Some(child_thread_state);
    bootstrap.set_exec_path(parent_state.exec_path());

    let child_pid = multitask::spawn_user_process_with_parent(
        child_address_space,
        bootstrap,
        Some(parent_pid),
        multitask::DEFAULT_USER_TASK_WEIGHT_MICROS,
    )
    .map_err(|err| match err {
        multitask::SpawnTaskError::InvalidWeightMicros => LinuxSysopError::InvalidArgument,
        multitask::SpawnTaskError::NoFreeTaskSlot => LinuxSysopError::TryAgain,
    })?;

    multitask::with_process_state_by_pid_mut(child_pid, |child_state| {
        child_state.inherit_fork_process_metadata_from(parent_state);
    })
    .ok_or(LinuxSysopError::NoSuchProcess)?;
    drop(retained);

    Ok(child_pid)
}

pub(crate) fn prlimit64(
    pid: u64,
    resource: u64,
    new_limit_ptr: u64,
    old_limit_ptr: u64,
) -> Result<(), LinuxSysopError> {
    if pid != 0 && Some(pid) != multitask::current_user_process_id() {
        return Err(LinuxSysopError::PermissionDenied);
    }
    if resource != linux_abi::RLIMIT_STACK {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if new_limit_ptr != 0 {
        let mut requested = linux_abi::LinuxRlimit::default();
        let requested_bytes = unsafe {
            slice::from_raw_parts_mut(
                core::ptr::addr_of_mut!(requested).cast::<u8>(),
                size_of::<linux_abi::LinuxRlimit>(),
            )
        };
        usermem::copy_from_current_user_exact(new_limit_ptr, requested_bytes)?;
    }
    if old_limit_ptr != 0 {
        let current = linux_abi::LinuxRlimit {
            rlim_cur: DEFAULT_STACK_RLIMIT_BYTES,
            rlim_max: DEFAULT_STACK_RLIMIT_BYTES,
        };
        let bytes = unsafe {
            slice::from_raw_parts(
                core::ptr::addr_of!(current).cast::<u8>(),
                size_of::<linux_abi::LinuxRlimit>(),
            )
        };
        usermem::write_current_user_bytes(old_limit_ptr, bytes)?;
    }
    Ok(())
}

pub(crate) fn getrandom(
    user_ptr: u64,
    user_len: u64,
    flags: u64,
) -> Result<usize, LinuxSysopError> {
    if flags & !(GETRANDOM_FLAG_NONBLOCK | GETRANDOM_FLAG_RANDOM) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    if len == 0 {
        return Ok(0);
    }

    let mut rng = nucleus_core::util::random::Random::new();
    let mut copied = 0usize;
    let mut chunk = [0_u8; 256];
    while copied < len {
        let chunk_len = (len - copied).min(chunk.len());
        rng.fill_bytes(&mut chunk[..chunk_len]);
        let chunk_ptr = user_ptr
            .checked_add(copied as u64)
            .ok_or(LinuxSysopError::InvalidArgument)?;
        usermem::write_current_user_bytes(chunk_ptr, &chunk[..chunk_len])?;
        copied += chunk_len;
    }
    Ok(len)
}

pub(crate) fn ioctl(fd: u64, _request: u64, _arg: u64) -> Result<u64, LinuxSysopError> {
    let handle = super::fd::current_handle(fd)?;
    if let Some(stream) = handle.console_stream() {
        return console_tty_ioctl(stream, _request, _arg);
    }
    if handle.socket_handle().is_some() {
        return socket_ioctl(fd, _request, _arg);
    }
    if let Some(device_handle) = handle.device_handle() {
        return device::ioctl_current_process_device_handle(device_handle, _request, _arg)
            .map_err(Into::into);
    }
    Err(LinuxSysopError::Unsupported)
}

pub(crate) fn getpid() -> u64 {
    multitask::current_user_process_id().unwrap_or(0)
}

pub(crate) fn wait4(
    pid: i64,
    status_ptr: u64,
    options: u64,
    rusage_ptr: u64,
) -> Result<i64, LinuxSysopError> {
    if options & !(linux_abi::WNOHANG as u64) != 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }
    if pid < -1 || pid == 0 {
        return Err(LinuxSysopError::Unsupported);
    }

    let parent_pid = multitask::current_user_process_id().ok_or(LinuxSysopError::Unsupported)?;
    let nohang = options & linux_abi::WNOHANG as u64 != 0;

    loop {
        match multitask::wait_for_child(parent_pid, pid) {
            multitask::WaitChildResult::Exited {
                pid: child_pid,
                status,
            } => {
                if status_ptr != 0 {
                    usermem::write_current_user_struct(status_ptr, &status)?;
                }
                if rusage_ptr != 0 {
                    usermem::write_current_user_struct(
                        rusage_ptr,
                        &linux_abi::LinuxRusage::default(),
                    )?;
                }
                return Ok(child_pid as i64);
            }
            multitask::WaitChildResult::Pending if nohang => return Ok(0),
            multitask::WaitChildResult::Pending => multitask::yield_now(),
            multitask::WaitChildResult::NoMatchingChild => {
                return Err(LinuxSysopError::NoSuchProcess);
            }
        }
    }
}

pub(crate) fn gettid() -> u64 {
    multitask::current_user_id().unwrap_or(0)
}

pub(crate) fn sched_getaffinity(
    pid: u64,
    user_len: u64,
    mask_ptr: u64,
) -> Result<u64, LinuxSysopError> {
    if user_len == 0 {
        return Err(LinuxSysopError::InvalidArgument);
    }

    let current_pid = multitask::current_user_process_id().unwrap_or(0);
    if pid != 0 && pid != current_pid {
        return Err(LinuxSysopError::NoSuchProcess);
    }

    let len = usize::try_from(user_len).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let write_len = len.min(size_of::<u64>());
    let mut mask = [0_u8; size_of::<u64>()];
    mask[0] = 0x1;
    usermem::write_current_user_bytes(mask_ptr, &mask[..write_len])?;
    Ok(write_len as u64)
}

pub(crate) fn getuid() -> u64 {
    current_process_security_context()
        .map(|security| security.uid() as u64)
        .unwrap_or(0)
}

pub(crate) fn geteuid() -> u64 {
    current_process_security_context()
        .map(|security| security.euid() as u64)
        .unwrap_or(0)
}

pub(crate) fn getgid() -> u64 {
    current_process_security_context()
        .map(|security| security.gid() as u64)
        .unwrap_or(0)
}

pub(crate) fn getegid() -> u64 {
    current_process_security_context()
        .map(|security| security.egid() as u64)
        .unwrap_or(0)
}

pub(crate) fn setuid(uid: u64) -> Result<u64, LinuxSysopError> {
    let uid = u32::try_from(uid).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }
        let security = process_state.security();
        if security.euid() != 0
            && security.uid() != 0
            && uid != security.uid()
            && uid != security.euid()
        {
            return Err(LinuxSysopError::PermissionDenied);
        }
        process_state.set_uid(uid);
        Ok(0_u64)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };
    result
}

pub(crate) fn setgid(gid: u64) -> Result<u64, LinuxSysopError> {
    let gid = u32::try_from(gid).map_err(|_| LinuxSysopError::InvalidArgument)?;
    let Some(result) = multitask::with_current_user_process_state_mut(|_, abi, process_state| {
        if abi != UserAbi::Linux {
            return Err(LinuxSysopError::Unsupported);
        }
        let security = process_state.security();
        if security.euid() != 0
            && security.uid() != 0
            && gid != security.gid()
            && gid != security.egid()
        {
            return Err(LinuxSysopError::PermissionDenied);
        }
        process_state.set_gid(gid);
        Ok(0_u64)
    }) else {
        return Err(LinuxSysopError::Unsupported);
    };
    result
}

pub(crate) fn sched_yield() -> u64 {
    multitask::yield_now();
    0
}

pub(crate) fn uname(buf_ptr: u64) -> Result<(), LinuxSysopError> {
    let mut uts = linux_abi::LinuxUtsName::default();
    write_uts_field(&mut uts.sysname, b"RustOS");
    write_uts_field(&mut uts.nodename, b"rustos");
    write_uts_field(&mut uts.release, b"0.1");
    write_uts_field(&mut uts.version, b"RustOS 0.1");
    write_uts_field(&mut uts.machine, b"x86_64");
    write_uts_field(&mut uts.domainname, b"localdomain");

    let bytes = unsafe {
        slice::from_raw_parts(
            core::ptr::addr_of!(uts).cast::<u8>(),
            size_of::<linux_abi::LinuxUtsName>(),
        )
    };
    usermem::write_current_user_bytes(buf_ptr, bytes)?;
    Ok(())
}

fn write_uts_field(dest: &mut [u8; 65], value: &[u8]) {
    let len = value.len().min(dest.len().saturating_sub(1));
    dest[..len].copy_from_slice(&value[..len]);
    dest[len] = 0;
}

fn current_process_security_context() -> Option<ProcessSecurityContext> {
    let snapshot = multitask::current_user_snapshot()?;
    if snapshot.abi() != UserAbi::Linux {
        return None;
    }
    Some(snapshot.security())
}

fn clone_fork_thread_state(mut state: linux_abi::LinuxThreadState) -> linux_abi::LinuxThreadState {
    state.clear_child_tid = 0;
    state.robust_list_head = 0;
    state.robust_list_len = 0;
    state.rseq_area = 0;
    state.rseq_len = 0;
    state.rseq_signature = 0;
    state.pending_signals = 0;
    state
}

fn console_tty_ioctl(
    _stream: ConsoleStreamKind,
    request: u64,
    arg: u64,
) -> Result<u64, LinuxSysopError> {
    let session = multitask::current_console_session();

    match request {
        linux_abi::TCGETS => {
            let termios = tty::termios_for_session(session);
            let bytes = unsafe {
                slice::from_raw_parts(
                    core::ptr::addr_of!(termios).cast::<u8>(),
                    size_of::<linux_abi::LinuxTermios>(),
                )
            };
            usermem::write_current_user_bytes(arg, bytes)?;
            Ok(0)
        }
        linux_abi::TIOCGWINSZ => {
            let winsize = linux_abi::LinuxWinsize::default_console();
            let bytes = unsafe {
                slice::from_raw_parts(
                    core::ptr::addr_of!(winsize).cast::<u8>(),
                    size_of::<linux_abi::LinuxWinsize>(),
                )
            };
            usermem::write_current_user_bytes(arg, bytes)?;
            Ok(0)
        }
        linux_abi::FIONREAD => {
            let pending = tty::pending_input_len_for_session(session);
            let pending = u32::try_from(pending).unwrap_or(u32::MAX);
            usermem::write_current_user_bytes(arg, &pending.to_le_bytes())?;
            Ok(0)
        }
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF => {
            let mut termios = linux_abi::LinuxTermios::default();
            let bytes = unsafe {
                slice::from_raw_parts_mut(
                    core::ptr::addr_of_mut!(termios).cast::<u8>(),
                    size_of::<linux_abi::LinuxTermios>(),
                )
            };
            usermem::copy_from_current_user_exact(arg, bytes)?;
            tty::set_termios_for_session(session, termios, request == linux_abi::TCSETSF);
            Ok(0)
        }
        _ => Err(LinuxSysopError::NotTty),
    }
}

fn socket_ioctl(fd: u64, request: u64, arg: u64) -> Result<u64, LinuxSysopError> {
    match request {
        linux_abi::FIONREAD => {
            let available = super::socket::readable_socket_bytes_for_fd(fd)?
                .ok_or(LinuxSysopError::BadFileDescriptor)?;
            let available = u32::try_from(available.min(u64::from(u32::MAX)))
                .map_err(|_| LinuxSysopError::InvalidArgument)?;
            usermem::write_current_user_u32(arg, available)?;
            Ok(0)
        }
        linux_abi::FIONBIO => {
            let enable = usermem::read_current_user_u32(arg)? != 0;
            let Some(result) =
                multitask::with_current_user_process_state_mut(|_, _, process_state| {
                    let Some(entry) = process_state.handles_mut().get_entry_mut(fd) else {
                        return Err(LinuxSysopError::BadFileDescriptor);
                    };
                    if entry.handle().socket_handle().is_none() {
                        return Err(LinuxSysopError::NotSocket);
                    }
                    let mut status_flags = entry.status_flags();
                    if enable {
                        status_flags |= linux_abi::O_NONBLOCK;
                    } else {
                        status_flags &= !linux_abi::O_NONBLOCK;
                    }
                    entry.set_status_flags(status_flags);
                    Ok(0)
                })
            else {
                return Err(LinuxSysopError::Unsupported);
            };
            result
        }
        _ => Err(LinuxSysopError::Unsupported),
    }
}
