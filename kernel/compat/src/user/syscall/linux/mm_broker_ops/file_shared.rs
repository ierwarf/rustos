//! Writable `MAP_SHARED` of a TmpFs file through its vfsd-owned shared frames.
//!
//! - **Owner:** vfsd owns the file bytes (a memfd it keeps per shared-backed
//!   TmpFs inode, see `services/vfsd/src/tmpfs.rs`); syscalld owns the mmap
//!   policy; ring0 only maps the frames and holds them.
//! - **Boundary:** the client's descriptor must be a remote-VFS regular file
//!   with the rights the mapping needs. vfsd names the memfd by
//!   `(descriptor, object id)`; neither the guest nor syscalld supplies them.
//! - **Lifecycle:** the mapping takes an ordinary memfd mapping hold, so the
//!   frames outlive an `unlink`, a vfsd close and the client's descriptor and
//!   die with the last mapping. Because the hold is the memfd's own, `fork`,
//!   `munmap`, `mprotect` and exit use the existing shared-memfd paths.
//! - **Concurrency:** the vfsd round trip runs with no process-state lock
//!   held. The descriptor pair is revalidated against vfsd's table under that
//!   table's lock, the object id is never recycled, and `acquire_mapping`
//!   re-checks the extent under the memfd lock, so a shrink racing the lookup
//!   either completes first (the map is refused) or finds the mapping and is
//!   refused itself with `EBUSY`.
//! - **Failure:** every refusal happens before a PTE exists; `ENODEV` from
//!   vfsd (FAT, early-system image) becomes `EACCES`, the answer a writable
//!   shared mapping of those files has always had.
//! - **Forbidden:** mapping a memfd not named by vfsd's own reply, trusting a
//!   descriptor number without its object id, or copying the file into a
//!   private region and calling it shared.
use super::*;

use rustos_user_abi::syscall::{VFS_IPC_OP_SHARED_BACKING, VFS_SHARED_BACKING_FLAG_WRITE};

const LINUX_ENODEV_ERRNO: i64 = 19;

/// Asks vfsd for the shared backing of `remote_id`, on behalf of its owning
/// process (the broker runs in syscalld's context; see
/// `call_remote_vfs_file_identity` for the identity split).
fn call_remote_vfs_shared_backing(
    remote_id: u64,
    client_pid: u64,
    writable: bool,
) -> Result<(u64, u64), i64> {
    let mut request = new_vfs_request(VFS_IPC_OP_SHARED_BACKING);
    request.remote_id = remote_id;
    request.pid = client_pid;
    request.arg0 = if writable { VFS_SHARED_BACKING_FLAG_WRITE } else { 0 };
    let response = call_vfs_ipc_request(&request)?;
    ensure_vfs_status(&response)?;
    decode_shared_backing(response.value, response.aux)
}

/// A reply names a descriptor and a non-zero memfd object id, nothing else.
fn decode_shared_backing(token: u64, object: u64) -> Result<(u64, u64), i64> {
    if object == 0 || i32::try_from(token).is_err() {
        return Err(LINUX_EINVAL);
    }
    Ok((token, object))
}

/// Maps vfsd's own memfd, found by descriptor *and* object id in vfsd's table.
fn service_memfd(service_pid: u64, token: u64, object: u64) -> Option<MemfdHandle> {
    multitask::with_process_state_by_pid(service_pid, |state| {
        match state.handles().get_entry(token)?.handle() {
            KernelHandle::Memfd(memfd) if memfd.object_id() == object => Some(memfd.clone()),
            _ => None,
        }
    })?
}

/// Maps a vfsd answer onto the errno `mmap` reports.
fn map_vfs_errno(errno: i64) -> i64 {
    if errno == LINUX_ENODEV_ERRNO {
        LINUX_EACCES
    } else {
        errno
    }
}

