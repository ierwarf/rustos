//! Runtime-control socket admission and bounded command dispatch.
//!
//! - **Owner:** `runtimed` owns desktop/session control policy.
//! - **Boundary:** Local socket peers, frame lengths, command payloads, and
//!   claimed process identities are untrusted.
//! - **Lifecycle:** Accept, obtain kernel peer credentials, authorize live
//!   role, parse exact frame, dispatch, reply, and close/revoke on process exit.
//! - **Concurrency:** Connection work is bounded; no global runtime-state lock
//!   is held across blocking I/O or process-control IPC. `accept4` and
//!   `SO_PEERCRED` run on a dedicated acceptor thread and reach the broker
//!   through a bounded queue: each AF_UNIX syscall is a netd IPC with its own
//!   reply deadline, and the broker loop also serves the session endpoint, so it
//!   must not issue them. Reads of already-admitted clients and reply writes
//!   still run on the broker (see `docs/ai/ui-startup-readiness.md`, deferred).
//! - **Failure:** Partial/malformed frame, unauthorized peer, timeout, queue
//!   pressure, command failure, and owner exit close without state confusion.
//! - **Forbidden:** No path/name authorization, unbounded allocation, implicit
//!   admin, ambiguous framing, or stale uiserver role.
//! - **Evidence:** `runtime-control-ingress`.
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use runtime_control::RuntimeRunningProgram;
use rustos_user_abi::syscall::IPC_SERVICE_UISERVER;

use super::{
    boot_line, op_carries_program_payload, running_programs_digest, LAUNCH_TARGET_NEW_SESSION,
    MAX_LAUNCH_RETRY_BACKOFF, MAX_PENDING_RUNTIME_CLIENTS, MAX_POLICY_LAUNCH_ATTEMPTS_PER_TICK,
    MAX_RUNTIME_CLIENTS_PER_TICK, MAX_RUNTIME_PROGRAMS, MAX_RUNTIME_WATCHERS, OP_NOTIFY_READY,
    OP_REQUEST_LAUNCH_PATH, OP_REQUEST_TERMINATE, OP_SNAPSHOT_RUNNING_PROGRAMS,
    OP_WATCH_RUNNING_PROGRAMS, PROTOCOL_VERSION, READY_COMPONENT_UI_SERVER, RETRY_BACKOFF,
    RUNTIME_WATCH_MAX_WAIT_MS, SERVICE_REQUEST_TIMEOUT, STORAGE_NOT_READY_RETRY_BACKOFF,
    TERMINATE_TARGET_PID, TERMINATE_TARGET_SESSION, UI_SERVER_EXEC_PATH,
};
use super::{BrokerState, LaunchEntry, RuntimeRequest, RuntimeResponse};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimePeerCredentials {
    pid: i32,
}

struct PendingRuntimeClient {
    stream: UnixStream,
    peer: RuntimePeerCredentials,
    request: RuntimeRequest,
    bytes_read: usize,
    deadline: Instant,
}

/// A watch whose reply is deliberately owed rather than sent.
///
/// The caller asked to be told when the running set stops matching
/// `known_digest`. Until that happens there is nothing truthful to say, so the
/// connection is retained and answered later - the reply capability is the open
/// socket. `deadline` is the point at which the server re-arms the caller
/// instead of continuing to hold it, which is what keeps a held socket
/// distinguishable from a hung server.
struct RuntimeWatchClient {
    stream: UnixStream,
    known_digest: u64,
    deadline: Instant,
}

/// A connection the acceptor thread has accepted and authenticated at the
/// kernel level (peer credentials), not yet read from.
struct AcceptedClient {
    stream: UnixStream,
    peer: RuntimePeerCredentials,
    accepted_at: Instant,
}

pub(super) struct RuntimeConnections {
    pending: VecDeque<PendingRuntimeClient>,
    watchers: VecDeque<RuntimeWatchClient>,
    /// Connections handed over by the acceptor thread. The broker thread never
    /// calls `accept4`/`SO_PEERCRED` itself: on RustOS every AF_UNIX syscall is
    /// an IPC to netd with its own reply deadline, so a stalled netd would
    /// freeze the one loop that also drains the session endpoint.
    accepted: Receiver<AcceptedClient>,
    acceptor_stop_reported: bool,
}

impl RuntimeConnections {
    /// Move the listener onto its own acceptor thread and return the broker's
    /// side of the hand-off. The queue is bounded, so a slow broker applies
    /// back-pressure to the listener backlog instead of pinning descriptors.
    pub(super) fn with_acceptor(listener: UnixListener) -> Result<Self, i32> {
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_RUNTIME_CLIENTS);
        std::thread::Builder::new()
            .name(String::from("runtimed-accept"))
            .spawn(move || accept_loop(&listener, |client| sender.send(client).is_ok()))
            .map_err(|err| err.raw_os_error().unwrap_or(libc::EAGAIN))?;
        Ok(Self::from_receiver(receiver))
    }

    fn from_receiver(accepted: Receiver<AcceptedClient>) -> Self {
        Self {
            pending: VecDeque::with_capacity(MAX_PENDING_RUNTIME_CLIENTS),
            watchers: VecDeque::with_capacity(MAX_RUNTIME_WATCHERS),
            accepted,
            acceptor_stop_reported: false,
        }
    }
}

