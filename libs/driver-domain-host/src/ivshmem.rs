//! Launch-private ivshmem-doorbell broker.
//!
//! QEMU's reference server is explicitly not suitable for production.  This
//! broker intentionally implements only launch-local, two-peer fixed-vector
//! topologies. The GUI path uses two vectors (control/offline); the L0-to-RustOS
//! input path uses exactly one producer-to-consumer vector. The socket lives in
//! the launch owner's 0700 directory, accepts only that uid, and never allocates
//! a peer, vector, or shared-memory object from guest input.

use std::fs::{self, File};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{
    Arc,
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

const IVSHMEM_PROTOCOL_VERSION: i64 = 0;
const IVSHMEM_SHARED_MEMORY_MESSAGE: i64 = -1;
const IVSHMEM_GUI_VECTOR_COUNT: usize = 2;
const IVSHMEM_INPUT_VECTOR_COUNT: usize = 1;
const IVSHMEM_PEER_COUNT: usize = 2;
const POLL_TIMEOUT_MS: i32 = 100;

#[repr(align(8))]
struct AncillaryBuffer([u8; 32]);

/// Owns the private server for one fixed, launch-local two-peer topology.
pub struct IvshmemDoorbellServer {
    socket_path: PathBuf,
    shutdown: SyncSender<()>,
    peer_count: Arc<AtomicUsize>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl IvshmemDoorbellServer {
    /// Start the strict two-peer, two-vector GUI topology.
    pub fn start(socket_path: &Path, shared_memory: &File) -> Result<Self> {
        Self::start_with_vector_count(socket_path, shared_memory, IVSHMEM_GUI_VECTOR_COUNT)
    }

    /// Start the strict two-peer input topology. Peer 0 is RustOS. Peer 1 is
    /// the L0 producer and receives only peer 0's one fixed eventfd; it cannot
    /// select a peer or vector at runtime.
    pub fn start_input(socket_path: &Path, shared_memory: &File) -> Result<Self> {
        Self::start_with_vector_count(socket_path, shared_memory, IVSHMEM_INPUT_VECTOR_COUNT)
    }

    fn start_with_vector_count(
        socket_path: &Path,
        shared_memory: &File,
        vector_count: usize,
    ) -> Result<Self> {
        if vector_count == 0 || vector_count > IVSHMEM_GUI_VECTOR_COUNT {
            bail!("invalid fixed ivshmem vector count {vector_count}");
        }
        validate_launch_socket_path(socket_path)?;
        validate_shared_memory(shared_memory)?;
        let _ = fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("bind ivshmem doorbell {}", socket_path.display()))?;
        fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!("restrict ivshmem doorbell socket {}", socket_path.display())
        })?;
        listener
            .set_nonblocking(true)
            .context("make ivshmem doorbell listener nonblocking")?;
        let shared_memory = shared_memory
            .try_clone()
            .context("duplicate ivshmem shared-memory backing file")?;
        let (shutdown, shutdown_rx) = mpsc::sync_channel(1);
        let peer_count = Arc::new(AtomicUsize::new(0));
        let worker_peer_count = Arc::clone(&peer_count);
        let socket_path = socket_path.to_path_buf();
        let worker = thread::Builder::new()
            .name("rustos-ivshmem-doorbell".into())
            .spawn(move || {
                serve(
                    listener,
                    shared_memory,
                    shutdown_rx,
                    worker_peer_count,
                    vector_count,
                )
            })
            .context("start ivshmem doorbell broker")?;
        Ok(Self {
            socket_path,
            shutdown,
            peer_count,
            worker: Some(worker),
        })
    }

    /// Pin RustOS as peer 0 before the GUI DVM may connect. QEMU's ivshmem
    /// server protocol carries no client identity, so launch order must be
    /// made observable rather than inferred from scheduler timing.
    pub fn wait_for_peer_count(&self, expected: usize, timeout: Duration) -> Result<()> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow!("ivshmem peer wait deadline overflow"))?;
        loop {
            if self.peer_count.load(Ordering::Acquire) >= expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "ivshmem peer {} did not connect within {:?}; refusing GUI-DVM launch",
                    expected,
                    timeout
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for IvshmemDoorbellServer {
    fn drop(&mut self) {
        let _ = self.shutdown.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

struct Peer {
    id: i64,
    stream: UnixStream,
    receive_events: Vec<OwnedFd>,
}

fn serve(
    listener: UnixListener,
    shared_memory: File,
    shutdown: Receiver<()>,
    peer_count: Arc<AtomicUsize>,
    vector_count: usize,
) -> Result<()> {
    let mut peers = Vec::<Peer>::with_capacity(IVSHMEM_PEER_COUNT);
    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => {}
        }
        accept_ready_peers(
            &listener,
            &shared_memory,
            &mut peers,
            &peer_count,
            vector_count,
        )?;
        reap_disconnected_peers(&mut peers)?;
        thread::sleep(Duration::from_millis(POLL_TIMEOUT_MS as u64));
    }
}

fn accept_ready_peers(
    listener: &UnixListener,
    shared_memory: &File,
    peers: &mut Vec<Peer>,
    peer_count: &AtomicUsize,
    vector_count: usize,
) -> Result<()> {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if peers.len() >= IVSHMEM_PEER_COUNT || !peer_is_launch_owner(&stream)? {
                    drop(stream);
                    continue;
                }
                stream
                    .set_nonblocking(true)
                    .context("make ivshmem client socket nonblocking")?;
                let id = i64::try_from(peers.len()).expect("two peers fit ivshmem ID");
                let receive_events = (0..vector_count)
                    .map(|_| create_eventfd())
                    .collect::<Result<Vec<_>>>()?;
                send_i64_with_fd(stream.as_raw_fd(), IVSHMEM_PROTOCOL_VERSION, None)?;
                send_i64_with_fd(stream.as_raw_fd(), id, None)?;
                send_i64_with_fd(
                    stream.as_raw_fd(),
                    IVSHMEM_SHARED_MEMORY_MESSAGE,
                    Some(shared_memory.as_raw_fd()),
                )?;
                for existing in peers.iter() {
                    for receive_event in &existing.receive_events {
                        send_i64_with_fd(
                            stream.as_raw_fd(),
                            existing.id,
                            Some(receive_event.as_raw_fd()),
                        )?;
                    }
                }
                for receive_event in &receive_events {
                    send_i64_with_fd(stream.as_raw_fd(), id, Some(receive_event.as_raw_fd()))?;
                }

                for existing in peers.iter() {
                    for receive_event in &receive_events {
                        send_i64_with_fd(
                            existing.stream.as_raw_fd(),
                            id,
                            Some(receive_event.as_raw_fd()),
                        )?;
                    }
                }
                peers.push(Peer {
                    id,
                    stream,
                    receive_events,
                });
                peer_count.store(peers.len(), Ordering::Release);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error).context("accept ivshmem doorbell client"),
        }
    }
}

