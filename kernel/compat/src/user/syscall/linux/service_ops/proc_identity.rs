//! Kernel-attested caller identity for vfsd's `/proc` namespace.
//!
//! vfsd owns the `/proc` policy (which nodes exist, ENOENT for foreign pids)
//! but only the kernel knows the caller's executable path
//! (`UserProcessState::exec_path`). The Ring0 front-end therefore attaches
//! that path, as a mechanism-level fact, to the path-metadata requests that
//! can name `/proc/<pid>/exe` (`readlinkat`, `newfstatat`, `statx`). vfsd uses
//! it only for the caller's own pid, so no other process's path is exposed.

use super::*;

/// Copies `exec_path` into `payload`, NUL-free and bounded; returns the number
/// of bytes copied. A path longer than the payload is dropped entirely rather
/// than truncated, so vfsd can never return a wrong-but-plausible link target.
pub(crate) fn copy_exec_path(payload: &mut [u8], exec_path: &str) -> usize {
    let bytes = exec_path.as_bytes();
    if bytes.len() > payload.len() || bytes.contains(&0) {
        return 0;
    }
    payload[..bytes.len()].copy_from_slice(bytes);
    bytes.len()
}

/// Attaches the calling process's executable path to `request.payload`.
pub(crate) fn attach_caller_exec_path(request: &mut VfsIpcRequest) {
    let len = multitask::with_current_user_process_state(|_, _, process_state| {
        copy_exec_path(&mut request.payload, process_state.exec_path())
    })
    .unwrap_or(0);
    request.payload_len = len as u32;
}

#[cfg(test)]
mod tests {
    use super::copy_exec_path;

    #[test]
    fn exec_path_is_copied_whole_or_not_at_all() {
        let mut payload = [0xff_u8; 8];
        assert_eq!(copy_exec_path(&mut payload, "/a/bc"), 5);
        assert_eq!(&payload[..5], b"/a/bc");
        assert_eq!(copy_exec_path(&mut payload, "/too/long/path"), 0);
        assert_eq!(copy_exec_path(&mut payload, ""), 0);
        assert_eq!(copy_exec_path(&mut payload, "/a\0b"), 0);
    }
}
