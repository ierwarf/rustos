use std::ffi::CString;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rustos_user_abi::console::{self as console_abi};
use rustos_user_abi::syscall::{
    IPC_SERVICE_LOADERD, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, LoaderSpawnRequest,
    LoaderSpawnResponse, RustosDeviceIoctlBrokerArgs, SYS_RUSTOS_DEVICE_IOCTL_BROKER,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT,
};

use super::{BrokerState, LaunchEntry, RunningProcess};
use super::{
    CONSOLE_PATH, CONSOLE_SESSION_STATE_LOADING_IMAGE, CONSOLE_SESSION_STATE_RUNNING,
    CONSOLE_SESSION_STATE_SPAWNING, IDLE_POLL_INTERVAL, LOADER_ENDPOINT_CACHE,
    MIN_EFFECTIVE_TASK_WEIGHT_MICROS, DEFAULT_USER_TASK_WEIGHT_MICROS, O_RDWR, RETRY_BACKOFF,
    SYS_IOCTL, SYS_OPENAT, AT_FDCWD, boot_line,
};

use rustos_user_abi::console::{
    ConsoleCloseSessionRequest, ConsoleCreateSessionRequest, ConsoleSetFocusRequest,
    ConsoleSetSessionStateRequest,
};

const CONSOLE_IOCTL_SET_FOCUS: usize = console_abi::CONSOLE_IOCTL_SET_FOCUS as usize;
const CONSOLE_IOCTL_CREATE_SESSION: usize = console_abi::CONSOLE_IOCTL_CREATE_SESSION as usize;
const CONSOLE_IOCTL_CLOSE_SESSION: usize = console_abi::CONSOLE_IOCTL_CLOSE_SESSION as usize;
const CONSOLE_IOCTL_SET_SESSION_STATE: usize =
    console_abi::CONSOLE_IOCTL_SET_SESSION_STATE as usize;

pub(super) fn spawn_tracked_process(
    state: &mut BrokerState,
    entry: LaunchEntry,
) -> Result<(), i32> {
    boot_line(
        format!(
            "runtimed: spawn begin desktop_id={} exec={} console_hosted={} logical_admin={}",
            entry.desktop_file_id, entry.exec, entry.console_hosted, entry.logical_admin
        )
        .as_str(),
    );
    let session_handle = if entry.console_hosted {
        let console_fd = ensure_console_fd(state)?;
        let session = create_console_session(
            console_fd,
            0,
            entry.display_name.as_str(),
            entry.exec.as_str(),
        )?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_LOADING_IMAGE)?;
        Some(session)
    } else {
        None
    };
    let pid = match spawn_exec(
        entry.exec.as_str(),
        entry.args.as_slice(),
        entry.env.as_slice(),
        entry.logical_admin,
        entry.weight_micros,
        session_handle.unwrap_or(0),
    ) {
        Ok(pid) => pid,
        Err(err) => {
            if let Some(session) = session_handle {
                let _ = close_console_session(ensure_console_fd(state)?, session);
            }
            if is_permanent_launch_failure(err) {
                observability_client::warn!(
                    "runtimed",
                    service,
                    "spawn exec permanent failure desktop_id={} exec={} errno={err}",
                    entry.desktop_file_id,
                    entry.exec
                );
            } else {
                observability_client::error!(
                    "runtimed",
                    service,
                    "spawn exec failed desktop_id={} exec={} errno={err}",
                    entry.desktop_file_id,
                    entry.exec
                );
            }
            return Err(err);
        }
    };
    boot_line(
        format!(
            "runtimed: spawned desktop_id={} exec={} pid={}",
            entry.desktop_file_id, entry.exec, pid
        )
        .as_str(),
    );
    state.retry_after.remove(entry.desktop_file_id.as_str());
    if let Some(session) = session_handle {
        let console_fd = ensure_console_fd(state)?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_SPAWNING)?;
        set_console_session_state(console_fd, session, CONSOLE_SESSION_STATE_RUNNING)?;
        let _ = console_set_focus(console_fd, session);
    }
    state.running.insert(
        pid,
        RunningProcess {
            pid,
            package_id: entry.package_id,
            desktop_file_id: entry.desktop_file_id,
            display_name: entry.display_name,
            exec: entry.exec,
            session_handle: session_handle.unwrap_or(0),
            restart: entry.restart,
        },
    );
    Ok(())
}

