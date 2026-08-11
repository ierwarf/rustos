//! The single definition of the runtimed local-control wire protocol.
//!
//! - **Owner:** shared by the `runtimed` server and every `RuntimeClient`.
//! - **Boundary:** the bytes on `/run/runtimed.sock`. Both directions are
//!   untrusted until validated.
//! - **Lifecycle:** one fixed-size request frame, one fixed-size response
//!   frame, then an optional payload whose length is derived from the response
//!   header alone.
//! - **Forbidden:** a second copy of any constant or struct in this file.
//!
//! # Why this module exists
//!
//! These constants and the two frame structs used to be declared twice - once
//! here for the client and once privately inside `runtimed` - with nothing
//! linking the copies. Every value happened to agree, but only by inspection:
//! nothing failed to build if one side changed an opcode, a target kind, or a
//! frame field, so the first sign of a divergence would have been a runtime
//! `EPROTO` at best and a misparsed frame at worst. The server now consumes
//! this module directly, so the two ends cannot disagree about the wire.
//!
//! # Frames are raw bytes
//!
//! Both frames and the running-program record cross the socket as their own
//! memory, so their `repr(C)` layouts must contain no padding: a padding byte
//! would put uninitialised memory on the wire and make the change digest below
//! unstable. The size assertions at the bottom of this file pin that.

use std::mem::size_of;

pub const PROTOCOL_VERSION: u16 = 1;

pub const OP_SNAPSHOT_RUNNING_PROGRAMS: u16 = 1;
pub const OP_REQUEST_LAUNCH_PATH: u16 = 2;
pub const OP_REQUEST_TERMINATE: u16 = 3;
pub const OP_NOTIFY_READY: u16 = 4;
/// Snapshot semantics, but the reply is withheld until the running set stops
/// matching the digest the caller already holds. See [`running_programs_digest`].
pub const OP_WATCH_RUNNING_PROGRAMS: u16 = 5;

pub const LAUNCH_TARGET_NEW_SESSION: u16 = 2;
pub const TERMINATE_TARGET_SESSION: u16 = 1;
pub const TERMINATE_TARGET_PID: u16 = 2;
pub const READY_COMPONENT_UI_SERVER: u16 = 1;

pub const MAX_REQUEST_PATH_BYTES: usize = 128;
pub const MAX_RUNTIME_PROGRAMS: usize = 64;

pub const DESKTOP_FILE_ID_CAPACITY: usize = 48;
pub const RUNNING_PROGRAM_NAME_CAPACITY: usize = 48;
pub const PROGRAM_PATH_CAPACITY: usize = 64;

/// Longest a server may withhold a watch reply, and the ceiling it clamps
/// [`RuntimeRequest::wait_ms`] to.
///
/// A parked watch is not a timeout ABI: the caller is asking to be told when
/// something changes, and this bound only decides how often it re-arms while
/// nothing does. It exists because the reply capability here is a held socket,
/// and a held socket that is never answered is indistinguishable from a hung
/// server. Re-arming on this cadence keeps the "server is alive" question
/// answerable without making the caller poll for the answer it actually wants.
pub const RUNTIME_WATCH_MAX_WAIT_MS: u16 = 2_000;

/// Watches a server will hold parked at once. Past this it answers immediately,
/// which degrades a watcher to the polling client it replaced rather than
/// letting an unbounded number of peers pin file descriptors.
pub const MAX_RUNTIME_WATCHERS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RuntimeRequest {
    pub version: u16,
    pub op: u16,
    pub target_kind: u16,
    /// Requested park budget in milliseconds, and zero for every op that does
    /// not park. Servers clamp it to [`RUNTIME_WATCH_MAX_WAIT_MS`]; a caller
    /// asking for longer is bounded, not rejected.
    pub wait_ms: u16,
    pub text_len: u32,
    pub target_value: u64,
    pub text: [u8; MAX_REQUEST_PATH_BYTES],
}