fn reap_disconnected_peers(peers: &mut [Peer]) -> Result<()> {
    for peer in peers.iter() {
        let fd = peer.stream.as_raw_fd();
        let mut byte = [0_u8; 1];
        let result = unsafe {
            libc::recv(
                fd,
                byte.as_mut_ptr().cast::<libc::c_void>(),
                byte.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if result == 0 {
            bail!(
                "ivshmem peer {} disconnected; tear down the paired GUI topology",
                peer.id
            );
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error)
                    .with_context(|| format!("ivshmem peer {} lifecycle socket failed", peer.id));
            }
        }
    }
    Ok(())
}

fn create_eventfd() -> Result<OwnedFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("create ivshmem eventfd");
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn peer_is_launch_owner(stream: &UnixStream) -> Result<bool> {
    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast::<libc::c_void>(),
            &mut length,
        )
    };
    if result != 0 || length as usize != size_of::<libc::ucred>() {
        return Err(io::Error::last_os_error()).context("read ivshmem peer credentials");
    }
    Ok(unsafe { credentials.assume_init().uid } == unsafe { libc::geteuid() })
}

fn send_i64_with_fd(fd: RawFd, value: i64, attached_fd: Option<RawFd>) -> Result<()> {
    let bytes = value.to_le_bytes();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast::<libc::c_void>(),
        iov_len: bytes.len(),
    };
    let mut message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let mut control = AncillaryBuffer([0_u8; 32]);
    if let Some(attached_fd) = attached_fd {
        let needed = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as _) as usize };
        if needed > control.0.len() {
            bail!("platform SCM_RIGHTS control record exceeds fixed broker bound");
        }
        message.msg_control = control.0.as_mut_ptr().cast::<libc::c_void>();
        message.msg_controllen = needed;
        let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
        if header.is_null() {
            bail!("failed to initialize SCM_RIGHTS control header");
        }
        unsafe {
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as _) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), attached_fd);
        }
    }
    let written = unsafe { libc::sendmsg(fd, &message, libc::MSG_NOSIGNAL) };
    if written != bytes.len() as isize {
        return Err(io::Error::last_os_error()).context("send fixed ivshmem server record");
    }
    Ok(())
}

