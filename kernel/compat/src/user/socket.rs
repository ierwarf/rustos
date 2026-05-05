use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ptr;

use lazy_static::lazy_static;
use spin::Mutex;

use crate::arch::rtc;
use crate::user::handles::KernelHandle;
use crate::user::linux as linux_abi;
use kernel_object::api::handle::HandleRights;

const MAX_LISTEN_BACKLOG: usize = 128;
const SOCKET_BUFFER_CAPACITY: usize = 1024 * 1024;
const UNIX_PRIVATE_SOCKET_MODE: u32 = 0o600;
const UNIX_SYSTEM_SOCKET_MODE: u32 = 0o666;
const UNIX_SYSTEM_RUNTIME_ROOT: &str = "/run";
const UNIX_RUNTIME_ROOT: &str = "/run/user";

lazy_static! {
    static ref UNIX_BINDINGS: Mutex<Vec<UnixBinding>> = Mutex::new(Vec::new());
}

#[derive(Debug)]
struct UnixBinding {
    path: String,
    owner: SocketCredentials,
    mode: u32,
    socket: Weak<SocketObject>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SocketError {
    AddressInUse,
    BrokenPipe,
    ConnectionRefused,
    InvalidArgument,
    IsConnected,
    NotConnected,
    NotFound,
    PermissionDenied,
    TryAgain,
}

#[derive(Debug)]
enum SocketState {
    Idle,
    Listening {
        backlog: usize,
        pending: VecDeque<SocketHandle>,
    },
    Connected(ConnectedState),
}

#[derive(Debug)]
struct ConnectedState {
    incoming_bytes: VecDeque<u8>,
    incoming_rights: VecDeque<SocketAncillary>,
    peer: Weak<SocketObject>,
    peer_closed: bool,
    peer_read_closed: bool,
    peer_write_closed: bool,
    peer_credentials: SocketCredentials,
    recv_closed: bool,
    send_closed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PassedHandle {
    handle: KernelHandle,
    status_flags: u64,
    rights: HandleRights,
}

#[derive(Clone, Debug)]
struct SocketAncillary {
    byte_offset: usize,
    rights: Vec<PassedHandle>,
}

#[derive(Debug)]
struct SocketInner {
    bound_path: Option<String>,
    local_path: Option<String>,
    peer_path: Option<String>,
    state: SocketState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SocketCredentials {
    pid: i32,
    uid: u32,
    gid: u32,
}

impl SocketCredentials {
    pub(crate) const fn new(pid: i32, uid: u32, gid: u32) -> Self {
        Self { pid, uid, gid }
    }

    pub(crate) const fn pid(self) -> i32 {
        self.pid
    }

    pub(crate) const fn uid(self) -> u32 {
        self.uid
    }

    pub(crate) const fn gid(self) -> u32 {
        self.gid
    }
}

#[derive(Debug)]
struct SocketObject {
    state: Mutex<SocketInner>,
    owner: SocketCredentials,
}

#[derive(Clone, Debug)]
pub struct SocketHandle {
    inner: Arc<SocketObject>,
}

impl SocketHandle {
    pub(crate) fn new_unix_stream_with_owner(owner: SocketCredentials) -> Self {
        Self {
            inner: Arc::new(SocketObject {
                state: Mutex::new(SocketInner {
                    bound_path: None,
                    local_path: None,
                    peer_path: None,
                    state: SocketState::Idle,
                }),
                owner,
            }),
        }
    }

    pub(crate) fn socketpair(credentials: SocketCredentials) -> (Self, Self) {
        let left = Self::new_unix_stream_with_owner(credentials);
        let right = Self::new_unix_stream_with_owner(credentials);
        connect_pair(
            &left,
            &right,
            None,
            None,
            credentials,
            None,
            None,
            credentials,
        )
        .expect("fresh socketpair endpoints must connect");
        (left, right)
    }

    pub(crate) fn token_id(&self) -> u64 {
        Arc::as_ptr(&self.inner) as usize as u64
    }

    pub(crate) fn bound_path(&self) -> Option<String> {
        let state = self.inner.state.lock();
        state
            .local_path
            .clone()
            .or_else(|| state.bound_path.clone())
    }

    pub(crate) fn local_path(&self) -> Option<String> {
        self.inner.state.lock().local_path.clone()
    }