pub(super) fn reap_children(state: &mut BrokerState) -> bool {
    let mut reaped_any = false;
    loop {
        let mut status = 0_i32;
        let pid = unsafe {
            libc::syscall(
                libc::SYS_wait4 as libc::c_long,
                -1_i32,
                &mut status as *mut i32,
                libc::WNOHANG,
                std::ptr::null_mut::<libc::rusage>(),
            ) as i32
        };
        if pid > 0 {
            reaped_any = true;
            if let Some(process) = state.running.remove(&pid) {
                if process.session_handle != 0 {
                    if let Ok(console_fd) = ensure_console_fd(state) {
                        let _ = close_console_session(console_fd, process.session_handle);
                    }
                }
                if process.restart {
                    state
                        .retry_after
                        .insert(process.desktop_file_id, Instant::now() + RETRY_BACKOFF);
                }
            }
            continue;
        }
        if pid == 0 || (pid == -1 && last_errno() == libc::ECHILD) {
            break;
        }
        break;
    }
    reaped_any
}

pub(super) fn next_idle_delay(state: &BrokerState) -> Duration {
    let now = Instant::now();
    let retry_delay = state
        .retry_after
        .values()
        .map(|deadline| deadline.saturating_duration_since(now))
        .min()
        .unwrap_or(IDLE_POLL_INTERVAL);
    retry_delay.min(IDLE_POLL_INTERVAL)
}

pub(super) fn spawn_exec(
    exec_path: &str,
    argv: &[String],
    env: &[String],
    logical_admin: bool,
    weight_micros: u64,
    session_handle: u64,
) -> Result<i32, i32> {
    boot_line(format!("runtimed: loader request begin exec={}", exec_path).as_str());
    let argv_storage = build_exec_argv(exec_path, argv);
    let env_storage = build_exec_env(env);
    let request = build_loader_spawn_request(
        exec_path,
        &argv_storage,
        &env_storage,
        logical_admin,
        weight_micros,
        session_handle,
    )?;
    let endpoint = lookup_loader_endpoint()?;
    let mut response = LoaderSpawnResponse::default();
    let call = unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_CALL as libc::c_long,
            endpoint,
            (&request as *const LoaderSpawnRequest) as u64,
            size_of::<LoaderSpawnRequest>() as u64,
            (&mut response as *mut LoaderSpawnResponse) as u64,
            size_of::<LoaderSpawnResponse>() as u64,
        ) as i64
    };
    if call < 0 {
        LOADER_ENDPOINT_CACHE.store(0, Ordering::Relaxed);
        return Err((-call) as i32);
    }
    if call as usize != size_of::<LoaderSpawnResponse>()
        || response.version != LOADER_REQUEST_ABI_VERSION
        || response.op != LOADER_OP_SPAWN_EXEC
    {
        return Err(libc::EINVAL);
    }
    if response.status != 0 {
        return Err(response.status);
    }
    let Ok(pid) = i32::try_from(response.pid) else {
        return Err(libc::EOVERFLOW);
    };
    boot_line(
        format!(
            "runtimed: loader request returned exec={} pid={}",
            exec_path, pid
        )
        .as_str(),
    );
    Ok(pid)
}