/// L0-side peer for the fixed one-vector input topology. It owns neither the
/// backing memory descriptor nor an ivshmem peer identifier: it may only write
/// the eventfd which the broker bound to RustOS peer 0 before this connection.
pub struct IvshmemInputProducer {
    _stream: UnixStream,
    rustos_event: OwnedFd,
}

impl IvshmemInputProducer {
    /// Attach only after RustOS has claimed peer 0. The exact five-record
    /// ivshmem handshake is validated here so a generic client cannot turn the
    /// broker into a peer/vector selection interface.
    pub fn connect(socket_path: &Path, timeout: Duration) -> Result<Self> {
        let started = Instant::now();
        let stream = loop {
            match UnixStream::connect(socket_path) {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::AddrNotAvailable
                    ) && started.elapsed() < timeout =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("connect L0 input ivshmem peer {}", socket_path.display())
                    });
                }
            }
        };
        stream
            .set_read_timeout(Some(timeout))
            .context("set L0 input ivshmem handshake deadline")?;

        let (version, version_fd) = recv_i64_with_fd(stream.as_raw_fd())?;
        if version != IVSHMEM_PROTOCOL_VERSION || version_fd.is_some() {
            bail!("input ivshmem broker rejected protocol version");
        }
        let (self_id, self_id_fd) = recv_i64_with_fd(stream.as_raw_fd())?;
        if self_id != 1 || self_id_fd.is_some() {
            bail!("input ivshmem producer was not pinned as peer 1");
        }
        let (shared_message, shared_memory) = recv_i64_with_fd(stream.as_raw_fd())?;
        if shared_message != IVSHMEM_SHARED_MEMORY_MESSAGE || shared_memory.is_none() {
            bail!("input ivshmem broker omitted the launch-owned backing descriptor");
        }
        // L0 maps the same owner-private file independently. Retaining the
        // descriptor is unnecessary; accepting it here only verifies that the
        // producer joined the exact broker instance used by RustOS.
        drop(shared_memory);
        let (rustos_id, rustos_event) = recv_i64_with_fd(stream.as_raw_fd())?;
        if rustos_id != 0 || rustos_event.is_none() {
            bail!("input ivshmem broker did not bind the fixed RustOS eventfd");
        }
        let (own_id, own_event) = recv_i64_with_fd(stream.as_raw_fd())?;
        if own_id != 1 || own_event.is_none() {
            bail!("input ivshmem broker did not complete the fixed peer handshake");
        }
        // This peer never receives input work, so its local eventfd is not an
        // authority to signal RustOS. Drop it immediately.
        drop(own_event);
        Ok(Self {
            _stream: stream,
            rustos_event: rustos_event.expect("checked eventfd"),
        })
    }

    /// Notify the one RustOS receive vector after the producer has committed
    /// a bounded ring record with release ordering.
    pub fn notify_rustos(&self) -> Result<()> {
        let value = 1_u64.to_ne_bytes();
        let written = unsafe {
            libc::write(
                self.rustos_event.as_raw_fd(),
                value.as_ptr().cast::<libc::c_void>(),
                value.len(),
            )
        };
        if written == value.len() as isize {
            return Ok(());
        }
        Err(io::Error::last_os_error()).context("ring fixed RustOS input eventfd")
    }
}

