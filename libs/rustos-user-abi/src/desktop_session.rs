//! Identity facts shared by every producer and consumer of the desktop
//! session's filesystem layout.
//!
//! Nothing here is a launch-time fact: the uid a launch actually runs as is
//! decided per launch by [`launch_credentials`] (the loader spawn request
//! carries only the `logical_admin` bit; ring0 maps it to uid/gid 0 or the
//! desktop identity, and `kernel-ps` `DEFAULT_DESKTOP_UID/GID` must stay equal
//! to [`DESKTOP_SESSION_UID`]/[`DESKTOP_SESSION_GID`]). Every path derived from
//! the uid goes through [`RuntimeDir`], so runtimed (which advertises it as
//! `XDG_RUNTIME_DIR`), uiserver (which binds [`WAYLAND_SOCKET_NAME`] inside
//! it) and vfsd (which seeds the desktop session's directory) cannot disagree.

use core::fmt;

/// Uid of the interactive desktop session and of every non-admin application.
pub const DESKTOP_SESSION_UID: u32 = 1000;
/// Gid of the interactive desktop session and of every non-admin application.
pub const DESKTOP_SESSION_GID: u32 = 1000;
/// Parent of every per-uid runtime directory; seeded 0755 root by vfsd.
pub const RUN_USER_DIR: &str = "/run/user";
/// Wayland socket name inside a [`RuntimeDir`] (`WAYLAND_DISPLAY`).
pub const WAYLAND_SOCKET_NAME: &str = "wayland-0";
/// Mount point of the volatile world-writable TmpFs (`TMPDIR`).
pub const TEMP_DIR: &str = "/tmp";
/// Mount point of the optional persistent data volume (second FAT partition).
pub const PERSISTENT_DATA_DIR: &str = "/data";

/// `/run/user/<uid>`: the session directory of `uid` (`XDG_RUNTIME_DIR`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeDir(pub u32);

impl fmt::Display for RuntimeDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{RUN_USER_DIR}/{}", self.0)
    }
}

/// `(uid, gid)` a loader spawn runs as for the request's `logical_admin` bit.
pub const fn launch_credentials(logical_admin: bool) -> (u32, u32) {
    if logical_admin {
        (0, 0)
    } else {
        (DESKTOP_SESSION_UID, DESKTOP_SESSION_GID)
    }
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use super::*;

    struct Buf {
        bytes: [u8; 48],
        len: usize,
    }

    impl Write for Buf {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.len + text.len();
            self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?.copy_from_slice(text.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    fn render(dir: RuntimeDir) -> Buf {
        let mut buf = Buf { bytes: [0; 48], len: 0 };
        write!(buf, "{dir}").unwrap();
        buf
    }

    #[test]
    fn runtime_dir_is_run_user_uid() {
        let buf = render(RuntimeDir(DESKTOP_SESSION_UID));
        assert_eq!(&buf.bytes[..buf.len], b"/run/user/1000");
        let buf = render(RuntimeDir(0));
        assert_eq!(&buf.bytes[..buf.len], b"/run/user/0");
    }

    #[test]
    fn launch_credentials_follow_the_admin_bit() {
        assert_eq!(launch_credentials(true), (0, 0));
        assert_eq!(launch_credentials(false), (DESKTOP_SESSION_UID, DESKTOP_SESSION_GID));
    }
}