fn build_loader_spawn_request(
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
    logical_admin: bool,
    weight_micros: u64,
    session_handle: u64,
) -> Result<LoaderSpawnRequest, i32> {
    let exec_bytes = exec_path.as_bytes();
    if exec_bytes.is_empty()
        || exec_bytes.len() > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || exec_bytes.contains(&0)
    {
        return Err(libc::EINVAL);
    }
    let mut request = LoaderSpawnRequest {
        version: LOADER_REQUEST_ABI_VERSION,
        op: LOADER_OP_SPAWN_EXEC,
        flags: u32::from(logical_admin),
        console_session: session_handle,
        weight_micros: effective_task_weight_micros(weight_micros),
        exec_path_len: exec_bytes.len() as u32,
        argv_count: u16::try_from(argv.len()).map_err(|_| libc::E2BIG)?,
        env_count: u16::try_from(env.len()).map_err(|_| libc::E2BIG)?,
        ..LoaderSpawnRequest::default()
    };
    request.exec_path[..exec_bytes.len()].copy_from_slice(exec_bytes);
    request.argv_bytes_len =
        copy_cstring_blob(argv, &mut request.argv_bytes, LOADER_SPAWN_ARG_BYTES)?;
    request.env_bytes_len =
        copy_cstring_blob(env, &mut request.env_bytes, LOADER_SPAWN_ENV_BYTES)?;
    Ok(request)
}

fn copy_cstring_blob(values: &[CString], dest: &mut [u8], capacity: usize) -> Result<u32, i32> {
    let mut offset = 0usize;
    for value in values {
        let bytes = value.as_bytes();
        let next = offset
            .checked_add(bytes.len())
            .and_then(|value| value.checked_add(1))
            .ok_or(libc::E2BIG)?;
        if next > capacity || next > dest.len() {
            return Err(libc::E2BIG);
        }
        dest[offset..offset + bytes.len()].copy_from_slice(bytes);
        dest[offset + bytes.len()] = 0;
        offset = next;
    }
    u32::try_from(offset).map_err(|_| libc::E2BIG)
}

fn effective_task_weight_micros(weight_micros: u64) -> u64 {
    let requested = if weight_micros == 0 {
        DEFAULT_USER_TASK_WEIGHT_MICROS
    } else {
        weight_micros
    };
    requested.max(MIN_EFFECTIVE_TASK_WEIGHT_MICROS)
}

pub(super) fn lookup_service_endpoint(service_id: u64) -> i64 {
    unsafe {
        libc::syscall(
            SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT as libc::c_long,
            service_id,
        ) as i64
    }
}

fn lookup_loader_endpoint() -> Result<u64, i32> {
    let cached = LOADER_ENDPOINT_CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Ok(cached);
    }
    let endpoint = lookup_service_endpoint(IPC_SERVICE_LOADERD);
    if endpoint < 0 {
        return Err((-endpoint) as i32);
    }
    let endpoint = endpoint as u64;
    if endpoint != 0 {
        LOADER_ENDPOINT_CACHE.store(endpoint, Ordering::Relaxed);
    }
    Ok(endpoint)
}

pub(super) fn loader_endpoint_ready() -> bool {
    lookup_loader_endpoint().is_ok()
}

pub(super) fn stderr_line(message: &str) {
    let mut line = message.as_bytes().to_vec();
    line.push(b'\n');
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            line.as_ptr().cast::<libc::c_void>(),
            line.len(),
        );
    }
}

pub(super) fn terminate_pid(pid: i32) -> Result<(), i32> {
    let rc = unsafe {
        libc::syscall(libc::SYS_tgkill as libc::c_long, pid, pid, libc::SIGKILL) as i32
    };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

// --- console IPC (used by spawn and socket handlers) ---

pub(super) fn create_console_session(
    console_fd: RawFd,
    program_id: u32,
    title: &str,
    exec_path: &str,
) -> Result<u64, i32> {
    let mut request = ConsoleCreateSessionRequest::new(
        program_id,
        title.as_ptr() as u64,
        title.len() as u64,
        exec_path.as_ptr() as u64,
        exec_path.len() as u64,
    );
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_CREATE_SESSION, &mut request)?;
    Ok(request.session_handle)
}

