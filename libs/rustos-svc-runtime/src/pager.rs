//! Pagerd's fixed fault-rendezvous syscall wrappers.

use crate::syscall::syscall1;
use rustos_user_abi::pager::{PagerFaultDispatchWire, PagerFaultReplyWire};
use rustos_user_abi::syscall::{
    PAGER_FAULT_RENDEZVOUS_ABI_VERSION, RustosPagerFaultReplyArgs, RustosPagerFaultWaitArgs,
    SYS_RUSTOS_PAGER_FAULT_REPLY, SYS_RUSTOS_PAGER_FAULT_WAIT,
};

/// Wait for one kernel-stamped, worker-bound anonymous page-fault dispatch.
#[inline]
pub unsafe fn fault_wait(dispatch_out: &mut PagerFaultDispatchWire) -> i64 {
    let args = RustosPagerFaultWaitArgs {
        abi_version: PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
        flags: 0,
        reserved0: 0,
        dispatch_out: (dispatch_out as *mut PagerFaultDispatchWire) as u64,
    };
    syscall1(
        SYS_RUSTOS_PAGER_FAULT_WAIT,
        (&args as *const RustosPagerFaultWaitArgs) as u64,
    )
}

/// Return pagerd's exact response for a dispatch previously received by this
/// worker. The kernel rejects stale or cross-worker tokens.
#[inline]
pub unsafe fn fault_reply(reply: PagerFaultReplyWire) -> i64 {
    let args = RustosPagerFaultReplyArgs {
        abi_version: PAGER_FAULT_RENDEZVOUS_ABI_VERSION,
        flags: 0,
        reserved0: 0,
        reply,
    };
    syscall1(
        SYS_RUSTOS_PAGER_FAULT_REPLY,
        (&args as *const RustosPagerFaultReplyArgs) as u64,
    )
}

fn source_control(args: rustos_user_abi::syscall::RustosFrameSourceArgs) -> Result<u64, i64> {
    // SAFETY: fixed-size argument storage lives through the synchronous copy.
    let result = unsafe {
        syscall1(
            rustos_user_abi::syscall::SYS_RUSTOS_FRAME_SOURCE,
            (&args as *const rustos_user_abi::syscall::RustosFrameSourceArgs) as u64,
        )
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as u64)
    }
}

/// Supply one immutable cache source. The kernel captures the live pager lease
/// and charges the pager's Job; this does not map it into an application.
/// Rights require READ and allow optional EXECUTE, never source WRITE. Private
/// writable mappings read this source and write a separately owned COW frame.
pub fn source_supply(
    key: rustos_user_abi::syscall::FrameSourceKeyWire,
    rights: u64,
    bytes: &[u8],
) -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    if bytes.len() > 4096 {
        return Err(-22);
    }
    source_control(RustosFrameSourceArgs {
        abi_version: FRAME_SOURCE_ABI_VERSION,
        op: FRAME_SOURCE_OP_SUPPLY,
        key,
        rights,
        payload_ptr: if bytes.is_empty() {
            0
        } else {
            bytes.as_ptr() as u64
        },
        payload_len: bytes.len() as u64,
        ..Default::default()
    })
}

/// Withdraw cache ownership without revoking already installed private views.
/// Physical storage is released only by bounded cold cleanup after all source
/// pins and mapped-leaf references have returned.
pub fn source_retire(token: u64) -> Result<(), i64> {
    use rustos_user_abi::syscall::*;
    source_control(RustosFrameSourceArgs {
        abi_version: FRAME_SOURCE_ABI_VERSION,
        op: FRAME_SOURCE_OP_RETIRE,
        token,
        ..Default::default()
    })
    .map(|_| ())
}

/// Exact current service publication epoch, not the endpoint object's reusable
/// identity and not a credential. Admission must still validate its authority.
pub fn source_owner_epoch() -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    source_control(RustosFrameSourceArgs {
        abi_version: FRAME_SOURCE_ABI_VERSION,
        op: FRAME_SOURCE_OP_OWNER_EPOCH,
        ..Default::default()
    })
}

fn library_control(
    args: rustos_user_abi::syscall::RustosLibraryObjectArgs,
) -> Result<u64, i64> {
    // SAFETY: fixed-size argument storage lives through the synchronous copy.
    let result = unsafe {
        syscall1(
            rustos_user_abi::syscall::SYS_RUSTOS_LIBRARY_OBJECT,
            (&args as *const rustos_user_abi::syscall::RustosLibraryObjectArgs) as u64,
        )
    };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as u64)
    }
}

/// Start an unsealed shared-library cache object for one attested immutable
/// file identity. Invisible to guests until `library_seal`.
pub fn library_create(
    identity: rustos_user_abi::syscall::LibraryIdentityWire,
) -> Result<u64, i64> {
    use rustos_user_abi::syscall::*;
    library_control(RustosLibraryObjectArgs {
        abi_version: LIBRARY_OBJECT_ABI_VERSION,
        op: LIBRARY_OBJECT_OP_CREATE,
        identity,
        ..Default::default()
    })
}

/// Supply one page; the final page carries only the file's remaining bytes.
pub fn library_supply_page(object: u64, page_index: u64, bytes: &[u8]) -> Result<(), i64> {
    use rustos_user_abi::syscall::*;
    if bytes.is_empty() || bytes.len() > 4096 {
        return Err(-22);
    }
    library_control(RustosLibraryObjectArgs {
        abi_version: LIBRARY_OBJECT_ABI_VERSION,
        op: LIBRARY_OBJECT_OP_SUPPLY_PAGE,
        object,
        page_index,
        payload_ptr: bytes.as_ptr() as u64,
        payload_len: bytes.len() as u64,
        ..Default::default()
    })
    .map(|_| ())
}

pub fn library_seal(object: u64) -> Result<(), i64> {
    use rustos_user_abi::syscall::*;
    library_control(RustosLibraryObjectArgs {
        abi_version: LIBRARY_OBJECT_ABI_VERSION,
        op: LIBRARY_OBJECT_OP_SEAL,
        object,
        ..Default::default()
    })
    .map(|_| ())
}

/// Allocator counters for the pager's retention policy (read-only).
pub fn library_pressure() -> Result<rustos_user_abi::syscall::LibraryPressureWire, i64> {
    use rustos_user_abi::syscall::*;
    let mut wire = LibraryPressureWire::default();
    library_control(RustosLibraryObjectArgs {
        abi_version: LIBRARY_OBJECT_ABI_VERSION,
        op: LIBRARY_OBJECT_OP_PRESSURE,
        payload_ptr: (&mut wire as *mut LibraryPressureWire) as u64,
        payload_len: core::mem::size_of::<LibraryPressureWire>() as u64,
        ..Default::default()
    })?;
    Ok(wire)
}

/// Withdraw the object from new bindings; existing private views stay valid.
pub fn library_retire(object: u64) -> Result<(), i64> {
    use rustos_user_abi::syscall::*;
    library_control(RustosLibraryObjectArgs {
        abi_version: LIBRARY_OBJECT_ABI_VERSION,
        op: LIBRARY_OBJECT_OP_RETIRE,
        object,
        ..Default::default()
    })
    .map(|_| ())
}
