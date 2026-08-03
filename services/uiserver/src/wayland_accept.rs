//! Readiness-gated Wayland listener admission.
//!
//! - **Owner:** Uiserver owns the listener, accepted-stream queue, and worker.
//! - **Boundary:** Listener readiness and accepted local sockets are untrusted.
//! - **Lifecycle:** The worker and listener live for the uiserver process.
//! - **Concurrency:** One worker publishes queue ownership before the UI wake;
//!   the UI thread is the only consumer and validates every ownership token.
//! - **Failure:** Wait, accept, and ownership failures terminate uiserver.
//! - **Forbidden:** Fixed-cadence accept probing, unbounded queues, silent
//!   ownership underflow, and blocking the UI thread on listener RPC.
//! - **Evidence:** `wayland-accept-isolation/WaylandAcceptIsolation` and the
//!   host epoll-readiness test below.

use std::io::ErrorKind;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread;

use crate::input_loop::UiWakeSender;
use crate::sys::{diag_line, require_background_thread_class};

const WAYLAND_ACCEPT_QUEUE_CAPACITY: usize = 16;
const WAYLAND_ACCEPT_WAIT_TIMEOUT_MS: i32 = -1;

pub(crate) struct WaylandAcceptor {
    receiver: Receiver<UnixStream>,
    pending: Arc<AtomicUsize>,
}

impl WaylandAcceptor {
    pub(crate) fn has_pending(&self) -> bool {
        // ORDERING: Acquire pairs with the worker's Release publication; a
        // positive token makes the preceding channel send visible to UI.
        self.pending.load(Ordering::Acquire) != 0
    }

    pub(crate) fn try_recv(&self) -> Result<UnixStream, TryRecvError> {
        let stream = self.receiver.try_recv()?;
        // ORDERING: AcqRel consumes exactly one Release-published ownership
        // token after the channel has transferred the corresponding stream.
        let previous = self.pending.fetch_sub(1, Ordering::AcqRel);
        if previous == 0 {
            diag_line("uiserver: Wayland accept readiness accounting underflow");
            std::process::exit(134);
        }
        Ok(stream)
    }
}

