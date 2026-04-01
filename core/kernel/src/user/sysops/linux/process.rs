use super::*;

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

    let mut rng = crate::util::random::Random::new();
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
    match super::fd::current_handle(fd)? {
        KernelHandle::Console(stream) => console_tty_ioctl(stream, _request, _arg),
        KernelHandle::Device(device_handle) => {
            device::ioctl_current_process_device_handle(device_handle, _request, _arg)
                .map_err(Into::into)
        }
        KernelHandle::VfsFile(_)
        | KernelHandle::Memfd(_)
        | KernelHandle::VfsDirectory(_)
        | KernelHandle::Socket(_)
        | KernelHandle::DisplaySurface(_) => Err(LinuxSysopError::Unsupported),
    }
}

pub(crate) fn getpid() -> u64 {
    multitask::current_user_process_id().unwrap_or(0)
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
    0
}

pub(crate) fn geteuid() -> u64 {
    0
}

pub(crate) fn getgid() -> u64 {
    0
}

pub(crate) fn getegid() -> u64 {
    0
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
