use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Instant;

use runtime_control::RuntimeRunningProgram;
use rustos_user_abi::syscall::IPC_SERVICE_UISERVER;

use super::{
    boot_line, LAUNCH_TARGET_NEW_SESSION, MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK,
    MAX_RUNTIME_CLIENTS_PER_TICK, MAX_RUNTIME_PROGRAMS, OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH,
    OP_REQUEST_TERMINATE, OP_SNAPSHOT_RUNNING_PROGRAMS, PROTOCOL_VERSION,
    READY_COMPONENT_UI_SERVER, RETRY_BACKOFF, TERMINATE_TARGET_PID, TERMINATE_TARGET_SESSION,
    UI_SERVER_EXEC_PATH,
};
use super::{BrokerState, LaunchEntry, RuntimeRequest, RuntimeResponse};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimePeerCredentials {
    pid: i32,
}

pub(super) fn bind_listener(path: &str) -> Result<UnixListener, i32> {
    let started_at = Instant::now();
    boot_line("runtimed: bind listener begin");

    let socket_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if socket_fd < 0 {
        return Err(last_errno());
    }
    boot_line(
        format!(
            "runtimed: bind listener socket elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let unlink_rc = unsafe { libc::unlink(path.as_ptr()) };
    if unlink_rc < 0 {
        let err = last_errno();
        if err != libc::ENOENT {
            let _ = unsafe { libc::close(socket_fd) };
            return Err(err);
        }
    }
    boot_line(
        format!(
            "runtimed: bind listener unlink elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    let path_bytes = path.as_bytes_with_nul();
    let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    if path_bytes.len() > addr.sun_path.len() {
        let _ = unsafe { libc::close(socket_fd) };
        return Err(libc::ENAMETOOLONG);
    }
    for (index, byte) in path_bytes.iter().enumerate() {
        addr.sun_path[index] = *byte as libc::c_char;
    }

    let bind_rc = unsafe {
        libc::bind(
            socket_fd,
            (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if bind_rc < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }
    boot_line(
        format!(
            "runtimed: bind listener bind elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    if unsafe { libc::listen(socket_fd, 16) } < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }
    boot_line(
        format!(
            "runtimed: bind listener listen elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );

    Ok(unsafe { UnixListener::from_raw_fd(socket_fd) })
}

pub(super) fn service_listener(listener: &UnixListener, state: &mut BrokerState) -> bool {
    let mut did_work = false;
    for _ in 0..MAX_RUNTIME_CLIENTS_PER_TICK {
        match accept_runtime_client(listener) {
            Ok((mut stream, peer)) => {
                did_work = true;
                if let Err(err) = service_stream(&mut stream, peer, state) {
                    let _ = super::util::write_response(
                        &mut stream,
                        RuntimeResponse {
                            version: PROTOCOL_VERSION,
                            op: 0,
                            status: -err,
                            count: 0,
                        },
                    );
                }
                if state.ui_ready && !state.launch_catalog_loaded {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => {
                observability_client::error!(
                    "runtimed",
                    service,
                    "accept failed: errno={}",
                    super::util::io_errno(err)
                );
                break;
            }
        }
    }
    did_work
}

fn accept_runtime_client(
    listener: &UnixListener,
) -> std::io::Result<(UnixStream, RuntimePeerCredentials)> {
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let peer = runtime_peer_credentials(stream.as_raw_fd())?;
    Ok((stream, peer))
}

fn runtime_peer_credentials(fd: i32) -> std::io::Result<RuntimePeerCredentials> {
    let mut credentials = unsafe { std::mem::zeroed::<libc::ucred>() };
    let mut credentials_len = size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast::<libc::c_void>(),
            &mut credentials_len,
        )
    };
    if status < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if credentials_len as usize != size_of::<libc::ucred>() || credentials.pid <= 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EPROTO));
    }
    Ok(RuntimePeerCredentials {
        pid: credentials.pid,
    })
}

fn runtime_request_role_authorized(op: u16, is_uiserver: bool, is_logical_admin: bool) -> bool {
    match op {
        OP_NOTIFY_READY => is_uiserver,
        OP_SNAPSHOT_RUNNING_PROGRAMS | OP_REQUEST_LAUNCH_PATH | OP_REQUEST_TERMINATE => {
            is_uiserver || is_logical_admin
        }
        _ => false,
    }
}

fn authorize_runtime_request(
    request: &RuntimeRequest,
    peer: RuntimePeerCredentials,
    state: &BrokerState,
) -> Result<(), i32> {
    if peer.pid <= 0 {
        return Err(libc::EACCES);
    }
    let peer_pid = peer.pid as u64;
    let is_uiserver =
        rustos_svc_runtime::ipc::validate_service_owner(IPC_SERVICE_UISERVER, peer_pid) >= 0;
    let is_logical_admin = state
        .running
        .get(&peer.pid)
        .is_some_and(|process| process.logical_admin);
    if runtime_request_role_authorized(request.op, is_uiserver, is_logical_admin) {
        Ok(())
    } else {
        Err(libc::EACCES)
    }
}

fn service_stream(
    stream: &mut UnixStream,
    peer: RuntimePeerCredentials,
    state: &mut BrokerState,
) -> Result<(), i32> {
    let mut request = RuntimeRequest::default();
    read_exact_retry(stream, super::util::as_bytes_mut(&mut request))?;
    if request.version != PROTOCOL_VERSION {
        return Err(libc::EPROTO);
    }
    super::util::validate_runtime_request(&request)?;
    authorize_runtime_request(&request, peer, state)?;

    match request.op {
        OP_SNAPSHOT_RUNNING_PROGRAMS => handle_snapshot(stream, state),
        OP_REQUEST_LAUNCH_PATH => handle_launch(stream, state, request),
        OP_REQUEST_TERMINATE => handle_terminate(stream, state, request),
        OP_NOTIFY_READY => handle_ready(stream, state, request),
        _ => Err(libc::EINVAL),
    }
}

fn read_exact_retry(stream: &mut UnixStream, mut bytes: &mut [u8]) -> Result<(), i32> {
    use super::SERVICE_REQUEST_TIMEOUT;
    let deadline = Instant::now() + SERVICE_REQUEST_TIMEOUT;
    while !bytes.is_empty() {
        match stream.read(bytes) {
            Ok(0) => return Err(libc::EPIPE),
            Ok(read) => {
                let remaining = bytes;
                bytes = &mut remaining[read..];
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(libc::ETIMEDOUT);
                }
                std::thread::yield_now();
            }
            Err(err) => return Err(super::util::io_errno(err)),
        }
    }
    Ok(())
}

fn handle_snapshot(stream: &mut UnixStream, state: &BrokerState) -> Result<(), i32> {
    let mut programs = state
        .running
        .values()
        .take(MAX_RUNTIME_PROGRAMS)
        .map(|program| {
            let mut snapshot = RuntimeRunningProgram::default();
            snapshot.pid = program.pid as u64;
            snapshot.program_id = 0;
            snapshot.session_handle = program.session_handle;
            super::util::copy_ascii_into(&mut snapshot.desktop_file_id, &program.desktop_file_id);
            super::util::copy_ascii_into(&mut snapshot.display_name, &program.display_name);
            super::util::copy_ascii_into(&mut snapshot.exec_path, &program.exec);
            snapshot
        })
        .collect::<Vec<_>>();
    programs.sort_by_key(|program| program.pid);

    super::util::write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            status: 0,
            count: u32::try_from(programs.len()).unwrap_or(u32::MAX),
        },
    )?;
    if !programs.is_empty() {
        stream
            .write_all(unsafe {
                std::slice::from_raw_parts(
                    programs.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(programs.as_slice()),
                )
            })
            .map_err(super::util::io_errno)?;
    }
    Ok(())
}

fn handle_launch(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    if request.target_kind != LAUNCH_TARGET_NEW_SESSION {
        return Err(libc::EOPNOTSUPP);
    }
    let target = super::util::request_path(&request)?;
    let metadata = super::catalog::resolve_program_request(state, target.as_str())?;
    if !super::catalog::runtime_deps_satisfied(
        &metadata.runtime_deps,
        &super::catalog::running_packages(state),
        &state.launched_once,
    ) {
        return Err(libc::EAGAIN);
    }
    super::spawn::spawn_tracked_process(
        state,
        LaunchEntry {
            package_id: metadata.package_id,
            desktop_file_id: metadata.desktop_file_id,
            display_name: metadata.display_name,
            exec: metadata.exec,
            runtime_deps: metadata.runtime_deps,
            restart: false,
            weight_micros: metadata.weight_micros,
            logical_admin: metadata.logical_admin,
            console_hosted: metadata.console_hosted,
            args: metadata.args,
            env: metadata.env,
        },
    )?;
    super::util::write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_LAUNCH_PATH,
            status: 0,
            count: 0,
        },
    )
}

fn handle_terminate(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    let mut terminated = false;
    match request.target_kind {
        TERMINATE_TARGET_PID => {
            let pid = i32::try_from(request.target_value).map_err(|_| libc::EINVAL)?;
            super::spawn::terminate_pid(pid)?;
            terminated = true;
        }
        TERMINATE_TARGET_SESSION => {
            if request.target_value == 0 {
                return Err(libc::EINVAL);
            }
            let pids = state
                .running
                .values()
                .filter(|program| program.session_handle == request.target_value)
                .map(|program| program.pid)
                .collect::<Vec<_>>();
            for pid in pids {
                match super::spawn::terminate_pid(pid) {
                    Ok(()) => terminated = true,
                    Err(err) if err == libc::ESRCH => {
                        state.running.remove(&pid);
                    }
                    Err(err) => return Err(err),
                }
            }
            if super::spawn::close_console_session(
                super::spawn::ensure_console_fd(state)?,
                request.target_value,
            )? {
                state.session_runtime.remove_session(request.target_value);
                super::session::clear_focused_session_if(state, request.target_value);
                terminated = true;
            }
        }
        _ => return Err(libc::EOPNOTSUPP),
    }

    if !terminated {
        return Err(libc::ESRCH);
    }

    super::util::write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: OP_REQUEST_TERMINATE,
            status: 0,
            count: 0,
        },
    )
}