fn set_fd_nonblocking(fd: i32) -> std::io::Result<()> {
    // SAFETY: `fd` is a live accepted socket owned by this worker; F_GETFL
    // neither dereferences a pointer nor transfers descriptor ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: The same live fd remains owned here and F_SETFL only updates its
    // status flags; preserving all existing bits avoids semantic truncation.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn accept_wayland_client(listener: &UnixListener) -> std::io::Result<UnixStream> {
    // SAFETY: The listener fd remains live for the worker lifetime; null peer
    // address pointers are explicitly permitted by accept4 when unused.
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
    // SAFETY: A nonnegative accept4 result is a fresh descriptor whose sole
    // ownership is transferred into this UnixStream exactly once.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

fn create_wayland_accept_epoll(listener: &UnixListener) -> std::io::Result<OwnedFd> {
    // SAFETY: epoll_create1 takes no pointer and returns either a fresh owned
    // descriptor or a negative errno result checked immediately below.
    let raw_epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if raw_epoll < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: The checked nonnegative descriptor is freshly returned and is
    // transferred exactly once into OwnedFd for automatic close on failure.
    let epoll = unsafe { OwnedFd::from_raw_fd(raw_epoll) };
    let mut event = libc::epoll_event {
        events: (libc::EPOLLIN | libc::EPOLLERR | libc::EPOLLHUP) as u32,
        u64: 1,
    };
    // SAFETY: Both descriptors remain live for this call and `event` is a
    // valid writable epoll_event for the duration of epoll_ctl.
    if unsafe {
        libc::epoll_ctl(
            epoll.as_raw_fd(),
            libc::EPOLL_CTL_ADD,
            listener.as_raw_fd(),
            &mut event,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(epoll)
}

pub(crate) fn start_wayland_acceptor(
    listener: UnixListener,
    ui_wake_sender: UiWakeSender,
) -> std::io::Result<WaylandAcceptor> {
    let accept_epoll = create_wayland_accept_epoll(&listener)?;
    let (sender, receiver) = sync_channel(WAYLAND_ACCEPT_QUEUE_CAPACITY);
    let pending = Arc::new(AtomicUsize::new(0));
    let worker_pending = Arc::clone(&pending);
    thread::Builder::new()
        .name(String::from("wayland-accept"))
        .spawn(move || {
            require_background_thread_class();
            // Uiserver owns this listener for its process lifetime. Service
            // retirement terminates all threads together; there is no
            // in-process compositor detach that could orphan this wait.
            loop {
                let mut event = libc::epoll_event { events: 0, u64: 0 };
                // SAFETY: The epoll fd is live in this worker and `event` is a
                // valid one-element output buffer for this blocking call.
                let ready = unsafe {
                    libc::epoll_wait(
                        accept_epoll.as_raw_fd(),
                        &mut event,
                        1,
                        WAYLAND_ACCEPT_WAIT_TIMEOUT_MS,
                    )
                };
                if ready < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == ErrorKind::Interrupted {
                        continue;
                    }
                    diag_line(format!(
                        "uiserver: Wayland accept readiness wait failed: {err}"
                    ));
                    std::process::exit(134);
                }
                if ready == 0 {
                    continue;
                }
                if event.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0 {
                    diag_line("uiserver: Wayland listener readiness revoked");
                    std::process::exit(134);
                }

                loop {
                    match accept_wayland_client(&listener) {
                        Ok(stream) => {
                            if let Err(err) = set_fd_nonblocking(stream.as_raw_fd()) {
                                diag_line(format!(
                                    "uiserver: accepted Wayland client nonblocking failed: {err}"
                                ));
                                continue;
                            }
                            // ORDERING: Release publishes the ownership token
                            // before the channel send and coalesced UI wake.
                            worker_pending.fetch_add(1, Ordering::Release);
                            match sender.try_send(stream) {
                                Ok(()) => ui_wake_sender.signal(),
                                Err(TrySendError::Full(_)) => {
                                    // ORDERING: AcqRel retracts only this
                                    // unpublished stream's Release token.
                                    worker_pending.fetch_sub(1, Ordering::AcqRel);
                                    diag_line(
                                        "uiserver: Wayland accept queue full; client rejected",
                                    );
                                    break;
                                }
                                Err(TrySendError::Disconnected(_)) => {
                                    // ORDERING: No consumer can observe the
                                    // failed send, so retract its exact token.
                                    worker_pending.fetch_sub(1, Ordering::AcqRel);
                                    return;
                                }
                            }
                        }
                        Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                        Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                        Err(err) => {
                            diag_line(format!("uiserver: Wayland accept failed: {err}"));
                            std::process::exit(134);
                        }
                    }
                }
            }
        })?;
    Ok(WaylandAcceptor { receiver, pending })
}

#[cfg(test)]
mod tests {
    use super::{create_wayland_accept_epoll, WAYLAND_ACCEPT_WAIT_TIMEOUT_MS};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixListener;

    #[test]
    fn wayland_accept_uses_blocking_readiness_not_probe_cadence() {
        assert_eq!(WAYLAND_ACCEPT_WAIT_TIMEOUT_MS, -1);
        let socket =
            std::env::temp_dir().join(format!("rustos-wayland-readiness-{}", std::process::id()));
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).expect("bind Wayland listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking Wayland listener");
        let epoll = create_wayland_accept_epoll(&listener).expect("epoll Wayland listener");

        let _client =
            std::os::unix::net::UnixStream::connect(&socket).expect("connect Wayland listener");
        let mut event = libc::epoll_event { events: 0, u64: 0 };
        // SAFETY: `epoll` and `event` are live and the test requests exactly
        // one nonblocking result into the one-element output buffer.
        assert_eq!(
            unsafe { libc::epoll_wait(epoll.as_raw_fd(), &mut event, 1, 0) },
            1
        );
        let event_data = event.u64;
        let event_mask = event.events;
        assert_eq!(event_data, 1);
        assert_ne!(event_mask & libc::EPOLLIN as u32, 0);
        std::fs::remove_file(socket).expect("remove Wayland listener test socket");
    }
}