    pub(crate) fn peer_path(&self) -> Option<String> {
        self.inner.state.lock().peer_path.clone()
    }

    pub(crate) fn owner_credentials(&self) -> SocketCredentials {
        self.inner.owner
    }

    pub(crate) fn peer_credentials(&self) -> Option<SocketCredentials> {
        let state = self.inner.state.lock();
        match &state.state {
            SocketState::Connected(connected) => Some(connected.peer_credentials),
            _ => None,
        }
    }

    pub(crate) fn is_listening(&self) -> bool {
        matches!(self.inner.state.lock().state, SocketState::Listening { .. })
    }

    pub(crate) fn bind(&self, path: &str) -> Result<(), SocketError> {
        let path = validate_bind_path(path, self.owner_credentials())?;

        let mut state = self.inner.state.lock();
        if state.bound_path.is_some() || !matches!(state.state, SocketState::Idle) {
            return Err(SocketError::InvalidArgument);
        }

        let mut bindings = UNIX_BINDINGS.lock();
        prune_dead_bindings(&mut bindings);
        if bindings.iter().any(|binding| binding.path == path) {
            return Err(SocketError::AddressInUse);
        }

        bindings.push(UnixBinding {
            path: path.clone(),
            owner: self.owner_credentials(),
            mode: socket_mode_for_bound_path(path.as_str(), self.owner_credentials()),
            socket: Arc::downgrade(&self.inner),
        });
        state.bound_path = Some(path.clone());
        state.local_path = Some(path);
        Ok(())
    }

    pub(crate) fn listen(&self, backlog: usize) -> Result<(), SocketError> {
        let backlog = backlog.clamp(1, MAX_LISTEN_BACKLOG);

        let mut state = self.inner.state.lock();
        if state.bound_path.is_none() || !matches!(state.state, SocketState::Idle) {
            return Err(SocketError::InvalidArgument);
        }

        state.state = SocketState::Listening {
            backlog,
            pending: VecDeque::new(),
        };
        Ok(())
    }

    pub(crate) fn connect(&self, path: &str) -> Result<(), SocketError> {
        let path = validate_connect_path(path, self.owner_credentials())?;

        {
            let state = self.inner.state.lock();
            if state.bound_path.is_some() {
                return Err(SocketError::InvalidArgument);
            }
            if matches!(state.state, SocketState::Connected(_)) {
                return Err(SocketError::IsConnected);
            }
            if matches!(state.state, SocketState::Listening { .. }) {
                return Err(SocketError::InvalidArgument);
            }
        }

        let listener = {
            let mut bindings = UNIX_BINDINGS.lock();
            prune_dead_bindings(&mut bindings);
            let Some(binding) = bindings.iter().find(|binding| binding.path == path) else {
                return Err(SocketError::NotFound);
            };
            if !binding_allows_connect(binding, self.owner_credentials()) {
                return Err(SocketError::PermissionDenied);
            }
            binding
                .socket
                .upgrade()
                .ok_or(SocketError::ConnectionRefused)?
        };

        let mut listener_state = listener.state.lock();
        let listener_path = listener_state.bound_path.clone();
        let accepted = Self::new_unix_stream_with_owner(listener.owner);
        let SocketState::Listening { backlog, pending } = &mut listener_state.state else {
            return Err(SocketError::ConnectionRefused);
        };
        if pending.len() >= *backlog {
            return Err(SocketError::TryAgain);
        }

        connect_pair(
            self,
            &accepted,
            None,
            listener_path.clone(),
            listener.owner,
            listener_path,
            None,
            self.owner_credentials(),
        )?;
        pending.push_back(accepted);
        Ok(())
    }

    pub(crate) fn accept(&self, nonblocking: bool) -> Result<Self, SocketError> {
        loop {
            let maybe_pending = {
                let mut state = self.inner.state.lock();
                match &mut state.state {
                    SocketState::Listening { pending, .. } => pending.pop_front(),
                    _ => return Err(SocketError::InvalidArgument),
                }
            };

            if let Some(socket) = maybe_pending {
                return Ok(socket);
            }
            if nonblocking {
                return Err(SocketError::TryAgain);
            }

            rtc::sleep(1);
        }
    }