fn handle_ready(
    stream: &mut UnixStream,
    state: &mut BrokerState,
    request: RuntimeRequest,
) -> Result<(), i32> {
    if request.target_kind != READY_COMPONENT_UI_SERVER {
        return Err(libc::EOPNOTSUPP);
    }
    state.ui_ready = true;
    observability_client::info!("runtimed", service, "ui ready received");
    boot_line("runtimed: ui ready received");
    // RuntimeClient::notify_ui_ready is deliberately one-way; the compositor
    // closes this stream after the request, so replying here can stall boot.
    let _ = stream;
    Ok(())
}

pub(super) fn ensure_policy_launches(state: &mut BrokerState) -> bool {
    let now = Instant::now();
    let running_programs = state
        .running
        .values()
        .map(|program| program.desktop_file_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let running_packages = super::catalog::running_packages(state);
    let mut pending_service_launch = false;
    let mut pending_desktop_launch = false;
    for entry in &state.launch_entries {
        if state
            .permanent_launch_failures
            .contains_key(entry.desktop_file_id.as_str())
        {
            continue;
        }
        if state
            .retry_after
            .get(entry.desktop_file_id.as_str())
            .is_some_and(|deadline| now < *deadline)
        {
            continue;
        }
        let already_satisfied = if entry.restart {
            running_packages.contains(&entry.package_id)
        } else {
            running_programs.contains(&entry.desktop_file_id)
                || state.launched_once.contains(entry.package_id.as_str())
        };
        if already_satisfied {
            continue;
        }
        if !super::catalog::runtime_deps_satisfied(
            &entry.runtime_deps,
            &running_packages,
            &state.launched_once,
        ) {
            continue;
        }
        if entry.exec.starts_with("services/") {
            pending_service_launch = true;
        } else {
            pending_desktop_launch = true;
        }
    }

    let mut attempts = 0usize;
    let mut launched_any = false;
    for entry in state.launch_entries.clone() {
        if attempts >= MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK {
            break;
        }
        if state
            .permanent_launch_failures
            .contains_key(entry.desktop_file_id.as_str())
        {
            continue;
        }
        if state
            .retry_after
            .get(entry.desktop_file_id.as_str())
            .is_some_and(|deadline| now < *deadline)
        {
            continue;
        }
        if pending_service_launch
            && !entry.exec.starts_with("services/")
            && (!state.ui_ready || !pending_desktop_launch)
        {
            continue;
        }
        if !state.ui_ready && entry.exec != UI_SERVER_EXEC_PATH {
            continue;
        }
        if state.ui_ready
            && pending_desktop_launch
            && entry.exec.starts_with("services/")
            && entry.exec != UI_SERVER_EXEC_PATH
        {
            continue;
        }
        if !entry.exec.starts_with("services/") && !super::spawn::loader_endpoint_ready() {
            state.retry_after.insert(
                entry.desktop_file_id.clone(),
                Instant::now() + RETRY_BACKOFF,
            );
            continue;
        }

        if entry.restart {
            if running_packages.contains(&entry.package_id) {
                continue;
            }
        } else if running_programs.contains(&entry.desktop_file_id)
            || state.launched_once.contains(entry.package_id.as_str())
        {
            continue;
        }
        if !super::catalog::runtime_deps_satisfied(
            &entry.runtime_deps,
            &running_packages,
            &state.launched_once,
        ) {
            continue;
        }

        attempts += 1;
        match super::spawn::spawn_tracked_process(state, entry.clone()) {
            Ok(()) => {
                observability_client::info!(
                    "runtimed",
                    service,
                    "launched {} ({})",
                    entry.desktop_file_id,
                    entry.exec
                );
                launched_any = true;
                if !entry.restart {
                    state.launched_once.insert(entry.package_id);
                }
            }
            Err(err) => {
                if super::spawn::is_permanent_launch_failure(err) {
                    observability_client::warn!(
                        "runtimed",
                        service,
                        "launch {} ({}) disabled after permanent failure: errno={err}",
                        entry.desktop_file_id,
                        entry.exec
                    );
                    state
                        .permanent_launch_failures
                        .insert(entry.desktop_file_id, err);
                } else {
                    observability_client::error!(
                        "runtimed",
                        service,
                        "launch {} ({}) failed: errno={err}",
                        entry.desktop_file_id,
                        entry.exec
                    );
                    state
                        .retry_after
                        .insert(entry.desktop_file_id, Instant::now() + RETRY_BACKOFF);
                }
            }
        }
    }
    launched_any
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use super::{
        runtime_request_role_authorized, OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH,
        OP_REQUEST_TERMINATE, OP_SNAPSHOT_RUNNING_PROGRAMS,
    };

    #[test]
    fn runtime_control_mutations_require_live_uiserver_or_logical_admin() {
        for op in [
            OP_SNAPSHOT_RUNNING_PROGRAMS,
            OP_REQUEST_LAUNCH_PATH,
            OP_REQUEST_TERMINATE,
        ] {
            assert!(runtime_request_role_authorized(op, true, false));
            assert!(runtime_request_role_authorized(op, false, true));
            assert!(!runtime_request_role_authorized(op, false, false));
        }

        assert!(runtime_request_role_authorized(
            OP_NOTIFY_READY,
            true,
            false
        ));
        assert!(!runtime_request_role_authorized(
            OP_NOTIFY_READY,
            false,
            true
        ));
        assert!(!runtime_request_role_authorized(u16::MAX, true, true));
    }
}
