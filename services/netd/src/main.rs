use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::mem::size_of;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use rustos_user_abi::linux as linux_abi;
use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, NetdIpcRequest, NetdIpcResponse,
    RustosNetBrokerArgs, COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND, COMMERCIAL_MAX_NETD_OP_FD_TRANSFER,
    COMMERCIAL_MAX_NETD_OP_PACKET_LEASE, COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY,
    COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE, COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS,
    COMMERCIAL_MAX_PROTOCOL_NETD, IPC_SERVICE_NETD, NETD_IPC_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP,
    SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_LISTEN,
    SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDMSG, SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
    SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_NET_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(1);
const MAX_LISTEN_BACKLOG: usize = 128;
const SOCKET_BUFFER_CAPACITY: usize = 1024 * 1024;

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_NETD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "netd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("netd: network policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = NetdIpcRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut NetdIpcRequest) as u64,
            size_of::<NetdIpcRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        if received as usize == size_of::<CommercialMaxProtocolRequest>() {
            let commercial_request = unsafe {
                &*((&request as *const NetdIpcRequest).cast::<CommercialMaxProtocolRequest>())
            };
            let reply = reply_commercial_request(reply_cap, commercial_request);
            if reply < 0 {
                let _ = writeln!(std::io::stderr(), "netd: reply failed errno={}", -reply);
            }
            continue;
        }

        let mut response = NetdIpcResponse {
            version: NETD_IPC_ABI_VERSION,
            op: request.op,
            ..NetdIpcResponse::default()
        };
        response.status = match validate_request(received as usize, &request) {
            Ok(()) => dispatch_request(&request, &mut response),
            Err(errno) => errno,
        };
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const NetdIpcResponse) as u64,
            size_of::<NetdIpcResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "netd: reply failed errno={}", -reply);
        }
    }
}

fn reply_commercial_request(reply_cap: u64, request: &CommercialMaxProtocolRequest) -> i64 {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    response.status = match validate_commercial_request(request) {
        Ok(()) => dispatch_commercial_request(request, &mut response),
        Err(errno) => errno,
    };
    syscall3(
        SYS_RUSTOS_IPC_REPLY,
        reply_cap,
        (&response as *const CommercialMaxProtocolResponse) as u64,
        size_of::<CommercialMaxProtocolResponse>() as u64,
    )
}