    pub(crate) fn send(&self, src: &[u8], nonblocking: bool) -> Result<usize, SocketError> {
        if src.is_empty() {
            return Ok(0);
        }

        let mut sent = 0usize;
        while sent < src.len() {
            let peer = {
                let state = self.inner.state.lock();
                match &state.state {
                    SocketState::Connected(connected) => {
                        if connected.send_closed {
                            return if sent == 0 {
                                Err(SocketError::BrokenPipe)
                            } else {
                                Ok(sent)
                            };
                        }
                        if connected.peer_read_closed {
                            return if sent == 0 {
                                Err(SocketError::BrokenPipe)
                            } else {
                                Ok(sent)
                            };
                        }
                        if connected.peer_closed {
                            return if sent == 0 {
                                Err(SocketError::BrokenPipe)
                            } else {
                                Ok(sent)
                            };
                        }
                        match connected.peer.upgrade() {
                            Some(peer) => peer,
                            None => {
                                return if sent == 0 {
                                    Err(SocketError::BrokenPipe)
                                } else {
                                    Ok(sent)
                                };
                            }
                        }
                    }
                    _ => return Err(SocketError::NotConnected),
                }
            };

            let written = {
                let mut peer_state = peer.state.lock();
                let SocketState::Connected(connected) = &mut peer_state.state else {
                    return if sent == 0 {
                        Err(SocketError::NotConnected)
                    } else {
                        Ok(sent)
                    };
                };
                if connected.recv_closed {
                    return if sent == 0 {
                        Err(SocketError::BrokenPipe)
                    } else {
                        Ok(sent)
                    };
                }
                let space = SOCKET_BUFFER_CAPACITY.saturating_sub(connected.incoming_bytes.len());
                if space == 0 {
                    0
                } else {
                    let write_len = space.min(src.len() - sent);
                    connected
                        .incoming_bytes
                        .extend(src[sent..sent + write_len].iter().copied());
                    write_len
                }
            };

            if written == 0 {
                if sent != 0 {
                    return Ok(sent);
                }
                if nonblocking {
                    return Err(SocketError::TryAgain);
                }
                rtc::sleep(1);
                continue;
            }

            sent += written;
        }

        Ok(sent)
    }

    pub(crate) fn send_message(
        &self,
        bytes: Vec<u8>,
        rights: Vec<PassedHandle>,
        nonblocking: bool,
    ) -> Result<usize, SocketError> {
        if bytes.is_empty() {
            return if rights.is_empty() {
                Ok(0)
            } else {
                Err(SocketError::InvalidArgument)
            };
        }

        loop {
            let peer = {
                let state = self.inner.state.lock();
                match &state.state {
                    SocketState::Connected(connected) => {
                        if connected.send_closed {
                            return Err(SocketError::BrokenPipe);
                        }
                        if connected.peer_read_closed {
                            return Err(SocketError::BrokenPipe);
                        }
                        if connected.peer_closed {
                            return Err(SocketError::BrokenPipe);
                        }
                        connected.peer.upgrade().ok_or(SocketError::BrokenPipe)?
                    }
                    _ => return Err(SocketError::NotConnected),
                }
            };

            let queued = {
                let mut peer_state = peer.state.lock();
                let SocketState::Connected(connected) = &mut peer_state.state else {
                    return Err(SocketError::NotConnected);
                };
                if connected.recv_closed {
                    return Err(SocketError::BrokenPipe);
                }
                let space = SOCKET_BUFFER_CAPACITY.saturating_sub(connected.incoming_bytes.len());
                if space == 0 {
                    0
                } else {
                    let write_len = space.min(bytes.len());
                    let byte_offset = connected.incoming_bytes.len();
                    connected
                        .incoming_bytes
                        .extend(bytes[..write_len].iter().copied());
                    if !rights.is_empty() {
                        connected.incoming_rights.push_back(SocketAncillary {
                            byte_offset,
                            rights: rights.clone(),
                        });
                    }
                    write_len
                }
            };

            if queued != 0 {
                return Ok(queued);
            }
            if nonblocking {
                return Err(SocketError::TryAgain);
            }
            rtc::sleep(1);
        }
    }

    pub(crate) fn recv(&self, dest: &mut [u8], nonblocking: bool) -> Result<usize, SocketError> {
        self.recv_with_rights(dest, nonblocking)
            .map(|(read, _)| read)
    }