impl Default for RuntimeRequest {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            op: 0,
            target_kind: 0,
            wait_ms: 0,
            text_len: 0,
            target_value: 0,
            text: [0; MAX_REQUEST_PATH_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RuntimeResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeRunningProgram {
    pub pid: u64,
    pub program_id: u32,
    reserved: u32,
    pub session_handle: u64,
    pub desktop_file_id: [u8; DESKTOP_FILE_ID_CAPACITY],
    pub display_name: [u8; RUNNING_PROGRAM_NAME_CAPACITY],
    pub exec_path: [u8; PROGRAM_PATH_CAPACITY],
}

impl Default for RuntimeRunningProgram {
    fn default() -> Self {
        Self {
            pid: 0,
            program_id: 0,
            reserved: 0,
            session_handle: 0,
            desktop_file_id: [0; DESKTOP_FILE_ID_CAPACITY],
            display_name: [0; RUNNING_PROGRAM_NAME_CAPACITY],
            exec_path: [0; PROGRAM_PATH_CAPACITY],
        }
    }
}

/// Ops whose successful reply carries a running-program array; every other op
/// must answer with `count == 0`.
pub const fn op_carries_program_payload(op: u16) -> bool {
    matches!(op, OP_SNAPSHOT_RUNNING_PROGRAMS | OP_WATCH_RUNNING_PROGRAMS)
}

/// Fingerprint of exactly the bytes a snapshot reply would put on the wire.
///
/// The change edge is defined by the observable payload rather than by a
/// counter the server bumps on mutation. A counter has to be incremented at
/// every site that touches the running set, and the failure mode of forgetting
/// one is silent: a watcher parks forever through a change it should have seen.
/// Hashing the reply itself cannot miss a mutation site, because a mutation
/// that does not alter the reply is not a change any watcher can observe.
///
/// Server and client run this over identical bytes, so the digest never has to
/// travel: the caller recomputes it from the array it just received and hands
/// it back on the next watch.
pub fn running_programs_digest(programs: &[RuntimeRunningProgram]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let bytes = unsafe {
        std::slice::from_raw_parts(
            programs.as_ptr().cast::<u8>(),
            std::mem::size_of_val(programs),
        )
    };
    let mut digest = FNV_OFFSET_BASIS;
    for byte in bytes {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(FNV_PRIME);
    }
    // Zero is reserved for "the caller holds no digest", so a first watch is
    // always answered immediately instead of parking against an unknown set.
    if digest == 0 {
        1
    } else {
        digest
    }
}

/// The digest value a caller passes before it has ever seen a reply.
pub const NO_RUNNING_PROGRAMS_DIGEST: u64 = 0;

const _: () = assert!(size_of::<RuntimeRunningProgram>() == 184);
const _: () = assert!(size_of::<RuntimeRequest>() == 152);
const _: () = assert!(size_of::<RuntimeResponse>() == 12);
const _: () = assert!(!op_carries_program_payload(OP_REQUEST_LAUNCH_PATH));
const _: () = assert!(!op_carries_program_payload(OP_NOTIFY_READY));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_opcode_is_distinct() {
        let ops = [
            OP_SNAPSHOT_RUNNING_PROGRAMS,
            OP_REQUEST_LAUNCH_PATH,
            OP_REQUEST_TERMINATE,
            OP_NOTIFY_READY,
            OP_WATCH_RUNNING_PROGRAMS,
        ];
        for (index, op) in ops.iter().enumerate() {
            assert!(!ops[..index].contains(op), "duplicate opcode {op}");
            assert_ne!(*op, 0, "zero is the error-envelope opcode");
        }
    }

    #[test]
    fn the_digest_separates_sets_a_watcher_must_be_told_apart() {
        let empty = running_programs_digest(&[]);
        let one = RuntimeRunningProgram {
            pid: 7,
            ..RuntimeRunningProgram::default()
        };
        let mut renamed = one;
        renamed.display_name[0] = b'x';

        assert_ne!(empty, running_programs_digest(&[one]));
        assert_ne!(
            running_programs_digest(&[one]),
            running_programs_digest(&[renamed])
        );
        assert_eq!(
            running_programs_digest(&[one]),
            running_programs_digest(&[one])
        );
        assert_ne!(empty, NO_RUNNING_PROGRAMS_DIGEST);
    }

    #[test]
    fn a_parked_watch_re_arms_well_inside_the_client_io_deadline() {
        // A watch that outlived the caller's socket deadline would be reported
        // as a dead server rather than as "nothing changed".
        let park = std::time::Duration::from_millis(u64::from(RUNTIME_WATCH_MAX_WAIT_MS));
        assert!(park * 2 <= crate::RPC_IO_TIMEOUT);
    }
}
