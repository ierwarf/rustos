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

const MAX_LISTEN_BACKLOG: usize = 128;
const SOCKET_BUFFER_CAPACITY: usize = 1024 * 1024;

lazy_static! {
    static ref UNIX_BINDINGS: Mutex<Vec<UnixBinding>> = Mutex::new(Vec::new());
}

#[derive(Debug)]
struct UnixBinding {
    path: String,
    socket: Weak<SocketObject>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SocketError {
    AddressInUse,
    ConnectionRefused,
    InvalidArgument,
    IsConnected,
    NotConnected,
    NotFound,
    TryAgain,
    Unsupported,
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
    incoming: VecDeque<SocketMessage>,
    buffered_bytes: usize,
    peer: Weak<SocketObject>,
    peer_closed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PassedHandle {
    handle: KernelHandle,
    status_flags: u64,
}

#[derive(Clone, Debug)]
struct SocketMessage {
    bytes: Vec<u8>,
    cursor: usize,
    rights: Vec<PassedHandle>,
}

#[derive(Debug)]
struct SocketInner {
    bound_path: Option<String>,
    state: SocketState,
}

#[derive(Debug)]
struct SocketObject {
    state: Mutex<SocketInner>,
}

#[derive(Clone, Debug)]
pub(crate) struct SocketHandle {
    inner: Arc<SocketObject>,
}

impl SocketHandle {
    pub(crate) fn new_unix_stream() -> Self {
        Self {
            inner: Arc::new(SocketObject {
                state: Mutex::new(SocketInner {
                    bound_path: None,
                    state: SocketState::Idle,
                }),
            }),
        }
    }

    pub(crate) fn socketpair() -> (Self, Self) {
        let left = Self::new_unix_stream();
        let right = Self::new_unix_stream();
        connect_pair(&left, &right).expect("fresh socketpair endpoints must connect");
        (left, right)
    }

    pub(crate) fn bound_path(&self) -> Option<String> {
        self.inner.state.lock().bound_path.clone()
    }

    pub(crate) fn bind(&self, path: &str) -> Result<(), SocketError> {
        validate_unix_path(path)?;

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
            path: String::from(path),
            socket: Arc::downgrade(&self.inner),
        });
        state.bound_path = Some(String::from(path));
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
        validate_unix_path(path)?;

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
            binding
                .socket
                .upgrade()
                .ok_or(SocketError::ConnectionRefused)?
        };

        let accepted = Self::new_unix_stream();
        let mut listener_state = listener.state.lock();
        let SocketState::Listening { backlog, pending } = &mut listener_state.state else {
            return Err(SocketError::ConnectionRefused);
        };
        if pending.len() >= *backlog {
            return Err(SocketError::TryAgain);
        }

        connect_pair(self, &accepted)?;
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
                        if connected.peer_closed {
                            return if sent == 0 {
                                Err(SocketError::NotConnected)
                            } else {
                                Ok(sent)
                            };
                        }
                        match connected.peer.upgrade() {
                            Some(peer) => peer,
                            None => {
                                return if sent == 0 {
                                    Err(SocketError::NotConnected)
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
                let space = SOCKET_BUFFER_CAPACITY.saturating_sub(connected.buffered_bytes);
                if space == 0 {
                    0
                } else {
                    let write_len = space.min(src.len() - sent);
                    connected.incoming.push_back(SocketMessage {
                        bytes: src[sent..sent + write_len].to_vec(),
                        cursor: 0,
                        rights: Vec::new(),
                    });
                    connected.buffered_bytes = connected.buffered_bytes.saturating_add(write_len);
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

        let message = SocketMessage {
            cursor: 0,
            bytes,
            rights,
        };
        loop {
            let peer = {
                let state = self.inner.state.lock();
                match &state.state {
                    SocketState::Connected(connected) => {
                        if connected.peer_closed {
                            return Err(SocketError::NotConnected);
                        }
                        connected.peer.upgrade().ok_or(SocketError::NotConnected)?
                    }
                    _ => return Err(SocketError::NotConnected),
                }
            };

            let queued = {
                let mut peer_state = peer.state.lock();
                let SocketState::Connected(connected) = &mut peer_state.state else {
                    return Err(SocketError::NotConnected);
                };
                let space = SOCKET_BUFFER_CAPACITY.saturating_sub(connected.buffered_bytes);
                if space < message.bytes.len() {
                    false
                } else {
                    connected.buffered_bytes =
                        connected.buffered_bytes.saturating_add(message.bytes.len());
                    connected.incoming.push_back(message.clone());
                    true
                }
            };

            if queued {
                return Ok(message.bytes.len());
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

            if !connected.incoming.is_empty() {
                let mut collected_rights = Vec::new();
                let mut read = 0usize;
                while read < dest.len() {
                    let Some(front) = connected.incoming.front_mut() else {
                        break;
                    };
                    if front.cursor >= front.bytes.len() {
                        connected.incoming.pop_front();
                        continue;
                    }

                    if !front.rights.is_empty() {
                        collected_rights.append(&mut front.rights);
                    }

                    let remaining = front.bytes.len() - front.cursor;
                    let chunk_len = (dest.len() - read).min(remaining);
                    dest[read..read + chunk_len]
                        .copy_from_slice(&front.bytes[front.cursor..front.cursor + chunk_len]);
                    front.cursor += chunk_len;
                    read += chunk_len;
                    connected.buffered_bytes = connected.buffered_bytes.saturating_sub(chunk_len);
                    if front.cursor == front.bytes.len() {
                        connected.incoming.pop_front();
                    }
                }
                return Ok((read, collected_rights));
            }

            if connected.peer_closed {
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
                    if connected.buffered_bytes != 0 {
                        ready |= requested & (linux_abi::POLLIN | linux_abi::POLLPRI);
                    }
                    if connected.peer_closed {
                        ready |= linux_abi::POLLHUP;
                        return ready;
                    }
                    connected.peer.upgrade()
                }
            }
        };

        if let Some(peer) = maybe_peer {
            let peer_state = peer.state.lock();
            if let SocketState::Connected(connected) = &peer_state.state {
                if connected.buffered_bytes < SOCKET_BUFFER_CAPACITY {
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
}

impl PassedHandle {
    pub(crate) fn new(handle: KernelHandle, status_flags: u64) -> Self {
        Self {
            handle,
            status_flags,
        }
    }

    pub(crate) fn handle(&self) -> &KernelHandle {
        &self.handle
    }

    pub(crate) fn status_flags(&self) -> u64 {
        self.status_flags
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
                    connected.peer = Weak::new();
                }
            }
        }
    }
}

fn connect_pair(left: &SocketHandle, right: &SocketHandle) -> Result<(), SocketError> {
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
        incoming: VecDeque::new(),
        buffered_bytes: 0,
        peer: Arc::downgrade(&right.inner),
        peer_closed: false,
    });
    right_state.state = SocketState::Connected(ConnectedState {
        incoming: VecDeque::new(),
        buffered_bytes: 0,
        peer: Arc::downgrade(&left.inner),
        peer_closed: false,
    });
    Ok(())
}

fn validate_unix_path(path: &str) -> Result<(), SocketError> {
    if path.is_empty() || path.len() > linux_abi::UNIX_PATH_MAX {
        return Err(SocketError::InvalidArgument);
    }
    if path.as_bytes()[0] == 0 {
        return Err(SocketError::Unsupported);
    }
    Ok(())
}

fn prune_dead_bindings(bindings: &mut Vec<UnixBinding>) {
    bindings.retain(|binding| binding.socket.upgrade().is_some());
}