    pub(crate) fn recv_with_rights(
        &self,
        dest: &mut [u8],
        nonblocking: bool,
    ) -> Result<(usize, Vec<PassedHandle>), SocketError> {
        if dest.is_empty() {
            return Ok((0, Vec::new()));
        }

        loop {
            let mut state = self.inner.state.lock();
            let SocketState::Connected(connected) = &mut state.state else {
                return Err(SocketError::NotConnected);
            };

            if connected.recv_closed {
                return Ok((0, Vec::new()));
            }

            if !connected.incoming_bytes.is_empty() {
                let mut collected_rights = Vec::new();
                let read = dest.len().min(connected.incoming_bytes.len());
                while connected
                    .incoming_rights
                    .front()
                    .is_some_and(|marker| marker.byte_offset < read)
                {
                    if let Some(mut marker) = connected.incoming_rights.pop_front() {
                        collected_rights.append(&mut marker.rights);
                    }
                }
                for marker in connected.incoming_rights.iter_mut() {
                    marker.byte_offset = marker.byte_offset.saturating_sub(read);
                }
                for byte in dest.iter_mut().take(read) {
                    *byte = connected
                        .incoming_bytes
                        .pop_front()
                        .expect("incoming byte count must match readable length");
                }
                return Ok((read, collected_rights));
            }

            if connected.peer_closed || connected.peer_write_closed {
                return Ok((0, Vec::new()));
            }

            if nonblocking {
                return Err(SocketError::TryAgain);
            }

            drop(state);
            rtc::sleep(1);
        }
    }

    pub(crate) fn poll_revents(&self, requested: i16) -> i16 {
        let mut ready = 0_i16;
        let maybe_peer = {
            let state = self.inner.state.lock();
            match &state.state {
                SocketState::Idle => return 0,
                SocketState::Listening { pending, .. } => {
                    if !pending.is_empty() {
                        ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
                    }
                    return ready;
                }
                SocketState::Connected(connected) => {
                    if !connected.incoming_bytes.is_empty() {
                        ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
                    }
                    if connected.recv_closed {
                        ready |= linux_abi::POLLHUP;
                        ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
                        return ready;
                    }
                    if connected.peer_closed || connected.peer_write_closed {
                        ready |= linux_abi::POLLHUP;
                        ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
                        return ready;
                    }
                    if connected.peer_read_closed {
                        ready |= linux_abi::POLLERR | linux_abi::POLLHUP;
                        return ready;
                    }
                    if connected.send_closed {
                        return ready;
                    }
                    connected.peer.upgrade()
                }
            }
        };

        if let Some(peer) = maybe_peer {
            let peer_state = peer.state.lock();
            if let SocketState::Connected(connected) = &peer_state.state {
                if !connected.recv_closed && connected.incoming_bytes.len() < SOCKET_BUFFER_CAPACITY
                {
                    ready |= requested & linux_abi::POLLOUT;
                }
            } else {
                ready |= linux_abi::POLLHUP;
            }
        } else {
            ready |= linux_abi::POLLHUP;
        }

        ready
    }

    pub(crate) fn readable_bytes(&self) -> usize {
        let state = self.inner.state.lock();
        match &state.state {
            SocketState::Connected(connected) => connected.incoming_bytes.len(),
            SocketState::Listening { pending, .. } => pending.len(),
            SocketState::Idle => 0,
        }
    }

    pub(crate) fn shutdown(&self, how: u64) -> Result<(), SocketError> {
        let (shutdown_read, shutdown_write) = match how {
            linux_abi::SHUT_RD => (true, false),
            linux_abi::SHUT_WR => (false, true),
            linux_abi::SHUT_RDWR => (true, true),
            _ => return Err(SocketError::InvalidArgument),
        };

        let (peer, notify_read_closed, notify_write_closed) = {
            let mut state = self.inner.state.lock();
            let SocketState::Connected(connected) = &mut state.state else {
                return Err(SocketError::NotConnected);
            };
            if shutdown_read {
                connected.recv_closed = true;
                connected.incoming_bytes.clear();
                connected.incoming_rights.clear();
            }
            if shutdown_write {
                connected.send_closed = true;
            }
            (
                if shutdown_read || shutdown_write {
                    connected.peer.upgrade()
                } else {
                    None
                },
                shutdown_read,
                shutdown_write,
            )
        };

        if let Some(peer) = peer {
            let self_ptr = Arc::as_ptr(&self.inner);
            let mut peer_state = peer.state.lock();
            if let SocketState::Connected(connected) = &mut peer_state.state {
                if connected.peer.as_ptr() == self_ptr.cast_mut() {
                    if notify_read_closed {
                        connected.peer_read_closed = true;
                    }
                    if notify_write_closed {
                        connected.peer_write_closed = true;
                    }
                }
            }
        }

        Ok(())
    }
}

impl PassedHandle {
    pub(crate) fn new(handle: KernelHandle, status_flags: u64) -> Self {
        let rights = handle.default_rights(status_flags);
        Self::new_with_rights(handle, status_flags, rights)
    }

