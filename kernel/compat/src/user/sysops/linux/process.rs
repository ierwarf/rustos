use super::*;

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
    let entry = super::fd::current_handle_entry(fd)?;
    let handle = entry.handle();
    if let Some(stream) = handle.console_stream() {
        return console_tty_ioctl(stream, _request, _arg);
    }
    if handle.socket_handle().is_some() {
        return socket_ioctl(fd, _request, _arg);
    }
    if let Some(device_handle) = handle.device_handle() {
        if !entry.rights().allows_device_ioctl() {
            return Err(LinuxSysopError::PermissionDenied);
        }
        return device::ioctl_current_process_device_handle(device_handle, _request, _arg)
            .map_err(Into::into);
    }
    Err(LinuxSysopError::Unsupported)
}

pub(crate) fn ioctl_for_process(
    process_id: u64,
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, LinuxSysopError> {
    device::ioctl_process_device_handle(process_id, fd, request, arg).map_err(Into::into)
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

pub(crate) fn sched_yield() -> u64 {
    multitask::yield_now();
    0
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