/// Pause between accept polls and after a failed accept. A failed `accept4`
/// that returns only after an IPC deadline must not become a hot loop.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(10);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Accept until `deliver` reports the broker is gone. Never exits on an
/// accept error: the control socket must outlive a transient netd stall.
fn accept_loop(listener: &UnixListener, mut deliver: impl FnMut(AcceptedClient) -> bool) {
    let mut consecutive_errors = 0_u32;
    loop {
        match accept_runtime_client(listener) {
            Ok((stream, peer)) => {
                consecutive_errors = 0;
                let client = AcceptedClient {
                    stream,
                    peer,
                    accepted_at: Instant::now(),
                };
                if !deliver(client) {
                    return;
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(err) => {
                // Log the first failure of a run, then every 64th, so a
                // persistent fault is visible without flooding the console.
                if consecutive_errors % 64 == 0 {
                    observability_client::error!(
                        "runtimed",
                        service,
                        "accept failed: errno={} consecutive={}",
                        super::util::io_errno(err),
                        consecutive_errors + 1
                    );
                }
                consecutive_errors = consecutive_errors.saturating_add(1);
                std::thread::sleep(ACCEPT_ERROR_BACKOFF);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestReadProgress {
    Pending,
    Complete,
}

pub(super) fn bind_listener(path: &str) -> Result<UnixListener, i32> {
    let started_at = Instant::now();
    bind_timing("begin", started_at);

    // Validate the path before obtaining the descriptor. CString::new can
    // reject an embedded NUL; doing it after socket() leaked that descriptor
    // on the error path of an otherwise fallible bootstrap operation.
    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
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
    bind_timing("socket", started_at);

    let unlink_rc = unsafe { libc::unlink(path.as_ptr()) };
    if unlink_rc < 0 {
        let err = last_errno();
        if err != libc::ENOENT {
            let _ = unsafe { libc::close(socket_fd) };
            return Err(err);
        }
    }
    bind_timing("unlink", started_at);

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
    bind_timing("bind", started_at);

    if unsafe { libc::listen(socket_fd, 16) } < 0 {
        let err = last_errno();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }
    bind_timing("listen", started_at);

    Ok(unsafe { UnixListener::from_raw_fd(socket_fd) })
}

/// Keep the few bootstrap socket checkpoints visible in the kernel timestamp
/// stream. They happen once per service start and make a blocked local-socket
/// setup distinguishable from scheduler or loader delay without enabling the
/// high-volume runtime trace.
fn bind_timing(stage: &str, started_at: Instant) {
    super::debug_line(
        format!(
            "runtimed: bind listener stage={stage} elapsed_ms={}",
            started_at.elapsed().as_millis()
        )
        .as_str(),
    );
}

/// Adopt connections the acceptor thread has queued, then make progress on the
/// ones already pending. Never performs a socket-creation or accept syscall, so
/// its cost is bounded by the (nonblocking) reads of clients already admitted.
pub(super) fn service_listener(
    connections: &mut RuntimeConnections,
    state: &mut BrokerState,
) -> bool {
    let mut did_work = service_pending_clients(connections, state);
    for _ in 0..MAX_RUNTIME_CLIENTS_PER_TICK {
        if connections.pending.len() >= MAX_PENDING_RUNTIME_CLIENTS {
            break;
        }
        match connections.accepted.try_recv() {
            Ok(accepted) => {
                did_work = true;
                connections.pending.push_back(PendingRuntimeClient {
                    stream: accepted.stream,
                    peer: accepted.peer,
                    request: RuntimeRequest::default(),
                    bytes_read: 0,
                    // The request clock starts when the peer connected, not
                    // when this pass got around to it.
                    deadline: accepted.accepted_at + SERVICE_REQUEST_TIMEOUT,
                });
                if state.ui_ready && !state.launch_catalog_loaded {
                    break;
                }
            }
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                // The acceptor thread ended, so the control socket is dead.
                // Say so once per pass rather than pretend it is idle.
                if !connections.acceptor_stop_reported {
                    connections.acceptor_stop_reported = true;
                    observability_client::error!(
                        "runtimed",
                        service,
                        "control socket acceptor stopped"
                    );
                }
                break;
            }
        }
    }
    did_work
}

fn service_pending_clients(connections: &mut RuntimeConnections, state: &mut BrokerState) -> bool {
    let mut did_work = false;
    let service_count = connections.pending.len().min(MAX_RUNTIME_CLIENTS_PER_TICK);
    for _ in 0..service_count {
        let Some(mut client) = connections.pending.pop_front() else {
            break;
        };
        if Instant::now() >= client.deadline {
            write_runtime_error(&mut client.stream, libc::ETIMEDOUT);
            did_work = true;
            continue;
        }
        match read_request_progress(&mut client) {
            Ok(RequestReadProgress::Pending) => {
                connections.pending.push_back(client);
            }
            Ok(RequestReadProgress::Complete) => {
                did_work = true;
                if let Err(err) = admit_request(&client.request, client.peer, state) {
                    write_runtime_error(&mut client.stream, err);
                    continue;
                }
                if client.request.op == OP_WATCH_RUNNING_PROGRAMS {
                    begin_running_program_watch(client, state, &mut connections.watchers);
                    continue;
                }
                if let Err((err, response_started)) =
                    dispatch_request(&mut client.stream, client.request, state)
                {
                    // A nonblocking write_all can send a *prefix* before it
                    // fails with EAGAIN. Appending another error frame would
                    // corrupt the response stream. Only pre-write rejections
                    // are allowed to transmit a fresh terminal error.
                    if !response_started {
                        write_runtime_error(&mut client.stream, err);
                    }
                }
            }
            Err(err) => {
                did_work = true;
                write_runtime_error(&mut client.stream, err);
            }
        }
    }
    did_work
}

/// Retain a watch whose answer is not yet true, or answer it now.
///
/// A watch that already differs from what the caller holds is answered on the
/// spot; there is no reason to park a reply that is ready. Past the watcher
/// budget the same thing happens for a different reason - the server refuses to
/// pin an unbounded number of descriptors, and a caller that is answered
/// immediately simply degrades to the polling it did before.
fn begin_running_program_watch(
    mut client: PendingRuntimeClient,
    state: &BrokerState,
    watchers: &mut VecDeque<RuntimeWatchClient>,
) {
    let programs = running_program_snapshot(state);
    let known_digest = client.request.target_value;
    if running_programs_digest(&programs) != known_digest || watchers.len() >= MAX_RUNTIME_WATCHERS
    {
        // Once the response starts, a failed nonblocking write may already
        // have sent a partial header or payload. Drop the socket rather than
        // appending a second, misleading error frame.
        let _ = write_running_programs(
            &mut client.stream, OP_WATCH_RUNNING_PROGRAMS, &programs,
        );
        return;
    }

    let wait_ms = u64::from(client.request.wait_ms.min(RUNTIME_WATCH_MAX_WAIT_MS));
    watchers.push_back(RuntimeWatchClient {
        stream: client.stream,
        known_digest,
        deadline: Instant::now() + Duration::from_millis(wait_ms),
    });
}

impl RuntimeConnections {
    /// The soonest a parked watch must be re-armed, for the idle budget.
    ///
    /// Runtimed's visiting interval is currently far shorter than any watch
    /// budget, so this never shortens the wait today. It is folded in anyway
    /// because the rule is that a service holding a reply capability owes its
    /// deadline to its own idle wait - a broker that later earns a longer idle
    /// interval must not silently start sleeping through these.
    pub(super) fn earliest_watch_deadline(&self) -> Option<Instant> {
        self.watchers.iter().map(|watcher| watcher.deadline).min()
    }
}

/// Answer every parked watch the running set has moved past, and re-arm the
/// ones whose budget ran out.
///
/// Called once per broker pass, so a watcher learns about a launch or an exit
/// within one pass rather than within its own polling interval. The snapshot is
/// built at most once per pass and only while somebody is actually watching, so
/// an unwatched broker pays nothing for this.
pub(super) fn service_running_program_watchers(
    connections: &mut RuntimeConnections,
    state: &BrokerState,
) -> bool {
    if connections.watchers.is_empty() {
        return false;
    }

    let programs = running_program_snapshot(state);
    let digest = running_programs_digest(&programs);
    let now = Instant::now();
    let mut answered = false;
    for _ in 0..connections.watchers.len() {
        let Some(mut watcher) = connections.watchers.pop_front() else {
            break;
        };
        if digest == watcher.known_digest && now < watcher.deadline {
            connections.watchers.push_back(watcher);
            continue;
        }
        answered = true;
        // The watcher owns exactly one terminal frame. A partial failed
        // write is terminal for this socket; never append an error frame.
        let _ = write_running_programs(
            &mut watcher.stream, OP_WATCH_RUNNING_PROGRAMS, &programs,
        );
    }
    answered
}

fn read_request_progress(client: &mut PendingRuntimeClient) -> Result<RequestReadProgress, i32> {
    let request_bytes = super::util::as_bytes_mut(&mut client.request);
    while client.bytes_read < request_bytes.len() {
        match client.stream.read(&mut request_bytes[client.bytes_read..]) {
            Ok(0) => return Err(libc::EPIPE),
            Ok(read) => client.bytes_read += read,
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                return Ok(RequestReadProgress::Pending);
            }
            Err(err) => return Err(super::util::io_errno(err)),
        }
    }
    Ok(RequestReadProgress::Complete)
}

fn write_runtime_error(stream: &mut UnixStream, err: i32) {
    let _ = super::util::write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op: 0,
            status: -err,
            count: 0,
        },
    );
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
        OP_SNAPSHOT_RUNNING_PROGRAMS
        | OP_WATCH_RUNNING_PROGRAMS
        | OP_REQUEST_LAUNCH_PATH
        | OP_REQUEST_TERMINATE => is_uiserver || is_logical_admin,
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

/// Everything a request must survive before any handler sees it: the wire
/// version, the shape rules for its opcode, and the peer's live role. Split
/// from dispatch because a watch is admitted the same way as every other
/// request but is not answered the same way.
fn admit_request(
    request: &RuntimeRequest,
    peer: RuntimePeerCredentials,
    state: &BrokerState,
) -> Result<(), i32> {
    if request.version != PROTOCOL_VERSION {
        return Err(libc::EPROTO);
    }
    super::util::validate_runtime_request(request)?;
    authorize_runtime_request(request, peer, state)
}

/// The second result bit tells the broker whether an outbound response may
/// already have partially reached the peer. Only a pre-write policy rejection
/// can be followed by a fresh error envelope on the same stream.
fn dispatch_request(
    stream: &mut UnixStream,
    request: RuntimeRequest,
    state: &mut BrokerState,
) -> Result<(), (i32, bool)> {
    let mut response_started = false;
    let result = match request.op {
        OP_SNAPSHOT_RUNNING_PROGRAMS => {
            response_started = true;
            write_running_programs(stream, OP_SNAPSHOT_RUNNING_PROGRAMS,
                &running_program_snapshot(state))
        }
        OP_REQUEST_LAUNCH_PATH => handle_launch(stream, state, request, &mut response_started),
        OP_REQUEST_TERMINATE => handle_terminate(stream, state, request, &mut response_started),
        OP_NOTIFY_READY => handle_ready(stream, state, request),
        _ => Err(libc::EINVAL),
    };
    result.map_err(|err| (err, response_started))
}

/// The exact running-program array a reply would carry, in the exact order it
/// would carry it. Both the snapshot reply and the watch change edge read this,
/// so a watcher's digest is taken over the same bytes the caller will hash.
fn running_program_snapshot(state: &BrokerState) -> Vec<RuntimeRunningProgram> {
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
    programs
}

fn write_running_programs(
    stream: &mut UnixStream,
    op: u16,
    programs: &[RuntimeRunningProgram],
) -> Result<(), i32> {
    debug_assert!(op_carries_program_payload(op));
    super::util::write_response(
        stream,
        RuntimeResponse {
            version: PROTOCOL_VERSION,
            op,
            status: 0,
            count: u32::try_from(programs.len()).unwrap_or(u32::MAX),
        },
    )?;
    if !programs.is_empty() {
        stream
            .write_all(unsafe {
                std::slice::from_raw_parts(
                    programs.as_ptr().cast::<u8>(),
                    std::mem::size_of_val(programs),
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
    response_started: &mut bool,
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
            private_smp_qualification: None,
        },
    )?;
    *response_started = true;
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
    response_started: &mut bool,
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
                        if let Some(process) = state.running.remove(&pid) {
                            // Removing the record prevents the reaper from
                            // refunding it later; settle its session now.
                            super::spawn::release_failed_session(state, Some(process.session_handle));
                        }
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

    *response_started = true;
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
    // Record the edge before acknowledging it: the ack means "runtimed holds
    // this", so it must not be sent for state that could still be lost.
    // Retries are idempotent; only the first one is logged.
    if !state.ui_ready {
        state.ui_ready = true;
        observability_client::info!("runtimed", service, "ui ready received");
        boot_line("runtimed: ui ready received");
    }
    // A client that already gave up (closed the stream) loses only the ack:
    // the edge is recorded, and its retry is a no-op.
    if let Err(errno) = super::util::write_response(stream, ready_ack()) {
        boot_line(&format!("runtimed: ui ready ack not delivered errno={errno}"));
    }
    Ok(())
}

/// The acknowledgement of a recorded `OP_NOTIFY_READY` (wire version 2).
fn ready_ack() -> RuntimeResponse {
    RuntimeResponse {
        version: PROTOCOL_VERSION,
        op: OP_NOTIFY_READY,
        status: 0,
        count: 0,
    }
}

pub(super) fn ensure_policy_launches(state: &mut BrokerState) -> bool {
    // One supervisor transaction at a time. While a launch is in flight the
    // loop still owns every decision about it - it records the pid and applies
    // the outcome - but starting a second would mean two children racing for
    // the same launch bookkeeping.
    if state.launch_in_flight.is_some() {
        return false;
    }
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

        let may_precede_ui_ready = launch_may_precede_ui_ready(&entry);
        if pending_service_launch
            && !entry.exec.starts_with("services/")
            && !may_precede_ui_ready
            && (!state.ui_ready || !pending_desktop_launch)
        {
            continue;
        }
        if !state.ui_ready && !may_precede_ui_ready {
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
            schedule_launch_retry(state, entry.desktop_file_id.as_str(), libc::ENOSYS);
            continue;
        }

        attempts += 1;
        // Hand the launch to its worker rather than running it here. Every
        // console caller the loop owns - a keystroke, a parked read, the
        // compositor's parked graph wait - waits for this pass to end, and a
        // launch performed inline made that 149 ms.
        match super::spawn::begin_tracked_launch(state, entry.clone()) {
            Ok(()) => {
                launched_any = true;
                // The launch has not finished; the terminal bookkeeping is
                // applied when the worker reports. `launched_once` is recorded
                // there too, so a launch that fails cannot mark its package as
                // already started.
                break;
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
                        .insert(entry.desktop_file_id.clone(), err);
                    state
                        .launch_failure_counts
                        .remove(entry.desktop_file_id.as_str());
                } else {
                    observability_client::error!(
                        "runtimed",
                        service,
                        "launch {} ({}) failed: errno={err}",
                        entry.desktop_file_id,
                        entry.exec
                    );
                    schedule_launch_retry(state, entry.desktop_file_id.as_str(), err);
                }
            }
        }
    }
    launched_any
}

fn launch_may_precede_ui_ready(entry: &LaunchEntry) -> bool {
    entry.exec == UI_SERVER_EXEC_PATH
        || matches!(
            super::kvm_smp_qualification::qualification_contract_for_launch(entry),
            Ok(Some(_))
        )
}

fn launch_retry_backoff(errno: i32, consecutive_failures: u32) -> std::time::Duration {
    let base = if errno == libc::EAGAIN {
        STORAGE_NOT_READY_RETRY_BACKOFF
    } else {
        RETRY_BACKOFF
    };
    let exponent = consecutive_failures.saturating_sub(1).min(6);
    base.saturating_mul(1_u32 << exponent)
        .min(MAX_LAUNCH_RETRY_BACKOFF)
}

pub(super) fn schedule_launch_retry(state: &mut BrokerState, desktop_file_id: &str, errno: i32) {
    let consecutive_failures = state
        .launch_failure_counts
        .entry(desktop_file_id.to_string())
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
    state.retry_after.insert(
        desktop_file_id.to_string(),
        Instant::now() + launch_retry_backoff(errno, *consecutive_failures),
    );
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::{Duration, Instant};

    use super::{
        bind_listener, dispatch_request, handle_ready, launch_may_precede_ui_ready,
        launch_retry_backoff, read_request_progress,
        runtime_request_role_authorized, PendingRuntimeClient, RequestReadProgress,
        AcceptedClient, RuntimeConnections, RuntimePeerCredentials, OP_NOTIFY_READY, OP_REQUEST_LAUNCH_PATH,
        OP_REQUEST_TERMINATE, OP_SNAPSHOT_RUNNING_PROGRAMS, OP_WATCH_RUNNING_PROGRAMS,
    };
    use crate::kvm_smp_qualification::{
        inject_kvm_smp_qualification_launch, KvmSmpQualificationContract,
    };
    use crate::{
        BrokerState, LaunchEntry, RunningProcess, RuntimeRequest, RuntimeResponse,
        MAX_PENDING_RUNTIME_CLIENTS,
        DEFAULT_USER_TASK_WEIGHT_MICROS, PROTOCOL_VERSION, READY_COMPONENT_UI_SERVER,
        MAX_LAUNCH_RETRY_BACKOFF, RETRY_BACKOFF, SERVICE_REQUEST_TIMEOUT,
        STORAGE_NOT_READY_RETRY_BACKOFF, UI_SERVER_EXEC_PATH,
    };

    #[test]
    fn malformed_listener_path_is_rejected_before_opening_any_socket() {
        assert_eq!(bind_listener("bad\0path").err(), Some(libc::EINVAL));
    }

    #[test]
    fn dispatch_failure_classifies_prewrite_rejection_separately_from_started_reply() {
        let (mut stream, _peer) = UnixStream::pair().expect("socket pair");
        let mut state = BrokerState::default();
        let denied = RuntimeRequest {
            op: OP_REQUEST_LAUNCH_PATH,
            target_kind: 0,
            ..RuntimeRequest::default()
        };
        assert_eq!(
            dispatch_request(&mut stream, denied, &mut state),
            Err((libc::EOPNOTSUPP, false)),
            "policy rejection must still receive a terminal error envelope",
        );
        // Snapshot begins writing immediately. A disconnected peer is used
        // to force an I/O error after the response path is entered.
        drop(_peer);
        let snapshot = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        assert!(
            matches!(dispatch_request(&mut stream, snapshot, &mut state), Err((_, true))),
            "write failure must never trigger a second reply frame",
        );
    }

    #[test]
    fn vanished_session_process_is_refunded_before_its_reaper_record_is_lost() {
        let source = include_str!("socket.rs").split("#[cfg(test)]").next().unwrap();
        let missing = source.split_once("Err(err) if err == libc::ESRCH => {").unwrap().1;
        let removed = missing.find("state.running.remove(&pid)").unwrap();
        let refunded = missing.find("release_failed_session(state, Some(process.session_handle))").unwrap();
        assert!(removed < refunded);
    }

    #[test]
    fn only_ui_bootstrap_or_private_smp_qualification_may_launch_before_ui_ready() {
        let ordinary = LaunchEntry {
            package_id: String::from("settings"),
            desktop_file_id: String::from("settings.desktop"),
            display_name: String::from("Settings"),
            exec: String::from("apps/settings/settings.elf"),
            runtime_deps: Vec::new(),
            restart: false,
            weight_micros: DEFAULT_USER_TASK_WEIGHT_MICROS,
            logical_admin: false,
            console_hosted: false,
            args: Vec::new(),
            env: Vec::new(),
            private_smp_qualification: None,
        };
        assert!(!launch_may_precede_ui_ready(&ordinary));

        let mut ui_server = ordinary.clone();
        ui_server.exec = String::from(UI_SERVER_EXEC_PATH);
        assert!(launch_may_precede_ui_ready(&ui_server));

        let contract = KvmSmpQualificationContract {
            workers: 1,
            work_units: 1_000_000,
            deadline_ms: 5_000,
        };
        let mut entries = Vec::new();
        inject_kvm_smp_qualification_launch(&mut entries, Some(contract))
            .expect("inject exact private launch");
        let private = entries.pop().expect("private launch");
        assert!(launch_may_precede_ui_ready(&private));

        let mut forged = private;
        forged.exec = ordinary.exec;
        assert!(!launch_may_precede_ui_ready(&forged));
    }

    #[test]
    fn partial_background_client_never_busy_waits_the_policy_loop() {
        let (mut sender, receiver) = UnixStream::pair().expect("socket pair");
        sender.set_nonblocking(true).expect("sender nonblocking");
        receiver
            .set_nonblocking(true)
            .expect("receiver nonblocking");
        let request = RuntimeRequest {
            op: OP_SNAPSHOT_RUNNING_PROGRAMS,
            ..RuntimeRequest::default()
        };
        let bytes = crate::util::as_bytes(&request);
        let split = bytes.len() / 2;
        sender
            .write_all(&bytes[..split])
            .expect("first request half");
        let mut client = PendingRuntimeClient {
            stream: receiver,
            peer: RuntimePeerCredentials {
                pid: std::process::id() as i32,
            },
            request: RuntimeRequest::default(),
            bytes_read: 0,
            deadline: Instant::now() + SERVICE_REQUEST_TIMEOUT,
        };

        assert_eq!(
            read_request_progress(&mut client).expect("partial request"),
            RequestReadProgress::Pending
        );
        assert_eq!(client.bytes_read, split);

        sender
            .write_all(&bytes[split..])
            .expect("second request half");
        assert_eq!(
            read_request_progress(&mut client).expect("complete request"),
            RequestReadProgress::Complete
        );
        assert_eq!(client.request.op, OP_SNAPSHOT_RUNNING_PROGRAMS);
    }

    /// Build a watch request the way a client would, carrying the digest the
    /// caller believes in.
    fn watch_client(stream: UnixStream, known_digest: u64, wait_ms: u16) -> PendingRuntimeClient {
        PendingRuntimeClient {
            stream,
            peer: RuntimePeerCredentials {
                pid: std::process::id() as i32,
            },
            request: RuntimeRequest {
                op: OP_WATCH_RUNNING_PROGRAMS,
                target_value: known_digest,
                wait_ms,
                ..RuntimeRequest::default()
            },
            bytes_read: 0,
            deadline: Instant::now() + SERVICE_REQUEST_TIMEOUT,
        }
    }

    fn state_running(pids: &[i32]) -> BrokerState {
        let mut state = BrokerState::default();
        for pid in pids {
            state.running.insert(
                *pid,
                RunningProcess {
                    pid: *pid,
                    package_id: format!("pkg{pid}"),
                    desktop_file_id: format!("pkg{pid}.desktop"),
                    display_name: format!("Program {pid}"),
                    exec: format!("apps/pkg{pid}/pkg{pid}.elf"),
                    session_handle: 0,
                    restart: false,
                    logical_admin: false,
                },
            );
        }
        state
    }

    fn peer_has_reply(stream: &UnixStream) -> bool {
        let mut probe = [0_u8; 1];
        stream.set_nonblocking(true).expect("nonblocking probe");
        !matches!(
            (&mut &*stream).read(&mut probe),
            Err(err) if err.kind() == ErrorKind::WouldBlock
        )
    }

    #[test]
    fn a_watch_that_matches_the_running_set_is_held_rather_than_answered() {
        let state = state_running(&[11, 12]);
        let digest = super::running_programs_digest(&super::running_program_snapshot(&state));
        let (peer, server) = UnixStream::pair().expect("socket pair");
        let mut watchers = VecDeque::new();

        super::begin_running_program_watch(
            watch_client(server, digest, 1_000),
            &state,
            &mut watchers,
        );

        assert_eq!(watchers.len(), 1, "a matching watch must be retained");
        assert!(
            !peer_has_reply(&peer),
            "holding the reply is the whole point; answering it restores the poll"
        );
    }

    #[test]
    fn a_watch_holding_a_stale_digest_is_answered_without_parking() {
        let state = state_running(&[11]);
        let (peer, server) = UnixStream::pair().expect("socket pair");
        let mut watchers = VecDeque::new();

        super::begin_running_program_watch(watch_client(server, 0, 1_000), &state, &mut watchers);

        assert!(watchers.is_empty(), "a ready answer must never park");
        assert!(peer_has_reply(&peer));
    }

    #[test]
    fn a_change_to_the_running_set_answers_every_parked_watch() {
        let mut state = state_running(&[11]);
        let digest = super::running_programs_digest(&super::running_program_snapshot(&state));
        let (_acceptor, mut connections) = test_connections();
        let peers = (0..3)
            .map(|_| {
                let (peer, server) = UnixStream::pair().expect("socket pair");
                super::begin_running_program_watch(
                    watch_client(server, digest, 60_000),
                    &state,
                    &mut connections.watchers,
                );
                peer
            })
            .collect::<Vec<_>>();
        assert_eq!(connections.watchers.len(), 3);
        assert!(
            !super::service_running_program_watchers(&mut connections, &state),
            "an unchanged set owes nobody an answer"
        );

        state = state_running(&[11, 12]);
        assert!(super::service_running_program_watchers(
            &mut connections,
            &state
        ));
        assert!(connections.watchers.is_empty());
        for peer in &peers {
            assert!(peer_has_reply(peer));
        }
    }

    #[test]
    fn an_expired_park_re_arms_the_watcher_instead_of_holding_it_forever() {
        let state = state_running(&[11]);
        let digest = super::running_programs_digest(&super::running_program_snapshot(&state));
        let (_acceptor, mut connections) = test_connections();
        let (peer, server) = UnixStream::pair().expect("socket pair");
        // A zero budget is the shortest legal park: the caller is told nothing
        // changed rather than left unable to distinguish that from a hang.
        super::begin_running_program_watch(
            watch_client(server, digest, 0),
            &state,
            &mut connections.watchers,
        );
        assert_eq!(connections.watchers.len(), 1);

        assert!(super::service_running_program_watchers(
            &mut connections,
            &state
        ));
        assert!(connections.watchers.is_empty());
        assert!(peer_has_reply(&peer));
    }

    #[test]
    fn the_watcher_budget_degrades_to_an_immediate_answer_rather_than_pinning_descriptors() {
        let state = state_running(&[11]);
        let digest = super::running_programs_digest(&super::running_program_snapshot(&state));
        let mut watchers = VecDeque::new();
        let _held = (0..super::MAX_RUNTIME_WATCHERS)
            .map(|_| {
                let (peer, server) = UnixStream::pair().expect("socket pair");
                super::begin_running_program_watch(
                    watch_client(server, digest, 60_000),
                    &state,
                    &mut watchers,
                );
                peer
            })
            .collect::<Vec<_>>();
        assert_eq!(watchers.len(), super::MAX_RUNTIME_WATCHERS);

        let (peer, server) = UnixStream::pair().expect("socket pair");
        super::begin_running_program_watch(
            watch_client(server, digest, 60_000),
            &state,
            &mut watchers,
        );

        assert_eq!(
            watchers.len(),
            super::MAX_RUNTIME_WATCHERS,
            "the budget is a ceiling on pinned descriptors, not a queue"
        );
        assert!(peer_has_reply(&peer));
    }

    #[test]
    fn runtime_control_mutations_require_live_uiserver_or_logical_admin() {
        for op in [
            OP_SNAPSHOT_RUNNING_PROGRAMS,
            OP_WATCH_RUNNING_PROGRAMS,
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

    fn test_connections() -> (
        std::sync::mpsc::SyncSender<AcceptedClient>,
        RuntimeConnections,
    ) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(MAX_PENDING_RUNTIME_CLIENTS);
        (sender, RuntimeConnections::from_receiver(receiver))
    }

    fn accepted_at(age: Duration) -> (UnixStream, AcceptedClient) {
        let (peer, server) = UnixStream::pair().expect("socket pair");
        server.set_nonblocking(true).expect("nonblocking");
        (
            peer,
            AcceptedClient {
                stream: server,
                peer: RuntimePeerCredentials { pid: 1 },
                accepted_at: Instant::now() - age,
            },
        )
    }

    #[test]
    fn a_stalled_acceptor_never_holds_up_the_broker_pass() {
        // The acceptor thread is the one that can sit inside an IPC deadline.
        // While it has nothing to hand over, the broker pass must return
        // immediately and report no work, not wait for it.
        let (_acceptor, mut connections) = test_connections();
        let mut state = BrokerState::default();
        let started = Instant::now();
        assert!(!super::service_listener(&mut connections, &mut state));
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(connections.pending.is_empty());
    }

    #[test]
    fn accepted_clients_are_adopted_and_their_request_clock_starts_at_accept() {
        let (acceptor, mut connections) = test_connections();
        let (_peer, client) = accepted_at(Duration::from_secs(2));
        let accepted_at = client.accepted_at;
        acceptor.send(client).expect("queue");
        let mut state = BrokerState::default();
        assert!(super::service_listener(&mut connections, &mut state));
        let pending = connections.pending.front().expect("adopted");
        assert_eq!(pending.deadline, accepted_at + SERVICE_REQUEST_TIMEOUT);
        assert!(
            pending.deadline < Instant::now() + SERVICE_REQUEST_TIMEOUT,
            "time spent queued behind a stalled pass counts against the client"
        );
    }

    #[test]
    fn adoption_is_bounded_by_the_pending_queue() {
        let (acceptor, mut connections) = test_connections();
        let mut peers = Vec::new();
        for _ in 0..MAX_PENDING_RUNTIME_CLIENTS {
            let (peer, client) = accepted_at(Duration::ZERO);
            peers.push(peer);
            acceptor.send(client).expect("queue");
        }
        let mut state = BrokerState::default();
        for _ in 0..4 {
            super::service_listener(&mut connections, &mut state);
        }
        assert_eq!(connections.pending.len(), MAX_PENDING_RUNTIME_CLIENTS);
        let (_peer, extra) = accepted_at(Duration::ZERO);
        acceptor.send(extra).expect("queued, not adopted");
        super::service_listener(&mut connections, &mut state);
        assert_eq!(
            connections.pending.len(),
            MAX_PENDING_RUNTIME_CLIENTS,
            "a full pending queue leaves the rest with the acceptor"
        );
    }

    #[test]
    fn the_acceptor_thread_delivers_a_real_connection_with_kernel_credentials() {
        let dir = std::env::temp_dir().join(format!("rtd-accept-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let listener = UnixListener::bind(&dir).expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let mut connections = RuntimeConnections::with_acceptor(listener).expect("acceptor");
        let _client = UnixStream::connect(&dir).expect("connect");
        let mut state = BrokerState::default();
        let started = Instant::now();
        while connections.pending.is_empty() {
            super::service_listener(&mut connections, &mut state);
            assert!(started.elapsed() < Duration::from_secs(5), "never adopted");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            connections.pending.front().expect("adopted").peer.pid,
            std::process::id() as i32
        );
        let _ = std::fs::remove_file(&dir);
    }

    fn ready_request() -> RuntimeRequest {
        RuntimeRequest {
            version: PROTOCOL_VERSION,
            op: OP_NOTIFY_READY,
            target_kind: READY_COMPONENT_UI_SERVER,
            ..RuntimeRequest::default()
        }
    }

    fn read_ack(peer: &mut UnixStream) -> RuntimeResponse {
        let mut response = RuntimeResponse::default();
        peer.read_exact(super::super::util::as_bytes_mut(&mut response))
            .expect("ack frame");
        response
    }

    #[test]
    fn ui_ready_is_recorded_and_then_acknowledged() {
        let (mut peer, mut server) = UnixStream::pair().expect("socket pair");
        let mut state = BrokerState::default();
        assert!(!state.ui_ready);
        handle_ready(&mut server, &mut state, ready_request()).expect("handled");
        assert!(state.ui_ready, "the edge is recorded before the ack leaves");
        drop(server); // an absent ack must read as EOF, never as a hang
        let ack = read_ack(&mut peer);
        assert_eq!(ack.version, PROTOCOL_VERSION);
        assert_eq!(ack.op, OP_NOTIFY_READY);
        assert_eq!((ack.status, ack.count), (0, 0));
    }

    #[test]
    fn a_retried_ui_ready_is_idempotent_and_acknowledged_again() {
        let mut state = BrokerState::default();
        for _ in 0..2 {
            let (mut peer, mut server) = UnixStream::pair().expect("socket pair");
            handle_ready(&mut server, &mut state, ready_request()).expect("handled");
            drop(server);
            assert_eq!(read_ack(&mut peer).status, 0);
            assert!(state.ui_ready);
        }
    }

    #[test]
    fn a_client_that_closed_before_the_ack_still_leaves_the_edge_recorded() {
        let (peer, mut server) = UnixStream::pair().expect("socket pair");
        drop(peer);
        let mut state = BrokerState::default();
        handle_ready(&mut server, &mut state, ready_request()).expect("not fatal");
        assert!(state.ui_ready);
    }

    #[test]
    fn ui_ready_for_an_unknown_component_is_refused_without_recording() {
        let (_peer, mut server) = UnixStream::pair().expect("socket pair");
        let mut state = BrokerState::default();
        let mut request = ready_request();
        request.target_kind = READY_COMPONENT_UI_SERVER + 1;
        assert_eq!(
            handle_ready(&mut server, &mut state, request),
            Err(libc::EOPNOTSUPP)
        );
        assert!(!state.ui_ready);
    }

    #[test]
    fn storage_not_ready_launches_back_off_without_disabling_the_program() {
        assert_eq!(
            launch_retry_backoff(libc::EAGAIN, 1),
            STORAGE_NOT_READY_RETRY_BACKOFF
        );
        assert_eq!(launch_retry_backoff(libc::ETIMEDOUT, 1), RETRY_BACKOFF);
        assert_eq!(
            launch_retry_backoff(libc::EIO, 64),
            MAX_LAUNCH_RETRY_BACKOFF
        );
    }
}