    pub(crate) fn new_with_rights(
        handle: KernelHandle,
        status_flags: u64,
        rights: HandleRights,
    ) -> Self {
        Self {
            handle,
            status_flags,
            rights,
        }
    }

    pub(crate) fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    pub(crate) fn status_flags(&self) -> u64 {
        self.status_flags
    }

    pub(crate) fn rights(&self) -> HandleRights {
        self.rights
    }
}

impl Drop for SocketObject {
    fn drop(&mut self) {
        let self_ptr = self as *const SocketObject;
        let (bound_path, peer) = {
            let state = self.state.lock();
            let peer = match &state.state {
                SocketState::Connected(connected) => connected.peer.upgrade(),
                _ => None,
            };
            (state.bound_path.clone(), peer)
        };

        if let Some(path) = bound_path {
            let mut bindings = UNIX_BINDINGS.lock();
            bindings.retain(|binding| {
                if binding.path != path {
                    return binding.socket.upgrade().is_some();
                }
                let Some(socket) = binding.socket.upgrade() else {
                    return false;
                };
                !ptr::eq(Arc::as_ptr(&socket), self_ptr)
            });
        }

        if let Some(peer) = peer {
            let mut state = peer.state.lock();
            if let SocketState::Connected(connected) = &mut state.state {
                if connected.peer.as_ptr() == self_ptr.cast_mut() {
                    connected.peer_closed = true;
                    connected.peer_write_closed = true;
                    connected.peer = Weak::new();
                }
            }
        }
    }
}

fn connect_pair(
    left: &SocketHandle,
    right: &SocketHandle,
    left_local_path: Option<String>,
    left_peer_path: Option<String>,
    left_peer_credentials: SocketCredentials,
    right_local_path: Option<String>,
    right_peer_path: Option<String>,
    right_peer_credentials: SocketCredentials,
) -> Result<(), SocketError> {
    let left_ptr = Arc::as_ptr(&left.inner) as usize;
    let right_ptr = Arc::as_ptr(&right.inner) as usize;
    if left_ptr == right_ptr {
        return Err(SocketError::InvalidArgument);
    }

    let (first, second, left_is_first) = if left_ptr < right_ptr {
        (&left.inner, &right.inner, true)
    } else {
        (&right.inner, &left.inner, false)
    };

    let mut first_state = first.state.lock();
    let mut second_state = second.state.lock();
    let (left_state, right_state) = if left_is_first {
        (&mut first_state, &mut second_state)
    } else {
        (&mut second_state, &mut first_state)
    };

    match &left_state.state {
        SocketState::Connected(_) => return Err(SocketError::IsConnected),
        SocketState::Listening { .. } => return Err(SocketError::InvalidArgument),
        SocketState::Idle => {}
    }
    match &right_state.state {
        SocketState::Connected(_) => return Err(SocketError::IsConnected),
        SocketState::Listening { .. } => return Err(SocketError::InvalidArgument),
        SocketState::Idle => {}
    }

    left_state.state = SocketState::Connected(ConnectedState {
        incoming_bytes: VecDeque::new(),
        incoming_rights: VecDeque::new(),
        peer: Arc::downgrade(&right.inner),
        peer_closed: false,
        peer_read_closed: false,
        peer_write_closed: false,
        peer_credentials: left_peer_credentials,
        recv_closed: false,
        send_closed: false,
    });
    left_state.local_path = left_local_path;
    left_state.peer_path = left_peer_path;
    right_state.state = SocketState::Connected(ConnectedState {
        incoming_bytes: VecDeque::new(),
        incoming_rights: VecDeque::new(),
        peer: Arc::downgrade(&left.inner),
        peer_closed: false,
        peer_read_closed: false,
        peer_write_closed: false,
        peer_credentials: right_peer_credentials,
        recv_closed: false,
        send_closed: false,
    });
    right_state.local_path = right_local_path;
    right_state.peer_path = right_peer_path;
    Ok(())
}

fn validate_unix_path(path: &str) -> Result<(), SocketError> {
    if path.is_empty() || path.len() > linux_abi::UNIX_PATH_MAX {
        return Err(SocketError::InvalidArgument);
    }
    if path.as_bytes()[0] == 0 {
        return Err(SocketError::InvalidArgument);
    }
    Ok(())
}

fn validate_bind_path(path: &str, owner: SocketCredentials) -> Result<String, SocketError> {
    let normalized = normalize_unix_path(path)?;
    if !path_is_in_runtime_dir(normalized.as_str(), owner.uid())
        && !(owner.uid() == 0 && path_is_in_system_runtime_dir(normalized.as_str()))
    {
        return Err(SocketError::PermissionDenied);
    }
    Ok(normalized)
}

fn validate_connect_path(path: &str, owner: SocketCredentials) -> Result<String, SocketError> {
    let normalized = normalize_unix_path(path)?;
    if !path_is_in_runtime_dir(normalized.as_str(), owner.uid())
        && !path_is_in_system_runtime_dir(normalized.as_str())
    {
        return Err(SocketError::PermissionDenied);
    }
    Ok(normalized)
}

fn normalize_unix_path(path: &str) -> Result<String, SocketError> {
    validate_unix_path(path)?;
    if !path.starts_with('/') {
        return Err(SocketError::InvalidArgument);
    }

    let mut components = Vec::new();
    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            components.pop();
            continue;
        }
        components.push(component);
    }