pub(super) fn close_console_session(
    console_fd: RawFd,
    session_handle: u64,
) -> Result<bool, i32> {
    let mut request = ConsoleCloseSessionRequest::new(session_handle);
    match sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_CLOSE_SESSION, &mut request) {
        Ok(()) => Ok(true),
        Err(err) if err == libc::ENOENT || err == libc::EINVAL => Ok(false),
        Err(err) => Err(err),
    }
}

fn set_console_session_state(
    console_fd: RawFd,
    session_handle: u64,
    state: u16,
) -> Result<(), i32> {
    let mut request = ConsoleSetSessionStateRequest::new(session_handle, state);
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_SET_SESSION_STATE, &mut request)
}

fn console_set_focus(console_fd: RawFd, session_handle: u64) -> Result<(), i32> {
    let mut request = ConsoleSetFocusRequest::new(session_handle);
    sessiond_console_ioctl(console_fd, CONSOLE_IOCTL_SET_FOCUS, &mut request)
}

fn sessiond_console_ioctl<T>(console_fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let args = RustosDeviceIoctlBrokerArgs {
        process_id: 0,
        fd: console_fd as u64,
        request: request as u64,
        arg: arg as *mut T as u64,
        reserved0: 0,
    };
    let rc = unsafe {
        libc::syscall(
            SYS_RUSTOS_DEVICE_IOCTL_BROKER as libc::c_long,
            (&args as *const RustosDeviceIoctlBrokerArgs) as u64,
        ) as i64
    };
    if rc < 0 {
        let errno = (-rc) as i32;
        if errno == libc::EPERM {
            return ioctl_with_mut(console_fd, request, arg);
        }
        return Err(errno);
    }
    Ok(())
}