pub(super) fn broker_map_file_shared(args: &RustosMmBrokerArgs, t: MmTarget) -> Result<(), i64> {
    let len = checked_len(args.len)?;
    let start = checked_page_addr(args.addr)?;
    let mapping_end = checked_mapping_end(start, page_count(len)?)?;
    let writable = args.prot & linux_abi::PROT_WRITE != 0;
    let page_flags = protection_to_page_flags(args.prot)?;
    let offset = usize::try_from(args.offset).map_err(|_| LINUX_EINVAL)?;

    let remote_id = multitask::with_process_state_by_generation_mut(
        t.pid,
        t.process_generation,
        t.mm_generation,
        |process_state| {
            let entry = process_state.handles().get_entry(args.fd).ok_or(LINUX_EBADF)?;
            if !entry.rights().allows_read() || (writable && !entry.rights().allows_write()) {
                return Err(LINUX_EACCES);
            }
            match entry.handle() {
                KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::File => {
                    Ok(remote.remote_id())
                }
                _ => Err(LINUX_EINVAL),
            }
        },
    )
    .ok_or(LINUX_ESRCH)??;

    let (token, object) = match call_remote_vfs_shared_backing(remote_id, t.pid, writable) {
        Ok(pair) => pair,
        // Immutable non-shared backings (e.g. early-system image) have no live
        // writers; read-only MAP_SHARED safely falls back to a private copy.
        Err(LINUX_ENODEV_ERRNO) if !writable => return super::broker_map_file_private(args, t),
        Err(errno) => return Err(map_vfs_errno(errno)),
    };
    let (service_pid, _epoch) =
        ipc_ops::live_service_endpoint_owner_and_epoch(rustos_user_abi::syscall::IPC_SERVICE_VFSD)
            .ok_or(LINUX_ESTALE)?;
    let memfd = service_memfd(service_pid, token, object).ok_or(LINUX_ESTALE)?;
    let result = memfd.with_reconcile_lock(|| {
        let (frames, page_count, hold) = memfd
            .acquire_mapping_with_eof_guard(offset, len, writable)
            .map_err(memfd_error_to_errno)?;

        multitask::with_process_state_by_generation_mut(
            t.pid,
            t.process_generation,
            t.mm_generation,
            |process_state| {
                if !process_state.mapping_below_stack_guard(start, mapping_end) {
                    return Err(LINUX_ENOMEM);
                }
                if !process_state.admit_mapping_records(1) {
                    return Err(LINUX_ENOMEM);
                }
                process_state
                    .address_space_mut()
                    .map_external_user_pages_with_eof_guard(
                        VirtAddr::new(start),
                        &frames,
                        page_count,
                        page_flags,
                    )
                    .map_err(address_space_error_to_linux_errno)?;
                process_state.record_shared_memfd_mapping(
                    start,
                    args.len,
                    args.offset,
                    page_flags,
                    hold,
                );
                process_state.set_mapping_cursor(mapping_end);
                Ok::<RustosMmMapBrokerResult, i64>(RustosMmMapBrokerResult {
                    addr: start,
                    len: args.len,
                })
            },
        )
        .ok_or(LINUX_ESRCH)?
    })?;
    publish_map_result(args, t, &result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_must_name_a_descriptor_and_an_object() {
        assert_eq!(decode_shared_backing(5, 9), Ok((5, 9)));
        assert_eq!(decode_shared_backing(5, 0), Err(LINUX_EINVAL));
        assert_eq!(decode_shared_backing(1 << 32, 9), Err(LINUX_EINVAL));
    }

    #[test]
    fn only_not_a_tmpfs_file_becomes_access_denied() {
        assert_eq!(map_vfs_errno(LINUX_ENODEV_ERRNO), LINUX_EACCES);
        assert_eq!(map_vfs_errno(28), 28);
        assert_eq!(map_vfs_errno(LINUX_EACCES), LINUX_EACCES);
    }

    #[test]
    fn mapping_follows_the_memfd_hold_not_a_private_copy() {
        let source = include_str!("file_shared.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(production.contains("record_shared_memfd_mapping"));
        assert!(production.contains("acquire_mapping_with_eof_guard"));
        assert!(production.contains("map_external_user_pages_with_eof_guard"));
        assert!(production.contains("with_reconcile_lock"));
        for forbidden in ["map_zeroed_user_bytes_at", "copy_file_mapping", "with_process_state_by_pid_mut"] {
            assert!(!production.contains(forbidden), "{forbidden} would make this a copy or a by-pid mutation");
        }
    }
}