    if components.is_empty() {
        return Err(SocketError::InvalidArgument);
    }

    let mut normalized = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    Ok(normalized)
}

fn path_is_in_runtime_dir(path: &str, uid: u32) -> bool {
    let runtime_dir = runtime_dir_for_uid(uid);
    path != runtime_dir
        && path
            .strip_prefix(runtime_dir.as_str())
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

fn path_is_in_system_runtime_dir(path: &str) -> bool {
    let user_runtime_prefix = alloc::format!("{UNIX_RUNTIME_ROOT}/");
    path != UNIX_SYSTEM_RUNTIME_ROOT
        && !path.starts_with(user_runtime_prefix.as_str())
        && path
            .strip_prefix(UNIX_SYSTEM_RUNTIME_ROOT)
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

fn runtime_dir_for_uid(uid: u32) -> String {
    alloc::format!("{UNIX_RUNTIME_ROOT}/{uid}")
}

fn socket_mode_for_bound_path(path: &str, owner: SocketCredentials) -> u32 {
    if owner.uid() == 0 && path_is_in_system_runtime_dir(path) {
        UNIX_SYSTEM_SOCKET_MODE
    } else {
        UNIX_PRIVATE_SOCKET_MODE
    }
}

fn binding_allows_connect(binding: &UnixBinding, owner: SocketCredentials) -> bool {
    if binding.mode == UNIX_SYSTEM_SOCKET_MODE && path_is_in_system_runtime_dir(&binding.path) {
        return true;
    }
    binding.mode == UNIX_PRIVATE_SOCKET_MODE && binding.owner.uid() == owner.uid()
}

fn prune_dead_bindings(bindings: &mut Vec<UnixBinding>) {
    bindings.retain(|binding| binding.socket.upgrade().is_some());
}

pub(crate) fn unlink_bound_path(
    path: &str,
    requester: SocketCredentials,
) -> Result<(), SocketError> {
    let path = validate_bind_path(path, requester)?;

    let binding = {
        let mut bindings = UNIX_BINDINGS.lock();
        prune_dead_bindings(&mut bindings);
        let Some(index) = bindings.iter().position(|binding| binding.path == path) else {
            return Err(SocketError::NotFound);
        };
        if bindings[index].owner.uid() != requester.uid() && requester.uid() != 0 {
            return Err(SocketError::PermissionDenied);
        }
        bindings.remove(index)
    };

    if let Some(socket) = binding.socket.upgrade() {
        let mut state = socket.state.lock();
        if state.bound_path.as_deref() == Some(path.as_str()) {
            state.bound_path = None;
        }
        if state.local_path.as_deref() == Some(path.as_str()) {
            state.local_path = None;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user::handles::{KernelHandle, VfsFileHandle};

    #[test]
    fn stream_reads_can_span_multiple_writes() {
        let (left, right) = SocketHandle::socketpair(SocketCredentials::default());
        assert_eq!(left.send(b"hello", false).unwrap(), 5);
        assert_eq!(left.send(b"world", false).unwrap(), 5);

        let mut buffer = [0_u8; 10];
        let read = right.recv(&mut buffer, false).unwrap();
        assert_eq!(read, 10);
        assert_eq!(&buffer, b"helloworld");
    }

    #[test]
    fn rights_arrive_with_associated_byte() {
        let (left, right) = SocketHandle::socketpair(SocketCredentials::default());
        let passed = PassedHandle::new(
            KernelHandle::VfsFile(VfsFileHandle::read_only_memory(
                String::from("/test"),
                Vec::new(),
            )),
            linux_abi::O_RDONLY,
        );

        assert_eq!(left.send(b"ab", false).unwrap(), 2);
        assert_eq!(
            left.send_message(vec![b'c', b'd'], vec![passed.clone()], false)
                .unwrap(),
            2
        );

        let mut first = [0_u8; 2];
        let (read, rights) = right.recv_with_rights(&mut first, false).unwrap();
        assert_eq!(read, 2);
        assert!(rights.is_empty());
        assert_eq!(&first, b"ab");

        let mut second = [0_u8; 1];
        let (read, rights) = right.recv_with_rights(&mut second, false).unwrap();
        assert_eq!(read, 1);
        assert_eq!(rights.len(), 1);
        assert_eq!(&second, b"c");
    }

    #[test]
    fn shutdown_write_turns_into_peer_eof() {
        let (left, right) = SocketHandle::socketpair(SocketCredentials::default());
        assert_eq!(left.send(b"x", false).unwrap(), 1);
        left.shutdown(linux_abi::SHUT_WR).unwrap();

        let mut first = [0_u8; 1];
        assert_eq!(right.recv(&mut first, false).unwrap(), 1);
        assert_eq!(&first, b"x");

        let mut second = [0_u8; 1];
        assert_eq!(right.recv(&mut second, false).unwrap(), 0);
    }

    #[test]
    fn sending_after_peer_shutdown_reports_broken_pipe() {
        let (left, right) = SocketHandle::socketpair(SocketCredentials::default());
        right.shutdown(linux_abi::SHUT_RD).unwrap();
        assert_eq!(left.send(b"x", false), Err(SocketError::BrokenPipe));
    }

    #[test]
    fn bind_requires_owner_runtime_dir() {
        let socket =
            SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(1, 1000, 1000));
        assert!(socket.bind("/run/user/1000/bind-test.sock").is_ok());

        let other = SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(2, 1000, 1000));
        assert_eq!(
            other.bind("/tmp/wayland-0"),
            Err(SocketError::PermissionDenied)
        );
    }

    #[test]
    fn connect_requires_matching_runtime_uid() {
        let listener =
            SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(1, 1000, 1000));
        listener.bind("/run/user/1000/connect-test.sock").unwrap();
        listener.listen(8).unwrap();

        let peer = SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(2, 1001, 1001));
        assert_eq!(
            peer.connect("/run/user/1000/connect-test.sock"),
            Err(SocketError::PermissionDenied)
        );
    }

    #[test]
    fn root_can_bind_system_runtime_socket() {
        let listener = SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(1, 0, 0));
        assert!(listener.bind("/run/root-bind.sock").is_ok());
    }

    #[test]
    fn desktop_uid_can_connect_to_root_system_runtime_socket() {
        let listener = SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(1, 0, 0));
        listener.bind("/run/root-connect.sock").unwrap();
        listener.listen(8).unwrap();

        let peer = SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(2, 1000, 1000));
        assert!(peer.connect("/run/root-connect.sock").is_ok());
    }

    #[test]
    fn owner_can_unlink_bound_runtime_socket_path() {
        let listener =
            SocketHandle::new_unix_stream_with_owner(SocketCredentials::new(1, 1000, 1000));
        listener.bind("/run/user/1000/unlink-test.sock").unwrap();

        assert!(unlink_bound_path(
            "/run/user/1000/unlink-test.sock",
            SocketCredentials::new(1, 1000, 1000)
        )
        .is_ok());
        assert!(listener.bound_path().is_none());
    }
}