fn open_device(path: &str, flags: usize) -> Result<OwnedFd, i32> {
    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let raw_fd = unsafe {
        libc::syscall(
            SYS_OPENAT as libc::c_long,
            AT_FDCWD,
            path.as_ptr(),
            flags,
            0usize,
        ) as i32
    };
    if raw_fd < 0 {
        return Err(last_errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

pub(super) fn ensure_console_fd(state: &mut BrokerState) -> Result<RawFd, i32> {
    if state.console_fd.is_none() {
        stderr_line("runtimed: console open begin");
        boot_line("runtimed: console open begin");
        let fd = open_device(CONSOLE_PATH, O_RDWR)?;
        stderr_line("runtimed: console open done");
        boot_line("runtimed: console ready");
        state.console_fd = Some(fd);
    }
    Ok(state
        .console_fd
        .as_ref()
        .map(|fd| fd.as_raw_fd())
        .unwrap_or(-1))
}

fn ioctl_with_mut<T>(fd: RawFd, request: usize, arg: &mut T) -> Result<(), i32> {
    let rc = unsafe {
        libc::syscall(SYS_IOCTL as libc::c_long, fd, request, arg as *mut T) as i32
    };
    if rc < 0 {
        return Err(last_errno());
    }
    Ok(())
}

fn build_exec_argv(exec_path: &str, argv: &[String]) -> Vec<CString> {
    use super::{MAX_EXEC_ARG_COUNT};
    if argv.is_empty() {
        return vec![c_string_or_fallback(exec_path, "/")];
    }
    let mut storage = argv
        .iter()
        .take(MAX_EXEC_ARG_COUNT)
        .filter(|arg| valid_exec_text(arg.as_str(), false))
        .filter_map(|arg| CString::new(arg.as_str()).ok())
        .collect::<Vec<_>>();
    if storage.is_empty() {
        storage.push(c_string_or_fallback(exec_path, "/"));
    }
    storage
}

fn build_exec_env(extra_env: &[String]) -> Vec<CString> {
    use runtime_control::{DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope, load_runtime_default_env};
    let default_env =
        load_runtime_default_env(DEFAULT_RUNTIME_ENV_REGISTRY_PATH, RuntimeEnvScope::Runtime)
            .unwrap_or_default();
    build_exec_env_with_defaults(extra_env, &default_env)
}

fn build_exec_env_with_defaults(extra_env: &[String], default_env: &[String]) -> Vec<CString> {
    use super::{MAX_EXEC_ENV_COUNT};
    let mut env = extra_env
        .iter()
        .filter(|item| valid_exec_text(item.as_str(), true))
        .take(MAX_EXEC_ENV_COUNT)
        .cloned()
        .collect::<Vec<_>>();
    for item in default_env {
        push_env_if_missing(&mut env, item);
    }
    env.into_iter()
        .filter_map(|item| CString::new(item).ok())
        .collect()
}

fn push_env_if_missing(env: &mut Vec<String>, item: &str) {
    use super::MAX_EXEC_ENV_COUNT;
    if env.len() >= MAX_EXEC_ENV_COUNT {
        return;
    }
    let key = env_key(item);
    if env.iter().any(|candidate| env_key(candidate) == key) {
        return;
    }
    env.push(item.to_string());
}

fn env_key(value: &str) -> &str {
    value.split_once('=').map(|(key, _)| key).unwrap_or(value)
}

fn c_string_or_fallback(value: &str, fallback: &str) -> CString {
    CString::new(value).unwrap_or_else(|_| CString::new(fallback).unwrap())
}

fn valid_exec_text(value: &str, require_env_assignment: bool) -> bool {
    use super::MAX_EXEC_TEXT_BYTES;
    if value.is_empty() || value.len() > MAX_EXEC_TEXT_BYTES {
        return false;
    }
    if !value
        .bytes()
        .all(|byte| matches!(byte, b' '..=b'~') && byte != b'\\')
    {
        return false;
    }
    !require_env_assignment || valid_env_assignment(value)
}

fn valid_env_assignment(value: &str) -> bool {
    let Some((key, _)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

pub(super) fn is_permanent_launch_failure(errno: i32) -> bool {
    matches!(
        errno,
        libc::EOPNOTSUPP | libc::ENOEXEC | libc::EINVAL | libc::ENOENT | libc::EACCES
    )
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::{build_exec_argv, build_exec_env_with_defaults};

    #[test]
    fn build_exec_argv_defaults_to_exec_path() {
        let argv = build_exec_argv("apps/demo/demo.elf", &[]);
        assert_eq!(argv.len(), 1);
        assert_eq!(argv[0].to_str().unwrap(), "apps/demo/demo.elf");
    }

    #[test]
    fn build_exec_env_preserves_explicit_values_and_adds_defaults() {
        let defaults = [
            String::from("PATH=/bin:/usr/bin:/usr/local/bin"),
            String::from("HOME=/home/user"),
            String::from("XDG_RUNTIME_DIR=/run/user/1000"),
            String::from("WAYLAND_DISPLAY=wayland-0"),
            String::from("XDG_SESSION_TYPE=wayland"),
            String::from("XDG_CURRENT_DESKTOP=RustOS"),
        ];
        let env = build_exec_env_with_defaults(
            &[
                String::from("PATH=/custom/bin"),
                String::from("XDG_RUNTIME_DIR=/run/custom"),
            ],
            &defaults,
        );
        let values = env
            .iter()
            .map(|item| item.to_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(values.iter().any(|item| item == "PATH=/custom/bin"));
        assert!(values.iter().any(|item| item == "XDG_RUNTIME_DIR=/run/custom"));
        assert!(values.iter().any(|item| item == "HOME=/home/user"));
        assert!(values.iter().any(|item| item == "WAYLAND_DISPLAY=wayland-0"));
        assert!(values.iter().any(|item| item == "XDG_SESSION_TYPE=wayland"));
        assert!(values.iter().any(|item| item == "XDG_CURRENT_DESKTOP=RustOS"));
        assert!(!values.iter().any(|item| item == "PATH=/bin:/usr/bin:/usr/local/bin"));
        assert!(!values.iter().any(|item| item == "XDG_RUNTIME_DIR=/run/user/1000"));
    }
}
