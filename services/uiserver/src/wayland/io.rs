//! Socket / runtime-dir setup for the Wayland compositor.
//!
//! Nothing in here knows about Wayland protocol state. The functions handle
//! the underlying Unix socket plumbing (creation, nonblocking flag, stale
//! socket detection) so the main compositor module can stay focused on the
//! protocol state machine.

use std::ffi::CString;
use std::fs;
use std::io::ErrorKind;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path};

const FALLBACK_RUNTIME_DIR: &str = "/run/user/1000";

pub(super) fn set_fd_nonblocking(fd: i32) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn current_runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|value| safe_runtime_dir(value.as_str()))
        .unwrap_or_else(|| String::from(FALLBACK_RUNTIME_DIR))
}

pub(super) fn bind_wayland_listener(
    runtime_dir: &str,
    socket_path: &str,
) -> std::io::Result<UnixListener> {
    fs::create_dir_all(runtime_dir)?;
    match bind_nonblocking_unix_listener(socket_path) {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            if stale_wayland_socket(runtime_dir, socket_path)? {
                fs::remove_file(socket_path)?;
                bind_nonblocking_unix_listener(socket_path)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

fn bind_nonblocking_unix_listener(socket_path: &str) -> std::io::Result<UnixListener> {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| {
        let path = CString::new(socket_path)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let path_bytes = path.as_bytes_with_nul();
        let mut addr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        if path_bytes.len() > addr.sun_path.len() {
            return Err(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        for (index, byte) in path_bytes.iter().enumerate() {
            addr.sun_path[index] = *byte as libc::c_char;
        }

        let bind_rc = unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_un).cast::<libc::sockaddr>(),
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if bind_rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::listen(fd, 16) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();

    if let Err(err) = result {
        let _ = unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(unsafe { UnixListener::from_raw_fd(fd) })
}

fn stale_wayland_socket(runtime_dir: &str, socket_path: &str) -> std::io::Result<bool> {
    let socket = Path::new(socket_path);
    let runtime = Path::new(runtime_dir);
    if !socket.starts_with(runtime) {
        return Ok(false);
    }
    if !fs::symlink_metadata(socket_path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
    {
        return Ok(false);
    }

    match UnixStream::connect(socket_path) {
        Ok(_) => Ok(false),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::ConnectionReset
            ) =>
        {
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

fn safe_runtime_dir(value: &str) -> bool {
    // sun_path is 108 bytes; we reserve room for the WAYLAND_SOCKET_NAME the
    // compositor appends + the null terminator.
    let socket_name_budget = super::WAYLAND_SOCKET_NAME.len() + 1;
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 108 - socket_name_budget
        && path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::RootDir | Component::Normal(_) | Component::Prefix(_)
            )
        })
}