fn dispatch_request(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET => handle_socket(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR => handle_socketpair(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_DUP => handle_dup(request),
        SYSCALL_OFFLOAD_OP_LINUX_CLOSE => handle_close(request),
        SYSCALL_OFFLOAD_OP_LINUX_BIND => handle_bind(request),
        SYSCALL_OFFLOAD_OP_LINUX_LISTEN => handle_listen(request),
        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => handle_accept(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_CONNECT => handle_connect(request),
        SYSCALL_OFFLOAD_OP_LINUX_SENDTO | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG => {
            handle_send(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_RECVFROM | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => {
            handle_recv(request, response)
        }
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME => handle_getsockname(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME => handle_getpeername(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT => handle_setsockopt(request),
        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => handle_getsockopt(request, response),
        SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => handle_shutdown(request),
        _ => libc::EINVAL,
    }
}

fn validate_request(received: usize, request: &NetdIpcRequest) -> Result<(), i32> {
    if received != size_of::<NetdIpcRequest>()
        || request.version != NETD_IPC_ABI_VERSION
        || request.flags != 0
        || request.reserved1 != 0
        || request.reserved0 != 0
        || request.pid == 0
        || request.tid == 0
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN
        | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT
        | SYSCALL_OFFLOAD_OP_LINUX_CONNECT
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME
        | SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Credentials {
    pid: i32,
    uid: u32,
    gid: u32,
}

#[derive(Debug)]
enum UnixSocketState {
    Idle,
    Listening {
        backlog: usize,
        pending: VecDeque<u64>,
    },
    Connected(ConnectedState),
}

#[derive(Debug)]
struct ConnectedState {
    incoming_bytes: VecDeque<u8>,
    peer: u64,
    peer_closed: bool,
    peer_read_closed: bool,
    peer_write_closed: bool,
    peer_credentials: Credentials,
    recv_closed: bool,
    send_closed: bool,
}

#[derive(Debug)]
struct UnixSocket {
    owner: Credentials,
    refs: usize,
    options: SocketOptions,
    bound_path: Option<String>,
    local_path: Option<String>,
    peer_path: Option<String>,
    state: UnixSocketState,
}

#[derive(Clone, Copy, Debug)]
struct SocketOptions {
    reuse_addr: bool,
    reuse_port: bool,
    keepalive: bool,
    send_buffer: i32,
    recv_buffer: i32,
    passcred: bool,
}

impl Default for SocketOptions {
    fn default() -> Self {
        Self {
            reuse_addr: false,
            reuse_port: false,
            keepalive: false,
            send_buffer: SOCKET_BUFFER_CAPACITY as i32,
            recv_buffer: SOCKET_BUFFER_CAPACITY as i32,
            passcred: false,
        }
    }
}

#[derive(Debug)]
struct NetState {
    next_token: u64,
    sockets: BTreeMap<u64, UnixSocket>,
    bindings: BTreeMap<String, u64>,
}

impl NetState {
    fn new() -> Self {
        Self {
            next_token: 1,
            sockets: BTreeMap::new(),
            bindings: BTreeMap::new(),
        }
    }

    fn allocate_token(&mut self) -> u64 {
        let token = self.next_token.max(1);
        self.next_token = token.saturating_add(1).max(1);
        token
    }
}

fn net_state() -> &'static Mutex<NetState> {
    static STATE: OnceLock<Mutex<NetState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(NetState::new()))
}

fn handle_socket(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let domain = request.arg0;
    let socket_type = request.arg1;
    let base_type = socket_type & linux_abi::SOCK_TYPE_MASK;
    if domain != linux_abi::AF_UNIX || base_type != linux_abi::SOCK_STREAM {
        return call_net_broker(request, response, 0, 0, 0);
    }

    let mut state = net_state().lock().unwrap();
    let token = state.allocate_token();
    state.sockets.insert(
        token,
        UnixSocket {
            owner: request_credentials(request),
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Idle,
        },
    );
    drop(state);
    call_net_broker(request, response, token, 0, 0)
}

fn handle_socketpair(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg0 != linux_abi::AF_UNIX
        || request.arg1 & linux_abi::SOCK_TYPE_MASK != linux_abi::SOCK_STREAM
    {
        return libc::EAFNOSUPPORT;
    }
    let credentials = request_credentials(request);
    let mut state = net_state().lock().unwrap();
    let left = state.allocate_token();
    let right = state.allocate_token();
    state.sockets.insert(
        left,
        UnixSocket {
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Connected(ConnectedState {
                incoming_bytes: VecDeque::new(),
                peer: right,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    state.sockets.insert(
        right,
        UnixSocket {
            owner: credentials,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: None,
            peer_path: None,
            state: UnixSocketState::Connected(ConnectedState {
                incoming_bytes: VecDeque::new(),
                peer: left,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    drop(state);
    call_net_broker(request, response, 0, left, right)
}

fn handle_dup(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    socket.refs = socket.refs.saturating_add(1).max(1);
    0
}

fn handle_close(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.refs > 1 {
        socket.refs -= 1;
        return 0;
    }
    let Some(socket) = state.sockets.remove(&request.socket_token) else {
        return libc::EBADF;
    };
    if let Some(path) = socket.bound_path {
        if state.bindings.get(&path).copied() == Some(request.socket_token) {
            state.bindings.remove(&path);
        }
    }
    if let UnixSocketState::Connected(connected) = socket.state {
        if let Some(peer) = state.sockets.get_mut(&connected.peer) {
            if let UnixSocketState::Connected(peer_connected) = &mut peer.state {
                peer_connected.peer_closed = true;
                peer_connected.peer_read_closed = true;
                peer_connected.peer_write_closed = true;
            }
        }
    }
    0
}

fn handle_bind(request: &NetdIpcRequest) -> i32 {
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let mut state = net_state().lock().unwrap();
    if state.bindings.contains_key(&path) {
        return libc::EADDRINUSE;
    }
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.bound_path.is_some() || !matches!(socket.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    socket.bound_path = Some(path.clone());
    socket.local_path = Some(path.clone());
    state.bindings.insert(path, request.socket_token);
    0
}

fn handle_listen(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if socket.bound_path.is_none() || !matches!(socket.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    let backlog = usize::try_from(request.arg1)
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_LISTEN_BACKLOG);
    socket.state = UnixSocketState::Listening {
        backlog,
        pending: VecDeque::new(),
    };
    0
}

fn handle_connect(request: &NetdIpcRequest) -> i32 {
    let path = match sockaddr_path_from_payload(request) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let credentials = request_credentials(request);
    let mut state = net_state().lock().unwrap();
    let Some(listener_token) = state.bindings.get(&path).copied() else {
        return libc::ENOENT;
    };
    let accepted = state.allocate_token();
    let Some(client) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    if client.bound_path.is_some() || !matches!(client.state, UnixSocketState::Idle) {
        return libc::EINVAL;
    }
    let client_local_path = client.local_path.clone();
    client.peer_path = Some(path.clone());
    client.state = UnixSocketState::Connected(ConnectedState {
        incoming_bytes: VecDeque::new(),
        peer: accepted,
        peer_closed: false,
        peer_read_closed: false,
        peer_write_closed: false,
        peer_credentials: credentials,
        recv_closed: false,
        send_closed: false,
    });
    let (listener_owner, listener_path) = {
        let Some(listener) = state.sockets.get_mut(&listener_token) else {
            return libc::ECONNREFUSED;
        };
        let UnixSocketState::Listening { backlog, pending } = &mut listener.state else {
            return libc::ECONNREFUSED;
        };
        if pending.len() >= *backlog {
            return libc::EAGAIN;
        }
        pending.push_back(accepted);
        (listener.owner, listener.local_path.clone())
    };
    state.sockets.insert(
        accepted,
        UnixSocket {
            owner: listener_owner,
            refs: 1,
            options: SocketOptions::default(),
            bound_path: None,
            local_path: listener_path,
            peer_path: client_local_path,
            state: UnixSocketState::Connected(ConnectedState {
                incoming_bytes: VecDeque::new(),
                peer: request.socket_token,
                peer_closed: false,
                peer_read_closed: false,
                peer_write_closed: false,
                peer_credentials: credentials,
                recv_closed: false,
                send_closed: false,
            }),
        },
    );
    0
}

fn handle_accept(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let nonblocking = request.status_flags & linux_abi::O_NONBLOCK != 0
        || request.arg3 & linux_abi::SOCK_NONBLOCK != 0;
    let accepted = {
        let mut state = net_state().lock().unwrap();
        let Some(listener) = state.sockets.get_mut(&request.socket_token) else {
            return libc::EBADF;
        };
        match &mut listener.state {
            UnixSocketState::Listening { pending, .. } => match pending.pop_front() {
                Some(token) => token,
                None if nonblocking => return libc::EAGAIN,
                None => return libc::EAGAIN,
            },
            _ => return libc::EINVAL,
        }
    };
    if request.arg1 != 0 && request.arg2 != 0 {
        let state = net_state().lock().unwrap();
        if let Some(socket) = state.sockets.get(&accepted) {
            if let Err(errno) =
                sockaddr_payload(socket.peer_path.as_deref().unwrap_or(""), response)
            {
                return errno;
            }
        }
    }
    call_net_broker(request, response, 0, accepted, 0)
}

fn handle_send(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let len = request.payload_len as usize;
    let bytes = &request.payload[..len];
    match send_socket_bytes(request, bytes) {
        Ok(sent) => {
            response.value = sent as u64;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_recv(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let requested = if request.op == SYSCALL_OFFLOAD_OP_LINUX_RECVMSG {
        request.payload_len as usize
    } else {
        usize::try_from(request.arg2).unwrap_or(usize::MAX)
    };
    let limit = requested.min(response.payload.len());
    match recv_socket_bytes(request, &mut response.payload[..limit]) {
        Ok(read) => {
            response.value = read as u64;
            response.payload_len = read as u32;
            0
        }
        Err(errno) => errno,
    }
}

fn handle_getsockname(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    sockaddr_payload(socket.local_path.as_deref().unwrap_or(""), response)
        .unwrap_or_else(|errno| errno)
}

fn handle_getpeername(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    let state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let Some(path) = socket.peer_path.as_deref() else {
        return libc::ENOTCONN;
    };
    sockaddr_payload(path, response).unwrap_or_else(|errno| errno)
}

fn handle_getsockopt(request: &NetdIpcRequest, response: &mut NetdIpcResponse) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::EOPNOTSUPP;
    }
    let state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get(&request.socket_token) else {
        return libc::EBADF;
    };
    let value = match request.arg2 {
        linux_abi::SO_ERROR => 0_i32,
        linux_abi::SO_TYPE => linux_abi::SOCK_STREAM as i32,
        linux_abi::SO_DOMAIN => linux_abi::AF_UNIX as i32,
        linux_abi::SO_PROTOCOL => 0_i32,
        linux_abi::SO_ACCEPTCONN => {
            if matches!(socket.state, UnixSocketState::Listening { .. }) {
                1
            } else {
                0
            }
        }
        linux_abi::SO_REUSEADDR => socket.options.reuse_addr as i32,
        linux_abi::SO_REUSEPORT => socket.options.reuse_port as i32,
        linux_abi::SO_KEEPALIVE => socket.options.keepalive as i32,
        linux_abi::SO_SNDBUF => socket.options.send_buffer,
        linux_abi::SO_RCVBUF => socket.options.recv_buffer,
        linux_abi::SO_PASSCRED => socket.options.passcred as i32,
        linux_abi::SO_PEERCRED => return peercred_payload_from_socket(socket, response),
        _ => return libc::EOPNOTSUPP,
    };
    response.payload[..size_of::<i32>()].copy_from_slice(&value.to_ne_bytes());
    response.payload_len = size_of::<i32>() as u32;
    response.value = 0;
    0
}

fn handle_setsockopt(request: &NetdIpcRequest) -> i32 {
    if request.arg1 != linux_abi::SOL_SOCKET {
        return libc::EOPNOTSUPP;
    }
    let Some(value) = request_i32_payload(request) else {
        return libc::EINVAL;
    };
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    match request.arg2 {
        linux_abi::SO_REUSEADDR => socket.options.reuse_addr = value != 0,
        linux_abi::SO_REUSEPORT => socket.options.reuse_port = value != 0,
        linux_abi::SO_KEEPALIVE => socket.options.keepalive = value != 0,
        linux_abi::SO_PASSCRED => socket.options.passcred = value != 0,
        linux_abi::SO_SNDBUF => socket.options.send_buffer = clamp_socket_buffer(value),
        linux_abi::SO_RCVBUF => socket.options.recv_buffer = clamp_socket_buffer(value),
        _ => libc::EOPNOTSUPP,
    };
    0
}

fn handle_shutdown(request: &NetdIpcRequest) -> i32 {
    let mut state = net_state().lock().unwrap();
    let Some(socket) = state.sockets.get_mut(&request.socket_token) else {
        return libc::EBADF;
    };
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return libc::ENOTCONN;
    };
    match request.arg1 {
        linux_abi::SHUT_RD => connected.recv_closed = true,
        linux_abi::SHUT_WR => connected.send_closed = true,
        linux_abi::SHUT_RDWR => {
            connected.recv_closed = true;
            connected.send_closed = true;
        }
        _ => return libc::EINVAL,
    }
    0
}

fn send_socket_bytes(request: &NetdIpcRequest, bytes: &[u8]) -> Result<usize, i32> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let nonblocking = request.status_flags & linux_abi::O_NONBLOCK != 0
        || request_msg_flags(request) & linux_abi::MSG_DONTWAIT != 0;
    let mut state = net_state().lock().unwrap();
    let peer = {
        let socket = state
            .sockets
            .get(&request.socket_token)
            .ok_or(libc::EBADF)?;
        let UnixSocketState::Connected(connected) = &socket.state else {
            return Err(libc::ENOTCONN);
        };
        if connected.send_closed || connected.peer_read_closed || connected.peer_closed {
            return Err(libc::EPIPE);
        }
        connected.peer
    };
    let peer_socket = state.sockets.get_mut(&peer).ok_or(libc::EPIPE)?;
    let UnixSocketState::Connected(peer_connected) = &mut peer_socket.state else {
        return Err(libc::ENOTCONN);
    };
    if peer_connected.recv_closed {
        return Err(libc::EPIPE);
    }
    let room = SOCKET_BUFFER_CAPACITY.saturating_sub(peer_connected.incoming_bytes.len());
    if room == 0 {
        return if nonblocking {
            Err(libc::EAGAIN)
        } else {
            Err(libc::EAGAIN)
        };
    }
    let write_len = room.min(bytes.len());
    peer_connected
        .incoming_bytes
        .extend(bytes[..write_len].iter().copied());
    Ok(write_len)
}

fn recv_socket_bytes(request: &NetdIpcRequest, dest: &mut [u8]) -> Result<usize, i32> {
    if dest.is_empty() {
        return Ok(0);
    }
    let nonblocking = request.status_flags & linux_abi::O_NONBLOCK != 0
        || request_msg_flags(request) & linux_abi::MSG_DONTWAIT != 0;
    let mut state = net_state().lock().unwrap();
    let socket = state
        .sockets
        .get_mut(&request.socket_token)
        .ok_or(libc::EBADF)?;
    let UnixSocketState::Connected(connected) = &mut socket.state else {
        return Err(libc::ENOTCONN);
    };
    if connected.incoming_bytes.is_empty() {
        if connected.peer_closed || connected.peer_write_closed {
            return Ok(0);
        }
        return if nonblocking {
            Err(libc::EAGAIN)
        } else {
            Err(libc::EAGAIN)
        };
    }
    let count = dest.len().min(connected.incoming_bytes.len());
    for slot in &mut dest[..count] {
        *slot = connected.incoming_bytes.pop_front().unwrap_or_default();
    }
    Ok(count)
}

fn call_net_broker(
    request: &NetdIpcRequest,
    response: &mut NetdIpcResponse,
    socket_token: u64,
    token_a: u64,
    token_b: u64,
) -> i32 {
    let args = RustosNetBrokerArgs {
        process_id: request.pid,
        op: request.op,
        reserved0: 0,
        reserved1: 0,
        arg0: request.arg0,
        arg1: request.arg1,
        arg2: request.arg2,
        arg3: if socket_token != 0 {
            socket_token
        } else {
            request.arg3
        },
        arg4: token_a,
        arg5: token_b,
    };
    let result = syscall1(
        SYS_RUSTOS_NET_BROKER,
        (&args as *const RustosNetBrokerArgs) as u64,
    );
    if result < 0 {
        return last_errno();
    }
    response.value = result as u64;
    0
}

fn request_msg_flags(request: &NetdIpcRequest) -> u64 {
    match request.op {
        SYSCALL_OFFLOAD_OP_LINUX_SENDMSG | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => request.arg2,
        _ => request.arg3,
    }
}

fn request_credentials(request: &NetdIpcRequest) -> Credentials {
    Credentials {
        pid: request.pid as i32,
        uid: request.euid,
        gid: request.egid,
    }
}

fn sockaddr_path_from_payload(request: &NetdIpcRequest) -> Result<String, i32> {
    let len = request.payload_len as usize;
    if len < size_of::<u16>() {
        return Err(libc::EINVAL);
    }
    let family = u16::from_ne_bytes([request.payload[0], request.payload[1]]) as u64;
    if family != linux_abi::AF_UNIX {
        return Err(libc::EAFNOSUPPORT);
    }
    let path_bytes = &request.payload[size_of::<u16>()..len];
    if path_bytes.first().copied() == Some(0) {
        return Err(libc::EINVAL);
    }
    let end = path_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path_bytes.len());
    if end == 0 || end >= linux_abi::UNIX_PATH_MAX {
        return Err(libc::EINVAL);
    }
    String::from_utf8(path_bytes[..end].to_vec()).map_err(|_| libc::EINVAL)
}

fn sockaddr_payload(path: &str, response: &mut NetdIpcResponse) -> Result<i32, i32> {
    let needed = size_of::<u16>()
        .checked_add(path.len())
        .and_then(|value| value.checked_add(1))
        .ok_or(libc::EINVAL)?;
    if needed > response.payload.len() || path.len() >= linux_abi::UNIX_PATH_MAX {
        return Err(libc::EINVAL);
    }
    response.payload[..2].copy_from_slice(&(linux_abi::AF_UNIX as u16).to_ne_bytes());
    response.payload[2..2 + path.len()].copy_from_slice(path.as_bytes());
    response.payload[2 + path.len()] = 0;
    response.payload_len = needed as u32;
    response.value = 0;
    Ok(0)
}

fn peercred_payload_from_socket(socket: &UnixSocket, response: &mut NetdIpcResponse) -> i32 {
    let UnixSocketState::Connected(connected) = &socket.state else {
        return libc::ENOTCONN;
    };
    let value = linux_abi::LinuxUCred {
        pid: connected.peer_credentials.pid,
        uid: connected.peer_credentials.uid,
        gid: connected.peer_credentials.gid,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&value as *const linux_abi::LinuxUCred).cast::<u8>(),
            size_of::<linux_abi::LinuxUCred>(),
        )
    };
    response.payload[..bytes.len()].copy_from_slice(bytes);
    response.payload_len = bytes.len() as u32;
    response.value = 0;
    0
}

fn request_i32_payload(request: &NetdIpcRequest) -> Option<i32> {
    if (request.payload_len as usize) < size_of::<i32>() {
        return None;
    }
    Some(i32::from_ne_bytes(request.payload[..4].try_into().ok()?))
}

fn clamp_socket_buffer(value: i32) -> i32 {
    value.clamp(4096, SOCKET_BUFFER_CAPACITY as i32)
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_NETD
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(libc::EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE
        | COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS
        | COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND
        | COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY
        | COMMERCIAL_MAX_NETD_OP_PACKET_LEASE
        | COMMERCIAL_MAX_NETD_OP_FD_TRANSFER => Ok(()),
        _ => Err(libc::EINVAL),
    }
}

fn dispatch_commercial_request(
    request: &CommercialMaxProtocolRequest,
    response: &mut CommercialMaxProtocolResponse,
) -> i32 {
    match request.header.op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE => {
            fill_net_descriptors(
                response,
                &[
                    ("socket", SYSCALL_OFFLOAD_OP_LINUX_SOCKET),
                    ("socketpair", SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR),
                    ("dup", SYSCALL_OFFLOAD_OP_LINUX_DUP),
                    ("close", SYSCALL_OFFLOAD_OP_LINUX_CLOSE),
                    ("shutdown", SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS => {
            fill_net_descriptors(
                response,
                &[
                    ("setsockopt", SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT),
                    ("getsockopt", SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND => {
            fill_net_descriptors(
                response,
                &[
                    ("bind", SYSCALL_OFFLOAD_OP_LINUX_BIND),
                    ("listen", SYSCALL_OFFLOAD_OP_LINUX_LISTEN),
                    ("getsockname", SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME),
                    ("getpeername", SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME),
                ],
            );
            0
        }
        COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY => {
            fill_net_descriptors(response, &[("connect", SYSCALL_OFFLOAD_OP_LINUX_CONNECT)]);
            0
        }
        COMMERCIAL_MAX_NETD_OP_PACKET_LEASE => {
            fill_net_descriptors(
                response,
                &[
                    ("sendto", SYSCALL_OFFLOAD_OP_LINUX_SENDTO),
                    ("sendmsg", SYSCALL_OFFLOAD_OP_LINUX_SENDMSG),
                    ("recvfrom", SYSCALL_OFFLOAD_OP_LINUX_RECVFROM),
                    ("recvmsg", SYSCALL_OFFLOAD_OP_LINUX_RECVMSG),
                ],
            );
            response.capability = net_capability("packet", request.header.op);
            0
        }
        COMMERCIAL_MAX_NETD_OP_FD_TRANSFER => {
            fill_net_descriptors(response, &[("accept", SYSCALL_OFFLOAD_OP_LINUX_ACCEPT)]);
            response.capability = net_capability("fd-transfer", request.header.op);
            0
        }
        _ => libc::EINVAL,
    }
}

fn fill_net_descriptors(response: &mut CommercialMaxProtocolResponse, entries: &[(&str, u16)]) {
    let count = entries.len().min(COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS);
    response.descriptor_count = count as u16;
    response.value0 = entries.len() as u64;
    for (index, (name, op)) in entries.iter().take(count).enumerate() {
        response.descriptors[index] = net_descriptor(name, *op);
    }
}

fn net_descriptor(name: &str, offload_op: u16) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_NETD,
        op: offload_op,
        flags: 0,
        service_id: IPC_SERVICE_NETD,
        capability_mask: net_capability_mask(offload_op),
        value0: offload_op as u64,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(name, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn net_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_NETD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_NETD,
        capability_mask: net_capability_mask(op),
        rights_mask: net_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn net_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKET
        | SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR
        | SYSCALL_OFFLOAD_OP_LINUX_DUP
        | SYSCALL_OFFLOAD_OP_LINUX_CLOSE
        | SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN => 1 << 0,
        COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS
        | SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT
        | SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT => 1 << 1,
        COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_BIND
        | SYSCALL_OFFLOAD_OP_LINUX_LISTEN => 1 << 2,
        COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY | SYSCALL_OFFLOAD_OP_LINUX_CONNECT => 1 << 3,
        COMMERCIAL_MAX_NETD_OP_PACKET_LEASE
        | SYSCALL_OFFLOAD_OP_LINUX_SENDTO
        | SYSCALL_OFFLOAD_OP_LINUX_SENDMSG
        | SYSCALL_OFFLOAD_OP_LINUX_RECVFROM
        | SYSCALL_OFFLOAD_OP_LINUX_RECVMSG => 1 << 4,
        COMMERCIAL_MAX_NETD_OP_FD_TRANSFER | SYSCALL_OFFLOAD_OP_LINUX_ACCEPT => 1 << 5,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