fn recv_i64_with_fd(fd: RawFd) -> Result<(i64, Option<OwnedFd>)> {
    let mut bytes = [0_u8; size_of::<i64>()];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: bytes.len(),
    };
    let mut control = AncillaryBuffer([0_u8; 32]);
    let mut message = libc::msghdr {
        msg_name: std::ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.0.as_mut_ptr().cast::<libc::c_void>(),
        msg_controllen: control.0.len(),
        msg_flags: 0,
    };
    let received = unsafe { libc::recvmsg(fd, &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received != bytes.len() as isize || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::last_os_error()).context("receive fixed ivshmem broker record");
    }
    let mut attached = None;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !header.is_null() {
        let is_fd = unsafe {
            (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS
        };
        if is_fd {
            let expected = unsafe { libc::CMSG_LEN(size_of::<RawFd>() as _) as usize };
            if unsafe { (*header).cmsg_len as usize } != expected || attached.is_some() {
                bail!("invalid fixed ivshmem SCM_RIGHTS record");
            }
            let raw_fd =
                unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>()) };
            if raw_fd < 0 {
                bail!("ivshmem broker returned an invalid file descriptor");
            }
            attached = Some(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        }
        header = unsafe { libc::CMSG_NXTHDR(&message, header) };
    }
    Ok((i64::from_le_bytes(bytes), attached))
}

fn validate_launch_socket_path(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("ivshmem socket path has no parent"))?;
    let metadata = fs::metadata(parent)
        .with_context(|| format!("stat ivshmem socket parent {}", parent.display()))?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "ivshmem socket parent {} must be current-user owned and mode 0700 or stricter",
            parent.display()
        );
    }
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("lstat ivshmem socket {}", path.display()))?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            bail!(
                "refusing to replace unsafe ivshmem socket {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn validate_shared_memory(file: &File) -> Result<()> {
    let metadata = file
        .metadata()
        .context("stat ivshmem shared-memory backing")?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        bail!("ivshmem backing must be a current-user-owned 0600 regular file");
    }
    if metadata.len() == 0 {
        bail!("ivshmem backing must be nonempty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use super::{IvshmemDoorbellServer, validate_launch_socket_path};

    #[test]
    fn rejects_a_nonprivate_socket_parent() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_launch_socket_path(&directory.path().join("doorbell.sock")).is_err());
    }

    #[test]
    fn starts_only_with_private_backing() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let backing_path = directory.path().join("display.bin");
        let backing = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backing_path)
            .unwrap();
        backing.set_len(4096).unwrap();
        let socket = directory.path().join("doorbell.sock");
        let server = IvshmemDoorbellServer::start(&socket, &backing).unwrap();
        assert!(socket.exists());
        drop(server);
        assert!(!socket.exists());
    }

    #[test]
    fn waits_until_the_first_pinned_peer_has_connected() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let backing_path = directory.path().join("display.bin");
        let backing = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backing_path)
            .unwrap();
        backing.set_len(4096).unwrap();
        let socket = directory.path().join("doorbell.sock");
        let server = IvshmemDoorbellServer::start(&socket, &backing).unwrap();
        assert!(
            server
                .wait_for_peer_count(1, Duration::from_millis(20))
                .is_err()
        );
        let _peer = UnixStream::connect(&socket).unwrap();
        server
            .wait_for_peer_count(1, Duration::from_secs(1))
            .unwrap();
    }
}
